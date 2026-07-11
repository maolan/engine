use super::{Audio, MidiHub, OSSChannel};
use crate::audio::io::AudioIO;
use crate::hw::common;
use crate::hw::latency;
use crate::hw::options::HwOptions;
use crate::hw::traits::HwWorkerDriver;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

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
    assist_lock: Arc<Mutex<()>>,
}

impl Default for HwOptions {
    fn default() -> Self {
        Self {
            exclusive: false,
            period_frames: 1024,
            nperiods: 1,
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
            assist_lock: Arc::new(Mutex::new(())),
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

    pub fn output_meter_linear(&self, gain: f32, balance: f32) -> Vec<f32> {
        if let Some(slot) = &self.playback.plan_slot {
            let plan = slot.load();
            common::output_meter_linear_from_plan(&plan, gain, balance)
        } else {
            common::output_meter_linear(&self.playback.channels, gain, balance)
        }
    }

    pub fn start_input_trigger(&self) -> std::io::Result<()> {
        self.capture.start_trigger()
    }

    pub fn start_output_trigger(&self) -> std::io::Result<()> {
        self.playback.start_trigger()
    }

    pub fn channel(&mut self) -> OSSChannel<'_> {
        OSSChannel {
            capture: &mut self.capture,
            playback: &mut self.playback,
            stop_requested: &self.stop_requested,
        }
    }

    fn run_cycle_with_assist(&mut self) -> std::io::Result<()> {
        let assist_lock = self.assist_lock.clone();
        let _guard = assist_lock.lock().expect("OSS assist mutex poisoned");
        self.channel().run_cycle()
    }

    fn run_assist_step(&mut self) -> std::io::Result<bool> {
        let assist_lock = self.assist_lock.clone();
        self.channel().run_assist_step_with_lock(&assist_lock)
    }

    pub fn latency_ranges(&self) -> ((usize, usize), (usize, usize)) {
        latency::latency_ranges(
            self.cycle_samples(),
            self.nperiods,
            self.sync_mode,
            self.input_latency_frames,
            self.output_latency_frames,
        )
    }

    pub fn set_playing(&mut self, playing: bool) {
        self.playing.store(playing, Ordering::Relaxed);
        if playing {
            let _ = self.playback.start_trigger();
        } else {
            let _ = self.playback.stop_trigger();
            self.playback.force_silence_now();
        }
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
}

crate::impl_hw_device_for_driver!(HwDriver);
crate::impl_hw_midi_hub_traits!(MidiHub);
