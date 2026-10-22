use super::Audio;
use super::sync::ChannelState;

#[derive(Debug, Clone, Default)]
pub(super) struct Buffer {
    data: Vec<u8>,
    pos: usize,
}

impl Buffer {
    pub(super) fn with_size(size: usize) -> Self {
        Self {
            data: if size == 0 {
                Vec::new()
            } else {
                vec![0_u8; size]
            },
            pos: 0,
        }
    }

    pub(super) fn len(&self) -> usize {
        self.data.len()
    }

    pub(super) fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    pub(super) fn done(&self) -> bool {
        self.pos >= self.data.len()
    }

    pub(super) fn reset(&mut self) {
        self.pos = 0;
    }

    pub(super) fn clear(&mut self) {
        self.data.fill(0);
        self.pos = 0;
    }

    pub(super) fn advance(&mut self, bytes: usize) -> usize {
        let n = bytes.min(self.remaining());
        self.pos += n;
        n
    }

    pub(super) fn rewind(&mut self, bytes: usize) -> usize {
        let n = bytes.min(self.pos);
        self.pos -= n;
        n
    }

    pub(super) fn position(&mut self) -> &mut [u8] {
        let pos = self.pos;
        &mut self.data[pos..]
    }

    pub(super) fn as_slice(&self) -> &[u8] {
        &self.data
    }

    pub(super) fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.data
    }
}

#[derive(Debug, Clone)]
pub(super) struct BufferRecord {
    buffer: Buffer,
    end_frames: i64,
}

impl BufferRecord {
    fn empty() -> Self {
        Self {
            buffer: Buffer::default(),
            end_frames: 0,
        }
    }

    fn valid(&self) -> bool {
        self.buffer.len() > 0
    }
}

#[derive(Debug, Default)]
pub(super) struct ReadChannel {
    st: ChannelState,
    map_progress: i64,
    read_position: i64,
}
#[derive(Debug, Default)]
pub(super) struct WriteChannel {
    st: ChannelState,
    map_progress: i64,
    write_position: i64,
}

#[derive(Debug)]
pub(super) enum ChannelKind {
    Read(ReadChannel),
    Write(WriteChannel),
}

#[derive(Debug)]
pub(super) struct DoubleBufferedChannel {
    kind: ChannelKind,
    buffer_a: BufferRecord,
    buffer_b: BufferRecord,
}

impl DoubleBufferedChannel {
    pub(super) fn new_empty_read() -> Self {
        Self {
            kind: ChannelKind::Read(ReadChannel::default()),
            buffer_a: BufferRecord::empty(),
            buffer_b: BufferRecord::empty(),
        }
    }

    pub(super) fn new_empty_write() -> Self {
        Self {
            kind: ChannelKind::Write(WriteChannel::default()),
            buffer_a: BufferRecord::empty(),
            buffer_b: BufferRecord::empty(),
        }
    }

    pub(super) fn new_read(buffer_bytes: usize, frames: i64) -> Self {
        let mut s = Self::new_empty_read();
        s.set_buffer(Buffer::with_size(buffer_bytes), 0);
        s.set_buffer(Buffer::with_size(buffer_bytes), frames);
        s
    }

    pub(super) fn new_write(buffer_bytes: usize, frames: i64) -> Self {
        let mut s = Self::new_empty_write();
        s.set_buffer(Buffer::with_size(buffer_bytes), 0);
        s.set_buffer(Buffer::with_size(buffer_bytes), frames);
        s
    }

    pub(super) fn set_buffer(&mut self, buffer: Buffer, end_frames: i64) -> bool {
        if !self.buffer_b.valid() {
            self.buffer_b = BufferRecord { buffer, end_frames };
            if !self.buffer_a.valid() {
                std::mem::swap(&mut self.buffer_a, &mut self.buffer_b);
            }
            return true;
        }
        false
    }

    pub(super) fn take_buffer(&mut self) -> Buffer {
        std::mem::swap(&mut self.buffer_a, &mut self.buffer_b);
        std::mem::take(&mut self.buffer_b.buffer)
    }

    /// Silence queued audio without moving it relative to the capture grid.
    pub(super) fn silence_buffers(&mut self) {
        self.buffer_a.buffer.clear();
        self.buffer_b.buffer.clear();
    }

    pub(super) fn capture_overshoot(&self) -> i64 {
        match &self.kind {
            ChannelKind::Read(read) => {
                (read.st.last_progress - (self.end_frames() - read.st.max_progress)).max(0)
            }
            ChannelKind::Write(_) => 0,
        }
    }

    pub(super) fn reset_buffers(&mut self, end_frames: i64, frame_size: usize) {
        if self.buffer_a.valid() {
            self.buffer_a.buffer.clear();
            self.buffer_a.end_frames = end_frames;
        }
        if self.buffer_b.valid() {
            self.buffer_b.buffer.clear();
            self.buffer_b.end_frames =
                end_frames + (self.buffer_b.buffer.len() / frame_size) as i64;
        }
    }

    pub(super) fn end_frames(&self) -> i64 {
        if self.buffer_a.valid() {
            self.buffer_a.end_frames
        } else {
            0
        }
    }

    pub(super) fn process(&mut self, audio: &mut Audio, now: i64) -> std::io::Result<()> {
        let now = now - (now % audio.stepping());
        match &mut self.kind {
            ChannelKind::Read(read) => {
                Self::process_read(audio, read, &mut self.buffer_a, now)?;
                if self.buffer_a.buffer.done() && self.buffer_b.valid() {
                    Self::process_read(audio, read, &mut self.buffer_b, now)?;
                }
            }
            ChannelKind::Write(write) => {
                Self::process_write(audio, write, &mut self.buffer_a, now)?;
                if self.buffer_a.buffer.done() && self.buffer_b.valid() {
                    Self::process_write(audio, write, &mut self.buffer_b, now)?;
                }
            }
        }
        Ok(())
    }

    fn process_read(
        audio: &mut Audio,
        read: &mut ReadChannel,
        rec: &mut BufferRecord,
        now: i64,
    ) -> std::io::Result<()> {
        if read.st.last_processing != now {
            if audio.mapped {
                let progress = audio.update_map_progress()?;
                read.map_progress = audio.map_progress.frames;
                let available = read.st.last_progress + progress - read.read_position;
                let loss = read.st.mark_loss(available - audio.buffer_frames());
                read.st.mark_progress(progress, now, audio.stepping());
                if loss > 0 {
                    read.read_position = read.st.last_progress - audio.buffer_frames();
                }
            } else {
                let queued = audio.queued_samples() as i64;
                let overdue = now - read.st.estimated_dropout(queued, audio.buffer_frames());
                if (overdue > 0 && audio.get_rec_overruns() > 0) || overdue > read.st.max_progress {
                    let progress = audio.buffer_frames() - queued;
                    let loss = read.st.mark_loss_from(progress, now);
                    read.st
                        .mark_progress(progress + loss, now, audio.stepping());
                    read.read_position = read.st.last_progress - audio.buffer_frames();
                } else {
                    let progress = queued - (read.st.last_progress - read.read_position);
                    read.st.mark_progress(progress, now, audio.stepping());
                    read.read_position = read.st.last_progress - queued;
                }
            }
        }

        if audio.mapped {
            let extra_latency = read.st.max_progress;
            let mut position = rec.end_frames
                - extra_latency
                - (rec.buffer.remaining() / audio.frame_size()) as i64;
            let oldest = if read.map_progress < audio.buffer_frames() {
                read.st.last_progress - read.map_progress
            } else {
                read.st.last_progress - audio.buffer_frames()
            };
            if oldest > position {
                let skip = rec
                    .buffer
                    .remaining()
                    .min(((oldest - position) as usize) * audio.frame_size());
                if skip > 0 {
                    rec.buffer.position()[..skip].fill(0);
                    rec.buffer.advance(skip);
                    position += (skip / audio.frame_size()) as i64;
                }
            } else if position != read.read_position {
                let rewind = rec
                    .buffer
                    .pos
                    .min(((position - oldest).max(0) as usize) * audio.frame_size());
                if rewind > 0 {
                    rec.buffer.rewind(rewind);
                    position -= (rewind / audio.frame_size()) as i64;
                }
            }

            if position >= oldest && position < read.st.last_progress && !rec.buffer.done() {
                let offset = read.st.last_progress - position;
                let len = rec
                    .buffer
                    .remaining()
                    .min((offset as usize) * audio.frame_size());
                let pointer = ((read.map_progress - offset).rem_euclid(audio.buffer_frames())
                    as usize)
                    * audio.frame_size();
                let len = audio.read_map(rec.buffer.position(), pointer, len);
                rec.buffer.advance(len);
                read.read_position = rec.end_frames
                    - extra_latency
                    - (rec.buffer.remaining() / audio.frame_size()) as i64;
            }
        } else if audio.queued_samples() > 0 && !rec.buffer.done() {
            let mut position =
                rec.end_frames - (rec.buffer.remaining() / audio.frame_size()) as i64;
            if read.read_position > position {
                let skip = rec
                    .buffer
                    .remaining()
                    .min(((read.read_position - position) as usize) * audio.frame_size());
                if skip > 0 {
                    rec.buffer.position()[..skip].fill(0);
                    rec.buffer.advance(skip);
                    position += (skip / audio.frame_size()) as i64;
                }
            } else if position > read.read_position {
                let rewind = rec
                    .buffer
                    .pos
                    .min(((position - read.read_position) as usize) * audio.frame_size());
                if rewind > 0 {
                    rec.buffer.rewind(rewind);
                    position -= (rewind / audio.frame_size()) as i64;
                }
            }

            if position > read.read_position {
                let gap = (position - read.read_position) as usize;
                let read_limit = rec.buffer.remaining().min(gap * audio.frame_size());
                let mut bytes_read = 0_usize;
                audio.read_io(rec.buffer.position(), read_limit, &mut bytes_read)?;
                read.read_position += (bytes_read / audio.frame_size()) as i64;
            } else if position == read.read_position {
                let mut bytes_read = 0_usize;
                let remaining = rec.buffer.remaining();
                audio.read_io(rec.buffer.position(), remaining, &mut bytes_read)?;
                read.read_position += (bytes_read / audio.frame_size()) as i64;
                rec.buffer.advance(bytes_read);
            }
        }

        if read.st.freewheel() && now >= rec.end_frames + read.st.balance && !rec.buffer.done() {
            rec.buffer.position().fill(0);
            let advanced = rec.buffer.advance(rec.buffer.remaining());
            read.read_position += (advanced / audio.frame_size()) as i64;
        }

        Ok(())
    }

    fn process_write(
        audio: &mut Audio,
        write: &mut WriteChannel,
        rec: &mut BufferRecord,
        now: i64,
    ) -> std::io::Result<()> {
        if write.st.last_processing != now {
            if audio.mapped {
                let progress = audio.update_map_progress()?;
                if progress > 0 {
                    let start = (write.map_progress.rem_euclid(audio.buffer_frames()) as usize)
                        * audio.frame_size();
                    // After a stall, at most one ring needs clearing. Keep
                    // all elapsed frames in the progress/loss accounting.
                    let length = progress.min(audio.buffer_frames()) as usize * audio.frame_size();
                    audio.write_map(None, start, length);
                    write.map_progress = audio.map_progress.frames;
                }
                let loss = write
                    .st
                    .mark_loss(write.st.last_progress + progress - write.write_position);
                write.st.mark_progress(progress, now, audio.stepping());
                if loss > 0 {
                    write.write_position = write.st.last_progress;
                }
            } else {
                let queued = audio.queued_samples() as i64;
                let overdue = now - write.st.estimated_dropout(queued, audio.buffer_frames());
                if (overdue > 0 && audio.get_play_underruns() > 0)
                    || overdue > write.st.max_progress
                {
                    let progress = write.write_position - write.st.last_progress;
                    let loss = write.st.mark_loss_from(progress, now);
                    write
                        .st
                        .mark_progress(progress + loss, now, audio.stepping());
                    write.write_position = write.st.last_progress;
                } else {
                    let progress = (write.write_position - write.st.last_progress) - queued;
                    write.st.mark_progress(progress, now, audio.stepping());
                    write.write_position = write.st.last_progress + queued;
                }
            }
        }

        if audio.mapped {
            let mut position =
                rec.end_frames - (rec.buffer.remaining() / audio.frame_size()) as i64;
            let skip = rec
                .buffer
                .remaining()
                .min(((write.st.last_progress - position).max(0) as usize) * audio.frame_size());
            if skip > 0 {
                rec.buffer.advance(skip);
                position += (skip / audio.frame_size()) as i64;
            } else if position != write.write_position {
                let rewind = rec.buffer.pos.min(
                    ((position - write.st.last_progress).max(0) as usize) * audio.frame_size(),
                );
                if rewind > 0 {
                    rec.buffer.rewind(rewind);
                    position -= (rewind / audio.frame_size()) as i64;
                }
            }

            if !rec.buffer.done()
                && position >= write.st.last_progress
                && position < write.st.last_progress + audio.buffer_frames()
            {
                if write.write_position < position && write.write_position + 8 >= position {
                    let offset = write.write_position - write.st.last_progress;
                    let pointer = ((write.map_progress + offset).rem_euclid(audio.buffer_frames())
                        as usize)
                        * audio.frame_size();
                    let len = rec
                        .buffer
                        .remaining()
                        .min(((position - write.write_position) as usize) * audio.frame_size());
                    let written = audio.write_map(Some(rec.buffer.position()), pointer, len);
                    if written < len {
                        tracing::warn!(
                            "OSS mapped audio write was short: wrote {written} of {len} bytes"
                        );
                    }
                }

                let offset = position - write.st.last_progress;
                let pointer = ((write.map_progress + offset).rem_euclid(audio.buffer_frames())
                    as usize)
                    * audio.frame_size();
                let len = rec
                    .buffer
                    .remaining()
                    .min(((audio.buffer_frames() - offset) as usize) * audio.frame_size());
                let written = audio.write_map(Some(rec.buffer.position()), pointer, len);
                if written > 0 {
                    crate::cycle_trace::set_frame_metric(
                        crate::cycle_trace::FrameMetric::PlaybackLeadFrames,
                        offset,
                    );
                    crate::cycle_trace::mark(crate::cycle_trace::TracePoint::PlaybackMapDone);
                }
                rec.buffer.advance(written);
                write.write_position =
                    rec.end_frames - (rec.buffer.remaining() / audio.frame_size()) as i64;
            }
        } else if audio.queued_samples() < audio.buffer_frames() as i32 && !rec.buffer.done() {
            let mut position =
                rec.end_frames - (rec.buffer.remaining() / audio.frame_size()) as i64;
            let rewind = rec
                .buffer
                .pos
                .min(((position - write.write_position).max(0) as usize) * audio.frame_size());
            if rewind > 0 {
                rec.buffer.rewind(rewind);
                position -= (rewind / audio.frame_size()) as i64;
            } else {
                let skip = rec
                    .buffer
                    .remaining()
                    .min(((write.write_position - position).max(0) as usize) * audio.frame_size());
                if skip > 0 {
                    rec.buffer.advance(skip);
                    position += (skip / audio.frame_size()) as i64;
                }
            }

            if position > write.write_position {
                let gap = (position - write.write_position) as usize;
                let write_limit = rec.buffer.remaining().min(gap * audio.frame_size());
                let mut bytes_written = 0_usize;
                audio.write_io(rec.buffer.position(), write_limit, &mut bytes_written)?;
                write.write_position += (bytes_written / audio.frame_size()) as i64;
            } else if position == write.write_position {
                let mut bytes_written = 0_usize;
                let remaining = rec.buffer.remaining();
                audio.write_io(rec.buffer.position(), remaining, &mut bytes_written)?;
                write.write_position += (bytes_written / audio.frame_size()) as i64;
                rec.buffer.advance(bytes_written);
            }
        }

        if write.st.freewheel() && now >= rec.end_frames + write.st.balance && !rec.buffer.done() {
            let advanced = rec.buffer.advance(rec.buffer.remaining());
            write.write_position += (advanced / audio.frame_size()) as i64;
        }

        Ok(())
    }

    pub(super) fn wakeup_time(&self, audio: &Audio, now: i64) -> i64 {
        let available = match &self.kind {
            ChannelKind::Read(read) => read.st.last_progress - read.read_position,
            ChannelKind::Write(write) => {
                write.st.last_progress + audio.buffer_frames() - write.write_position
            }
        };
        if available > 0
            && ((self.buffer_a.valid() && !self.buffer_a.buffer.done())
                || (self.buffer_b.valid() && !self.buffer_b.buffer.done()))
        {
            return now;
        }
        let sync_frames = if !self.buffer_a.valid() {
            i64::MAX
        } else if !self.finished(now) {
            self.end_frames()
                + match &self.kind {
                    ChannelKind::Read(read) => read.st.balance,
                    ChannelKind::Write(write) => write.st.balance,
                }
        } else if self.buffer_b.valid() && !self.total_finished(now) {
            self.buffer_b.end_frames
                + match &self.kind {
                    ChannelKind::Read(read) => read.st.balance,
                    ChannelKind::Write(write) => write.st.balance,
                }
        } else {
            i64::MAX
        };

        match &self.kind {
            ChannelKind::Read(read) => read.st.wakeup_time(
                sync_frames,
                (read.st.last_progress - read.read_position).clamp(0, audio.buffer_frames()),
                audio.buffer_frames(),
                audio.stepping(),
            ),
            ChannelKind::Write(write) => write.st.wakeup_time(
                sync_frames,
                (write.st.last_progress + audio.buffer_frames() - write.write_position)
                    .clamp(0, audio.buffer_frames()),
                audio.buffer_frames(),
                audio.stepping(),
            ),
        }
        .max(now)
    }

    pub(super) fn finished(&self, now: i64) -> bool {
        if !self.buffer_a.valid() {
            return true;
        }
        match &self.kind {
            ChannelKind::Read(read) => {
                (self.buffer_a.end_frames + read.st.balance) <= now && self.buffer_a.buffer.done()
            }
            ChannelKind::Write(write) => {
                (self.buffer_a.end_frames + write.st.balance) <= now && self.buffer_a.buffer.done()
            }
        }
    }

    pub(super) fn total_finished(&self, now: i64) -> bool {
        if !self.buffer_a.valid() {
            return true;
        }
        let end = if self.buffer_b.valid() {
            match &self.kind {
                ChannelKind::Read(read) => self.buffer_b.end_frames + read.st.balance,
                ChannelKind::Write(write) => self.buffer_b.end_frames + write.st.balance,
            }
        } else {
            match &self.kind {
                ChannelKind::Read(read) => self.buffer_a.end_frames + read.st.balance,
                ChannelKind::Write(write) => self.buffer_a.end_frames + write.st.balance,
            }
        };
        end <= now && self.buffer_a.buffer.done() && self.buffer_b.buffer.done()
    }

    pub(super) fn total_end(&self) -> i64 {
        if !self.buffer_a.valid() {
            return 0;
        }
        if self.buffer_b.valid() {
            match &self.kind {
                ChannelKind::Read(read) => self.buffer_b.end_frames + read.st.balance,
                ChannelKind::Write(write) => self.buffer_b.end_frames + write.st.balance,
            }
        } else {
            match &self.kind {
                ChannelKind::Read(read) => self.buffer_a.end_frames + read.st.balance,
                ChannelKind::Write(write) => self.buffer_a.end_frames + write.st.balance,
            }
        }
    }

    pub(super) fn balance(&self) -> i64 {
        match &self.kind {
            ChannelKind::Read(read) => read.st.balance,
            ChannelKind::Write(write) => write.st.balance,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hw::oss::{
        ioctl::BufferInfo,
        position::MmapProgress,
        sync::{DuplexSync, FrameClock},
    };
    use nix::libc;
    use std::{
        fs::File,
        sync::{Arc, Mutex, atomic::AtomicBool},
    };

    // Anonymous memory and /dev/null keep these tests independent of the
    // user's live audio device. Injected channel progress avoids ioctls.
    fn mapped_audio() -> Audio {
        let capacity = 8;
        // SAFETY: anonymous private mapping, owned and unmapped by Audio::drop.
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                capacity,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        assert_ne!(map, libc::MAP_FAILED);
        Audio {
            dsp: File::open("/dev/null").unwrap(),
            channels: vec![],
            input: false,
            output_gain_linear: 1.0,
            output_balance: 0.0,
            rate: 48_000,
            format: super::super::AFMT_U8,
            chsamples: 4,
            buffer: vec![0; 4],
            f32_buffer: vec![],
            buffer_info: BufferInfo {
                fragments: 2,
                fragstotal: 2,
                fragsize: 4,
                bytes: capacity as i32,
            },
            frame_size_bytes: 1,
            fragment_bytes: 4,
            buffer_frames_cached: capacity as i64,
            caps: 0,
            mapped: true,
            map,
            zero_block: vec![],
            map_progress: MmapProgress::default(),
            last_published_balance: 0,
            frame_clock: FrameClock::default(),
            frame_stamp: 1008,
            duplex_sync: Arc::new(Mutex::new(DuplexSync::new(48_000, 4))),
            channel: DoubleBufferedChannel::new_empty_write(),
            consumed_capture_end: None,
            xrun_count: 0,
            playing: Arc::new(AtomicBool::new(true)),
            was_playing_last_cycle: true,
            stop_fade_remaining_frames: 0,
            stop_fade_total_frames: 0,
            plan_slot: None,
            inline_render: None,
            stale_silence_once: false,
        }
    }

    fn writer(head: i64, physical: i64) -> WriteChannel {
        WriteChannel {
            st: ChannelState {
                last_processing: 1008,
                last_progress: head,
                sync_level: 0,
                ..ChannelState::default()
            },
            map_progress: physical,
            write_position: head,
        }
    }

    fn block(end_frames: i64) -> BufferRecord {
        BufferRecord {
            buffer: Buffer {
                data: vec![1, 2, 3, 4],
                pos: 0,
            },
            end_frames,
        }
    }

    fn ring(audio: &Audio) -> Vec<u8> {
        let mut result = vec![0; 8];
        assert_eq!(audio.read_map(&mut result, 0, 8), 8);
        result
    }

    #[test]
    fn mapped_period_wraps_without_overwriting_other_frames() {
        let mut audio = mapped_audio();
        let mut write = writer(8, 6);
        let mut rec = block(12);
        DoubleBufferedChannel::process_write(&mut audio, &mut write, &mut rec, 1008).unwrap();
        assert!(rec.buffer.done());
        assert_eq!(ring(&audio), [3, 4, 0, 0, 0, 0, 1, 2]);
        assert_eq!(write.write_position, 12);
    }

    #[test]
    fn late_write_keeps_the_unplayed_suffix() {
        let mut audio = mapped_audio();
        let mut write = writer(10, 8);
        let mut rec = block(12);
        DoubleBufferedChannel::process_write(&mut audio, &mut write, &mut rec, 1008).unwrap();
        assert!(rec.buffer.done());
        assert_eq!(ring(&audio), [3, 4, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn full_ring_retains_pending_output_until_space_opens() {
        let mut audio = mapped_audio();
        let mut write = writer(8, 6);
        // Earlier output already occupies frames 8..14.
        write.write_position = 14;
        let mut rec = block(18);
        DoubleBufferedChannel::process_write(&mut audio, &mut write, &mut rec, 1008).unwrap();
        assert_eq!(rec.buffer.pos, 2);
        assert_eq!(ring(&audio), [0, 0, 0, 0, 1, 2, 0, 0]);
        write.st.last_progress = 10;
        write.map_progress = 8;
        DoubleBufferedChannel::process_write(&mut audio, &mut write, &mut rec, 1008).unwrap();
        assert!(rec.buffer.done());
        assert_eq!(ring(&audio), [0, 0, 0, 0, 1, 2, 3, 4]);
    }

    #[test]
    fn capture_prefetch_does_not_advance_primary_period() {
        let mut audio = mapped_audio();
        audio.input = true;
        audio.write_map(Some(&mut [8, 9, 10, 11, 4, 5, 6, 7]), 0, 8);
        let mut channel = DoubleBufferedChannel::new_read(4, 4);
        channel.reset_buffers(8, 1);
        if let ChannelKind::Read(read) = &mut channel.kind {
            read.st.last_processing = 1008;
            read.st.last_progress = 12;
            read.st.sync_level = 0;
            read.map_progress = 12;
            read.read_position = 4;
        }
        assert_eq!(channel.wakeup_time(&audio, 1008), 1008);
        channel.process(&mut audio, 1008).unwrap();
        assert_eq!(channel.end_frames(), 8);
        assert_eq!(channel.take_buffer().as_slice(), [4, 5, 6, 7]);
        assert_eq!(channel.end_frames(), 12);
        assert_eq!(channel.take_buffer().as_slice(), [8, 9, 10, 11]);
    }

    #[test]
    fn recovery_moves_buffer_grid_without_faking_device_progress() {
        let mut channel = DoubleBufferedChannel::new_write(4, 4);
        channel.kind = ChannelKind::Write(writer(1024, 1024));
        channel.reset_buffers(1032, 1);
        assert_eq!(channel.end_frames(), 1032);
        assert_eq!(channel.total_end(), 1036);
        if let ChannelKind::Write(write) = &channel.kind {
            assert_eq!(write.st.last_progress, 1024);
            assert_eq!(write.map_progress, 1024);
        }
        channel.silence_buffers();
        assert_eq!(channel.end_frames(), 1032);
        assert_eq!(channel.total_end(), 1036);
    }
}
