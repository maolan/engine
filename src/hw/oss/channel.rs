//! Period scheduling follows JACK's OSS double-buffer model: consume the
//! primary capture buffer, render, and requeue output at the next period.
//! While node workers render, the coordinator services raw device buffers,
//! performing the I/O work JACK delegates to its channel assist thread.

use super::{
    Audio, DoubleBufferedChannel, convert_in_to_i32_connected, convert_out_from_i32_interleaved,
};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

pub struct OSSChannel<'a> {
    pub(super) capture: &'a mut Audio,
    pub(super) playback: &'a mut Audio,
    pub(super) stop_requested: &'a AtomicBool,
}

impl<'a> OSSChannel<'a> {
    pub fn run_cycle(&mut self) -> std::io::Result<()> {
        if self.capture.is_mapped() || self.playback.is_mapped() {
            DuplexChannelApi::new(self.capture, self.playback, self.stop_requested)?.run_cycle()
        } else {
            self.run_poll_cycle()
        }
    }

    pub fn run_cycle_with_assist(&mut self, assist_lock: &Mutex<()>) -> std::io::Result<()> {
        let _guard = assist_lock.lock().expect("OSS assist mutex poisoned");
        self.run_cycle()
    }

    pub fn run_assist_step(&mut self) -> std::io::Result<bool> {
        if !self.capture.is_mapped() && !self.playback.is_mapped() {
            return self.run_cycle().map(|_| true);
        }
        let mut api = DuplexChannelApi::new(self.capture, self.playback, self.stop_requested)?;
        if api.stop_requested() {
            return Ok(false);
        }
        api.check_time_and_run()?;
        if api.all_finished() {
            return Ok(false);
        }
        api.sleep()?;
        api.check_time_and_run()?;
        Ok(true)
    }

    pub fn run_assist_step_with_lock(&mut self, assist_lock: &Mutex<()>) -> std::io::Result<bool> {
        if !self.capture.is_mapped() && !self.playback.is_mapped() {
            return self.run_cycle().map(|_| true);
        }
        let mut api = DuplexChannelApi::new(self.capture, self.playback, self.stop_requested)?;
        if api.stop_requested() {
            return Ok(false);
        }
        {
            let _guard = assist_lock.lock().expect("OSS assist mutex poisoned");
            api.check_time_and_run()?;
            if api.all_finished() {
                return Ok(false);
            }
        }
        api.sleep()?;
        {
            let _guard = assist_lock.lock().expect("OSS assist mutex poisoned");
            api.check_time_and_run()?;
            Ok(!api.all_finished())
        }
    }

    fn check_config(&self) -> std::io::Result<()> {
        if !self.capture.input || self.playback.input {
            return Err(std::io::Error::other(
                "OSSChannel expects (capture=input, playback=output)",
            ));
        }
        Ok(())
    }

    /// Poll/read/write fallback used when neither device exposes
    /// `PCM_CAP_MMAP`.
    fn run_poll_cycle(&mut self) -> std::io::Result<()> {
        self.check_config()?;
        if self.stop_requested.load(Ordering::Acquire) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "OSS duplex cycle stopped",
            ));
        }

        self.playback.process(self.stop_requested)?;
        self.capture.process(self.stop_requested)?;

        if let Some(now) = self.capture.frame_clock.now() {
            self.capture.frame_stamp = now;
            self.playback.frame_stamp = now;
        }

        Ok(())
    }
}

struct DuplexChannelApi<'a> {
    capture: &'a mut Audio,
    playback: &'a mut Audio,
    stop_requested: &'a AtomicBool,
    now: i64,
}

impl<'a> DuplexChannelApi<'a> {
    fn new(
        capture: &'a mut Audio,
        playback: &'a mut Audio,
        stop_requested: &'a AtomicBool,
    ) -> std::io::Result<Self> {
        if !capture.input || playback.input {
            return Err(std::io::Error::other(
                "run_duplex_cycle expects (capture=input, playback=output)",
            ));
        }
        Ok(Self {
            capture,
            playback,
            stop_requested,
            now: 0,
        })
    }

    fn run_cycle(&mut self) -> std::io::Result<()> {
        self.check_stop()?;
        let frames = self.capture.chsamples as i64;
        let mut cycle_end = self.capture.shared_cycle_end_add(frames);
        self.check_time_and_run()?;

        let xrun = self.xrun_gap();
        if xrun > 0 {
            let skip = xrun + frames;
            cycle_end = self.capture.shared_cycle_end_add(skip);
            // Hardware progress was already sampled above. Move queued buffer
            // timestamps, not the device counters: advancing both counts a gap twice.
            self.capture.channel.reset_buffers(
                self.capture.channel.end_frames() + skip,
                self.capture.frame_size(),
            );
            self.playback.channel.reset_buffers(
                self.playback.channel.end_frames() + skip,
                self.playback.frame_size(),
            );
        }

        while !self.capture.channel.finished(self.now) {
            self.check_stop()?;
            self.sleep()?;
            self.check_time_and_run()?;
        }

        // The secondary buffer may already be prefetched. Anchor transport to
        // the primary period consumed here, never to that prefetch cursor.
        crate::cycle_trace::set_frame_metric(
            crate::cycle_trace::FrameMetric::CaptureOvershootFrames,
            self.capture.channel.capture_overshoot(),
        );
        self.capture.consumed_capture_end = Some(self.capture.channel.end_frames());
        let mut inbuf = self.capture.channel.take_buffer();
        if self
            .capture
            .channels
            .iter()
            .any(crate::hw::ports::has_audio_connections)
        {
            convert_in_to_i32_connected(
                self.capture.format,
                self.capture.chsamples,
                inbuf.as_slice(),
                self.capture.buffer.as_mut(),
                &self.capture.channels,
            );
        }
        inbuf.reset();
        let in_end = cycle_end + frames;
        if !self.capture.channel.set_buffer(inbuf, in_end) {
            return Err(std::io::Error::other("failed to requeue capture buffer"));
        }
        self.capture.process_ports();
        if let Some(ctx) = self.capture.inline_render.clone() {
            let mut service_error = None;
            ctx.render_cycle_with_service(
                self.capture.chsamples as u32,
                self.capture.current_read_frame(),
                || {
                    // Only raw driver buffers are touched here. Worker threads own
                    // the render arena until render/finish_pending completes.
                    if service_error.is_none()
                        && let Err(error) =
                            self.check_stop().and_then(|()| self.check_time_and_run())
                    {
                        service_error = Some(error);
                    }
                    let frames = (self.next_wakeup() - self.now)
                        .max(self.capture.stepping())
                        .min(self.capture.chsamples as i64);
                    std::time::Duration::from_secs_f64(frames as f64 / self.capture.rate as f64)
                },
            );
            if let Some(error) = service_error {
                return Err(error);
            }
        }
        if let Some(ctx) = self.capture.inline_render.as_ref()
            && ctx.take_stale_silence()
        {
            self.playback.write_silence_once();
        }

        self.check_time_and_run()?;

        while !self.playback.channel.finished(self.now) {
            self.check_stop()?;
            self.sleep()?;
            self.check_time_and_run()?;
        }

        self.playback.process_ports();
        let mut outbuf = self.playback.channel.take_buffer();
        convert_out_from_i32_interleaved(
            self.playback.format,
            self.playback.channels.len(),
            self.playback.chsamples,
            self.playback.buffer.as_mut(),
            outbuf.as_mut_slice(),
        );
        let mut out_end = self.capture.shared_cycle_end_get() + frames;
        out_end += self.playback.playback_correction();
        if !self.playback.channel.set_buffer(outbuf, out_end) {
            return Err(std::io::Error::other("failed to requeue playback buffer"));
        }

        self.check_time_and_run()?;
        Ok(())
    }

    fn process_one_now(audio: &mut Audio, now: i64) -> std::io::Result<()> {
        audio.frame_stamp = now;
        let wake = audio.channel.wakeup_time(audio, now);
        let mut processed = false;
        if now >= wake && !audio.channel.total_finished(now) {
            let mut chan = std::mem::replace(
                &mut audio.channel,
                if audio.input {
                    DoubleBufferedChannel::new_empty_read()
                } else {
                    DoubleBufferedChannel::new_empty_write()
                },
            );
            let res = chan.process(audio, now);
            audio.channel = chan;
            res?;
            processed = true;
        }
        if processed {
            audio.publish_balance(audio.channel.balance());
        }
        Ok(())
    }

    fn check_time_and_run(&mut self) -> std::io::Result<()> {
        self.update_now()?;
        Self::process_one_now(self.capture, self.now)?;
        Self::process_one_now(self.playback, self.now)?;
        Ok(())
    }

    fn update_now(&mut self) -> std::io::Result<()> {
        self.now = self
            .capture
            .frame_clock
            .now()
            .ok_or_else(|| std::io::Error::other("failed to read frame clock"))?;
        Ok(())
    }

    fn xrun_gap(&mut self) -> i64 {
        let max_end = self
            .capture
            .channel
            .total_end()
            .max(self.playback.channel.total_end());
        let buffer_gap = if max_end < self.now {
            self.now - max_end
        } else {
            0
        };

        if buffer_gap > 0 {
            self.capture.xrun_count += 1;
            self.playback.xrun_count += 1;
        }
        buffer_gap
    }

    fn all_finished(&self) -> bool {
        self.capture.channel.total_finished(self.now)
            && self.playback.channel.total_finished(self.now)
    }

    fn sleep(&self) -> std::io::Result<()> {
        self.sleep_until(self.next_wakeup())
    }

    fn next_wakeup(&self) -> i64 {
        self.capture
            .channel
            .wakeup_time(self.capture, self.capture.frame_stamp)
            .min(
                self.playback
                    .channel
                    .wakeup_time(self.playback, self.playback.frame_stamp),
            )
    }

    fn sleep_until(&self, wake: i64) -> std::io::Result<()> {
        self.check_stop()?;
        let now = self.capture.frame_stamp.max(self.playback.frame_stamp);
        // Never sleep longer than one buffer: after a transport stop the
        // progress bookkeeping lags the frame clock, and an uncapped wakeup
        // can sit a whole stop-gap in the future, stalling the resume.
        let wake = wake.min(now + self.capture.chsamples as i64);
        if wake > now {
            if !self.capture.frame_clock.sleep_until_frame(wake) {
                return Err(std::io::Error::other("duplex sleep failed"));
            }
        } else if !self.capture.playing.load(Ordering::Relaxed) {
            let frames = self.capture.chsamples as i64;
            if !self.capture.frame_clock.sleep_until_frame(now + frames) {
                tracing::warn!("OSS capture frame clock sleep failed");
            }
        }
        Ok(())
    }

    fn stop_requested(&self) -> bool {
        self.stop_requested.load(Ordering::Acquire)
    }

    fn check_stop(&self) -> std::io::Result<()> {
        if self.stop_requested() {
            Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "OSS duplex cycle stopped",
            ))
        } else {
            Ok(())
        }
    }
}

unsafe impl Send for Audio {}
unsafe impl Sync for Audio {}
