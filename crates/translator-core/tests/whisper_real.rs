//! End-to-end check of the local Whisper integration against a real speech clip.
//!
//! A 16 kHz mono WAV (`tests/speech_test.wav`, generated with Windows TTS) is
//! decoded and streamed into `WhisperAsr` exactly the way the live session would
//! feed 16 kHz mono frames. We then assert the engine produced non-empty text
//! containing at least one expected keyword, which proves the model loads and the
//! FFI transcription pipeline works.

#![cfg(feature = "whisper")]

use translator_core::asr::{self, AsrOptions, RecognitionEvent};

fn read_wav_mono_f32(path: &str) -> Vec<f32> {
    let data = std::fs::read(path).expect("read wav");
    assert_eq!(&data[0..4], b"RIFF");
    assert_eq!(&data[8..12], b"WAVE");

    let mut pos = 12usize;
    let mut pcm_start = 0usize;
    let mut pcm_len = 0usize;
    let mut channels = 1usize;
    while pos + 8 <= data.len() {
        let id = &data[pos..pos + 4];
        let size =
            u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = pos + 8;
        if id == b"fmt " {
            channels =
                u16::from_le_bytes(data[body + 2..body + 4].try_into().unwrap()) as usize;
            let bits =
                u16::from_le_bytes(data[body + 14..body + 16].try_into().unwrap()) as usize;
            assert_eq!(bits, 16, "only 16-bit PCM is supported by this test");
        } else if id == b"data" {
            pcm_start = body;
            pcm_len = size;
            break;
        }
        pos = body + size + (size & 1);
    }

    let samples = pcm_len / 2;
    let mut out = Vec::with_capacity(samples);
    for i in 0..samples {
        let s = i16::from_le_bytes(
            data[pcm_start + i * 2..pcm_start + i * 2 + 2]
                .try_into()
                .unwrap(),
        );
        out.push(s as f32 / 32768.0);
    }
    if channels == 2 {
        out = out
            .chunks(2)
            .map(|c| if c.len() == 2 { (c[0] + c[1]) / 2.0 } else { c[0] })
            .collect();
    }
    out
}

#[tokio::test]
async fn transcribes_speech_wav() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let wav = format!("{manifest}/tests/speech_test.wav");
    let samples = read_wav_mono_f32(&wav);
    assert!(!samples.is_empty(), "wav decoded to no samples");

    let mut asr = asr::create(
        "whisper",
        AsrOptions {
            sentences: Vec::new(),
            model: "ggml-tiny.bin".to_string(),
            language: "en".to_string(),
            initial_prompt: String::new(),
        },
    )
    .await
    .expect("whisper provider should construct");

    let chunk = 1600usize; // 100 ms @ 16 kHz
    let mut end_us = 0u64;
    let mut transcript = String::new();

    for frame in samples.chunks(chunk) {
        let events = asr.push_audio(frame, true, end_us).await.unwrap();
        for event in events {
            if let RecognitionEvent::Final { text, .. } = event {
                transcript.push_str(&text);
                transcript.push(' ');
            }
        }
        end_us += frame.len() as u64 * 1_000_000 / 16_000;
    }

    let events = asr.flush().await.unwrap();
    for event in events {
        if let RecognitionEvent::Final { text, .. } = event {
            transcript.push_str(&text);
            transcript.push(' ');
        }
    }

    println!("WHISPER TRANSCRIPT: {transcript}");

    assert!(
        !transcript.trim().is_empty(),
        "whisper returned no transcript for a speech clip"
    );
    let lower = transcript.to_lowercase();
    let keywords = [
        "whisper", "test", "hello", "fox", "recogn", "system", "quick", "brown", "lazy", "dog",
    ];
    assert!(
        keywords.iter().any(|k| lower.contains(k)),
        "expected keyword not found in transcript: {transcript}"
    );
}
