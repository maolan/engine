use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ModulatorController {
    Volume,
    Balance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ModulatorShape {
    #[default]
    Sine,
    Triangle,
    Saw,
    Square,
    SampleHold,
}

impl std::fmt::Display for ModulatorShape {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sine => write!(f, "Sine"),
            Self::Triangle => write!(f, "Triangle"),
            Self::Saw => write!(f, "Saw"),
            Self::Square => write!(f, "Square"),
            Self::SampleHold => write!(f, "Sample & Hold"),
        }
    }
}

/// Musical note division used for tempo-synced modulator rates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MusicalDivision {
    Bar,
    Half,
    Beat,
    Eighth,
    Sixteenth,
    ThirtySecond,
    SixtyFourth,
}

impl MusicalDivision {
    /// Convert this division to a frequency in Hz for the given tempo and time signature.
    /// Bar length is computed from the time signature; beat refers to a quarter-note beat.
    pub fn to_hz(self, bpm: f64, tsig_num: u16, tsig_denom: u16) -> f64 {
        let beat_hz = bpm / 60.0;
        let bar_hz = beat_hz / (tsig_num as f64 * 4.0 / tsig_denom.max(1) as f64);
        match self {
            Self::Bar => bar_hz,
            Self::Half => beat_hz / 2.0,
            Self::Beat => beat_hz,
            Self::Eighth => beat_hz * 2.0,
            Self::Sixteenth => beat_hz * 4.0,
            Self::ThirtySecond => beat_hz * 8.0,
            Self::SixtyFourth => beat_hz * 16.0,
        }
    }
}

impl std::fmt::Display for MusicalDivision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bar => write!(f, "1/1"),
            Self::Half => write!(f, "1/2"),
            Self::Beat => write!(f, "1/4"),
            Self::Eighth => write!(f, "1/8"),
            Self::Sixteenth => write!(f, "1/16"),
            Self::ThirtySecond => write!(f, "1/32"),
            Self::SixtyFourth => write!(f, "1/64"),
        }
    }
}

/// Modulator rate specified either as a fixed frequency or as a tempo-synced division.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum ModulatorRate {
    Hz(f32),
    Musical(MusicalDivision),
}

impl Default for ModulatorRate {
    fn default() -> Self {
        Self::Hz(1.0)
    }
}

impl ModulatorRate {
    /// Return the effective frequency in Hz for the given transport timing.
    pub fn effective_hz(&self, bpm: f64, tsig_num: u16, tsig_denom: u16) -> f64 {
        match *self {
            Self::Hz(hz) => f64::from(hz),
            Self::Musical(div) => div.to_hz(bpm, tsig_num, tsig_denom),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ModulatorTarget {
    TrackVolume {
        track_name: String,
        min: f32,
        max: f32,
    },
    TrackBalance {
        track_name: String,
        min: f32,
        max: f32,
    },
    HwOutVolume {
        min: f32,
        max: f32,
    },
    HwOutBalance {
        min: f32,
        max: f32,
    },
    ClapParameter {
        track_name: String,
        instance_id: usize,
        param_id: u32,
        min: f64,
        max: f64,
    },
    Vst3Parameter {
        track_name: String,
        instance_id: usize,
        param_id: u32,
        min: f32,
        max: f32,
    },
    #[cfg(all(unix, not(target_os = "macos")))]
    Lv2Parameter {
        track_name: String,
        instance_id: usize,
        index: u32,
        min: f32,
        max: f32,
    },
    MidiCc {
        track_name: String,
        channel: u8,
        cc: u8,
    },
}

impl ModulatorTarget {
    pub fn track_name(&self) -> Option<&str> {
        match self {
            Self::TrackVolume { track_name, .. }
            | Self::TrackBalance { track_name, .. }
            | Self::ClapParameter { track_name, .. }
            | Self::Vst3Parameter { track_name, .. }
            | Self::MidiCc { track_name, .. } => Some(track_name),
            #[cfg(all(unix, not(target_os = "macos")))]
            Self::Lv2Parameter { track_name, .. } => Some(track_name),
            Self::HwOutVolume { .. } | Self::HwOutBalance { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Modulator {
    pub id: usize,
    pub name: String,
    pub shape: ModulatorShape,
    pub rate: ModulatorRate,
    pub phase: f32,
    pub enabled: bool,
    pub targets: Vec<ModulatorTarget>,
}

/// Map a normalized modulator value in `[0, 1]` to a target range.
/// When `min <= max` the output increases with the modulator value.
/// When `min > max` the output is reversed (decreases as the modulator value increases).
pub fn map_value(value: f32, min: f32, max: f32) -> f32 {
    if min <= max {
        (min + value * (max - min)).clamp(min, max)
    } else {
        (max + (1.0 - value) * (min - max)).clamp(max, min)
    }
}

/// `map_value` for `f64` target ranges.
pub fn map_value_f64(value: f32, min: f64, max: f64) -> f64 {
    let value = f64::from(value);
    if min <= max {
        (min + value * (max - min)).clamp(min, max)
    } else {
        (max + (1.0 - value) * (min - max)).clamp(max, min)
    }
}

impl Modulator {
    pub fn new(id: usize) -> Self {
        Self {
            id,
            name: format!("Modulator {id}"),
            shape: ModulatorShape::default(),
            rate: ModulatorRate::default(),
            phase: 0.0,
            enabled: true,
            targets: Vec::new(),
        }
    }

    /// Evaluate the modulator at a given transport sample, sample rate, and timing.
    /// Returns a normalized value in `[0, 1]`.
    pub fn value_at(
        &self,
        sample: usize,
        sample_rate: f64,
        bpm: f64,
        tsig_num: u16,
        tsig_denom: u16,
    ) -> f32 {
        let rate_hz = self.rate.effective_hz(bpm, tsig_num, tsig_denom);
        let cycles = sample as f64 / sample_rate * rate_hz + self.phase as f64;
        let phase = cycles.rem_euclid(1.0) as f32;
        let raw = match self.shape {
            ModulatorShape::Sine => (phase * 2.0 * std::f32::consts::PI).sin(),
            ModulatorShape::Triangle => {
                if phase < 0.5 {
                    4.0 * phase - 1.0
                } else {
                    3.0 - 4.0 * phase
                }
            }
            ModulatorShape::Saw => 2.0 * phase - 1.0,
            ModulatorShape::Square => {
                if phase < 0.5 {
                    1.0
                } else {
                    -1.0
                }
            }
            ModulatorShape::SampleHold => {
                let step = (phase * 16.0).floor() as i32;
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                use std::hash::{Hash, Hasher};
                step.hash(&mut hasher);
                let h = hasher.finish();
                ((h as f32 / u64::MAX as f32) * 2.0) - 1.0
            }
        };
        ((raw + 1.0) / 2.0).clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_value_maps_forward_range() {
        assert!((map_value(0.0, 0.0, 100.0) - 0.0).abs() < f32::EPSILON);
        assert!((map_value(1.0, 0.0, 100.0) - 100.0).abs() < f32::EPSILON);
        assert!((map_value(0.5, 0.0, 100.0) - 50.0).abs() < f32::EPSILON);
    }

    #[test]
    fn map_value_reverses_when_min_greater_than_max() {
        assert!((map_value(0.0, 100.0, 0.0) - 100.0).abs() < f32::EPSILON);
        assert!((map_value(1.0, 100.0, 0.0) - 0.0).abs() < f32::EPSILON);
        assert!((map_value(0.5, 100.0, 0.0) - 50.0).abs() < f32::EPSILON);
    }

    #[test]
    fn map_value_clamps_out_of_range() {
        assert!((map_value(-0.5, 0.0, 100.0) - 0.0).abs() < f32::EPSILON);
        assert!((map_value(1.5, 0.0, 100.0) - 100.0).abs() < f32::EPSILON);
        assert!((map_value(-0.5, 100.0, 0.0) - 100.0).abs() < f32::EPSILON);
        assert!((map_value(1.5, 100.0, 0.0) - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn map_value_f64_reverses_when_min_greater_than_max() {
        assert!((map_value_f64(0.0, 1.0, 0.0) - 1.0).abs() < f64::EPSILON);
        assert!((map_value_f64(1.0, 1.0, 0.0) - 0.0).abs() < f64::EPSILON);
        assert!((map_value_f64(0.5, 1.0, 0.0) - 0.5).abs() < f64::EPSILON);
    }
}
