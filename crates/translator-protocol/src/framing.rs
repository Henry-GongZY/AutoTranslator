//! Length-prefixed framing for `Envelope` messages.

use std::io;

use prost::Message;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::Envelope;

/// Hard cap so a corrupt peer cannot make us allocate unbounded memory.
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// Serialise `msg` into `[len: u32 be][payload]`.
pub fn encode_frame(msg: &Envelope) -> Vec<u8> {
    let payload = msg.encode_to_vec();
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&payload);
    out
}

/// Decode a length-prefixed frame. `Ok(None)` means clean end of stream.
pub fn decode_frame(bytes: &[u8]) -> io::Result<Envelope> {
    if bytes.len() < 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame shorter than 4-byte header",
        ));
    }
    let len = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let payload = bytes
        .get(4..4 + len)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "truncated frame"))?;
    Envelope::decode(payload).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub async fn write_frame<W>(writer: &mut W, msg: &Envelope) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let buf = encode_frame(msg);
    writer.write_all(&buf).await?;
    writer.flush().await
}

/// Read exactly one frame. `Ok(None)` means the peer closed the connection.
pub async fn read_frame<R>(reader: &mut R) -> io::Result<Option<Envelope>>
where
    R: AsyncRead + Unpin,
{
    let mut header = [0u8; 4];
    match reader.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }

    let len = u32::from_be_bytes(header) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {len} bytes exceeds {MAX_FRAME_BYTES} byte limit"),
        ));
    }

    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await?;

    Envelope::decode(payload.as_slice())
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HandshakeRequest, SampleFormat};

    #[test]
    fn round_trips_a_frame() {
        let env = Envelope {
            seq: 7,
            payload: Some(crate::envelope::Payload::HandshakeRequest(HandshakeRequest {
                protocol_version: PROTOCOL_VERSION_,
                client_id: "test".into(),
                client_version: "0.0.0".into(),
                features: vec!["audio.f32".into()],
            })),
        };

        let bytes = encode_frame(&env);
        assert_eq!(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize, bytes.len() - 4);
        assert_eq!(decode_frame(&bytes).unwrap(), env);
    }

    const PROTOCOL_VERSION_: u32 = crate::PROTOCOL_VERSION;

    #[test]
    fn rejects_truncated_frames() {
        let env = Envelope {
            seq: 1,
            payload: Some(crate::envelope::Payload::AudioFrame(crate::AudioFrame {
                session_id: "s".into(),
                timestamp_us: 0,
                format: Some(crate::AudioFormat {
                    sample_rate: 48000,
                    channels: 2,
                    format: SampleFormat::F32 as i32,
                }),
                frames: 1,
                pcm: vec![0u8; 8],
            })),
        };
        let bytes = encode_frame(&env);
        assert!(decode_frame(&bytes[..bytes.len() - 1]).is_err());
    }
}
