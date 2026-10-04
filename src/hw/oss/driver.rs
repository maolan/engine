use super::{Audio, MidiHub, OSSChannel, add_to_sync_group, start_sync_group};
use crate::audio::io::AudioIO;
use crate::hw::common;
use crate::hw::latency;
use crate::hw::options::HwOptions;
use crate::hw::traits::HwWorkerDriver;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Debug)]
struct AssistGate {
    active: AtomicBool,
}

impl AssistGate {
    fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
        }
    }

    fn try_enter(gate: &Arc<Self>) -> Option<AssistGateGuard> {
        gate.active
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| AssistGateGuard { gate: gate.clone() })
    }
}

#[derive(Debug)]
struct AssistGateGuard {
    gate: Arc<AssistGate>,
}

impl Drop for AssistGateGuard {
    fn drop(&mut self) {
        self.gate.active.store(false, Ordering::Release);
    }
}

#[derive(Debug)]
pub struct HwDriver {
    capture: Audio,
    playback: Audio,
    nperiods: usize,
    sync_mode: bool,
    input_latency_frames: usize,
    output_latency_frames: usize,
    playing: Arc<AtomicBool>,
    stop_requested: Arc<AtomicBool>,
    assist_gate: Arc<AssistGate>,
}

impl Default for HwOptions {
    fn default() -> Self {
        Self {
            exclusive: false,
            period_frames: 1024,
            nperiods: 1,
            input_channels: 0,
            output_channels: 0,
            ignore_hwbuf: false,
            sync_mode: false,
            input_latency_frames: 0,
            output_latency_frames: 0,
        }
    }
}

impl HwDriver {
    pub fn new(path: &str, rate: i32, bits: i32) -> std::io::Result<Self> {
        Self::new_with_options(path, None, rate, bits, HwOptions::default())
    }

    pub fn new_with_options(
        playback_path: &str,
        capture_path: Option<&str>,
        rate: i32,
        bits: i32,
        options: HwOptions,
    ) -> std::io::Result<Self> {
        let playing = Arc::new(AtomicBool::new(false));
        let stop_requested = Arc::new(AtomicBool::new(false));
        let capture_path = capture_path.unwrap_or(playback_path);
        let sync_key = if capture_path == playback_path {
            playback_path.to_string()
        } else {
            format!("{capture_path}|{playback_path}")
        };
        let capture = Audio::new(
            capture_path,
            &sync_key,
            rate,
            bits,
            true,
            options,
            playing.clone(),
        )
        .map_err(|e| {
            std::io::Error::other(format!("Failed to open OSS input '{capture_path}': {e}"))
        })?;
        let playback = Audio::new(
            playback_path,
            &sync_key,
            rate,
            bits,
            false,
            options,
            playing.clone(),
        )
        .map_err(|e| {
            std::io::Error::other(format!("Failed to open OSS output '{playback_path}': {e}"))
        })?;
        Ok(Self {
            capture,
            playback,
            nperiods: options.nperiods.max(1),
            sync_mode: options.sync_mode,
            input_latency_frames: options.input_latency_frames,
            output_latency_frames: options.output_latency_frames,
            playing,
            stop_requested,
            assist_gate: Arc::new(AssistGate::new()),
        })
    }

    pub fn input_fd(&self) -> i32 {
        self.capture.fd()
    }

    pub fn output_fd(&self) -> i32 {
        self.playback.fd()
    }

    pub fn input_channels(&self) -> usize {
        self.capture.channels.len()
    }

    pub fn output_channels(&self) -> usize {
        self.playback.channels.len()
    }

    pub fn sample_rate(&self) -> i32 {
        self.playback.rate
    }

    pub fn cycle_samples(&self) -> usize {
        self.playback.chsamples
    }

    pub fn sample_bits(&self) -> i32 {
        self.playback.sample_bits()
    }

    pub fn frame_size_bytes(&self) -> usize {
        self.playback.frame_size_bytes()
    }

    pub fn input_port(&self, idx: usize) -> Option<Arc<AudioIO>> {
        self.capture.channels.get(idx).cloned()
    }

    pub fn output_port(&self, idx: usize) -> Option<Arc<AudioIO>> {
        self.playback.channels.get(idx).cloned()
    }

    pub fn set_output_gain_balance(&mut self, gain: f32, balance: f32) {
        self.playback.output_gain_linear = gain;
        self.playback.output_balance = balance;
    }

    pub fn set_plan_slot(&mut self, slot: Arc<crate::render_plan::PlanSlot>) {
        self.capture.set_plan_slot(slot.clone());
        self.playback.set_plan_slot(slot);
    }

    pub fn set_inline_render(&mut self, ctx: Option<Arc<crate::inline_render::InlineRender>>) {
        self.capture.set_inline_render(ctx.clone());
        self.playback.set_inline_render(ctx);
    }

    pub fn output_meter_linear(&self, gain: f32, balance: f32) -> Vec<f32> {
        if let Some(slot) = &self.playback.plan_slot {
            let plan = slot.load();
            common::output_meter_linear_from_plan(&plan, gain, balance)
        } else {
            common::output_meter_linear(self.playback.channels.len(), gain, balance)
        }
    }

    /// Current capture frame position reported by the OSS driver
    /// (`SNDCTL_DSP_CURRENT_IPTR`), in frames since the input stream started.
    pub fn current_capture_frame(&self) -> Option<i64> {
        self.capture.current_capture_frame()
    }

    /// Total capture buffer capacity in frames. The CURRENT_IPTR counter is
    /// monotonic since device open, so at a take start at most this many
    /// captured frames can still be pending in the ring; it bounds the
    /// record-start discard.
    pub fn capture_buffer_frames(&self) -> usize {
        usize::try_from(self.capture.buffer_frames().max(0)).unwrap_or(0)
    }

    /// Join capture and playback in an OSS sync group and start them
    /// together (or fall back to per-direction triggers when the device
    /// cannot do sync groups). Returns true when a sync group was started.
    /// The result is recorded in the shared `DuplexSync` so that later
    /// transport transitions know whether per-direction `SNDCTL_DSP_TRIGGER`
    /// ioctls are safe to issue.
    pub fn start_duplex_sync(&self) -> bool {
        let in_fd = self.capture.fd();
        let out_fd = self.playback.fd();
        let mut group = 0;
        let in_group = add_to_sync_group(in_fd, group, true);
        if in_group > 0 {
            group = in_group;
        }
        let out_group = add_to_sync_group(out_fd, group, false);
        if out_group > 0 {
            group = out_group;
        }
        let started = group > 0 && start_sync_group(in_fd, group).is_ok();
        if !started {
            let _ = self.capture.start_trigger();
            let _ = self.playback.start_trigger();
        }
        started
    }

    pub fn channel(&mut self) -> OSSChannel<'_> {
        OSSChannel {
            capture: &mut self.capture,
            playback: &mut self.playback,
            stop_requested: &self.stop_requested,
        }
    }

    fn run_cycle_with_assist(&mut self) -> std::io::Result<()> {
        let assist_gate = self.assist_gate.clone();
        let Some(_guard) = AssistGate::try_enter(&assist_gate) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "OSS assist cycle already running",
            ));
        };
        self.channel().run_cycle()
    }

    fn run_assist_step(&mut self) -> std::io::Result<bool> {
        let assist_gate = self.assist_gate.clone();
        let Some(_guard) = AssistGate::try_enter(&assist_gate) else {
            return Ok(false);
        };
        self.channel().run_assist_step()
    }

    pub fn latency_ranges(&self) -> ((usize, usize), (usize, usize)) {
        if self.fresh_capture() {
            let input = self.cycle_samples() + self.input_latency_frames;
            let output = self.playback.mmap_write_ahead() as usize + self.output_latency_frames;
            // Actual write-ahead is learned from the feeder quantum at
            // startup. Report its bounds; these exclude the device FIFO
            // and converters, for which user calibration is still needed.
            let max_output = (self.playback.buffer_frames() as usize - self.cycle_samples())
                + self.output_latency_frames;
            return ((input, input), (output, max_output));
        }
        latency::latency_ranges(
            self.cycle_samples(),
            self.nperiods,
            self.sync_mode,
            self.input_latency_frames,
            self.output_latency_frames,
        )
    }

    pub fn set_playing(&mut self, playing: bool) {
        if self.playing.load(Ordering::Relaxed) != playing {
            self.capture.mmap_cycle = super::mmap_cycle::CycleCursor::default();
        }
        // State flag only: playback DMA never halts. While stopped, the
        // per-cycle render is silence (fill_output_buffer zeroes it when
        // !playing) and the ring is kept/-fed silent via
        // `zero_fill_hw_buffers` — no trigger ioctls on stop/resume.
        self.playing.store(playing, Ordering::Relaxed);
        if !playing {
            self.playback.force_silence_now();
        }
    }

    /// Direct mmap cycles start with fresh capture; kernel uptime is not
    /// invalid startup audio and must not seed the recording discard.
    pub fn fresh_capture(&self) -> bool {
        super::mmap_cycle::direct_enabled(&self.capture, &self.playback)
    }

    /// One full write of the persistent zero buffer into the mapped
    /// playback ring, plus userspace buffer silence. Called when the
    /// transport stops: the zeros drain as silence and, with no cycles
    /// running while stopped, nothing overwrites them.
    pub fn zero_fill_hw_buffers(&mut self) {
        self.playback.zero_fill_hw_buffers();
    }

    pub fn close_fds(&mut self) {
        self.capture.close_fd();
        self.playback.close_fd();
    }
}

impl HwWorkerDriver for HwDriver {
    fn cycle_samples(&self) -> usize {
        self.cycle_samples()
    }

    fn sample_rate(&self) -> i32 {
        self.sample_rate()
    }

    fn close_fds(&mut self) {
        self.close_fds()
    }

    fn set_playing(&mut self, playing: bool) {
        self.set_playing(playing)
    }

    fn set_output_gain_balance(&mut self, gain: f32, balance: f32) {
        self.set_output_gain_balance(gain, balance)
    }

    fn zero_fill_hw_buffers(&mut self) {
        self.zero_fill_hw_buffers()
    }

    fn run_cycle_for_worker(&mut self) -> Result<(), String> {
        self.run_cycle_with_assist().or_else(|e| {
            if e.kind() == std::io::ErrorKind::Interrupted {
                Ok(())
            } else {
                Err(e.to_string())
            }
        })
    }

    fn run_assist_step_for_worker(&mut self) -> Result<bool, String> {
        self.run_assist_step().or_else(|e| {
            if e.kind() == std::io::ErrorKind::Interrupted {
                Ok(false)
            } else {
                Err(e.to_string())
            }
        })
    }

    fn request_stop(&mut self) {
        self.stop_requested.store(true, Ordering::Release);
        let _ = self.playback.stop_trigger();
        let _ = self.playback.halt();
        let _ = self.capture.halt();
    }

    #[cfg(unix)]
    fn capture_fd(&self) -> Option<std::os::fd::RawFd> {
        Some(self.capture.fd())
    }

    #[cfg(unix)]
    fn playback_fd(&self) -> Option<std::os::fd::RawFd> {
        Some(self.playback.fd())
    }

    fn current_capture_frame(&self) -> Option<i64> {
        self.current_capture_frame()
    }

    fn stop_signaller(&self) -> Option<Arc<AtomicBool>> {
        Some(self.stop_requested.clone())
    }
}

crate::impl_hw_device_for_driver!(HwDriver);
crate::impl_hw_midi_hub_traits!(MidiHub);
