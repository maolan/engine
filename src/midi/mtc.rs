#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Timecode {
    pub hours: u8,
    pub minutes: u8,
    pub seconds: u8,
    pub frames: u8,
    pub frame_rate: FrameRate,
}

impl Timecode {
    fn from_seconds(seconds: f64, frame_rate: FrameRate) -> Self {
        let seconds = seconds.max(0.0);
        let real_frame_rate = frame_rate.frames_per_second();
        let real_frames = (seconds * real_frame_rate + 1e-6).floor() as u64;
        let nominal_frames = if frame_rate == FrameRate::Fps2997Drop {
            let tens = real_frames / 17_982;
            let remainder = real_frames % 17_982;
            let dropped_minutes = if remainder < 2 {
                0
            } else {
                (remainder - 2) / 1798
            };
            real_frames + 18 * tens + 2 * dropped_minutes
        } else {
            real_frames
        };
        let nominal_rate = u64::from(frame_rate.max_frames());
        let frames_per_day = 24 * 3600 * nominal_rate;
        let nominal_frames = nominal_frames % frames_per_day;
        let whole_seconds = nominal_frames / nominal_rate;
        Self {
            hours: (whole_seconds / 3600) as u8,
            minutes: ((whole_seconds / 60) % 60) as u8,
            seconds: (whole_seconds % 60) as u8,
            frames: (nominal_frames % nominal_rate) as u8,
            frame_rate,
        }
    }

    fn new(hours: u8, minutes: u8, seconds: u8, frames: u8, frame_rate: FrameRate) -> Option<Self> {
        if hours > 23
            || minutes > 59
            || seconds > 59
            || frames >= frame_rate.max_frames()
            || (frame_rate == FrameRate::Fps2997Drop
                && seconds == 0
                && frames < 2
                && minutes % 10 != 0)
        {
            return None;
        }
        Some(Self {
            hours,
            minutes,
            seconds,
            frames,
            frame_rate,
        })
    }

    pub fn seconds(self) -> f64 {
        let whole_seconds =
            u64::from(self.hours) * 3600 + u64::from(self.minutes) * 60 + u64::from(self.seconds);
        if self.frame_rate == FrameRate::Fps2997Drop {
            let total_minutes = u64::from(self.hours) * 60 + u64::from(self.minutes);
            let dropped_frames = 2 * (total_minutes - total_minutes / 10);
            let frame_number = whole_seconds * 30 + u64::from(self.frames) - dropped_frames;
            frame_number as f64 / self.frame_rate.frames_per_second()
        } else {
            whole_seconds as f64 + f64::from(self.frames) / self.frame_rate.frames_per_second()
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FrameRate {
    Fps24,
    Fps25,
    Fps2997Drop,
    #[default]
    Fps30,
}

impl std::fmt::Display for FrameRate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Fps24 => "24 fps",
            Self::Fps25 => "25 fps",
            Self::Fps2997Drop => "29.97 fps drop-frame",
            Self::Fps30 => "30 fps",
        })
    }
}

impl FrameRate {
    fn code(self) -> u8 {
        match self {
            Self::Fps24 => 0,
            Self::Fps25 => 1,
            Self::Fps2997Drop => 2,
            Self::Fps30 => 3,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Fps24),
            1 => Some(Self::Fps25),
            2 => Some(Self::Fps2997Drop),
            3 => Some(Self::Fps30),
            _ => None,
        }
    }

    pub fn frames_per_second(self) -> f64 {
        match self {
            Self::Fps24 => 24.0,
            Self::Fps25 => 25.0,
            Self::Fps2997Drop => 30_000.0 / 1001.0,
            Self::Fps30 => 30.0,
        }
    }

    fn max_frames(self) -> u8 {
        match self {
            Self::Fps24 => 24,
            Self::Fps25 => 25,
            Self::Fps2997Drop | Self::Fps30 => 30,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TimecodeMessage {
    pub timecode: Timecode,
    /// Quarter-frame messages describe a position two frames after the last
    /// piece arrives. Full-frame SysEx messages describe the current position.
    pub running: bool,
}

impl TimecodeMessage {
    pub fn position_seconds(self) -> f64 {
        let offset = if self.running { 2.0 } else { 0.0 };
        self.timecode.seconds() + offset / self.timecode.frame_rate.frames_per_second()
    }
}

#[derive(Debug, Default)]
pub struct Decoder {
    pieces: [u8; 8],
    next_piece: Option<u8>,
}

impl Decoder {
    pub fn feed(&mut self, data: &[u8]) -> Option<TimecodeMessage> {
        match data {
            [0xF1, value] => self.feed_quarter_frame(*value),
            [
                0xF0,
                0x7F,
                0x7F,
                0x01,
                0x01,
                hours,
                minutes,
                seconds,
                frames,
                0xF7,
                ..,
            ] => {
                self.next_piece = None;
                Timecode::new(
                    hours & 0x1F,
                    *minutes,
                    *seconds,
                    *frames,
                    FrameRate::from_code(hours >> 5)?,
                )
                .map(|timecode| TimecodeMessage {
                    timecode,
                    running: false,
                })
            }
            _ => None,
        }
    }

    fn feed_quarter_frame(&mut self, value: u8) -> Option<TimecodeMessage> {
        let piece = value >> 4;
        if piece > 7 {
            self.next_piece = None;
            return None;
        }
        if piece == 0 {
            self.next_piece = Some(0);
        }
        if self.next_piece != Some(piece) {
            self.next_piece = None;
            return None;
        }
        self.pieces[piece as usize] = value & 0x0F;
        self.next_piece = if piece == 7 { None } else { Some(piece + 1) };
        if piece != 7 {
            return None;
        }

        let hours = (self.pieces[7] & 0x01) << 4 | self.pieces[6];
        Timecode::new(
            hours,
            self.pieces[5] << 4 | self.pieces[4],
            self.pieces[3] << 4 | self.pieces[2],
            self.pieces[1] << 4 | self.pieces[0],
            FrameRate::from_code(self.pieces[7] >> 1)?,
        )
        .map(|timecode| TimecodeMessage {
            timecode,
            running: true,
        })
    }
}

#[derive(Debug)]
pub struct Generator {
    sample_rate: f64,
    frame_rate: FrameRate,
    anchor_sample: usize,
    next_quarter: u64,
    expected_sample: Option<usize>,
    offset_seconds: f64,
}

impl Generator {
    pub fn new(sample_rate: f64, frame_rate: FrameRate, position: usize) -> Self {
        let sample_rate = sample_rate.max(1.0);
        let anchor_sample = aligned_frame_sample(sample_rate, frame_rate, position, 0.0);
        Self {
            sample_rate,
            frame_rate,
            anchor_sample,
            next_quarter: 0,
            expected_sample: Some(position),
            offset_seconds: 0.0,
        }
    }

    pub fn set_offset_seconds(&mut self, offset_seconds: f64) {
        self.offset_seconds = offset_seconds;
    }

    pub fn set_sample_rate(&mut self, sample_rate: f64, position: usize) {
        self.sample_rate = sample_rate.max(1.0);
        self.reset(position);
    }

    pub fn reset(&mut self, position: usize) -> usize {
        self.anchor_sample = aligned_frame_sample(
            self.sample_rate,
            self.frame_rate,
            position,
            self.offset_seconds,
        );
        self.next_quarter = 0;
        self.expected_sample = Some(position);
        self.anchor_sample
    }

    pub fn full_frame(&self, position: usize) -> crate::midi::io::MidiEvent {
        let timecode = Timecode::from_seconds(
            position as f64 / self.sample_rate + self.offset_seconds,
            self.frame_rate,
        );
        let hours = timecode.hours | (self.frame_rate.code() << 5);
        crate::midi::io::MidiEvent::new(
            0,
            vec![
                0xF0,
                0x7F,
                0x7F,
                0x01,
                0x01,
                hours,
                timecode.minutes,
                timecode.seconds,
                timecode.frames,
                0xF7,
            ],
        )
    }

    pub fn generate(
        &mut self,
        cycle_start: usize,
        frames: usize,
    ) -> Vec<crate::midi::io::MidiEvent> {
        let mut events = Vec::new();
        if self.expected_sample != Some(cycle_start) {
            let anchor = self.reset(cycle_start);
            events.push(self.full_frame(anchor));
        }
        let cycle_end = cycle_start.saturating_add(frames);
        let samples_per_quarter = self.sample_rate / (self.frame_rate.frames_per_second() * 4.0);
        loop {
            let sample = self
                .anchor_sample
                .saturating_add((self.next_quarter as f64 * samples_per_quarter).round() as usize);
            if sample >= cycle_end {
                break;
            }
            if sample >= cycle_start {
                let piece = (self.next_quarter % 8) as u8;
                let group = self.next_quarter / 8;
                let seconds = self.anchor_sample as f64 / self.sample_rate
                    + self.offset_seconds
                    + group as f64 * 2.0 / self.frame_rate.frames_per_second();
                let timecode = Timecode::from_seconds(seconds, self.frame_rate);
                let value = quarter_value(timecode, piece);
                events.push(crate::midi::io::MidiEvent::new(
                    sample.saturating_sub(cycle_start).min(u32::MAX as usize) as u32,
                    vec![0xF1, (piece << 4) | value],
                ));
            }
            self.next_quarter = self.next_quarter.saturating_add(1);
        }
        self.expected_sample = Some(cycle_end);
        events
    }
}

fn aligned_frame_sample(
    sample_rate: f64,
    frame_rate: FrameRate,
    position: usize,
    offset_seconds: f64,
) -> usize {
    let frames_per_second = frame_rate.frames_per_second();
    let seconds = position as f64 / sample_rate + offset_seconds;
    let frame = (seconds * frames_per_second - 1e-9).ceil().max(0.0) as u64;
    let frame = if frame_rate != FrameRate::Fps25 && frame % 2 == 1 {
        frame + 1
    } else {
        frame
    };
    ((frame as f64 / frames_per_second - offset_seconds) * sample_rate)
        .round()
        .max(0.0) as usize
}

fn quarter_value(timecode: Timecode, piece: u8) -> u8 {
    match piece {
        0 => timecode.frames & 0x0F,
        1 => (timecode.frames >> 4) & 0x01,
        2 => timecode.seconds & 0x0F,
        3 => (timecode.seconds >> 4) & 0x03,
        4 => timecode.minutes & 0x0F,
        5 => (timecode.minutes >> 4) & 0x03,
        6 => timecode.hours & 0x0F,
        7 => ((timecode.hours >> 4) & 0x01) | (timecode.frame_rate.code() << 1),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quarter_frames(hours: u8, minutes: u8, seconds: u8, frames: u8, rate: u8) -> [[u8; 2]; 8] {
        let values = [
            frames & 0x0F,
            (frames >> 4) & 0x01,
            seconds & 0x0F,
            (seconds >> 4) & 0x03,
            minutes & 0x0F,
            (minutes >> 4) & 0x03,
            hours & 0x0F,
            ((hours >> 4) & 0x01) | (rate << 1),
        ];
        std::array::from_fn(|piece| [0xF1, values[piece] | ((piece as u8) << 4)])
    }

    #[test]
    fn decodes_quarter_frame_sequence_with_two_frame_compensation() {
        let mut decoder = Decoder::default();
        let mut message = None;
        for event in quarter_frames(1, 2, 3, 4, 1) {
            if let Some(decoded) = decoder.feed(&event) {
                message = Some(decoded);
            }
        }

        let message = message.expect("complete quarter-frame sequence");
        assert!(message.running);
        assert_eq!(
            message.timecode,
            Timecode {
                hours: 1,
                minutes: 2,
                seconds: 3,
                frames: 4,
                frame_rate: FrameRate::Fps25,
            }
        );
        assert!((message.position_seconds() - (3723.0 + 6.0 / 25.0)).abs() < 1e-9);
    }

    #[test]
    fn decodes_full_frame_sysex_without_running_offset() {
        let mut decoder = Decoder::default();
        let message = decoder
            .feed(&[0xF0, 0x7F, 0x7F, 0x01, 0x01, 0x61, 2, 3, 4, 0xF7])
            .expect("valid full frame");

        assert!(!message.running);
        assert_eq!(message.timecode.hours, 1);
        assert_eq!(message.timecode.frame_rate, FrameRate::Fps30);
        assert!((message.position_seconds() - (3723.0 + 4.0 / 30.0)).abs() < 1e-9);
    }

    #[test]
    fn incomplete_or_out_of_order_quarter_frames_do_not_emit() {
        let mut decoder = Decoder::default();
        let events = quarter_frames(0, 0, 0, 0, 0);
        assert!(decoder.feed(&events[4]).is_none());
        assert!(decoder.feed(&events[5]).is_none());
        assert!(decoder.feed(&events[6]).is_none());
        assert!(decoder.feed(&events[7]).is_none());
    }

    #[test]
    fn rejects_invalid_timecode_fields() {
        let mut decoder = Decoder::default();
        assert!(
            decoder
                .feed(&[0xF0, 0x7F, 0x7F, 0x01, 0x01, 0, 0, 0, 24, 0xF7])
                .is_none()
        );
    }

    #[test]
    fn uses_drop_frame_rate_for_sample_conversion() {
        let mut decoder = Decoder::default();
        let message = decoder
            .feed(&[0xF0, 0x7F, 0x7F, 0x01, 0x01, 0x40, 0, 1, 0, 0xF7])
            .expect("valid drop-frame timecode");
        assert!((message.position_seconds() - (30.0 / (30_000.0 / 1001.0))).abs() < 1e-9);
        assert_eq!(
            message.timecode.frame_rate.frames_per_second(),
            30_000.0 / 1001.0
        );

        let message = decoder
            .feed(&[0xF0, 0x7F, 0x7F, 0x01, 0x01, 0x40, 1, 0, 2, 0xF7])
            .expect("valid frame after dropped labels");
        assert!((message.position_seconds() - (1800.0 / (30_000.0 / 1001.0))).abs() < 1e-9);
        assert!(
            decoder
                .feed(&[0xF0, 0x7F, 0x7F, 0x01, 0x01, 0x40, 1, 0, 0, 0xF7])
                .is_none()
        );
    }

    #[test]
    fn generator_emits_decodable_quarter_frames_at_sample_offsets() {
        let mut generator = Generator::new(48_000.0, FrameRate::Fps30, 0);
        let events = generator.generate(0, 3_200);

        assert_eq!(events.len(), 8);
        let offsets: Vec<u32> = events.iter().map(|event| event.frame).collect();
        assert_eq!(offsets, vec![0, 400, 800, 1200, 1600, 2000, 2400, 2800]);
        let mut decoder = Decoder::default();
        let decoded = events
            .iter()
            .filter_map(|event| decoder.feed(&event.data))
            .last()
            .expect("complete generated sequence");
        assert!(decoded.running);
        assert_eq!(decoded.timecode.frames, 0);
        assert!((decoded.position_seconds() - 2.0 / 30.0).abs() < 1e-9);
    }

    #[test]
    fn generator_reanchors_with_full_frame_after_a_jump() {
        let mut generator = Generator::new(48_000.0, FrameRate::Fps25, 0);
        let _ = generator.generate(0, 480);
        let events = generator.generate(48_000, 480);

        assert_eq!(events[0].data[0], 0xF0);
        let mut decoder = Decoder::default();
        let full = decoder.feed(&events[0].data).expect("full frame");
        assert_eq!(full.timecode.seconds, 1);
        assert!(!full.running);
    }

    #[test]
    fn drop_frame_encoding_round_trips_across_minute_boundary() {
        let generator = Generator::new(48_000.0, FrameRate::Fps2997Drop, 0);
        let seconds = 1800.0_f64 / (30_000.0 / 1001.0);
        let event = generator.full_frame((seconds * 48_000.0).round() as usize);
        let mut decoder = Decoder::default();
        let decoded = decoder.feed(&event.data).expect("valid full frame");

        assert_eq!(decoded.timecode.minutes, 1);
        assert_eq!(decoded.timecode.seconds, 0);
        assert_eq!(decoded.timecode.frames, 2);
        assert!((decoded.position_seconds() - seconds).abs() < 1.0 / 48_000.0);
    }

    #[test]
    fn generator_aligns_non_25_fps_sequences_to_even_frames() {
        let mut generator = Generator::new(48_000.0, FrameRate::Fps30, 0);
        let anchor = generator.reset(1_600);
        assert_eq!(anchor, 3_200);
        let full = Decoder::default()
            .feed(&generator.full_frame(anchor).data)
            .expect("full frame");
        assert_eq!(full.timecode.frames, 2);

        let mut generator = Generator::new(48_000.0, FrameRate::Fps25, 0);
        assert_eq!(generator.reset(1_920), 1_920);
    }

    #[test]
    fn generator_applies_timecode_offset() {
        let mut generator = Generator::new(48_000.0, FrameRate::Fps30, 0);
        generator.set_offset_seconds(3_600.0);
        let full = Decoder::default()
            .feed(&generator.full_frame(0).data)
            .expect("full frame");

        assert_eq!(full.timecode.hours, 1);
        assert_eq!(full.position_seconds(), 3_600.0);
    }
}
