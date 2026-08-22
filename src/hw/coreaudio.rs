//! macOS hardware backend.
//!
//! Maolan has no native CoreAudio driver yet. This module exists so the engine
//! builds and runs on macOS, where audio I/O goes through JACK (see
//! [`crate::hw::jack`]) rather than a native device. Opening a native device
//! reports that clearly instead of failing to compile.
//!
//! MIDI and the option struct are platform-neutral and are re-exported from the
//! shared modules, exactly as the other backends do.

use crate::audio::io::AudioIO;
use crate::hw::traits::{HwDevice, HwWorkerDriver};
use std::sync::Arc;

pub use super::midi_hub::MidiHub;
pub use super::options::HwOptions;

impl Default for HwOptions {
    fn default() -> Self {
        Self {
            exclusive: false,
            period_frames: 1024,
            nperiods: 2,
            ignore_hwbuf: false,
            sync_mode: false,
            input_latency_frames: 0,
            output_latency_frames: 0,
        }
    }
}

pub const UNSUPPORTED: &str =
    "Maolan has no native CoreAudio backend yet; run a JACK server and select a JACK device";

/// Placeholder native device. [`HwDriver::new_with_options`] always fails, so a
/// value of this type is never constructed; it exists to satisfy the backend
/// surface the engine and [`crate::workers::hw_worker::HwWorker`] expect.
#[derive(Debug)]
pub struct HwDriver {
    _private: (),
}

impl HwDriver {
    pub fn new_with_options(
        _device: &str,
        _input_device: Option<&str>,
        _sample_rate_hz: i32,
        _bits: i32,
        _options: HwOptions,
    ) -> Result<Self, String> {
        Err(UNSUPPORTED.to_string())
    }
}

impl HwDriver {
    pub fn input_channels(&self) -> usize {
        0
    }

    pub fn output_channels(&self) -> usize {
        0
    }

    pub fn sample_rate(&self) -> i32 {
        0
    }

    pub fn cycle_samples(&self) -> usize {
        0
    }

    pub fn sample_bits(&self) -> i32 {
        0
    }

    pub fn frame_size_bytes(&self) -> usize {
        0
    }

    pub fn latency_ranges(&self) -> ((usize, usize), (usize, usize)) {
        ((0, 0), (0, 0))
    }

    pub fn input_port(&self, _idx: usize) -> Option<Arc<AudioIO>> {
        None
    }

    pub fn output_port(&self, _idx: usize) -> Option<Arc<AudioIO>> {
        None
    }

    pub fn close_fds(&mut self) {}

    pub fn set_playing(&mut self, _playing: bool) {}

    pub fn set_output_gain_balance(&mut self, _gain: f32, _balance: f32) {}
}

impl HwWorkerDriver for HwDriver {
    fn cycle_samples(&self) -> usize {
        Self::cycle_samples(self)
    }

    fn sample_rate(&self) -> i32 {
        Self::sample_rate(self)
    }

    fn close_fds(&mut self) {
        Self::close_fds(self)
    }

    fn set_playing(&mut self, playing: bool) {
        Self::set_playing(self, playing)
    }

    fn set_output_gain_balance(&mut self, gain: f32, balance: f32) {
        Self::set_output_gain_balance(self, gain, balance)
    }

    fn run_cycle_for_worker(&mut self) -> Result<(), String> {
        Err(UNSUPPORTED.to_string())
    }

    fn run_assist_step_for_worker(&mut self) -> Result<bool, String> {
        Err(UNSUPPORTED.to_string())
    }
}

impl HwDevice for HwDriver {
    fn input_channels(&self) -> usize {
        Self::input_channels(self)
    }

    fn output_channels(&self) -> usize {
        Self::output_channels(self)
    }

    fn sample_rate(&self) -> i32 {
        Self::sample_rate(self)
    }

    fn latency_ranges(&self) -> ((usize, usize), (usize, usize)) {
        Self::latency_ranges(self)
    }
}

crate::impl_hw_midi_hub_traits!(MidiHub);
