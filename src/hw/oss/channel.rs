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
        DuplexChannelApi::new(self.capture, self.playback, self.stop_requested)?.run_cycle()
    }

    pub fn run_cycle_with_assist(&mut self, assist_lock: &Mutex<()>) -> std::io::Result<()> {
        DuplexChannelApi::new(self.capture, self.playback, self.stop_requested)?
            .run_cycle_with_assist(assist_lock)
    }

    pub fn run_assist_step(&mut self) -> std::io::Result<bool> {
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
        self.capture.process();

        self.check_time_and_run()?;

        while !self.playback.channel.finished(self.now) {
            self.check_stop()?;
            self.sleep()?;
            self.check_time_and_run()?;
        }

        self.playback.process();
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

    fn run_cycle_with_assist(&mut self, assist_lock: &Mutex<()>) -> std::io::Result<()> {
        let frames = self.capture.chsamples as i64;
        let mut cycle_end;
        {
            let _guard = assist_lock.lock().expect("OSS assist mutex poisoned");
            cycle_end = self.capture.shared_cycle_end_add(frames);
            self.update_now()?;

            let xrun = self.xrun_gap();
            if xrun > 0 {
                let skip = xrun + frames;
                cycle_end = self.capture.shared_cycle_end_add(skip);
                self.capture.channel.reset_buffers(
                    self.capture.channel.end_frames() + skip,
                    self.capture.frame_size(),
                );
                self.playback.channel.reset_buffers(
                    self.playback.channel.end_frames() + skip,
                    self.playback.frame_size(),
                );
            }
        }

        self.wait_for_capture_primary(assist_lock)?;

        {
            let _guard = assist_lock.lock().expect("OSS assist mutex poisoned");
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
            self.capture.process();
            self.update_now()?;
        }

        self.wait_for_playback_primary(assist_lock)?;

        {
            let _guard = assist_lock.lock().expect("OSS assist mutex poisoned");
            self.playback.process();
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
        }

        Ok(())
    }

    fn wait_for_capture_primary(&mut self, assist_lock: &Mutex<()>) -> std::io::Result<()> {
        loop {
            self.check_stop()?;
            let wake = {
                let _guard = assist_lock.lock().expect("OSS assist mutex poisoned");
                self.update_now()?;
                if self.capture.channel.primary_finished(self.now) {
                    return Ok(());
                }
                self.next_wakeup()
            };
            self.sleep_until(wake)?;
        }
    }

    fn wait_for_playback_primary(&mut self, assist_lock: &Mutex<()>) -> std::io::Result<()> {
        loop {
            self.check_stop()?;
            let wake = {
                let _guard = assist_lock.lock().expect("OSS assist mutex poisoned");
                self.update_now()?;
                if self.playback.channel.primary_finished(self.now) {
                    return Ok(());
                }
                self.next_wakeup()
            };
            self.sleep_until(wake)?;
        }
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
        let capture_enhanced_gap = self.capture.detect_xrun_enhanced();
        let playback_enhanced_gap = self.playback.detect_xrun_enhanced();
        let enhanced_gap = capture_enhanced_gap.max(playback_enhanced_gap);

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

        let gap = enhanced_gap.max(buffer_gap);

        if gap > 0 && enhanced_gap == 0 && buffer_gap > 0 {
            tracing::debug!(
                "OSS duplex buffer-position xrun detected (gap {} frames)",
                buffer_gap
            );
        }

        gap
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
        if wake > now && !self.capture.frame_clock.sleep_until_frame(wake) {
            return Err(std::io::Error::other("duplex sleep failed"));
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
