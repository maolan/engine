use crate::{message::TempoPoint, midi::io::MidiEvent};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

pub const CLOCKS_PER_QUARTER_NOTE: u64 = 24;
pub const MIN_CLOCK_BPM: f64 = 20.0;
pub const MAX_CLOCK_BPM: f64 = 400.0;
const CLOCKS_PER_SONG_POSITION_BEAT: u64 = 6;
const ACTIVE_SENSING_INTERVAL_SECONDS: f64 = 0.25;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputEvent {
    Start,
    Continue,
    Stop,
    SongPosition(u16),
    Clock { bpm: Option<f64>, pulse: u64 },
}

#[derive(Debug, Default)]
pub struct Receiver {
    last_clock: Option<Instant>,
    pending_clocks: u64,
    pulses: u64,
    clock_count: u64,
    time: f64,
    t0: f64,
    t1: f64,
    e2: f64,
    b: f64,
    c: f64,
    bpm: Option<f64>,
}

impl Receiver {
    pub fn feed(&mut self, data: &[u8], now: Instant) -> Option<InputEvent> {
        match data {
            [0xF8] => {
                self.pending_clocks = self.pending_clocks.saturating_add(1);
                if let Some(previous) = self.last_clock
                    && now > previous
                {
                    let elapsed = now.saturating_duration_since(previous);
                    let count = self.pending_clocks.min(u64::from(u32::MAX)) as u32;
                    let interval = elapsed / count;
                    if interval >= Duration::from_micros(100)
                        && interval <= Duration::from_millis(500)
                    {
                        for _ in 0..count {
                            self.update_dll(interval.as_secs_f64());
                        }
                    }
                    self.last_clock = Some(now);
                    self.pending_clocks = 0;
                } else if self.last_clock.is_none() {
                    self.last_clock = Some(now);
                    self.pending_clocks = 0;
                }
                self.pulses = self.pulses.saturating_add(1);
                Some(InputEvent::Clock {
                    bpm: self.bpm,
                    pulse: self.pulses,
                })
            }
            [0xFA] => {
                self.reset_position();
                Some(InputEvent::Start)
            }
            [0xFB] => {
                self.last_clock = None;
                self.pending_clocks = 0;
                Some(InputEvent::Continue)
            }
            [0xFC] => {
                self.last_clock = None;
                self.pending_clocks = 0;
                Some(InputEvent::Stop)
            }
            [0xF2, lsb, msb] => {
                let beats = u16::from(*lsb & 0x7F) | (u16::from(*msb & 0x7F) << 7);
                self.last_clock = None;
                self.pending_clocks = 0;
                Some(InputEvent::SongPosition(beats))
            }
            _ => None,
        }
    }

    fn update_dll(&mut self, interval: f64) {
        self.time += interval;
        if self.clock_count == 0 {
            self.clock_count = 1;
            return;
        }
        if self.clock_count == 1 {
            let bpm = 60.0 / (interval * CLOCKS_PER_QUARTER_NOTE as f64);
            if !(1.0..=999.0).contains(&bpm) {
                return;
            }
            self.bpm = Some(bpm.round().clamp(MIN_CLOCK_BPM, MAX_CLOCK_BPM));
            self.e2 = interval;
            let omega =
                2.0 * std::f64::consts::PI * (60.0 / 500.0) / CLOCKS_PER_QUARTER_NOTE as f64;
            self.b = 2.0_f64.sqrt() * omega;
            self.c = omega * omega;
            self.t0 = self.time;
            self.t1 = self.time + self.e2;
            self.clock_count = 2;
            return;
        }

        let error = self.time - self.t1;
        self.t0 = self.t1;
        self.t1 += self.b * error + self.e2;
        self.e2 += self.c * error;
        let samples_per_quarter = self.t1 - self.t0;
        if samples_per_quarter > 0.0 {
            let bpm = 60.0 / (samples_per_quarter * CLOCKS_PER_QUARTER_NOTE as f64);
            if (1.0..=999.0).contains(&bpm) {
                self.bpm = Some(bpm.round().clamp(MIN_CLOCK_BPM, MAX_CLOCK_BPM));
            }
        }
        self.clock_count = self.clock_count.saturating_add(1);
    }

    fn reset_position(&mut self) {
        self.last_clock = None;
        self.pending_clocks = 0;
        self.pulses = 0;
        self.clock_count = 0;
        self.time = 0.0;
        self.t0 = 0.0;
        self.t1 = 0.0;
        self.e2 = 0.0;
        self.b = 0.0;
        self.c = 0.0;
        self.bpm = None;
    }
}

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

pub fn song_position_sample(
    sample_rate: f64,
    tempo_points: &[TempoPoint],
    fallback_bpm: f64,
    beats: u16,
) -> usize {
    let map = TempoMap::new(sample_rate, tempo_points, fallback_bpm);
    map.pulse_sample(u64::from(beats) * CLOCKS_PER_SONG_POSITION_BEAT)
        .unwrap_or(usize::MAX)
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
    fn receiver_measures_tempo_over_24_ppqn() {
        let mut receiver = Receiver::default();
        let start = Instant::now();
        assert_eq!(receiver.feed(&[0xFA], start), Some(InputEvent::Start));
        let mut measured = None;
        for pulse in 1..=24 {
            if let InputEvent::Clock { bpm, .. } = receiver
                .feed(&[0xF8], start + Duration::from_millis(pulse * 25))
                .expect("clock event")
            {
                measured = bpm.or(measured);
            }
        }
        let measured = measured.expect("measured tempo");
        assert!((measured - 100.0).abs() < 0.0001, "measured {measured}");
    }

    #[test]
    fn receiver_measures_batched_clocks_at_300_bpm() {
        let mut receiver = Receiver::default();
        let start = Instant::now();
        receiver.feed(&[0xFA], start);
        let mut measured = None;
        for batch in 0..12 {
            let now = start + Duration::from_micros(batch * 16_667);
            for _ in 0..2 {
                if let InputEvent::Clock { bpm, .. } =
                    receiver.feed(&[0xF8], now).expect("clock event")
                {
                    measured = bpm.or(measured);
                }
            }
        }
        let measured = measured.expect("measured tempo");
        assert!((measured - 300.0).abs() < 0.1, "measured {measured}");
    }

    #[test]
    fn receiver_handles_transport_and_song_position() {
        let mut receiver = Receiver::default();
        let now = Instant::now();
        assert_eq!(
            receiver.feed(&[0xF2, 0x08, 0x01], now),
            Some(InputEvent::SongPosition(136))
        );
        assert_eq!(receiver.feed(&[0xFB], now), Some(InputEvent::Continue));
        assert_eq!(receiver.feed(&[0xFC], now), Some(InputEvent::Stop));
    }

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

    #[test]
    fn song_position_sample_inverts_the_tempo_map() {
        let tempo_points = vec![TempoPoint {
            sample: 48_000,
            bpm: 60.0,
        }];

        assert_eq!(
            song_position_sample(48_000.0, &tempo_points, 120.0, 8),
            48_000
        );
        assert_eq!(
            song_position_sample(48_000.0, &tempo_points, 120.0, 12),
            96_000
        );
    }

    #[test]
    fn receiver_clamps_tempo_to_the_supported_range() {
        let mut receiver = Receiver::default();
        let start = Instant::now();
        receiver.feed(&[0xFA], start);
        let mut measured = None;
        for pulse in 1..=24 {
            if let InputEvent::Clock { bpm, .. } = receiver
                .feed(&[0xF8], start + Duration::from_millis(pulse * 5))
                .expect("clock event")
            {
                measured = bpm.or(measured);
            }
        }

        assert_eq!(measured, Some(MAX_CLOCK_BPM));
    }
}
