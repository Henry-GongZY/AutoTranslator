//! Request dispatch and the accept loop.

pub mod transport;

use tracing::{info, warn};
use translator_protocol::envelope::Payload;
use translator_protocol::framing::{read_frame, write_frame};
use translator_protocol::{
    CoreStatus, Envelope, ErrorCode, HandshakeResponse, HeartbeatResponse, SetPausedResponse,
    StartSessionResponse, StopSessionResponse, PROTOCOL_VERSION,
};

use crate::error::Result;
use crate::session::Session;
use crate::VERSION;

/// Features the phase-1 core advertises during the handshake.
#[cfg(feature = "whisper")]
const FEATURES: &[&str] = &["audio.pcm", "asr.mock", "asr.whisper", "translation.none"];

#[cfg(not(feature = "whisper"))]
const FEATURES: &[&str] = &["audio.pcm", "asr.mock", "translation.none"];

pub struct ServeOptions {
    /// Shut the process down once the controlling client goes away.
    pub exit_on_disconnect: bool,
    /// Sentences rotated by the mock recogniser. Empty means built-in defaults.
    pub mock_sentences: Vec<String>,
}

pub async fn serve(mut listener: transport::Listener, options: ServeOptions) -> Result<()> {
    loop {
        let stream = listener.accept().await?;
        info!("client connected");
        match handle(stream, &options).await {
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

async fn handle(stream: transport::Stream, options: &ServeOptions) -> Result<()> {
    let (mut reader, mut writer) = tokio::io::split(stream);
    let mut session: Option<Session> = None;
    let mut handshaked = false;
    let mut server_seq = 0u32;

    while let Some(envelope) = read_frame(&mut reader).await? {
        let responses = dispatch(&mut session, &mut handshaked, &mut server_seq, envelope, options).await;
        for response in responses {
            write_frame(&mut writer, &response).await?;
        }
    }
    Ok(())
}

async fn dispatch(
    session: &mut Option<Session>,
    handshaked: &mut bool,
    server_seq: &mut u32,
    envelope: Envelope,
    options: &ServeOptions,
) -> Vec<Envelope> {
    let request_seq = envelope.seq;
    let mut out = Vec::new();

    let payload = match envelope.payload {
        Some(payload) => payload,
        None => {
            out.push(error_event(server_seq, ErrorCode::BadRequest, "empty envelope", false));
            return out;
        }
    };

    match payload {
        Payload::HandshakeRequest(request) => {
            if request.protocol_version != PROTOCOL_VERSION {
                out.push(event(
                    server_seq,
                    Payload::HandshakeResponse(HandshakeResponse {
                        accepted: false,
                        server_version: VERSION.to_string(),
                        negotiated_protocol_version: PROTOCOL_VERSION,
                        features: Vec::new(),
                        error: format!(
                            "client speaks protocol v{}, core speaks v{PROTOCOL_VERSION}",
                            request.protocol_version
                        ),
                    }),
                ));
                return out;
            }
            *handshaked = true;
            out.push(event(
                server_seq,
                Payload::HandshakeResponse(HandshakeResponse {
                    accepted: true,
                    server_version: VERSION.to_string(),
                    negotiated_protocol_version: PROTOCOL_VERSION,
                    features: FEATURES.iter().map(|f| f.to_string()).collect(),
                    error: String::new(),
                }),
            ));
        }

        Payload::StartSessionRequest(request) => {
            if !*handshaked {
                out.push(error_event(server_seq, ErrorCode::BadRequest, "handshake required", false));
                return out;
            }
            match Session::start(&request, &options.mock_sentences).await {
                Ok(started) => {
                    let id = started.id().to_string();
                    *session = Some(started);
                    out.push(event(
                        server_seq,
                        Payload::StartSessionResponse(StartSessionResponse {
                            accepted: true,
                            session_id: id,
                            error: String::new(),
                        }),
                    ));
                    out.push(status_event(server_seq, CoreStatus::Listening, "session started"));
                }
                Err(e) => {
                    warn!("rejected session: {e}");
                    out.push(event(
                        server_seq,
                        Payload::StartSessionResponse(StartSessionResponse {
                            accepted: false,
                            session_id: request.session_id.clone(),
                            error: e.to_string(),
                        }),
                    ));
                    out.push(error_event(server_seq, e.code(), e.to_string(), false));
                }
            }
        }

        Payload::AudioFrame(frame) => match session.as_mut() {
            Some(active) => match active.push_frame(frame).await {
                Ok(output) => {
                    for subtitle in output.subtitles {
                        out.push(event(server_seq, Payload::SubtitleEvent(subtitle)));
                    }
                    if let Some(metrics) = output.metrics {
                        out.push(event(server_seq, Payload::MetricsEvent(metrics)));
                    }
                }
                Err(e) => {
                    warn!("dropping audio frame: {e}");
                    out.push(error_event(server_seq, e.code(), e.to_string(), false));
                }
            },
            None => out.push(error_event(
                server_seq,
                ErrorCode::InvalidSession,
                "no active session",
                false,
            )),
        },

        Payload::SetPausedRequest(request) => match session.as_mut() {
            Some(active) => {
                active.set_paused(request.paused);
                out.push(event(
                    server_seq,
                    Payload::SetPausedResponse(SetPausedResponse {
                        ok: true,
                        error: String::new(),
                    }),
                ));
                let status = if request.paused {
                    CoreStatus::Paused
                } else {
                    CoreStatus::Listening
                };
                out.push(status_event(server_seq, status, "pause state updated"));
            }
            None => out.push(error_event(
                server_seq,
                ErrorCode::InvalidSession,
                "no active session",
                false,
            )),
        },

        Payload::StopSessionRequest(_) => {
            let mut subtitles = Vec::new();
            if let Some(mut active) = session.take() {
                match active.flush().await {
                    Ok(flushed) => subtitles = flushed,
                    Err(e) => warn!("flush failed: {e}"),
                }
            }
            for subtitle in subtitles {
                out.push(event(server_seq, Payload::SubtitleEvent(subtitle)));
            }
            out.push(event(
                server_seq,
                Payload::StopSessionResponse(StopSessionResponse {
                    ok: true,
                    error: String::new(),
                }),
            ));
            out.push(status_event(server_seq, CoreStatus::Idle, "session stopped"));
        }

        Payload::HeartbeatRequest(request) => out.push(event(
            server_seq,
            Payload::HeartbeatResponse(HeartbeatResponse {
                timestamp_us: request.timestamp_us,
            }),
        )),

        // Core -> client only messages; a well behaved client never sends these.
        other => {
            let name = payload_name(&other);
            out.push(error_event(
                server_seq,
                ErrorCode::BadRequest,
                format!("unexpected message from client: {name}"),
                false,
            ));
        }
    }

    let _ = request_seq;
    out
}

fn event(server_seq: &mut u32, payload: Payload) -> Envelope {
    *server_seq = server_seq.wrapping_add(1);
    Envelope {
        seq: *server_seq,
        payload: Some(payload),
    }
}

fn status_event(server_seq: &mut u32, status: CoreStatus, detail: &str) -> Envelope {
    event(
        server_seq,
        Payload::StatusEvent(translator_protocol::StatusEvent {
            status: status as i32,
            detail: detail.to_string(),
            timestamp_us: now_us(),
        }),
    )
}

fn error_event(
    server_seq: &mut u32,
    code: ErrorCode,
    message: impl Into<String>,
    fatal: bool,
) -> Envelope {
    event(
        server_seq,
        Payload::ErrorEvent(translator_protocol::ErrorEvent {
            code: code as i32,
            message: message.into(),
            fatal,
        }),
    )
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
