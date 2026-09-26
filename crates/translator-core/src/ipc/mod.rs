//! Request dispatch and the accept loop.
//!
//! The connection is split so Whisper inference never blocks it: a reader task
//! frames incoming messages and feeds bounded queues, a pipeline task owns the
//! `Session` (audio -> VAD -> ASR -> subtitles) and a writer task serialises
//! everything outbound. Audio reception, inference and subtitle output each
//! get their own bounded queue, so a slow decode backpressures the client
//! instead of stalling heartbeats, pause requests or session control.
//!
//! The pipeline drains every audio frame already queued as one batch and lets
//! only the newest frame trigger a partial decode, merging stale mid-utterance
//! inference into a single pass (see `SpeechRecognizer::push_audio`).

pub mod transport;

use std::sync::Arc;

use tokio::io::AsyncRead;
use tokio::sync::mpsc;
use tracing::{info, warn};
use translator_protocol::envelope::Payload;
use translator_protocol::framing::{read_frame, write_frame};
use translator_protocol::{
    AudioFrame, CoreStatus, Envelope, ErrorCode, HandshakeResponse, HeartbeatResponse,
    SetPausedResponse, StartSessionResponse, StopSessionResponse, PROTOCOL_VERSION,
};

use crate::error::{CoreError, Result};
use crate::session::Session;
use crate::VERSION;

/// Features the core advertises during the handshake.
#[cfg(feature = "whisper")]
const FEATURES: &[&str] = &[
    "audio.pcm",
    "asr.mock",
    "asr.whisper",
    "translation.none",
    "translation.mock",
    "translation.apple-translate",
];

#[cfg(not(feature = "whisper"))]
const FEATURES: &[&str] = &[
    "audio.pcm",
    "asr.mock",
    "translation.none",
    "translation.mock",
    "translation.apple-translate",
];

/// Audio frames buffered between the reader and the pipeline. Bounded on
/// purpose: a full queue backpressures the client instead of growing memory.
const AUDIO_QUEUE: usize = 128;
/// Session control commands buffered ahead of the pipeline.
const CONTROL_QUEUE: usize = 32;
/// Envelopes waiting for the socket writer.
const OUTBOUND_QUEUE: usize = 64;

pub struct ServeOptions {
    /// Shut the process down once the controlling client goes away.
    pub exit_on_disconnect: bool,
    /// Sentences rotated by the mock recogniser. Empty means built-in defaults.
    pub mock_sentences: Vec<String>,
}

pub async fn serve(mut listener: transport::Listener, options: ServeOptions) -> Result<()> {
    let options = Arc::new(options);
    loop {
        let stream = listener.accept().await?;
        info!("client connected");
        match handle(stream, options.clone()).await {
            Ok(()) => info!("client disconnected"),
            Err(e) if is_disconnect(&e) => info!("client disconnected abruptly"),
            Err(e) => warn!("client session ended with error: {e}"),
        }

        if options.exit_on_disconnect {
            info!("exit-on-disconnect enabled, shutting down");
            return Ok(());
        }
    }
}

async fn handle(stream: transport::Stream, options: Arc<ServeOptions>) -> Result<()> {
    let (mut reader, writer_half) = tokio::io::split(stream);

    let (ctrl_tx, ctrl_rx) = mpsc::channel(CONTROL_QUEUE);
    let (audio_tx, audio_rx) = mpsc::channel(AUDIO_QUEUE);
    let (out_tx, out_rx) = mpsc::channel(OUTBOUND_QUEUE);

    let writer_task = tokio::spawn(write_loop(writer_half, out_rx));
    let pipeline_task = tokio::spawn(session_worker(ctrl_rx, audio_rx, out_tx.clone(), options));

    let read_result = read_loop(&mut reader, ctrl_tx, audio_tx, out_tx).await;

    // Dropping the reader's queue handles (owned by read_loop) lets the
    // pipeline finish its current batch, observe the closed channels and exit;
    // the writer then drains whatever is still queued before returning.
    let pipeline_result = pipeline_task.await;
    let writer_result = writer_task.await;

    match read_result {
        Ok(()) => {
            pipeline_result
                .map_err(|e| CoreError::Internal(format!("pipeline task crashed: {e}")))??;
            writer_result.map_err(|e| CoreError::Internal(format!("writer task crashed: {e}")))??;
            Ok(())
        }
        // Report the read-side failure first; it is the one the client caused.
        Err(e) => Err(e),
    }
}

/// Messages that must be handled in order with respect to the session.
enum Command {
    Start(translator_protocol::StartSessionRequest),
    SetPaused(translator_protocol::SetPausedRequest),
    Stop,
}

async fn read_loop<R: AsyncRead + Unpin>(
    reader: &mut R,
    ctrl_tx: mpsc::Sender<Command>,
    audio_tx: mpsc::Sender<AudioFrame>,
    out_tx: mpsc::Sender<Envelope>,
) -> Result<()> {
    let mut handshaked = false;

    while let Some(envelope) = read_frame(reader).await? {
        let Some(payload) = envelope.payload else {
            send_or_fail(&out_tx, error_event(ErrorCode::BadRequest, "empty envelope", false)).await?;
            continue;
        };

        match payload {
            Payload::HandshakeRequest(request) => {
                if request.protocol_version != PROTOCOL_VERSION {
                    send_or_fail(
                        &out_tx,
                        env(Payload::HandshakeResponse(HandshakeResponse {
                            accepted: false,
                            server_version: VERSION.to_string(),
                            negotiated_protocol_version: PROTOCOL_VERSION,
                            features: Vec::new(),
                            error: format!(
                                "client speaks protocol v{}, core speaks v{PROTOCOL_VERSION}",
                                request.protocol_version
                            ),
                        })),
                    )
                    .await?;
                    continue;
                }
                handshaked = true;
                send_or_fail(
                    &out_tx,
                    env(Payload::HandshakeResponse(HandshakeResponse {
                        accepted: true,
                        server_version: VERSION.to_string(),
                        negotiated_protocol_version: PROTOCOL_VERSION,
                        features: {
                            let mut features: Vec<String> =
                                FEATURES.iter().map(|f| f.to_string()).collect();
                            #[cfg(feature = "whisper")]
                            features.push(format!(
                                "asr.whisper.engine.{}",
                                crate::asr::whisper::engine_id()
                            ));
                            features
                        },
                        error: String::new(),
                    })),
                )
                .await?;
            }

            // Heartbeats are answered inline: they must not wait behind a
            // running decode, which is the whole point of the split pipeline.
            Payload::HeartbeatRequest(request) => {
                send_or_fail(
                    &out_tx,
                    env(Payload::HeartbeatResponse(HeartbeatResponse {
                        timestamp_us: request.timestamp_us,
                    })),
                )
                .await?;
            }

            Payload::AudioFrame(frame) => {
                if !handshaked {
                    send_or_fail(
                        &out_tx,
                        error_event(ErrorCode::BadRequest, "handshake required", false),
                    )
                    .await?;
                } else {
                    send_or_fail(&audio_tx, frame).await?;
                }
            }

            Payload::StartSessionRequest(request) => {
                if !handshaked {
                    send_or_fail(
                        &out_tx,
                        error_event(ErrorCode::BadRequest, "handshake required", false),
                    )
                    .await?;
                } else {
                    send_or_fail(&ctrl_tx, Command::Start(request)).await?;
                }
            }

            Payload::SetPausedRequest(request) => {
                if !handshaked {
                    send_or_fail(
                        &out_tx,
                        error_event(ErrorCode::BadRequest, "handshake required", false),
                    )
                    .await?;
                } else {
                    send_or_fail(&ctrl_tx, Command::SetPaused(request)).await?;
                }
            }

            Payload::StopSessionRequest(_) => {
                if !handshaked {
                    send_or_fail(
                        &out_tx,
                        error_event(ErrorCode::BadRequest, "handshake required", false),
                    )
                    .await?;
                } else {
                    send_or_fail(&ctrl_tx, Command::Stop).await?;
                }
            }

            // Core -> client only messages; a well behaved client never sends these.
            other => {
                let name = payload_name(&other);
                send_or_fail(
                    &out_tx,
                    error_event(
                        ErrorCode::BadRequest,
                        format!("unexpected message from client: {name}"),
                        false,
                    ),
                )
                .await?;
            }
        }
    }
    Ok(())
}

async fn send_or_fail<T>(tx: &mpsc::Sender<T>, value: T) -> Result<()> {
    tx.send(value)
        .await
        .map_err(|_| CoreError::Internal("connection pipeline closed".into()))
}

async fn write_loop(
    mut writer: impl tokio::io::AsyncWrite + Unpin,
    mut out_rx: mpsc::Receiver<Envelope>,
) -> Result<()> {
    let mut seq = 0u32;
    while let Some(mut envelope) = out_rx.recv().await {
        // Seq is assigned here so wire order is exactly the write order, no
        // matter which task produced the envelope.
        seq = seq.wrapping_add(1);
        envelope.seq = seq;
        write_frame(&mut writer, &envelope).await?;
    }
    Ok(())
}

/// Owns the session and processes audio batches plus session control in order.
struct Pipeline {
    session: Option<Session>,
    options: Arc<ServeOptions>,
    out: mpsc::Sender<Envelope>,
}

impl Pipeline {
    /// Queue an outbound envelope. Fails only when the writer is gone, which
    /// means the connection is ending and the worker should stop.
    async fn emit(&self, envelope: Envelope) -> Result<()> {
        self.out
            .send(envelope)
            .await
            .map_err(|_| CoreError::Internal("connection writer closed".into()))
    }

    async fn control(&mut self, command: Command) -> Result<()> {
        match command {
            Command::Start(request) => match Session::start(&request, &self.options.mock_sentences).await {
                Ok(started) => {
                    let id = started.id().to_string();
                    self.session = Some(started);
                    self.emit(env(Payload::StartSessionResponse(StartSessionResponse {
                        accepted: true,
                        session_id: id,
                        error: String::new(),
                    })))
                    .await?;
                    self.emit(status_event(CoreStatus::Listening, "session started"))
                        .await?;
                }
                Err(e) => {
                    warn!("rejected session: {e}");
                    self.emit(env(Payload::StartSessionResponse(StartSessionResponse {
                        accepted: false,
                        session_id: request.session_id.clone(),
                        error: e.to_string(),
                    })))
                    .await?;
                    self.emit(error_event(e.code(), e.to_string(), false))
                        .await?;
                }
            },

            Command::SetPaused(request) => match self.session.as_mut() {
                Some(active) => {
                    active.set_paused(request.paused);
                    self.emit(env(Payload::SetPausedResponse(SetPausedResponse {
                        ok: true,
                        error: String::new(),
                    })))
                    .await?;
                    let status = if request.paused {
                        CoreStatus::Paused
                    } else {
                        CoreStatus::Listening
                    };
                    self.emit(status_event(status, "pause state updated"))
                        .await?;
                }
                None => {
                    self.emit(error_event(ErrorCode::InvalidSession, "no active session", false))
                        .await?;
                }
            },

            Command::Stop => {
                let mut subtitles = Vec::new();
                if let Some(mut active) = self.session.take() {
                    match active.flush().await {
                        Ok(flushed) => subtitles = flushed,
                        Err(e) => warn!("flush failed: {e}"),
                    }
                }
                for subtitle in subtitles {
                    self.emit(env(Payload::SubtitleEvent(subtitle))).await?;
                }
                self.emit(env(Payload::StopSessionResponse(StopSessionResponse {
                    ok: true,
                    error: String::new(),
                })))
                .await?;
                self.emit(status_event(CoreStatus::Idle, "session stopped"))
                    .await?;
            }
        }
        Ok(())
    }

    /// Drain `first` plus every audio frame already queued, feeding the
    /// recogniser continuously; only the newest frame of the batch may trigger
    /// a partial decode. Under backlog this merges the stale mid-utterance
    /// decodes into one pass instead of decoding every queued step.
    async fn audio_batch(
        &mut self,
        first: AudioFrame,
        audio_rx: &mut mpsc::Receiver<AudioFrame>,
    ) -> Result<()> {
        let mut batch = vec![first];
        while let Ok(frame) = audio_rx.try_recv() {
            batch.push(frame);
        }
        let newest = batch.len() - 1;
        for (i, frame) in batch.into_iter().enumerate() {
            let infer_partial = i == newest;
            match self.session.as_mut() {
                Some(active) => match active.push_frame(frame, infer_partial).await {
                    Ok(output) => {
                        for subtitle in output.subtitles {
                            self.emit(env(Payload::SubtitleEvent(subtitle))).await?;
                        }
                        if let Some(metrics) = output.metrics {
                            self.emit(env(Payload::MetricsEvent(metrics))).await?;
                        }
                    }
                    Err(e) => {
                        warn!("dropping audio frame: {e}");
                        self.emit(error_event(e.code(), e.to_string(), false)).await?;
                    }
                },
                None => {
                    self.emit(error_event(ErrorCode::InvalidSession, "no active session", false))
                        .await?;
                }
            }
        }
        Ok(())
    }
}

async fn session_worker(
    mut ctrl_rx: mpsc::Receiver<Command>,
    mut audio_rx: mpsc::Receiver<AudioFrame>,
    out_tx: mpsc::Sender<Envelope>,
    options: Arc<ServeOptions>,
) -> Result<()> {
    let mut pipeline = Pipeline {
        session: None,
        options,
        out: out_tx,
    };
    loop {
        tokio::select! {
            biased;
            // Control first: pause/stop must win against a queued audio burst.
            command = ctrl_rx.recv() => match command {
                Some(command) => pipeline.control(command).await?,
                None => break,
            },
            frame = audio_rx.recv() => match frame {
                Some(frame) => pipeline.audio_batch(frame, &mut audio_rx).await?,
                None => break,
            },
        }
    }
    Ok(())
}

fn env(payload: Payload) -> Envelope {
    Envelope {
        seq: 0, // assigned by the writer task, right before the frame goes out
        payload: Some(payload),
    }
}

fn status_event(status: CoreStatus, detail: &str) -> Envelope {
    env(Payload::StatusEvent(translator_protocol::StatusEvent {
        status: status as i32,
        detail: detail.to_string(),
        timestamp_us: now_us(),
    }))
}

fn error_event(code: ErrorCode, message: impl Into<String>, fatal: bool) -> Envelope {
    env(Payload::ErrorEvent(translator_protocol::ErrorEvent {
        code: code as i32,
        message: message.into(),
        fatal,
    }))
}

fn payload_name(payload: &Payload) -> &'static str {
    match payload {
        Payload::HandshakeRequest(_) => "HandshakeRequest",
        Payload::HandshakeResponse(_) => "HandshakeResponse",
        Payload::StartSessionRequest(_) => "StartSessionRequest",
        Payload::StartSessionResponse(_) => "StartSessionResponse",
        Payload::StopSessionRequest(_) => "StopSessionRequest",
        Payload::StopSessionResponse(_) => "StopSessionResponse",
        Payload::SetPausedRequest(_) => "SetPausedRequest",
        Payload::SetPausedResponse(_) => "SetPausedResponse",
        Payload::AudioFrame(_) => "AudioFrame",
        Payload::SubtitleEvent(_) => "SubtitleEvent",
        Payload::StatusEvent(_) => "StatusEvent",
        Payload::MetricsEvent(_) => "MetricsEvent",
        Payload::ErrorEvent(_) => "ErrorEvent",
        Payload::HeartbeatRequest(_) => "HeartbeatRequest",
        Payload::HeartbeatResponse(_) => "HeartbeatResponse",
    }
}

/// A client that dies mid-frame looks like a broken pipe; that is normal.
fn is_disconnect(error: &crate::error::CoreError) -> bool {
    let crate::error::CoreError::Io(io) = error else {
        return false;
    };
    // ERROR_NO_DATA: the named pipe is being closed.
    if io.raw_os_error() == Some(232) {
        return true;
    }
    matches!(
        io.kind(),
        std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
    )
}

fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use translator_protocol::{
        AsrConfig, AudioFormat, SampleFormat, StartSessionRequest, SubtitleEvent, SubtitleKind,
    };

    fn start_request() -> StartSessionRequest {
        StartSessionRequest {
            session_id: "test-session".into(),
            input_format: Some(AudioFormat {
                sample_rate: 16_000,
                channels: 1,
                format: SampleFormat::F32 as i32,
            }),
            target_sample_rate: 0,
            asr: Some(AsrConfig {
                provider: "mock".into(),
                model: String::new(),
                language: String::new(),
                engine: String::new(),
                model_directory: String::new(),
            }),
            translation: None,
            subtitle: None,
        }
    }

    /// One second of 16 kHz mono audio, loud (a 440 Hz tone) or silent.
    fn audio_frame(loud: bool) -> AudioFrame {
        let samples: Vec<f32> = (0..16_000)
            .map(|i| {
                if loud {
                    0.5 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 16_000.0).sin()
                } else {
                    0.0
                }
            })
            .collect();
        let mut pcm = Vec::with_capacity(samples.len() * 4);
        for s in samples {
            pcm.extend_from_slice(&s.to_le_bytes());
        }
        AudioFrame {
            session_id: "test-session".into(),
            timestamp_us: 0,
            format: Some(AudioFormat {
                sample_rate: 16_000,
                channels: 1,
                format: SampleFormat::F32 as i32,
            }),
            frames: 16_000,
            pcm,
        }
    }

    #[tokio::test]
    async fn audio_batch_drains_queue_and_merges_partials() {
        let (out_tx, mut out_rx) = mpsc::channel(256);
        let (audio_tx, mut audio_rx) = mpsc::channel(64);
        let options = Arc::new(ServeOptions {
            exit_on_disconnect: false,
            mock_sentences: vec![],
        });
        let mut pipeline = Pipeline {
            session: None,
            options,
            out: out_tx,
        };

        pipeline.control(Command::Start(start_request())).await.unwrap();
        assert!(pipeline.session.is_some());

        // Queue 10 loud seconds in 1 s frames: plenty to cross the partial
        // decode cadence several times, all merged into one pass.
        for _ in 0..10 {
            audio_tx.send(audio_frame(true)).await.unwrap();
        }
        let first = audio_rx.recv().await.unwrap();
        pipeline.audio_batch(first, &mut audio_rx).await.unwrap();
        assert!(audio_rx.try_recv().is_err(), "batch must drain the queue");

        let metrics = pipeline.session.as_ref().unwrap().snapshot_metrics();
        assert_eq!(metrics.audio_frames, 10);

        // A silent batch closes the VAD gate and commits an utterance.
        pipeline.audio_batch(audio_frame(false), &mut audio_rx).await.unwrap();

        let mut committed = 0;
        while let Ok(envelope) = out_rx.try_recv() {
            if let Some(Payload::SubtitleEvent(SubtitleEvent { kind, .. })) = envelope.payload {
                if kind == SubtitleKind::Committed as i32 {
                    committed += 1;
                }
            }
        }
        assert_eq!(committed, 1, "one utterance should have been committed");
    }

    #[tokio::test]
    async fn pipeline_preserves_stop_after_backlogged_audio() {
        let (out_tx, mut out_rx) = mpsc::channel(256);
        let (audio_tx, mut audio_rx) = mpsc::channel(64);
        let options = Arc::new(ServeOptions {
            exit_on_disconnect: false,
            mock_sentences: vec![],
        });
        let mut pipeline = Pipeline {
            session: None,
            options,
            out: out_tx,
        };

        pipeline.control(Command::Start(start_request())).await.unwrap();

        // Audio queued while a Stop arrives must still be flushed by the stop.
        for _ in 0..5 {
            audio_tx.send(audio_frame(true)).await.unwrap();
        }
        let first = audio_rx.recv().await.unwrap();
        pipeline.audio_batch(first, &mut audio_rx).await.unwrap();
        pipeline.control(Command::Stop).await.unwrap();

        let mut saw_committed = false;
        let mut saw_stop_ok = false;
        while let Ok(envelope) = out_rx.try_recv() {
            match envelope.payload {
                Some(Payload::SubtitleEvent(e)) if e.kind == SubtitleKind::Committed as i32 => {
                    saw_committed = true;
                }
                Some(Payload::StopSessionResponse(r)) => saw_stop_ok = r.ok,
                _ => {}
            }
        }
        assert!(saw_committed, "flush must commit the buffered utterance");
        assert!(saw_stop_ok, "stop must be acknowledged");
        assert!(pipeline.session.is_none());
    }
}
