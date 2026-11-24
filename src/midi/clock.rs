use crate::{message::TempoPoint, midi::io::MidiEvent};
use std::collections::BTreeMap;

pub const CLOCKS_PER_QUARTER_NOTE: u64 = 24;
const CLOCKS_PER_SONG_POSITION_BEAT: u64 = 6;
const ACTIVE_SENSING_INTERVAL_SECONDS: f64 = 0.25;

#[derive(Debug)]
struct Segment {
    start_sample: usize,
    end_sample: Option<usize>,
    start_pulse: f64,
    samples_per_pulse: f64,
}

#[derive(Debug)]
struct TempoMap {
    segments: Vec<Segment>,
}

impl TempoMap {
    fn new(sample_rate: f64, points: &[TempoPoint], fallback_bpm: f64) -> Self {
        let mut points_by_sample = BTreeMap::new();
        points_by_sample.insert(0, fallback_bpm.max(1.0));
        for point in points {
            points_by_sample.insert(point.sample, point.bpm.max(1.0));
        }
        let points: Vec<_> = points_by_sample.into_iter().collect();
        let mut segments = Vec::with_capacity(points.len());
        let mut start_pulse = 0.0;
        for (index, (start_sample, bpm)) in points.iter().copied().enumerate() {
            let end_sample = points.get(index + 1).map(|(sample, _)| *sample);
            let samples_per_pulse = sample_rate * 60.0 / (bpm * CLOCKS_PER_QUARTER_NOTE as f64);
            segments.push(Segment {
                start_sample,
                end_sample,
                start_pulse,
                samples_per_pulse,
            });
            if let Some(end_sample) = end_sample {
                start_pulse += (end_sample - start_sample) as f64 / samples_per_pulse;
            }
        }
        Self { segments }
    }

    fn first_pulse_at_or_after(&self, sample: usize) -> Option<u64> {
        let segment = self
            .segments
            .iter()
            .rev()
            .find(|segment| segment.start_sample <= sample)?;
        let relative = sample.saturating_sub(segment.start_sample) as f64;
        Some(
            (segment.start_pulse + relative / segment.samples_per_pulse - 1e-9)
                .ceil()
                .max(segment.start_pulse.ceil()) as u64,
        )
    }

    fn pulse_at(&self, sample: usize) -> u64 {
        let Some(segment) = self
            .segments
            .iter()
            .rev()
            .find(|segment| segment.start_sample <= sample)
        else {
            return 0;
        };
        (segment.start_pulse
            + sample.saturating_sub(segment.start_sample) as f64 / segment.samples_per_pulse)
            .floor()
            .max(0.0) as u64
    }

    fn pulse_sample(&self, pulse: u64) -> Option<usize> {
        self.segments.iter().find_map(|segment| {
            if (pulse as f64) < segment.start_pulse {
                return None;
            }
            let sample = segment.start_sample.saturating_add(
                ((pulse as f64 - segment.start_pulse) * segment.samples_per_pulse).round() as usize,
            );
            segment
                .end_sample
                .is_none_or(|end| sample < end)
                .then_some(sample)
        })
    }
}

#[derive(Debug)]
pub struct Generator {
    sample_rate: f64,
    expected_sample: Option<usize>,
    next_pulse: Option<u64>,
    next_active_sensing_sample: Option<usize>,
}

impl Generator {
    pub fn new(sample_rate: f64) -> Self {
        Self {
            sample_rate: sample_rate.max(1.0),
            expected_sample: None,
            next_pulse: None,
            next_active_sensing_sample: None,
        }
    }

    pub fn set_sample_rate(&mut self, sample_rate: f64) {
        self.sample_rate = sample_rate.max(1.0);
        self.expected_sample = None;
        self.next_pulse = None;
        self.next_active_sensing_sample = None;
    }

    pub fn reset(&mut self) {
        self.expected_sample = None;
        self.next_pulse = None;
        self.next_active_sensing_sample = None;
    }

    pub fn song_position(
        &self,
        position: usize,
        tempo_points: &[TempoPoint],
        fallback_bpm: f64,
    ) -> MidiEvent {
        let map = TempoMap::new(self.sample_rate, tempo_points, fallback_bpm);
        let beats = (map.pulse_at(position) / CLOCKS_PER_SONG_POSITION_BEAT).min(0x3FFF);
        MidiEvent::new(0, vec![0xF2, (beats & 0x7F) as u8, (beats >> 7) as u8])
    }

    pub fn generate(
        &mut self,
        cycle_start: usize,
        frames: usize,
        tempo_points: &[TempoPoint],
        fallback_bpm: f64,
    ) -> Vec<MidiEvent> {
        let cycle_end = cycle_start.saturating_add(frames);
        if frames == 0 {
            return vec![];
        }
        if self.expected_sample != Some(cycle_start) {
            self.next_pulse = None;
            self.next_active_sensing_sample = None;
        }
        let map = TempoMap::new(self.sample_rate, tempo_points, fallback_bpm);
        let mut pulse = self
            .next_pulse
            .or_else(|| map.first_pulse_at_or_after(cycle_start));
        let mut events = Vec::new();
        while let Some(current) = pulse {
            let Some(sample) = map.pulse_sample(current) else {
                pulse = current.checked_add(1);
                continue;
            };
            if sample >= cycle_end {
                break;
            }
            if sample >= cycle_start {
                events.push(MidiEvent::new(
                    sample.saturating_sub(cycle_start).min(u32::MAX as usize) as u32,
                    vec![0xF8],
                ));
            }
            pulse = current.checked_add(1);
        }
        self.next_pulse = pulse;
        let sensing_interval = (self.sample_rate * ACTIVE_SENSING_INTERVAL_SECONDS)
            .round()
            .max(1.0) as usize;
        let mut sensing_sample = self.next_active_sensing_sample.unwrap_or(cycle_start);
        while sensing_sample < cycle_end {
            if sensing_sample >= cycle_start {
                events.push(MidiEvent::new(
                    sensing_sample
                        .saturating_sub(cycle_start)
                        .min(u32::MAX as usize) as u32,
                    vec![0xFE],
                ));
            }
            sensing_sample = sensing_sample.saturating_add(sensing_interval);
        }
        self.next_active_sensing_sample = Some(sensing_sample);
        self.expected_sample = Some(cycle_end);
        events.sort_by_key(|event| event.frame);
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_24_clocks_per_quarter_note_at_sample_offsets() {
        let mut generator = Generator::new(48_000.0);
        let events = generator.generate(0, 24_000, &[], 120.0);

        let clocks: Vec<_> = events
            .iter()
            .filter(|event| event.data.as_ref() == [0xF8])
            .collect();
        assert_eq!(clocks.len(), 24);
        assert_eq!(clocks[1].frame, 1_000);
        assert_eq!(clocks[23].frame, 23_000);
        let second_half = generator.generate(24_000, 24_000, &[], 120.0);
        let clocks: Vec<_> = second_half
            .iter()
            .filter(|event| event.data.as_ref() == [0xF8])
            .collect();
        assert_eq!(clocks.len(), 24);
        assert_eq!(clocks[0].frame, 0);
    }

    #[test]
    fn clocks_follow_tempo_changes_within_a_cycle() {
        let mut generator = Generator::new(48_000.0);
        let tempo_points = vec![
            TempoPoint {
                sample: 0,
                bpm: 120.0,
            },
            TempoPoint {
                sample: 24_000,
                bpm: 60.0,
            },
        ];
        let events = generator.generate(0, 72_000, &tempo_points, 120.0);
        let clocks: Vec<_> = events
            .iter()
            .filter(|event| event.data.as_ref() == [0xF8])
            .collect();
        let offsets: Vec<_> = clocks.iter().map(|event| event.frame).collect();

        assert_eq!(clocks.len(), 48);
        assert_eq!(offsets[22], 22_000);
        assert_eq!(offsets[23], 23_000);
        assert_eq!(offsets[24], 24_000);
        assert_eq!(offsets[25], 26_000);
        assert_eq!(*offsets.last().expect("last clock"), 70_000);
    }

    #[test]
    fn resumes_from_an_arbitrary_position_without_duplicate_clocks() {
        let mut generator = Generator::new(48_000.0);
        let first = generator.generate(10_000, 4_000, &[], 120.0);
        let second = generator.generate(14_000, 4_000, &[], 120.0);
        let offsets: Vec<_> = first
            .iter()
            .filter(|event| event.data.as_ref() == [0xF8])
            .map(|event| event.frame + 10_000)
            .chain(
                second
                    .iter()
                    .filter(|event| event.data.as_ref() == [0xF8])
                    .map(|event| event.frame + 14_000),
            )
            .collect();

        assert_eq!(
            offsets,
            vec![
                10_000, 11_000, 12_000, 13_000, 14_000, 15_000, 16_000, 17_000
            ]
        );
    }

    #[test]
    fn discontinuity_reanchors_to_the_new_position() {
        let mut generator = Generator::new(48_000.0);
        let _ = generator.generate(0, 24_000, &[], 120.0);
        let events = generator.generate(48_000, 24_000, &[], 120.0);
        let clocks: Vec<_> = events
            .iter()
            .filter(|event| event.data.as_ref() == [0xF8])
            .collect();

        assert_eq!(clocks.len(), 24);
        assert_eq!(clocks[0].frame, 0);
    }

    #[test]
    fn emits_active_sensing_every_250_milliseconds() {
        let mut generator = Generator::new(48_000.0);
        let first = generator.generate(0, 24_000, &[], 120.0);
        let second = generator.generate(24_000, 24_000, &[], 120.0);
        let sensing: Vec<_> = first
            .iter()
            .chain(second.iter())
            .filter(|event| event.data.as_ref() == [0xFE])
            .map(|event| event.frame)
            .collect();

        assert_eq!(sensing, vec![0, 12_000, 0, 12_000]);
    }

    #[test]
    fn song_position_uses_midi_beats_and_tempo_map() {
        let generator = Generator::new(48_000.0);
        let event = generator.song_position(48_000, &[], 120.0);
        assert_eq!(event.data.as_ref(), [0xF2, 8, 0]);

        let tempo_points = vec![TempoPoint {
            sample: 24_000,
            bpm: 60.0,
        }];
        let event = generator.song_position(48_000, &tempo_points, 120.0);
        assert_eq!(event.data.as_ref(), [0xF2, 6, 0]);
    }
}
