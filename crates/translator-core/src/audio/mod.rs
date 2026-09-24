//! audio pre-processing: PCM decoding, channel down-mix and resampling.

pub mod pcm;
pub mod resampler;

/// Average all channels into a single mono track.
pub fn downmix_to_mono(input: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return input.to_vec();
    }
    let frames = input.len() / channels;
    let mut out = Vec::with_capacity(frames);
    for i in 0..frames {
        let mut acc = 0.0f32;
        for c in 0..channels {
            acc += input[i * channels + c];
        }
        out.push(acc / channels as f32);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downmixes_stereo() {
        let input = [1.0f32, 0.0, 0.5, 0.5, -1.0, 1.0];
        let mono = downmix_to_mono(&input, 2);
        assert_eq!(mono.len(), 3);
        assert!((mono[0] - 0.5).abs() < 1e-6);
        assert!((mono[1] - 0.5).abs() < 1e-6);
        assert!(mono[2].abs() < 1e-6);
    }

    #[test]
    fn passes_through_mono() {
        let input = [0.25f32, -0.25];
        assert_eq!(downmix_to_mono(&input, 1), input);
    }
}
