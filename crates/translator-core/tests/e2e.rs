//! End-to-end check: spawn the real `translator-core` binary, feed it synthetic
//! system audio over a named pipe and assert that subtitles come back.

#![cfg(windows)]

use std::process::{Child, Command};
use std::time::{Duration, Instant};

use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
use translator_protocol::envelope::Payload;
use translator_protocol::framing::{read_frame, write_frame};
use translator_protocol::{
    AsrConfig, AudioFormat, AudioFrame, Envelope, HandshakeRequest, SampleFormat, StartSessionRequest,
    StopSessionRequest, SubtitleConfig, SubtitleKind, TranslationConfig, PROTOCOL_VERSION,
};

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: usize = 2;
const CHUNK_MS: u32 = 20;

fn core_binary() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_translator-core"))
}

fn spawn_core(pipe: &str) -> Child {
    Command::new(core_binary())
        .args(["--pipe", pipe, "--log-level", "warn"])
        .spawn()
        .expect("failed to spawn translator-core")
}

fn connect_with_retry(pipe: &str) -> NamedPipeClient {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match ClientOptions::new().open(pipe) {
            Ok(client) => return client,
            Err(e) => {
                assert!(
                    Instant::now() < deadline,
                    "core never started listening on {pipe}: {e}"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Generate `seconds` of a 440 Hz tone (or near-silence for tiny amplitudes).
fn burst(seconds: f64, amplitude: f32, phase: &mut f64) -> Vec<f32> {
    let frames = (SAMPLE_RATE as f64 * seconds) as usize;
    let mut out = Vec::with_capacity(frames * CHANNELS);
    for _ in 0..frames {
        let value = amplitude * (2.0 * std::f64::consts::PI * 440.0 * *phase).sin() as f32;
        *phase += 1.0 / SAMPLE_RATE as f64;
        for _ in 0..CHANNELS {
            out.push(value);
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streams_subtitles_over_a_named_pipe() {
    let pipe = format!(r"\\.\pipe\translator-core-e2e-{}", std::process::id());
    let mut child = spawn_core(&pipe);
    let client = connect_with_retry(&pipe);

    let (mut reader, mut writer) = tokio::io::split(client);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Envelope>();
    let reader_task = tokio::spawn(async move {
        while let Ok(Some(envelope)) = read_frame(&mut reader).await {
            if tx.send(envelope).is_err() {
                break;
            }
        }
    });

    let mut seq = 1u32;

    // --- handshake ---------------------------------------------------------
    write_frame(
        &mut writer,
        &Envelope {
            seq,
            payload: Some(Payload::HandshakeRequest(HandshakeRequest {
                protocol_version: PROTOCOL_VERSION,
                client_id: "e2e".to_string(),
                client_version: "0.0.0".to_string(),
                features: Vec::new(),
            })),
        },
    )
    .await
    .unwrap();

    // --- start session -----------------------------------------------------
    seq += 1;
    write_frame(
        &mut writer,
        &Envelope {
            seq,
            payload: Some(Payload::StartSessionRequest(StartSessionRequest {
                session_id: "e2e-session".to_string(),
                input_format: Some(AudioFormat {
                    sample_rate: SAMPLE_RATE,
                    channels: CHANNELS as u32,
                    format: SampleFormat::F32 as i32,
                }),
                target_sample_rate: 16_000,
                asr: Some(AsrConfig {
                    provider: "mock".to_string(),
                    model: String::new(),
                    language: String::new(),
                }),
                translation: Some(TranslationConfig {
                    provider: "none".to_string(),
                    target_language: String::new(),
                }),
                subtitle: Some(SubtitleConfig {
                    max_lines: 2,
                    max_chars_per_line: 40,
                    partial_interval_ms: 100,
                    commit_silence_ms: 300,
                }),
            })),
        },
    )
    .await
    .unwrap();

    // --- feed 3 utterances separated by silence ----------------------------
    let mut phase = 0.0f64;
    let mut audio: Vec<f32> = Vec::new();
    for _ in 0..3 {
        audio.extend(burst(1.2, 0.3, &mut phase));
        audio.extend(burst(0.8, 0.00001, &mut phase));
    }

    let samples_per_chunk = SAMPLE_RATE as usize * CHUNK_MS as usize / 1000 * CHANNELS;
    let mut timestamp_us = 0u64;

    for chunk in audio.chunks(samples_per_chunk) {
        let frames = chunk.len() / CHANNELS;
        timestamp_us += CHUNK_MS as u64 * 1000;

        let mut pcm = Vec::with_capacity(chunk.len() * 4);
        for sample in chunk {
            pcm.extend_from_slice(&sample.to_le_bytes());
        }

        seq += 1;
        write_frame(
            &mut writer,
            &Envelope {
                seq,
                payload: Some(Payload::AudioFrame(AudioFrame {
                    session_id: "e2e-session".to_string(),
                    timestamp_us,
                    format: Some(AudioFormat {
                        sample_rate: SAMPLE_RATE,
                        channels: CHANNELS as u32,
                        format: SampleFormat::F32 as i32,
                    }),
                    frames: frames as u32,
                    pcm,
                })),
            },
        )
        .await
        .unwrap();
    }

    // --- collect responses -------------------------------------------------
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut handshake_accepted = false;
    let mut session_accepted = false;
    let mut partials = 0usize;
    let mut committed = 0usize;
    let mut last_lines: Vec<String> = Vec::new();

    while committed < 3 && Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
            Ok(Some(envelope)) => match envelope.payload {
                Some(Payload::HandshakeResponse(r)) => handshake_accepted = r.accepted,
                Some(Payload::StartSessionResponse(r)) => session_accepted = r.accepted,
                Some(Payload::SubtitleEvent(e)) => {
                    match SubtitleKind::try_from(e.kind) {
                        Ok(SubtitleKind::Partial) => partials += 1,
                        Ok(SubtitleKind::Committed) => committed += 1,
                        _ => {}
                    }
                    last_lines = e.lines;
                }
                Some(Payload::ErrorEvent(e)) => panic!("core reported an error: {}", e.message),
                _ => {}
            },
            Ok(None) => break,
            Err(_) => continue,
        }
    }

    assert!(handshake_accepted, "handshake was rejected");
    assert!(session_accepted, "session start was rejected");
    assert!(partials >= 3, "expected partial subtitles, got {partials}");
    assert!(committed >= 2, "expected committed subtitles, got {committed}");
    assert!(!last_lines.is_empty(), "subtitle payload carried no lines");

    // --- stop and verify the core exits with the client --------------------
    seq += 1;
    write_frame(
        &mut writer,
        &Envelope {
            seq,
            payload: Some(Payload::StopSessionRequest(StopSessionRequest {
                session_id: "e2e-session".to_string(),
            })),
        },
    )
    .await
    .unwrap();

    // Both halves of `split` must go away before the OS closes the pipe handle
    // and the core notices the client is gone.
    reader_task.abort();
    drop(writer);

    let wait_until = Instant::now() + Duration::from_secs(10);
    let mut exited = false;
    while Instant::now() < wait_until {
        match child.try_wait() {
            Ok(Some(_)) => {
                exited = true;
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => break,
        }
    }
    if !exited {
        let _ = child.kill();
    }
    assert!(exited, "core did not exit after the client disconnected");
}
