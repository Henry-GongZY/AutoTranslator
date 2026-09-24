//! Streaming windowed-sinc resampler (mono, `f32`).
//!
//! Implemented locally instead of pulling a DSP crate so the phase-1 core stays
//! dependency-light and builds identically on every platform. Quality is well
//! beyond what speech recognition needs (32 taps, 256 phases, Blackman window).

use std::collections::VecDeque;

use crate::error::{CoreError, Result};

const TAPS: usize = 32;
const HALF: usize = TAPS / 2;
const PHASES: usize = 256;

pub struct SincResampler {
    /// Input samples consumed per output sample (`in_rate / out_rate`).
    ratio: f64,
    /// `PHASES * TAPS` impulse response, one row per fractional phase.
    table: Vec<f32>,
    /// Unconsumed input samples; `buf_base` is their absolute stream index.
    buf: VecDeque<f32>,
    buf_base: i64,
    /// Absolute input position (fractional) of the next output sample.
    out_pos: f64,
}

impl SincResampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Result<Self> {
        if in_rate == 0 || out_rate == 0 {
            return Err(CoreError::AudioFormat(format!(
                "invalid resample rates {in_rate} -> {out_rate}"
            )));
        }

        // Cut off slightly below the lower Nyquist to avoid aliasing.
        let cutoff_hz = (in_rate.min(out_rate) as f64) * 0.5 * 0.92;
        let fc = cutoff_hz / in_rate as f64; // cycles per input sample

        let mut table = vec![0.0f32; PHASES * TAPS];
        for phase in 0..PHASES {
            let frac = phase as f64 / PHASES as f64;
            let mut sum = 0.0f64;
            for tap in 0..TAPS {
                let a = tap as f64 - HALF as f64 - frac;
                let window = blackman(((a + HALF as f64) / TAPS as f64).clamp(0.0, 1.0));
                let h = 2.0 * fc * sinc(2.0 * fc * a) * window;
                table[phase * TAPS + tap] = h as f32;
                sum += h;
            }
            // Normalise every phase so DC gain stays at exactly 1.0.
            if sum.abs() > f64::EPSILON {
                for tap in 0..TAPS {
                    table[phase * TAPS + tap] /= sum as f32;
                }
            }
        }

        Ok(Self {
            ratio: in_rate as f64 / out_rate as f64,
            table,
            buf: VecDeque::new(),
            buf_base: 0,
            out_pos: 0.0,
        })
    }

    /// Feed a chunk of mono input; produced samples are appended to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        self.buf.extend(input.iter().copied());
        let available_last = self.buf_base + self.buf.len() as i64 - 1;

        loop {
            let base = self.out_pos.floor() as i64;
            let highest_needed = base + HALF as i64 - 1;
            if available_last < highest_needed {
                break;
            }

            let frac = self.out_pos - base as f64;
            let phase = ((frac * PHASES as f64) as usize).min(PHASES - 1);
            let row = phase * TAPS;

            let mut acc = 0.0f32;
            for tap in 0..TAPS {
                let sample = self.sample_at(base + tap as i64 - HALF as i64);
                acc += sample * self.table[row + tap];
            }
            out.push(acc);
            self.out_pos += self.ratio;
        }

        // Drop input we can never touch again (keep one spare sample).
        let keep_from = self.out_pos.floor() as i64 - HALF as i64 - 1;
        while self.buf_base < keep_from {
            match self.buf.pop_front() {
                Some(_) => self.buf_base += 1,
                None => break,
            }
        }
    }

    fn sample_at(&self, index: i64) -> f32 {
        if index < self.buf_base {
            return 0.0;
        }
        self.buf
            .get((index - self.buf_base) as usize)
            .copied()
            .unwrap_or(0.0)
    }
}

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 {
        return 1.0;
    }
    let px = std::f64::consts::PI * x;
    px.sin() / px
}

/// Blackman window, `x` clamped to `[0, 1]`.
fn blackman(x: f64) -> f64 {
    let x = x.clamp(0.0, 1.0);
    0.42 - 0.5 * (2.0 * std::f64::consts::PI * x).cos() + 0.08 * (4.0 * std::f64::consts::PI * x).cos()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resamples_three_to_one_with_unity_dc_gain() {
        let mut resampler = SincResampler::new(48000, 16000).unwrap();
        let input = vec![0.5f32; 4800]; // 100 ms at 48 kHz
        let mut out = Vec::new();
        resampler.process(&input, &mut out);

        // 100 ms at 16 kHz is 1600 samples; allow for the filter delay.
        assert!(
            (1550..=1650).contains(&out.len()),
            "unexpected output length {}",
            out.len()
        );
        for (i, s) in out.iter().enumerate().skip(32) {
            assert!((s - 0.5).abs() < 0.02, "sample {i} = {s}");
        }
    }

    #[test]
    fn tracks_a_slow_sine() {
        let mut resampler = SincResampler::new(48000, 16000).unwrap();
        let mut input = Vec::new();
        for i in 0..4800 {
            let t = i as f64 / 48000.0;
            input.push((2.0 * std::f64::consts::PI * 1000.0 * t).sin() as f32);
        }
        let mut out = Vec::new();
        resampler.process(&input, &mut out);

        // Compare the steady-state portion against the analytically expected values.
        let mut worst = 0.0f32;
        for (i, s) in out.iter().enumerate().skip(64).take(200) {
            let t = (i as f64 + 16.0) / 16000.0;
            let expected = (2.0 * std::f64::consts::PI * 1000.0 * t).sin() as f32;
            worst = worst.max((s - expected).abs());
        }
        assert!(worst < 0.08, "worst resample error {worst}");
    }
}
