//! Conversion of raw PCM bytes into normalised `f32` samples.

use translator_protocol::SampleFormat;

use crate::error::{CoreError, Result};

/// Size of one sample in bytes.
pub fn bytes_per_sample(format: SampleFormat) -> Result<usize> {
    match format {
        SampleFormat::F32 => Ok(4),
        SampleFormat::I16 => Ok(2),
        SampleFormat::Unspecified => Err(CoreError::AudioFormat(
            "sample format was not specified".to_string(),
        )),
    }
}

/// Decode interleaved PCM into `f32` in the range `[-1, 1]`.
pub fn decode_to_f32(bytes: &[u8], format: SampleFormat) -> Result<Vec<f32>> {
    match format {
        SampleFormat::F32 => {
            if bytes.len() % 4 != 0 {
                return Err(CoreError::AudioFormat(format!(
                    "f32 payload of {} bytes is not a multiple of 4",
                    bytes.len()
                )));
            }
            Ok(bytes
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect())
        }
        SampleFormat::I16 => {
            if bytes.len() % 2 != 0 {
                return Err(CoreError::AudioFormat(format!(
                    "i16 payload of {} bytes is not a multiple of 2",
                    bytes.len()
                )));
            }
            Ok(bytes
                .chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
                .collect())
        }
        SampleFormat::Unspecified => Err(CoreError::AudioFormat(
            "sample format was not specified".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_i16() {
        let bytes = [0x00u8, 0x00, 0x00, 0x80, 0xff, 0x7f];
        let out = decode_to_f32(&bytes, SampleFormat::I16).unwrap();
        assert_eq!(out.len(), 3);
        assert!(out[0].abs() < 1e-9);
        assert!(out[1] < 0.0);
        assert!(out[2] > 0.99);
    }

    #[test]
    fn decodes_f32() {
        let bytes = 1.0f32.to_le_bytes();
        let out = decode_to_f32(&bytes, SampleFormat::F32).unwrap();
        assert!((out[0] - 1.0).abs() < 1e-9);
    }

    #[test]
    fn rejects_unspecified() {
        assert!(decode_to_f32(&[0u8; 4], SampleFormat::Unspecified).is_err());
    }
}
