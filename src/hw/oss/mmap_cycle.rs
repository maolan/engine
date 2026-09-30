use super::io_util::{map_read, map_write};
use super::{OSSChannel, convert_in_to_i32_connected, convert_out_from_i32_interleaved};
use std::sync::atomic::Ordering;

#[derive(Debug, Default)]
pub(super) struct CycleCursor {
    pub(super) capture_end: Option<i64>,
    playback_end: Option<i64>,
    cleared_until: i64,
    /// Cadence mismatch allowance plus a polling margin. OSS fragments
    /// describe the software ring, not the hardware's transfer quantum.
    write_ahead: i64,
    quantum_seen: bool,
}

/// Keep the original period grid while dropping whole missed periods. The
/// first cycle waits for new data rather than playing an arbitrary backlog.
fn capture_end(previous: Option<i64>, head: i64, period: i64, capacity: i64) -> i64 {
    let next = previous.map_or(head + period, |end| end + period);
    if head - (next - period) >= capacity {
        next + ((head - next).max(0) / period) * period
    } else {
        next
    }
}

fn output_fits(start: i64, head: i64, period: i64, capacity: i64) -> bool {
    start > head && start + period <= head + capacity
}

fn playback_lead(period: i64, quantum: i64, margin: i64) -> i64 {
    if quantum >= period {
        return quantum + margin;
    }
    // A period boundary can fall between feeder transfers. On the shared
    // period/transfer grid the largest overshoot is Q - gcd(P, Q), not Q.
    // Keep a scheduling margin as well; deadline checks still handle jitter
    // or a subsequent change in the observed cadence.
    let (mut a, mut b) = (period, quantum);
    while b != 0 {
        (a, b) = (b, a % b);
    }
    quantum - a + margin
}

pub(super) fn direct_enabled(capture: &super::Audio, playback: &super::Audio) -> bool {
    capture.is_mapped()
        && playback.is_mapped()
        && capture.direct_mmap_allowed
        && playback.direct_mmap_allowed
        && capture.inline_render.is_some()
        && capture.chsamples == playback.chsamples
        && capture.rate == playback.rate
        && capture.buffer_frames() >= 2 * capture.chsamples as i64
        && playback.buffer_frames() >= 2 * playback.chsamples as i64
}

impl OSSChannel<'_> {
    pub(super) fn direct_mmap_enabled(&self) -> bool {
        direct_enabled(self.capture, self.playback)
    }

    fn mmap_wait(&self) -> std::io::Result<()> {
        if self.stop_requested.load(Ordering::Acquire) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "OSS cycle stopped",
            ));
        }
        let now = self
            .capture
            .frame_clock
            .now()
            .ok_or_else(|| std::io::Error::other("OSS frame clock failed"))?;
        if !self
            .capture
            .frame_clock
            .sleep_until_frame(now + self.capture.stepping())
        {
            return Err(std::io::Error::other("OSS cycle sleep failed"));
        }
        Ok(())
    }

    pub(super) fn run_direct_mmap_cycle(&mut self) -> std::io::Result<()> {
        let period = self.capture.chsamples as i64;
        let mut end = capture_end(
            self.capture.mmap_cycle.capture_end,
            self.capture.stream_frame()?,
            period,
            self.capture.buffer_frames(),
        );
        let mut observed_output = self.playback.stream_frame()?;
        self.capture.mmap_cycle.write_ahead = self
            .capture
            .mmap_cycle
            .write_ahead
            .max(self.playback.mmap_write_ahead());
        let out;
        loop {
            if self.stop_requested.load(Ordering::Acquire) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "OSS cycle stopped",
                ));
            }
            let head = self.capture.stream_frame()?;
            if self
                .capture
                .mmap_cycle
                .capture_end
                .is_some_and(|previous| head < previous)
            {
                return Err(std::io::Error::other("OSS capture counter moved backwards"));
            }
            // Preserve consecutive periods when an interrupt delivers more
            // than one engine period. Drop backlog only when overwritten.
            end = capture_end(
                Some(end - period),
                head,
                period,
                self.capture.buffer_frames(),
            );
            let output_head = self.playback.stream_frame()?;
            if output_head < observed_output {
                return Err(std::io::Error::other(
                    "OSS playback counter moved backwards",
                ));
            }
            if output_head > observed_output {
                self.capture.mmap_cycle.quantum_seen = true;
                self.capture.mmap_cycle.write_ahead = self
                    .capture
                    .mmap_cycle
                    .write_ahead
                    .max(playback_lead(
                        period,
                        output_head - observed_output,
                        self.playback.mmap_write_ahead(),
                    ))
                    .min(self.playback.buffer_frames() - period);
                observed_output = output_head;
            }
            let start = self
                .capture
                .mmap_cycle
                .playback_end
                .unwrap_or(output_head + self.capture.mmap_cycle.write_ahead);
            if head >= end
                && self.capture.mmap_cycle.quantum_seen
                && (start <= output_head
                    || output_fits(start, output_head, period, self.playback.buffer_frames()))
            {
                if self.capture.mmap_cycle.capture_end.is_none() {
                    // Once the feeder quantum is known, begin with the
                    // newest complete period, not startup observation backlog.
                    end = head;
                }
                out = output_head;
                break;
            }
            self.mmap_wait()?;
        }

        let start = self
            .capture
            .mmap_cycle
            .playback_end
            .unwrap_or(out + self.capture.mmap_cycle.write_ahead);
        let clear = self.capture.mmap_cycle.cleared_until;
        if out < clear {
            return Err(std::io::Error::other(
                "OSS playback counter moved backwards",
            ));
        }
        // Retire played samples so a later scheduler stall repeats silence.
        self.playback.write_map(
            None,
            clear.rem_euclid(self.playback.buffer_frames()) as usize * self.playback.frame_size(),
            (out - clear).min(self.playback.buffer_frames()) as usize * self.playback.frame_size(),
        );
        self.capture.mmap_cycle.cleared_until = out;

        let input_start = end - period;
        let input_bytes = self.capture.mmap_scratch.len();
        map_read(
            self.capture.map,
            true,
            self.capture.buffer_info.bytes as usize,
            &mut self.capture.mmap_scratch,
            input_start.rem_euclid(self.capture.buffer_frames_cached) as usize
                * self.capture.frame_size_bytes,
            input_bytes,
        );
        let capture_head = self.capture.stream_frame()?;
        crate::cycle_trace::set_frame_metric(
            crate::cycle_trace::FrameMetric::CaptureOvershootFrames,
            capture_head - end,
        );
        let overwritten = capture_head - input_start >= self.capture.buffer_frames();
        if overwritten {
            self.capture.mmap_scratch.fill(0);
        }
        convert_in_to_i32_connected(
            self.capture.format,
            period as usize,
            &self.capture.mmap_scratch,
            &mut self.capture.buffer,
            &self.capture.channels,
        );
        self.capture.process_ports();
        let ctx = self
            .capture
            .inline_render
            .as_ref()
            .expect("direct inline context")
            .clone();
        ctx.render_cycle(period as u32, Some(end));
        let render_missed = ctx.take_stale_silence();
        if render_missed {
            self.playback.write_silence_once();
        }
        self.playback.process_ports();
        convert_out_from_i32_interleaved(
            self.playback.format,
            self.playback.channels.len(),
            period as usize,
            &mut self.playback.buffer,
            &mut self.playback.mmap_scratch,
        );

        let mut head = self.playback.stream_frame()?;
        crate::cycle_trace::set_frame_metric(
            crate::cycle_trace::FrameMetric::PlaybackLeadFrames,
            start - head,
        );
        // A render timeout may leave workers retiring after this driver
        // cycle returns. Clear the whole playback ring and reset scheduling
        // even if this particular write window is still in the future, so
        // the device cannot repeat old ring contents during that wait.
        let mut missed = render_missed
            || overwritten
            || !output_fits(start, head, period, self.playback.buffer_frames());
        if !missed {
            let len = self.playback.mmap_scratch.len();
            map_write(
                self.playback.map,
                true,
                self.playback.buffer_info.bytes as usize,
                Some(&mut self.playback.mmap_scratch),
                start.rem_euclid(self.playback.buffer_frames_cached) as usize
                    * self.playback.frame_size_bytes,
                len,
            );
            // A preemption during the copy must not count as a valid cycle.
            head = self.playback.stream_frame()?;
            missed = head >= start;
        }
        if !missed {
            crate::cycle_trace::mark(crate::cycle_trace::TracePoint::PlaybackMapDone);
        }
        if missed {
            self.playback
                .write_map(None, 0, self.playback.buffer_info.bytes as usize);
            self.capture.mmap_cycle.playback_end = None;
            self.playback.xrun_count += 1;
            let gap = ((head - start).max(0) / period + 1) * period;
            ctx.discard_completed_cycle(gap as u64);
        } else {
            self.capture.mmap_cycle.playback_end = Some(start + period);
        }
        self.capture.mmap_cycle.capture_end = Some(end);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playback_lead_covers_transfer_rounding_without_a_full_extra_transfer() {
        assert_eq!(playback_lead(512, 192, 32), 160);
        assert_eq!(playback_lead(512, 256, 32), 32);
        assert_eq!(playback_lead(512, 193, 32), 224);
        assert_eq!(playback_lead(512, 512, 32), 544);
        assert_eq!(playback_lead(512, 768, 32), 800);
        for quantum in [64, 128, 192, 193, 256, 384] {
            let lead = playback_lead(512, quantum, 32);
            for cycle in 0..quantum {
                let boundary = cycle * 512;
                let rounded = (boundary + quantum - 1) / quantum * quantum;
                assert!(rounded - boundary + 32 <= lead);
            }
        }
    }

    #[test]
    fn capture_waits_for_fresh_data_then_preserves_continuity() {
        assert_eq!(capture_end(None, 96, 128, 512), 224);
        assert_eq!(capture_end(Some(224), 240, 128, 512), 352);
        assert_eq!(capture_end(Some(224), 600, 128, 512), 352);
        assert_eq!(capture_end(Some(224), 900, 128, 512), 864);
    }

    #[test]
    fn playback_window_has_deadline_headroom_without_overwriting_queued_audio() {
        assert!(output_fits(640, 512, 128, 256));
        assert!(!output_fits(640, 511, 128, 256));
        assert!(output_fits(640, 639, 128, 256));
        assert!(!output_fits(640, 640, 128, 256));
        assert!(!output_fits(640, 896, 128, 256));
    }
}
