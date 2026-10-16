//! Windows hardware-audio dispatch: WASAPI (default) or ASIO (device ids
//! prefixed with `asio:`), behind the single `HwDriver` type the engine
//! stores and shuttles to its worker.
//!
//! The enum keeps `wasapi::HwDriver` untouched: construction selects the
//! backend from the device string and every method forwards to the active
//! variant. MIDI is not part of this dispatch — both backends share the
//! Windows MIDI hub (`wasapi::MidiHub`).

use crate::audio::io::AudioIO;
use crate::hw::{asio, options::HwOptions, traits, wasapi};
use std::sync::Arc;

pub enum HwDriver {
    Wasapi(wasapi::HwDriver),
    Asio(asio::HwDriver),
}

impl HwDriver {
    pub fn new_with_options(
        device: &str,
        input_device: Option<&str>,
        rate: i32,
        bits: i32,
        options: HwOptions,
    ) -> Result<Self, String> {
        if asio::is_asio_device_id(device) {
            asio::HwDriver::new_with_options(device, input_device, rate, bits, options)
                .map(Self::Asio)
        } else {
            wasapi::HwDriver::new_with_options(device, input_device, rate, bits, options)
                .map(Self::Wasapi)
        }
    }

    pub fn input_channels(&self) -> usize {
        match self {
            Self::Wasapi(driver) => driver.input_channels(),
            Self::Asio(driver) => driver.input_channels(),
        }
    }

    pub fn output_channels(&self) -> usize {
        match self {
            Self::Wasapi(driver) => driver.output_channels(),
            Self::Asio(driver) => driver.output_channels(),
        }
    }

    pub fn sample_rate(&self) -> i32 {
        match self {
            Self::Wasapi(driver) => driver.sample_rate(),
            Self::Asio(driver) => driver.sample_rate(),
        }
    }

    pub fn cycle_samples(&self) -> usize {
        match self {
            Self::Wasapi(driver) => driver.cycle_samples(),
            Self::Asio(driver) => driver.cycle_samples(),
        }
    }

    pub fn sample_bits(&self) -> i32 {
        match self {
            Self::Wasapi(driver) => driver.sample_bits(),
            Self::Asio(driver) => driver.sample_bits(),
        }
    }

    pub fn frame_size_bytes(&self) -> usize {
        match self {
            Self::Wasapi(driver) => driver.frame_size_bytes(),
            Self::Asio(driver) => driver.frame_size_bytes(),
        }
    }

    pub fn input_port(&self, idx: usize) -> Option<Arc<AudioIO>> {
        match self {
            Self::Wasapi(driver) => driver.input_port(idx),
            Self::Asio(driver) => driver.input_port(idx),
        }
    }

    pub fn output_port(&self, idx: usize) -> Option<Arc<AudioIO>> {
        match self {
            Self::Wasapi(driver) => driver.output_port(idx),
            Self::Asio(driver) => driver.output_port(idx),
        }
    }

    pub fn set_output_gain_balance(&mut self, gain: f32, balance: f32) {
        match self {
            Self::Wasapi(driver) => driver.set_output_gain_balance(gain, balance),
            Self::Asio(driver) => driver.set_output_gain_balance(gain, balance),
        }
    }

    pub fn set_plan_slot(&mut self, slot: Arc<crate::render_plan::PlanSlot>) {
        match self {
            Self::Wasapi(driver) => driver.set_plan_slot(slot),
            Self::Asio(driver) => driver.set_plan_slot(slot),
        }
    }

    pub fn output_meter_db(&self, gain: f32, balance: f32) -> Vec<f32> {
        match self {
            Self::Wasapi(driver) => driver.output_meter_db(gain, balance),
            Self::Asio(driver) => driver.output_meter_db(gain, balance),
        }
    }

    pub fn output_meter_linear(&self, gain: f32, balance: f32) -> Vec<f32> {
        match self {
            Self::Wasapi(driver) => driver.output_meter_linear(gain, balance),
            Self::Asio(driver) => driver.output_meter_linear(gain, balance),
        }
    }

    pub fn run_cycle(&mut self) -> Result<(), String> {
        match self {
            Self::Wasapi(driver) => driver.run_cycle(),
            Self::Asio(driver) => driver.run_cycle(),
        }
    }

    pub fn run_assist_step(&mut self) -> Result<bool, String> {
        match self {
            Self::Wasapi(driver) => driver.run_assist_step(),
            Self::Asio(driver) => driver.run_assist_step(),
        }
    }

    pub fn channel(&mut self) -> &mut Self {
        self
    }

    pub fn set_playing(&mut self, playing: bool) {
        match self {
            Self::Wasapi(driver) => driver.set_playing(playing),
            Self::Asio(driver) => driver.set_playing(playing),
        }
    }

    pub fn close_fds(&mut self) {
        match self {
            Self::Wasapi(driver) => driver.close_fds(),
            Self::Asio(driver) => driver.close_fds(),
        }
    }

    pub fn latency_ranges(&self) -> ((usize, usize), (usize, usize)) {
        match self {
            Self::Wasapi(driver) => traits::HwDevice::latency_ranges(driver),
            Self::Asio(driver) => driver.latency_ranges(),
        }
    }
}

impl traits::HwWorkerDriver for HwDriver {
    fn cycle_samples(&self) -> usize {
        self.cycle_samples()
    }

    fn sample_rate(&self) -> i32 {
        self.sample_rate()
    }

    fn xrun_count(&self) -> Option<u64> {
        match self {
            Self::Wasapi(_) => None,
            Self::Asio(driver) => Some(driver.xrun_count()),
        }
    }

    fn close_fds(&mut self) {
        self.close_fds();
    }

    fn set_playing(&mut self, playing: bool) {
        self.set_playing(playing);
    }

    fn set_output_gain_balance(&mut self, gain: f32, balance: f32) {
        self.set_output_gain_balance(gain, balance);
    }

    fn run_cycle_for_worker(&mut self) -> Result<(), String> {
        self.run_cycle()
    }

    fn run_assist_step_for_worker(&mut self) -> Result<bool, String> {
        self.run_assist_step()
    }

    fn set_plan_slot(&mut self, slot: Arc<crate::render_plan::PlanSlot>) {
        self.set_plan_slot(slot);
    }

    fn request_stop(&mut self) {
        match self {
            Self::Wasapi(driver) => driver.request_stop(),
            Self::Asio(driver) => driver.request_stop(),
        }
    }

    fn stop_signaller(&self) -> Option<Arc<std::sync::atomic::AtomicBool>> {
        match self {
            Self::Wasapi(_) => None,
            Self::Asio(driver) => traits::HwWorkerDriver::stop_signaller(driver),
        }
    }
}

crate::impl_hw_device_for_driver!(HwDriver);

/// Predicts the period (in frames) the engine will actually stream with for
/// `device_id` and `requested_period_frames`, without opening the device.
/// ASIO devices negotiate the power-of-two request against the driver's
/// buffer-size constraints at open; since the engine already rounds
/// requests up to a power of two, the prediction for an in-range request
/// is the request itself. Out-of-range requests are clamped by the driver
/// and skipped by the calibrator via the negotiated-size comparison.
pub fn effective_period_frames(
    device_id: &str,
    requested_period_frames: usize,
    sample_rate_hz: u32,
    exclusive: bool,
) -> Result<usize, String> {
    if asio::is_asio_device_id(device_id) {
        return Ok(requested_period_frames.max(1).next_power_of_two());
    }
    wasapi::effective_period_frames(
        device_id,
        requested_period_frames,
        sample_rate_hz,
        exclusive,
    )
}
