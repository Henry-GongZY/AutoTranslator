//! Energy based voice activity detection with an adaptive noise floor.

pub struct VadConfig {
    pub sample_rate: u32,
    /// Analysis frame length in milliseconds.
    pub frame_ms: u32,
    /// Consecutive loud frames required to open the gate.
    pub start_frames: usize,
    /// Consecutive quiet frames required to close the gate.
    pub end_frames: usize,
    /// How far above the noise floor a frame must sit to count as speech.
    pub threshold_db: f32,
    /// Absolute floor so pure silence can never open the gate.
    pub min_db: f32,
}

impl VadConfig {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate,
            frame_ms: 10,
            start_frames: 3,
            end_frames: 30,
            threshold_db: 10.0,
            min_db: -55.0,
        }
    }

    fn frame_samples(&self) -> usize {
        ((self.sample_rate as u64 * self.frame_ms as u64) / 1000).max(1) as usize
    }
}

pub struct VadResult {
    /// Whether the gate is currently open.
    pub active: bool,
    /// Frames completed while consuming this chunk.
    pub completed_frames: usize,
    /// Of those, how many were classified as speech.
    pub speech_frames: usize,
}

pub struct Vad {
    cfg: VadConfig,
    frame_samples: usize,
    frame_pos: usize,
    sum_sq: f32,
    noise_db: f32,
    speech_run: usize,
    silence_run: usize,
    active: bool,
    segment_start_us: u64,
}

impl Vad {
    pub fn new(cfg: VadConfig) -> Self {
        let frame_samples = cfg.frame_samples();
        Self {
            cfg,
            frame_samples,
            frame_pos: 0,
            sum_sq: 0.0,
            noise_db: -70.0,
            speech_run: 0,
            silence_run: usize::MAX / 2,
            active: false,
            segment_start_us: 0,
        }
    }

    pub fn push(&mut self, samples: &[f32], end_us: u64) -> VadResult {
        let us_per_sample = 1_000_000.0 / self.cfg.sample_rate.max(1) as f64;
        let chunk_start_us = end_us as f64 - samples.len() as f64 * us_per_sample;

        let mut completed = 0usize;
        let mut speech = 0usize;

        for (i, sample) in samples.iter().enumerate() {
            self.sum_sq += sample * sample;
            self.frame_pos += 1;
            if self.frame_pos < self.frame_samples {
                continue;
            }

            let rms = (self.sum_sq / self.frame_samples as f32).sqrt();
            let db = 20.0 * (rms as f64 + 1e-9).log10() as f32;
            let frame_end_us = (chunk_start_us + (i + 1) as f64 * us_per_sample) as u64;

            let loud = db > self.cfg.min_db && db > self.noise_db + self.cfg.threshold_db;
            if !loud {
                // Slowly track the ambient floor.
                self.noise_db += 0.05 * (db - self.noise_db);
                self.noise_db = self.noise_db.clamp(-90.0, -10.0);
            }

            if loud {
                self.speech_run += 1;
                self.silence_run = 0;
                speech += 1;
            } else {
                self.silence_run += 1;
                self.speech_run = 0;
            }

            completed += 1;

            if !self.active && self.speech_run >= self.cfg.start_frames {
                self.active = true;
                let lookback_us =
                    self.cfg.start_frames as u64 * self.cfg.frame_ms as u64 * 1000;
                self.segment_start_us = frame_end_us.saturating_sub(lookback_us);
            } else if self.active && self.silence_run >= self.cfg.end_frames {
                self.active = false;
            }

            self.sum_sq = 0.0;
            self.frame_pos = 0;
        }

        VadResult {
            active: self.active,
            completed_frames: completed,
            speech_frames: speech,
        }
    }

    /// Start timestamp of the segment currently being spoken.
    pub fn segment_start_us(&self) -> u64 {
        self.segment_start_us
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(sample_rate: u32, ms: u32, amplitude: f32) -> Vec<f32> {
        let n = sample_rate as usize * ms as usize / 1000;
        (0..n)
            .map(|i| amplitude * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / sample_rate as f64).sin() as f32)
            .collect()
    }

    #[test]
    fn opens_on_speech_and_closes_on_silence() {
        let rate = 16000;
        let mut vad = Vad::new(VadConfig::new(rate));

        let mut clock_us = 0u64;
        let speech = tone(rate, 500, 0.3);
        clock_us += speech.len() as u64 * 1_000_000 / rate as u64;
        let result = vad.push(&speech, clock_us);
        assert!(result.active, "gate should open on speech");

        let silence = vec![0.0f32; rate as usize]; // 1 s of digital silence
        clock_us += silence.len() as u64 * 1_000_000 / rate as u64;
        let result = vad.push(&silence, clock_us);
        assert!(!result.active, "gate should close after 1 s of silence");
    }

    #[test]
    fn stays_closed_on_silence() {
        let rate = 16000;
        let mut vad = Vad::new(VadConfig::new(rate));
        let silence = vec![0.0f32; rate as usize];
        let result = vad.push(&silence, 1_000_000);
        assert!(!result.active);
        assert_eq!(result.speech_frames, 0);
    }
}
