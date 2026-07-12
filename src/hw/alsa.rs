use super::common;
use super::convert_policy;
use super::error_fmt;
use super::latency;
use super::ports;
use crate::audio::io::AudioIO;
use alsa::pcm::{Access, Format, HwParams, PCM, State};
use alsa::{Direction, ValueOr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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

pub struct HwDriver {
    capture: PCM,
    playback: PCM,
    audio_ins: Vec<Arc<AudioIO>>,
    audio_outs: Vec<Arc<AudioIO>>,
    output_gain_linear: f32,
    output_balance: f32,
    sample_rate: usize,
    period_frames: usize,
    channels_in: usize,
    channels_out: usize,
    nperiods: usize,
    sync_mode: bool,
    input_latency_frames: usize,
    output_latency_frames: usize,
    capture_format: SampleFormat,
    playback_format: SampleFormat,
    capture_buffer_i8: Vec<i8>,
    capture_buffer_i16: Vec<i16>,
    capture_buffer_i32: Vec<i32>,
    capture_temp_i32: Vec<i32>,
    capture_f32_buffer: Vec<f32>,
    playback_buffer_i8: Vec<i8>,
    playback_buffer_i16: Vec<i16>,
    playback_buffer_i32: Vec<i32>,
    playback_f32_buffer: Vec<f32>,
    playing: Arc<AtomicBool>,
    stop_requested: Arc<AtomicBool>,
    xrun_count: u64,
    /// Current render plan; when set, the RT cycle reads/writes plan arena
    /// buffers instead of the legacy port buffers.
    plan_slot: Option<Arc<crate::render_plan::PlanSlot>>,
}

impl std::fmt::Debug for HwDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HwDriver")
            .field("audio_ins", &self.audio_ins.len())
            .field("audio_outs", &self.audio_outs.len())
            .field("output_gain_linear", &self.output_gain_linear)
            .field("output_balance", &self.output_balance)
            .field("sample_rate", &self.sample_rate)
            .field("period_frames", &self.period_frames)
            .field("channels_in", &self.channels_in)
            .field("channels_out", &self.channels_out)
            .field("nperiods", &self.nperiods)
            .field("sync_mode", &self.sync_mode)
            .field("input_latency_frames", &self.input_latency_frames)
            .field("output_latency_frames", &self.output_latency_frames)
            .finish()
    }
}

impl HwDriver {
    pub fn new_with_options(
        output_device: &str,
        input_device: Option<&str>,
        rate: i32,
        bits: i32,
        options: HwOptions,
    ) -> Result<Self, String> {
        let input_device = input_device.unwrap_or(output_device);
        let capture = PCM::new(input_device, Direction::Capture, false)
            .map_err(|e| error_fmt::backend_open_error("ALSA", "capture", input_device, e))?;
        let playback = PCM::new(output_device, Direction::Playback, false)
            .map_err(|e| error_fmt::backend_open_error("ALSA", "playback", output_device, e))?;

        let period = options.period_frames.max(1);
        let nperiods = options.nperiods.max(1);
        let buffer_frames = period.saturating_mul(nperiods);

        let capture_target = desired_channels(&capture, rate as usize, period, buffer_frames);
        let playback_target = desired_channels(&playback, rate as usize, period, buffer_frames);

        let (channels_in, capture_format) = configure_pcm(
            &capture,
            rate as usize,
            capture_target,
            period,
            buffer_frames,
            bits,
        )?;
        let (channels_out, playback_format) = configure_pcm(
            &playback,
            rate as usize,
            playback_target,
            period,
            buffer_frames,
            bits,
        )?;

        let actual_rate = capture
            .hw_params_current()
            .map_err(|e| e.to_string())?
            .get_rate()
            .map_err(|e| e.to_string())?;

        let sample_rate = actual_rate as usize;
        let audio_ins: Vec<Arc<AudioIO>> = (0..channels_in)
            .map(|_| Arc::new(AudioIO::new(period)))
            .collect();
        let audio_outs: Vec<Arc<AudioIO>> = (0..channels_out)
            .map(|_| Arc::new(AudioIO::new(period)))
            .collect();

        let mut driver = Self {
            capture,
            playback,
            audio_ins,
            audio_outs,
            output_gain_linear: 1.0,
            output_balance: 0.0,
            sample_rate,
            period_frames: period,
            channels_in,
            channels_out,
            nperiods,
            sync_mode: options.sync_mode,
            input_latency_frames: options.input_latency_frames,
            output_latency_frames: options.output_latency_frames,
            capture_format,
            playback_format,
            capture_buffer_i8: vec![0; period * channels_in],
            capture_buffer_i16: vec![0; period * channels_in],
            capture_buffer_i32: vec![0; period * channels_in],
            capture_temp_i32: vec![0; period * channels_in],
            capture_f32_buffer: vec![0.0; period * channels_in],
            playback_buffer_i8: vec![0; period * channels_out],
            playback_buffer_i16: vec![0; period * channels_out],
            playback_buffer_i32: vec![0; period * channels_out],
            playback_f32_buffer: vec![0.0; period * channels_out],
            playing: Arc::new(AtomicBool::new(false)),
            stop_requested: Arc::new(AtomicBool::new(false)),
            xrun_count: 0,
            plan_slot: None,
        };
        driver.prefill_playback();
        Ok(driver)
    }

    pub fn new(device: &str, rate: i32, bits: i32) -> Result<Self, String> {
        Self::new_with_options(device, None, rate, bits, HwOptions::default())
    }

    pub fn close_fds(&mut self) {
        // ALSA does not need explicit fd closing before process exit.
    }

    pub fn set_playing(&mut self, playing: bool) {
        self.playing.store(playing, Ordering::Relaxed);
        if playing {
            if self.capture.state() != State::Running {
                let _ = self.capture.prepare();
                let _ = self.capture.start();
            }
            if self.playback.state() != State::Running {
                let _ = self.playback.prepare();
                let _ = self.playback.start();
            }
        } else {
            self.force_silence_now();
        }
    }

    fn force_silence_now(&mut self) {
        self.capture_buffer_i8.fill(0);
        self.capture_buffer_i16.fill(0);
        self.capture_buffer_i32.fill(0);
        self.capture_temp_i32.fill(0);
        self.capture_f32_buffer.fill(0.0);
        self.playback_buffer_i8.fill(0);
        self.playback_buffer_i16.fill(0);
        self.playback_buffer_i32.fill(0);
        self.playback_f32_buffer.fill(0.0);
        for ch in &self.audio_ins {
            ch.buffer.lock().fill(0.0);
        }
        for ch in &self.audio_outs {
            ch.buffer.lock().fill(0.0);
        }
    }

    fn prefill_playback(&mut self) {
        let total_frames = self.period_frames * self.nperiods.max(1);
        match self.playback_format {
            SampleFormat::S8 => {
                let silence = vec![0i8; total_frames * self.channels_out];
                if let Ok(out_io) = self.playback.io_i8() {
                    let _ = out_io.writei(&silence);
                }
            }
            SampleFormat::S16LE | SampleFormat::S16BE => {
                let silence = vec![0i16; total_frames * self.channels_out];
                if let Ok(out_io) = self.playback.io_i16() {
                    let _ = out_io.writei(&silence);
                }
            }
            SampleFormat::S24LE
            | SampleFormat::S24BE
            | SampleFormat::S32LE
            | SampleFormat::S32BE => {
                let silence = vec![0i32; total_frames * self.channels_out];
                if let Ok(out_io) = self.playback.io_i32() {
                    let _ = out_io.writei(&silence);
                }
            }
        }
    }

    pub fn input_channels(&self) -> usize {
        self.channels_in
    }

    pub fn output_channels(&self) -> usize {
        self.channels_out
    }

    pub fn sample_rate(&self) -> i32 {
        self.sample_rate as i32
    }

    pub fn cycle_samples(&self) -> usize {
        self.period_frames
    }

    pub fn sample_bits(&self) -> i32 {
        self.playback_format.bits()
    }

    pub fn frame_size_bytes(&self) -> usize {
        self.channels_out * (self.playback_format.bits() as usize / 8)
    }

    pub fn input_port(&self, idx: usize) -> Option<Arc<AudioIO>> {
        self.audio_ins.get(idx).cloned()
    }

    pub fn output_port(&self, idx: usize) -> Option<Arc<AudioIO>> {
        self.audio_outs.get(idx).cloned()
    }

    pub fn set_output_gain_balance(&mut self, gain: f32, balance: f32) {
        self.output_gain_linear = gain.max(0.0);
        self.output_balance = balance.clamp(-1.0, 1.0);
    }

    pub fn set_plan_slot(&mut self, slot: Arc<crate::render_plan::PlanSlot>) {
        self.plan_slot = Some(slot);
    }

    pub fn output_meter_linear(&self, gain: f32, balance: f32) -> Vec<f32> {
        if let Some(slot) = &self.plan_slot {
            let plan = slot.load();
            common::output_meter_linear_from_plan(&plan, gain, balance)
        } else {
            common::output_meter_linear(&self.audio_outs, gain, balance)
        }
    }

    pub fn run_cycle(&mut self) -> Result<(), String> {
        let frames = self.period_frames;

        match self.capture_format {
            SampleFormat::S8 => {
                let in_io = self
                    .capture
                    .io_i8()
                    .map_err(|e| error_fmt::backend_io_error("ALSA", "capture", e))?;
                if let Err(e) = in_io.readi(&mut self.capture_buffer_i8) {
                    if self.capture.state() == State::XRun {
                        self.xrun_count += 1;
                        tracing::warn!("ALSA capture xrun #{}", self.xrun_count);
                        let _ = self.capture.prepare();
                        self.capture_buffer_i8.fill(0);
                    } else {
                        return Err(error_fmt::backend_rw_error("ALSA", "capture", "read", e));
                    }
                }
            }
            SampleFormat::S16LE | SampleFormat::S16BE => {
                let in_io = self
                    .capture
                    .io_i16()
                    .map_err(|e| error_fmt::backend_io_error("ALSA", "capture", e))?;
                if let Err(e) = in_io.readi(&mut self.capture_buffer_i16) {
                    if self.capture.state() == State::XRun {
                        self.xrun_count += 1;
                        tracing::warn!("ALSA capture xrun #{}", self.xrun_count);
                        let _ = self.capture.prepare();
                        self.capture_buffer_i16.fill(0);
                    } else {
                        return Err(error_fmt::backend_rw_error("ALSA", "capture", "read", e));
                    }
                }
            }
            SampleFormat::S24LE
            | SampleFormat::S24BE
            | SampleFormat::S32LE
            | SampleFormat::S32BE => {
                let in_io = self
                    .capture
                    .io_i32()
                    .map_err(|e| error_fmt::backend_io_error("ALSA", "capture", e))?;
                if let Err(e) = in_io.readi(&mut self.capture_buffer_i32) {
                    if self.capture.state() == State::XRun {
                        self.xrun_count += 1;
                        tracing::warn!("ALSA capture xrun #{}", self.xrun_count);
                        let _ = self.capture.prepare();
                        self.capture_buffer_i32.fill(0);
                    } else {
                        return Err(error_fmt::backend_rw_error("ALSA", "capture", "read", e));
                    }
                }
            }
        }

        let all_in_connected = self.audio_ins.iter().all(ports::has_audio_connections);
        match self.capture_format {
            SampleFormat::S8 => {
                let total = frames * self.channels_in;
                crate::simd::convert_i8_to_f32(
                    &self.capture_buffer_i8[..total],
                    &mut self.capture_f32_buffer[..total],
                    convert_policy::F32_FROM_I8,
                );
                if let Some(slot) = &self.plan_slot {
                    let plan = slot.load();
                    ports::fill_arena_from_interleaved(
                        &plan,
                        frames,
                        &self.capture_f32_buffer[..total],
                        self.channels_in,
                    );
                } else {
                    ports::fill_ports_from_interleaved_buffer(
                        &self.audio_ins,
                        frames,
                        !all_in_connected,
                        &self.capture_f32_buffer[..total],
                        self.channels_in,
                    );
                }
            }
            SampleFormat::S16LE => {
                let total = frames * self.channels_in;
                crate::simd::convert_i16_to_f32(
                    &self.capture_buffer_i16[..total],
                    &mut self.capture_f32_buffer[..total],
                    convert_policy::F32_FROM_I16,
                );
                if let Some(slot) = &self.plan_slot {
                    let plan = slot.load();
                    ports::fill_arena_from_interleaved(
                        &plan,
                        frames,
                        &self.capture_f32_buffer[..total],
                        self.channels_in,
                    );
                } else {
                    ports::fill_ports_from_interleaved_buffer(
                        &self.audio_ins,
                        frames,
                        !all_in_connected,
                        &self.capture_f32_buffer[..total],
                        self.channels_in,
                    );
                }
            }
            SampleFormat::S16BE => {
                let total = frames * self.channels_in;
                for s in &mut self.capture_buffer_i16[..total] {
                    *s = s.swap_bytes();
                }
                crate::simd::convert_i16_to_f32(
                    &self.capture_buffer_i16[..total],
                    &mut self.capture_f32_buffer[..total],
                    convert_policy::F32_FROM_I16,
                );
                for s in &mut self.capture_buffer_i16[..total] {
                    *s = s.swap_bytes();
                }
                if let Some(slot) = &self.plan_slot {
                    let plan = slot.load();
                    ports::fill_arena_from_interleaved(
                        &plan,
                        frames,
                        &self.capture_f32_buffer[..total],
                        self.channels_in,
                    );
                } else {
                    ports::fill_ports_from_interleaved_buffer(
                        &self.audio_ins,
                        frames,
                        !all_in_connected,
                        &self.capture_f32_buffer[..total],
                        self.channels_in,
                    );
                }
            }
            SampleFormat::S24LE => {
                let total = frames * self.channels_in;
                crate::simd::convert_i24_to_f32(
                    &self.capture_buffer_i32[..total],
                    &mut self.capture_f32_buffer[..total],
                    convert_policy::F32_FROM_I24,
                );
                if let Some(slot) = &self.plan_slot {
                    let plan = slot.load();
                    ports::fill_arena_from_interleaved(
                        &plan,
                        frames,
                        &self.capture_f32_buffer[..total],
                        self.channels_in,
                    );
                } else {
                    ports::fill_ports_from_interleaved_buffer(
                        &self.audio_ins,
                        frames,
                        !all_in_connected,
                        &self.capture_f32_buffer[..total],
                        self.channels_in,
                    );
                }
            }
            SampleFormat::S24BE => {
                let total = frames * self.channels_in;
                let needs_swap = self.capture_format.needs_swap();
                self.capture_temp_i32[..total].copy_from_slice(&self.capture_buffer_i32[..total]);
                if needs_swap {
                    for s in &mut self.capture_temp_i32[..total] {
                        *s = s.swap_bytes();
                    }
                }
                for s in &mut self.capture_temp_i32[..total] {
                    *s >>= 8;
                }
                crate::simd::convert_i32_to_f32(
                    &self.capture_temp_i32[..total],
                    &mut self.capture_f32_buffer[..total],
                    convert_policy::F32_FROM_I24,
                );
                if let Some(slot) = &self.plan_slot {
                    let plan = slot.load();
                    ports::fill_arena_from_interleaved(
                        &plan,
                        frames,
                        &self.capture_f32_buffer[..total],
                        self.channels_in,
                    );
                } else {
                    ports::fill_ports_from_interleaved_buffer(
                        &self.audio_ins,
                        frames,
                        !all_in_connected,
                        &self.capture_f32_buffer[..total],
                        self.channels_in,
                    );
                }
            }
            SampleFormat::S32LE => {
                let total = frames * self.channels_in;
                crate::simd::convert_i32_to_f32(
                    &self.capture_buffer_i32[..total],
                    &mut self.capture_f32_buffer[..total],
                    convert_policy::F32_FROM_I32,
                );
                if let Some(slot) = &self.plan_slot {
                    let plan = slot.load();
                    ports::fill_arena_from_interleaved(
                        &plan,
                        frames,
                        &self.capture_f32_buffer[..total],
                        self.channels_in,
                    );
                } else {
                    ports::fill_ports_from_interleaved_buffer(
                        &self.audio_ins,
                        frames,
                        !all_in_connected,
                        &self.capture_f32_buffer[..total],
                        self.channels_in,
                    );
                }
            }
            SampleFormat::S32BE => {
                let total = frames * self.channels_in;
                for s in &mut self.capture_buffer_i32[..total] {
                    *s = s.swap_bytes();
                }
                crate::simd::convert_i32_to_f32(
                    &self.capture_buffer_i32[..total],
                    &mut self.capture_f32_buffer[..total],
                    convert_policy::F32_FROM_I32,
                );
                for s in &mut self.capture_buffer_i32[..total] {
                    *s = s.swap_bytes();
                }
                if let Some(slot) = &self.plan_slot {
                    let plan = slot.load();
                    ports::fill_arena_from_interleaved(
                        &plan,
                        frames,
                        &self.capture_f32_buffer[..total],
                        self.channels_in,
                    );
                } else {
                    ports::fill_ports_from_interleaved_buffer(
                        &self.audio_ins,
                        frames,
                        !all_in_connected,
                        &self.capture_f32_buffer[..total],
                        self.channels_in,
                    );
                }
            }
        }

        let is_playing = self.playing.load(Ordering::Relaxed);
        let gain = self.output_gain_linear;
        let balance = self.output_balance;
        let all_out_connected = self.audio_outs.iter().all(ports::has_audio_connections);

        match self.playback_format {
            SampleFormat::S8 => {
                if is_playing {
                    if !all_out_connected {
                        self.playback_buffer_i8.fill(0);
                    }
                    let mut write_sample = |ch: usize, frame: usize, sample: f32| {
                        let idx = frame * self.channels_out + ch;
                        let v = (sample.clamp(-1.0, 1.0) * convert_policy::F32_TO_I8) as i8;
                        if let Some(dst) = self.playback_buffer_i8.get_mut(idx) {
                            *dst = v;
                        }
                    };
                    if let Some(slot) = &self.plan_slot {
                        let plan = slot.load();
                        ports::write_interleaved_from_arena(
                            &plan,
                            frames,
                            gain,
                            balance,
                            &mut write_sample,
                        );
                    } else {
                        ports::write_interleaved_from_ports(
                            &self.audio_outs,
                            frames,
                            gain,
                            balance,
                            !all_out_connected,
                            write_sample,
                        );
                    }
                } else {
                    self.playback_buffer_i8.fill(0);
                }
                let out_io = self
                    .playback
                    .io_i8()
                    .map_err(|e| error_fmt::backend_io_error("ALSA", "playback", e))?;
                if let Err(e) = out_io.writei(&self.playback_buffer_i8) {
                    if self.playback.state() == State::XRun {
                        self.xrun_count += 1;
                        tracing::warn!("ALSA playback xrun #{}", self.xrun_count);
                        let _ = self.playback.prepare();
                    } else {
                        return Err(error_fmt::backend_rw_error("ALSA", "playback", "write", e));
                    }
                }
            }
            SampleFormat::S16LE | SampleFormat::S16BE => {
                let needs_swap = self.playback_format.needs_swap();
                if is_playing {
                    if !all_out_connected {
                        self.playback_buffer_i16.fill(0);
                    }
                    let mut write_sample = |ch: usize, frame: usize, sample: f32| {
                        let idx = frame * self.channels_out + ch;
                        let mut v = (sample.clamp(-1.0, 1.0) * convert_policy::F32_TO_I16) as i16;
                        if needs_swap {
                            v = v.swap_bytes();
                        }
                        if let Some(dst) = self.playback_buffer_i16.get_mut(idx) {
                            *dst = v;
                        }
                    };
                    if let Some(slot) = &self.plan_slot {
                        let plan = slot.load();
                        ports::write_interleaved_from_arena(
                            &plan,
                            frames,
                            gain,
                            balance,
                            &mut write_sample,
                        );
                    } else {
                        ports::write_interleaved_from_ports(
                            &self.audio_outs,
                            frames,
                            gain,
                            balance,
                            !all_out_connected,
                            write_sample,
                        );
                    }
                } else {
                    self.playback_buffer_i16.fill(0);
                }
                let out_io = self
                    .playback
                    .io_i16()
                    .map_err(|e| error_fmt::backend_io_error("ALSA", "playback", e))?;
                if let Err(e) = out_io.writei(&self.playback_buffer_i16) {
                    if self.playback.state() == State::XRun {
                        self.xrun_count += 1;
                        tracing::warn!("ALSA playback xrun #{}", self.xrun_count);
                        let _ = self.playback.prepare();
                    } else {
                        return Err(error_fmt::backend_rw_error("ALSA", "playback", "write", e));
                    }
                }
            }
            SampleFormat::S24LE | SampleFormat::S24BE => {
                let needs_swap = self.playback_format.needs_swap();
                let is_be = matches!(self.playback_format, SampleFormat::S24BE);
                let total = frames * self.channels_out;
                if is_playing {
                    if !all_out_connected {
                        self.playback_buffer_i32.fill(0);
                    }
                    self.playback_f32_buffer[..total].fill(0.0);
                    let mut write_sample = |ch: usize, frame: usize, sample: f32| {
                        let idx = frame * self.channels_out + ch;
                        if let Some(dst) = self.playback_f32_buffer.get_mut(idx) {
                            *dst = sample;
                        }
                    };
                    if let Some(slot) = &self.plan_slot {
                        let plan = slot.load();
                        ports::write_interleaved_from_arena(
                            &plan,
                            frames,
                            gain,
                            balance,
                            &mut write_sample,
                        );
                    } else {
                        ports::write_interleaved_from_ports(
                            &self.audio_outs,
                            frames,
                            gain,
                            balance,
                            !all_out_connected,
                            write_sample,
                        );
                    }
                    crate::simd::convert_f32_to_i24(
                        &self.playback_f32_buffer[..total],
                        &mut self.playback_buffer_i32[..total],
                        convert_policy::F32_TO_I24,
                    );
                    if is_be {
                        for s in &mut self.playback_buffer_i32[..total] {
                            *s <<= 8;
                        }
                    }
                    if needs_swap {
                        for s in &mut self.playback_buffer_i32[..total] {
                            *s = s.swap_bytes();
                        }
                    }
                } else {
                    self.playback_buffer_i32.fill(0);
                }
                let out_io = self
                    .playback
                    .io_i32()
                    .map_err(|e| error_fmt::backend_io_error("ALSA", "playback", e))?;
                if let Err(e) = out_io.writei(&self.playback_buffer_i32) {
                    if self.playback.state() == State::XRun {
                        self.xrun_count += 1;
                        tracing::warn!("ALSA playback xrun #{}", self.xrun_count);
                        let _ = self.playback.prepare();
                    } else {
                        return Err(error_fmt::backend_rw_error("ALSA", "playback", "write", e));
                    }
                }
            }
            SampleFormat::S32LE | SampleFormat::S32BE => {
                let needs_swap = self.playback_format.needs_swap();
                if is_playing {
                    if !all_out_connected {
                        self.playback_buffer_i32.fill(0);
                    }
                    let mut write_sample = |ch: usize, frame: usize, sample: f32| {
                        let idx = frame * self.channels_out + ch;
                        let mut v = (sample.clamp(-1.0, 1.0) * convert_policy::F32_TO_I32) as i32;
                        if needs_swap {
                            v = v.swap_bytes();
                        }
                        if let Some(dst) = self.playback_buffer_i32.get_mut(idx) {
                            *dst = v;
                        }
                    };
                    if let Some(slot) = &self.plan_slot {
                        let plan = slot.load();
                        ports::write_interleaved_from_arena(
                            &plan,
                            frames,
                            gain,
                            balance,
                            &mut write_sample,
                        );
                    } else {
                        ports::write_interleaved_from_ports(
                            &self.audio_outs,
                            frames,
                            gain,
                            balance,
                            !all_out_connected,
                            write_sample,
                        );
                    }
                } else {
                    self.playback_buffer_i32.fill(0);
                }
                let out_io = self
                    .playback
                    .io_i32()
                    .map_err(|e| error_fmt::backend_io_error("ALSA", "playback", e))?;
                if let Err(e) = out_io.writei(&self.playback_buffer_i32) {
                    if self.playback.state() == State::XRun {
                        self.xrun_count += 1;
                        tracing::warn!("ALSA playback xrun #{}", self.xrun_count);
                        let _ = self.playback.prepare();
                    } else {
                        return Err(error_fmt::backend_rw_error("ALSA", "playback", "write", e));
                    }
                }
            }
        }

        Ok(())
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

    pub fn channel(&mut self) -> AlsaChannel<'_> {
        AlsaChannel { driver: self }
    }
}

impl crate::hw::traits::HwWorkerDriver for HwDriver {
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

    fn request_stop(&mut self) {
        self.stop_requested.store(true, Ordering::Release);
        let _ = self.capture.drop();
        let _ = self.playback.drop();
    }

    fn run_cycle_for_worker(&mut self) -> Result<(), String> {
        self.channel().run_cycle().map_err(|e| e.to_string())
    }

    fn run_assist_step_for_worker(&mut self) -> Result<bool, String> {
        self.channel().run_assist_step().map_err(|e| e.to_string())
    }
}

crate::impl_hw_device_for_driver!(HwDriver);
crate::impl_hw_midi_hub_traits!(MidiHub);

pub struct AlsaChannel<'a> {
    driver: &'a mut HwDriver,
}

impl<'a> AlsaChannel<'a> {
    pub fn run_cycle(&mut self) -> std::io::Result<()> {
        self.driver.run_cycle().map_err(std::io::Error::other)
    }

    pub fn run_assist_step(&mut self) -> std::io::Result<bool> {
        Ok(false)
    }
}

fn desired_channels(pcm: &PCM, rate: usize, period_frames: usize, buffer_frames: usize) -> usize {
    let _ = (rate, period_frames, buffer_frames);
    let Ok(hwp) = HwParams::any(pcm) else {
        return 2;
    };
    if hwp.set_access(Access::RWInterleaved).is_err() {
        return 2;
    }
    hwp.get_channels_max()
        .map(|v| v as usize)
        .unwrap_or(2)
        .max(1)
}

fn configure_pcm(
    pcm: &PCM,
    rate: usize,
    channels: usize,
    period_frames: usize,
    buffer_frames: usize,
    bits: i32,
) -> Result<(usize, SampleFormat), String> {
    let hwp = HwParams::any(pcm).map_err(|e| e.to_string())?;
    hwp.set_access(Access::RWInterleaved)
        .map_err(|e| e.to_string())?;
    let format = choose_best_format(&hwp, bits)?;
    let target = (channels.max(1)) as u32;
    let _chosen_channels = match hwp.set_channels_near(target) {
        Ok(v) if v > 0 => v,
        _ => {
            hwp.set_channels(2).map_err(|e| e.to_string())?;
            2
        }
    };
    hwp.set_rate(rate as u32, ValueOr::Nearest)
        .map_err(|e| e.to_string())?;
    let _actual_period = hwp
        .set_period_size_near(period_frames as i64, ValueOr::Nearest)
        .map_err(|e| e.to_string())?;
    let _actual_buffer = hwp
        .set_buffer_size_near(buffer_frames as i64)
        .map_err(|e| e.to_string())?;
    pcm.hw_params(&hwp).map_err(|e| e.to_string())?;

    let swp = pcm.sw_params_current().map_err(|e| e.to_string())?;
    let cur = pcm.hw_params_current().map_err(|e| e.to_string())?;
    let actual_buffer = cur.get_buffer_size().map_err(|e| e.to_string())?;
    let actual_period = cur.get_period_size().map_err(|e| e.to_string())?;
    let start_threshold = actual_buffer.saturating_sub(actual_period) as u32;
    swp.set_start_threshold(start_threshold as i64)
        .map_err(|e| e.to_string())?;
    swp.set_avail_min(actual_period)
        .map_err(|e| e.to_string())?;
    pcm.sw_params(&swp).map_err(|e| e.to_string())?;
    pcm.prepare().map_err(|e| e.to_string())?;

    let actual_channels = pcm
        .hw_params_current()
        .map_err(|e| e.to_string())?
        .get_channels()
        .map_err(|e| e.to_string())? as usize;

    Ok((actual_channels.max(1), format))
}

#[derive(Debug, Clone, Copy)]
enum SampleFormat {
    S8,
    S16LE,
    S16BE,
    S24LE,
    S24BE,
    S32LE,
    S32BE,
}

impl SampleFormat {
    fn bits(self) -> i32 {
        match self {
            SampleFormat::S8 => 8,
            SampleFormat::S16LE | SampleFormat::S16BE => 16,
            SampleFormat::S24LE | SampleFormat::S24BE => 24,
            SampleFormat::S32LE | SampleFormat::S32BE => 32,
        }
    }

    fn alsa_format(self) -> Format {
        match self {
            SampleFormat::S8 => Format::S8,
            SampleFormat::S16LE => Format::S16LE,
            SampleFormat::S16BE => Format::S16BE,
            SampleFormat::S24LE => Format::S24LE,
            SampleFormat::S24BE => Format::S24BE,
            SampleFormat::S32LE => Format::S32LE,
            SampleFormat::S32BE => Format::S32BE,
        }
    }

    fn needs_swap(self) -> bool {
        match self {
            SampleFormat::S8 => false,
            SampleFormat::S16LE | SampleFormat::S24LE | SampleFormat::S32LE => {
                cfg!(target_endian = "big")
            }
            SampleFormat::S16BE | SampleFormat::S24BE | SampleFormat::S32BE => {
                cfg!(target_endian = "little")
            }
        }
    }
}

fn choose_best_format(hwp: &HwParams<'_>, bits: i32) -> Result<SampleFormat, String> {
    let candidates = sample_format_candidates(bits);
    let mut last_err: Option<alsa::Error> = None;
    for candidate in candidates {
        match hwp.set_format(candidate.alsa_format()) {
            Ok(()) => return Ok(candidate),
            Err(e) => last_err = Some(e),
        }
    }
    Err(format!(
        "No supported integer PCM format after fallback chain; last set_format error: {}.",
        last_err
            .map(|e| e.to_string())
            .unwrap_or_else(|| "unknown".to_string())
    ))
}

fn sample_format_candidates(bits: i32) -> Vec<SampleFormat> {
    fn add_pair(candidates: &mut Vec<SampleFormat>, native: SampleFormat, foreign: SampleFormat) {
        candidates.push(native);
        candidates.push(foreign);
    }

    let mut candidates = Vec::with_capacity(7);
    match bits {
        32 => {
            add_pair(&mut candidates, native_s32(), foreign_s32());
            add_pair(&mut candidates, native_s24(), foreign_s24());
            add_pair(&mut candidates, native_s16(), foreign_s16());
            candidates.push(SampleFormat::S8);
        }
        24 => {
            add_pair(&mut candidates, native_s24(), foreign_s24());
            add_pair(&mut candidates, native_s16(), foreign_s16());
            candidates.push(SampleFormat::S8);
        }
        16 => {
            add_pair(&mut candidates, native_s16(), foreign_s16());
            candidates.push(SampleFormat::S8);
        }
        8 => candidates.push(SampleFormat::S8),
        _ => {
            add_pair(&mut candidates, native_s16(), foreign_s16());
            candidates.push(SampleFormat::S8);
        }
    }
    candidates
}

#[cfg(target_endian = "little")]
fn native_s16() -> SampleFormat {
    SampleFormat::S16LE
}
#[cfg(target_endian = "big")]
fn native_s16() -> SampleFormat {
    SampleFormat::S16BE
}
#[cfg(target_endian = "little")]
fn foreign_s16() -> SampleFormat {
    SampleFormat::S16BE
}
#[cfg(target_endian = "big")]
fn foreign_s16() -> SampleFormat {
    SampleFormat::S16LE
}

#[cfg(target_endian = "little")]
fn native_s24() -> SampleFormat {
    SampleFormat::S24LE
}
#[cfg(target_endian = "big")]
fn native_s24() -> SampleFormat {
    SampleFormat::S24BE
}
#[cfg(target_endian = "little")]
fn foreign_s24() -> SampleFormat {
    SampleFormat::S24BE
}
#[cfg(target_endian = "big")]
fn foreign_s24() -> SampleFormat {
    SampleFormat::S24LE
}

#[cfg(target_endian = "little")]
fn native_s32() -> SampleFormat {
    SampleFormat::S32LE
}
#[cfg(target_endian = "big")]
fn native_s32() -> SampleFormat {
    SampleFormat::S32BE
}
#[cfg(target_endian = "little")]
fn foreign_s32() -> SampleFormat {
    SampleFormat::S32BE
}
#[cfg(target_endian = "big")]
fn foreign_s32() -> SampleFormat {
    SampleFormat::S32LE
}
