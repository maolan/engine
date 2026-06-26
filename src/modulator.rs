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
    pub rate_hz: f32,
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
            rate_hz: 1.0,
            phase: 0.0,
            enabled: true,
            targets: Vec::new(),
        }
    }

    /// Evaluate the modulator at a given transport sample and sample rate.
    /// Returns a normalized value in `[0, 1]`.
    pub fn value_at(&self, sample: usize, sample_rate: f64) -> f32 {
        let cycles = sample as f64 / sample_rate * self.rate_hz as f64 + self.phase as f64;
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
