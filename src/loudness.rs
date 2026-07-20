use ebur128::{EbuR128, Mode as LoudnessMode};

/// Real-time EBU R128 loudness values (LUFS).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LoudnessValues {
    /// Momentary loudness (400 ms window).
    pub momentary: f32,
    /// Short-term loudness (3 s window).
    pub short_term: f32,
    /// Integrated loudness since the meter was created or reset.
    pub integrated: f32,
}

impl LoudnessValues {
    /// Value used when no loudness information is available (silence / no signal).
    pub const SILENCE: f32 = -90.0;

    /// Return a new `LoudnessValues` with all fields set to the silence sentinel.
    pub fn silence() -> Self {
        Self {
            momentary: Self::SILENCE,
            short_term: Self::SILENCE,
            integrated: Self::SILENCE,
        }
    }

    /// Clamp non-finite values to the silence sentinel.
    pub fn sanitized(self) -> Self {
        Self {
            momentary: sanitize_lufs(self.momentary),
            short_term: sanitize_lufs(self.short_term),
            integrated: sanitize_lufs(self.integrated),
        }
    }
}

fn sanitize_lufs(value: f32) -> f32 {
    if value.is_finite() {
        value
    } else {
        LoudnessValues::SILENCE
    }
}

/// Real-time EBU R128 loudness meter.
///
/// Wraps `ebur128` and exposes only the momentary / short-term / integrated
/// values needed for the master strip readout.
pub struct LoudnessMeter {
    meter: EbuR128,
    channels: usize,
    sample_rate: u32,
}

impl LoudnessMeter {
    /// Create a new meter for the given channel count and sample rate.
    pub fn new(channels: usize, sample_rate: u32) -> Result<Self, ebur128::Error> {
        let meter = EbuR128::new(
            channels as u32,
            sample_rate,
            LoudnessMode::M | LoudnessMode::S | LoudnessMode::I,
        )?;
        Ok(Self {
            meter,
            channels,
            sample_rate,
        })
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Feed a block of interleaved floating-point samples into the meter.
    /// This should be called every audio cycle so integrated loudness remains
    /// accurate; call [`Self::values`] only when the readout is actually needed
    /// (e.g. at the same rate as the VU meter snapshot).
    pub fn feed_interleaved(&mut self, samples: &[f32]) {
        if samples.is_empty() {
            return;
        }
        if let Err(e) = self.meter.add_frames_f32(samples) {
            tracing::warn!("EBU R128 loudness analysis failed: {}", e);
        }
    }

    /// Return the current momentary / short-term / integrated loudness values.
    pub fn values(&self) -> LoudnessValues {
        let momentary = self.meter.loudness_momentary().unwrap_or(f64::NEG_INFINITY) as f32;
        let short_term = self.meter.loudness_shortterm().unwrap_or(f64::NEG_INFINITY) as f32;
        let integrated = self.meter.loudness_global().unwrap_or(f64::NEG_INFINITY) as f32;

        LoudnessValues {
            momentary,
            short_term,
            integrated,
        }
        .sanitized()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_returns_silence_values() {
        let mut meter = LoudnessMeter::new(2, 48_000).unwrap();
        meter.feed_interleaved(&[0.0_f32; 48_000 * 2]);
        assert_eq!(meter.values(), LoudnessValues::silence());
    }

    #[test]
    fn sine_wave_produces_finite_lufs() {
        let sample_rate = 48_000;
        let duration = 3 * sample_rate;
        let frequency = 1000.0;
        let samples: Vec<f32> = (0..duration)
            .flat_map(|i| {
                let phase = 2.0 * std::f32::consts::PI * frequency * i as f32 / sample_rate as f32;
                let sample = phase.sin() * 0.1;
                [sample, sample]
            })
            .collect();

        let mut meter = LoudnessMeter::new(2, sample_rate).unwrap();
        meter.feed_interleaved(&samples);
        let values = meter.values();

        assert!(values.momentary.is_finite());
        assert!(values.short_term.is_finite());
        assert!(values.integrated.is_finite());
        assert!(values.integrated > -90.0);
    }

    #[test]
    fn integrated_changes_after_multiple_blocks() {
        let sample_rate = 48_000;
        let block: Vec<f32> = (0..sample_rate)
            .flat_map(|i| {
                let phase = 2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sample_rate as f32;
                let sample = phase.sin() * 0.05;
                [sample, sample]
            })
            .collect();

        let mut meter = LoudnessMeter::new(2, sample_rate).unwrap();
        meter.feed_interleaved(&block);
        let first = meter.values();
        meter.feed_interleaved(&block);
        let second = meter.values();

        assert_ne!(first.integrated, second.integrated);
    }
}
