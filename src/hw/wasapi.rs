use crate::audio::io::AudioIO;
use crate::hw::{common, options::HwOptions, traits};
use crate::message::HwMidiEvent;
use crate::midi::io::MidiEvent;
use midir::{Ignore, MidiInput, MidiInputConnection, MidiOutput, MidiOutputConnection};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::io::Write;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};
use windows::Win32::Devices::Properties::DEVPKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_SHAREMODE_EXCLUSIVE, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    DEVICE_STATE_ACTIVE, IAudioCaptureClient, IAudioClient, IAudioClient3, IAudioRenderClient,
    IMMDevice, IMMDeviceCollection, IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX,
    WAVEFORMATEXTENSIBLE, eCapture, eConsole, eRender,
};
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::StructuredStorage::PropVariantClear;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize, STGM_READ,
};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, SetEvent, WaitForMultipleObjects};
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::core::{Interface, PCWSTR, PWSTR};

const MIDI_IN_PREFIX: &str = "winmidi:in:";
const MIDI_OUT_PREFIX: &str = "winmidi:out:";
const WASAPI_PREFIX: &str = "wasapi:";
const REFTIME_PER_SEC: i64 = 10_000_000;

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

#[derive(Clone, Copy)]
enum WasapiMode {
    SharedLowLatency,
    Exclusive,
}

impl WasapiMode {
    fn from_options(options: &HwOptions) -> Self {
        if options.exclusive {
            Self::Exclusive
        } else {
            Self::SharedLowLatency
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::SharedLowLatency => "WASAPI shared low latency",
            Self::Exclusive => "WASAPI exclusive",
        }
    }
}

#[derive(Clone, Copy)]
struct StreamInfo {
    sample_rate: usize,
    channels: usize,
    period_frames: usize,
    actual_buffer_frames: usize,
    latency_frames: usize,
}

struct OutputStream {
    shutdown_event: HANDLE,
    thread: Option<JoinHandle<()>>,
}

impl OutputStream {
    fn stop(&mut self) {
        unsafe {
            let _ = SetEvent(self.shutdown_event);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for OutputStream {
    fn drop(&mut self) {
        self.stop();
        unsafe {
            let _ = CloseHandle(self.shutdown_event);
        }
    }
}

struct InputStream {
    shutdown_event: HANDLE,
    thread: Option<JoinHandle<()>>,
}

impl InputStream {
    fn stop(&mut self) {
        unsafe {
            let _ = SetEvent(self.shutdown_event);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for InputStream {
    fn drop(&mut self) {
        self.stop();
        unsafe {
            let _ = CloseHandle(self.shutdown_event);
        }
    }
}

pub struct HwDriver {
    input_stream: Option<InputStream>,
    output_stream: OutputStream,
    input_rx: Option<Receiver<Vec<f32>>>,
    output_tx: SyncSender<Vec<f32>>,
    cycle_tick_rx: Receiver<()>,
    input_queue: Vec<f32>,
    audio_ins: Vec<Arc<AudioIO>>,
    audio_outs: Vec<Arc<AudioIO>>,
    output_gain_linear: f32,
    output_balance: f32,
    sample_rate: usize,
    period_frames: usize,
    input_channels: usize,
    output_channels: usize,
    input_latency_frames: usize,
    output_latency_frames: usize,
    playing: bool,
    stop_requested: Arc<AtomicBool>,
    plan_slot: Option<Arc<crate::render_plan::PlanSlot>>,
}

impl HwDriver {
    pub fn new_with_options(
        device: &str,
        input_device: Option<&str>,
        rate: i32,
        _bits: i32,
        options: HwOptions,
    ) -> Result<Self, String> {
        let mode = WasapiMode::from_options(&options);
        let requested_output = strip_wasapi_prefix(device);
        let requested_input = input_device
            .map(strip_wasapi_prefix)
            .unwrap_or(requested_output);
        let requested_rate = rate.max(1) as u32;
        let requested_period_frames = options.period_frames.max(1);
        let output_queue_periods = options.nperiods.max(4);
        let (output_tx, output_rx) = mpsc::sync_channel::<Vec<f32>>(output_queue_periods);
        let (cycle_tick_tx, cycle_tick_rx) =
            mpsc::sync_channel::<()>(output_queue_periods.saturating_mul(4));
        let stop_requested = Arc::new(AtomicBool::new(false));

        let (output_stream, output_info) = start_output_stream(
            requested_output.to_string(),
            requested_rate,
            requested_period_frames,
            mode,
            output_rx,
            cycle_tick_tx,
        )?;

        let sample_rate = output_info.sample_rate;
        let period_frames = output_info.period_frames;
        let output_channels = output_info.channels;
        let audio_outs = (0..output_channels)
            .map(|_| Arc::new(AudioIO::new(period_frames)))
            .collect();

        let (input_stream, input_rx, input_channels, input_latency_frames) =
            match start_input_stream(
                requested_input.to_string(),
                sample_rate as u32,
                period_frames,
                mode,
            ) {
                Ok((stream, rx, info)) => {
                    (Some(stream), Some(rx), info.channels, info.latency_frames)
                }
                Err(err) => {
                    debug!(err, "WASAPI input disabled");
                    (None, None, 0, 0)
                }
            };

        let audio_ins = (0..input_channels)
            .map(|_| Arc::new(AudioIO::new(period_frames)))
            .collect();

        debug!(
            mode = mode.label(),
            period_frames,
            output_actual_buffer_frames = output_info.actual_buffer_frames,
            input_channels,
            output_channels,
            sample_rate,
            "WASAPI backend opened"
        );

        Ok(Self {
            input_stream,
            output_stream,
            input_rx,
            output_tx,
            cycle_tick_rx,
            input_queue: Vec::new(),
            audio_ins,
            audio_outs,
            output_gain_linear: 1.0,
            output_balance: 0.0,
            sample_rate,
            period_frames,
            input_channels,
            output_channels,
            input_latency_frames,
            output_latency_frames: output_info.latency_frames,
            playing: false,
            stop_requested,
            plan_slot: None,
        })
    }

    pub fn input_channels(&self) -> usize {
        self.input_channels
    }

    pub fn output_channels(&self) -> usize {
        self.output_channels
    }

    pub fn sample_rate(&self) -> i32 {
        self.sample_rate as i32
    }

    pub fn close_fds(&mut self) {
        self.output_stream.stop();
        if let Some(stream) = &mut self.input_stream {
            stream.stop();
        }
    }

    pub fn cycle_samples(&self) -> usize {
        self.period_frames
    }

    pub fn sample_bits(&self) -> i32 {
        32
    }

    pub fn frame_size_bytes(&self) -> usize {
        self.output_channels * 4
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

    pub fn output_meter_db(&self, gain: f32, balance: f32) -> Vec<f32> {
        common::output_meter_db(self.audio_outs.len(), gain, balance)
    }

    pub fn output_meter_linear(&self, gain: f32, balance: f32) -> Vec<f32> {
        if let Some(slot) = &self.plan_slot {
            let plan = slot.load();
            common::output_meter_linear_from_plan(&plan, gain, balance)
        } else {
            common::output_meter_linear(self.audio_outs.len(), gain, balance)
        }
    }

    pub fn run_cycle(&mut self) -> Result<(), String> {
        info!("wasapi run_cycle start");
        let input_frames = self.period_frames;
        let input_channels = self.input_channels.max(1);
        if let Some(rx) = &self.input_rx {
            while let Ok(chunk) = rx.try_recv() {
                self.input_queue.extend_from_slice(&chunk);
            }
        }

        let have_samples = self.input_queue.len();
        let have_frames = have_samples / input_channels;
        let consume_frames = have_frames.min(input_frames);
        let consume_samples = consume_frames.saturating_mul(input_channels);

        if let Some(slot) = &self.plan_slot {
            let plan = slot.load();
            crate::hw::ports::fill_arena_from_interleaved(
                &plan,
                input_frames,
                &self.input_queue[..consume_samples],
                input_channels,
            );
        } else {
            for io_port in &self.audio_ins {
                io_port.finished.store(true, Ordering::Release);
            }
        }

        if consume_samples > 0 {
            self.input_queue.drain(..consume_samples);
        }

        let frames = self.period_frames;
        let channels = self.output_channels;
        let gain = self.output_gain_linear;
        let balance = self.output_balance;
        let mut interleaved = vec![0.0_f32; frames.saturating_mul(channels)];
        if self.playing {
            if let Some(slot) = &self.plan_slot {
                let plan = slot.load();
                crate::hw::ports::write_interleaved_from_arena(
                    &plan,
                    frames,
                    gain,
                    balance,
                    |ch, frame, sample| {
                        let idx = frame * channels + ch;
                        if let Some(dst) = interleaved.get_mut(idx) {
                            *dst = sample;
                        }
                    },
                );
            }
        }

        if let Ok(path) = std::env::var("MAOLAN_WASAPI_DUMP") {
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let mut bytes = Vec::with_capacity(interleaved.len() * 4);
                for sample in &interleaved {
                    bytes.extend_from_slice(&sample.to_le_bytes());
                }
                let _ = file.write_all(&bytes);
            }
        }

        info!("wasapi run_cycle rendered");
        self.queue_output_period(interleaved)
    }

    fn queue_output_period(&mut self, mut interleaved: Vec<f32>) -> Result<(), String> {
        loop {
            if self.stop_requested.load(Ordering::Acquire) {
                return Ok(());
            }
            match self.output_tx.try_send(interleaved) {
                Ok(()) => {
                    info!("wasapi queue_output_period sent");
                    return Ok(());
                }
                Err(TrySendError::Full(buffer)) => {
                    interleaved = buffer;
                    info!("wasapi queue_output_period waiting for tick");
                    self.wait_for_cycle_tick()?;
                    info!("wasapi queue_output_period tick done");
                }
                Err(TrySendError::Disconnected(_)) => {
                    return Err("WASAPI output thread disconnected".to_string());
                }
            }
        }
    }

    fn wait_for_cycle_tick(&mut self) -> Result<(), String> {
        let tick_deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < tick_deadline {
            if self.stop_requested.load(Ordering::Acquire) {
                return Ok(());
            }
            match self.cycle_tick_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(()) => {
                    info!("wasapi cycle tick received");
                    return Ok(());
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    info!("wasapi cycle tick timeout");
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("WASAPI cycle clock disconnected".to_string());
                }
            }
        }
        Err("Timed out waiting for WASAPI render event".to_string())
    }

    pub fn run_assist_step(&mut self) -> Result<bool, String> {
        Ok(false)
    }

    pub fn channel(&mut self) -> &mut Self {
        self
    }

    pub fn set_playing(&mut self, playing: bool) {
        self.playing = playing;
    }
}

unsafe impl Send for HwDriver {}

fn start_output_stream(
    requested_device: String,
    requested_rate: u32,
    period_frames: usize,
    mode: WasapiMode,
    output_rx: Receiver<Vec<f32>>,
    cycle_tick_tx: SyncSender<()>,
) -> Result<(OutputStream, StreamInfo), String> {
    let shutdown_event = create_event("WASAPI output shutdown", true)?;
    let shutdown_event_raw = shutdown_event.0 as usize;
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let thread = thread::Builder::new()
        .name("maolan-wasapi-output".to_string())
        .spawn(move || {
            let shutdown_event = HANDLE(shutdown_event_raw as *mut c_void);
            let result = run_output_thread(
                requested_device,
                requested_rate,
                period_frames,
                mode,
                output_rx,
                cycle_tick_tx,
                ready_tx.clone(),
                shutdown_event,
            );
            if let Err(err) = &result {
                error!("WASAPI output thread failed: {err}");
                let _ = ready_tx.try_send(Err(err.clone()));
            }
        })
        .map_err(|e| format!("Failed to spawn WASAPI output thread: {e}"))?;

    match ready_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| format!("Timed out opening {}", mode.label()))?
    {
        Ok(info) => Ok((
            OutputStream {
                shutdown_event,
                thread: Some(thread),
            },
            info,
        )),
        Err(err) => {
            unsafe {
                let _ = SetEvent(shutdown_event);
            }
            let _ = thread.join();
            unsafe {
                let _ = CloseHandle(shutdown_event);
            }
            Err(err)
        }
    }
}

fn start_input_stream(
    requested_device: String,
    sample_rate: u32,
    period_frames: usize,
    mode: WasapiMode,
) -> Result<(InputStream, Receiver<Vec<f32>>, StreamInfo), String> {
    let shutdown_event = create_event("WASAPI input shutdown", true)?;
    let shutdown_event_raw = shutdown_event.0 as usize;
    let (input_tx, input_rx) = mpsc::sync_channel::<Vec<f32>>(8);
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let thread = thread::Builder::new()
        .name("maolan-wasapi-input".to_string())
        .spawn(move || {
            let shutdown_event = HANDLE(shutdown_event_raw as *mut c_void);
            let result = run_input_thread(
                requested_device,
                sample_rate,
                period_frames,
                mode,
                input_tx,
                ready_tx.clone(),
                shutdown_event,
            );
            if let Err(err) = &result {
                error!("WASAPI input thread failed: {err}");
                let _ = ready_tx.try_send(Err(err.clone()));
            }
        })
        .map_err(|e| format!("Failed to spawn WASAPI input thread: {e}"))?;

    match ready_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| format!("Timed out opening {} input", mode.label()))?
    {
        Ok(info) => Ok((
            InputStream {
                shutdown_event,
                thread: Some(thread),
            },
            input_rx,
            info,
        )),
        Err(err) => {
            unsafe {
                let _ = SetEvent(shutdown_event);
            }
            let _ = thread.join();
            unsafe {
                let _ = CloseHandle(shutdown_event);
            }
            Err(err)
        }
    }
}

fn run_output_thread(
    requested_device: String,
    requested_rate: u32,
    period_frames: usize,
    mode: WasapiMode,
    output_rx: Receiver<Vec<f32>>,
    cycle_tick_tx: SyncSender<()>,
    ready_tx: SyncSender<Result<StreamInfo, String>>,
    shutdown_event: HANDLE,
) -> Result<(), String> {
    let _com = ComApartment::new()?;
    let device = select_device(eRender, &requested_device)?;
    let client = open_client(&device, eRender, requested_rate, period_frames, mode)?;
    let render_client = unsafe {
        client
            .client
            .GetService::<IAudioRenderClient>()
            .map_err(|e| format!("Failed to get WASAPI render client: {e}"))?
    };
    unsafe {
        client
            .client
            .SetEventHandle(client.event)
            .map_err(|e| format!("Failed to set WASAPI render event: {e}"))?;
    }
    prime_output_with_silence(&client, &render_client)?;
    let info = client.info();
    unsafe {
        client
            .client
            .Start()
            .map_err(|e| format!("Failed to start WASAPI output: {e}"))?;
    }
    let _ = ready_tx.try_send(Ok(info));

    let mut pending = VecDeque::<f32>::new();
    loop {
        let handles = [shutdown_event, client.event];
        let wait = unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };
        if wait == WAIT_OBJECT_0 {
            break;
        }
        if wait.0 != WAIT_OBJECT_0.0 + 1 {
            return Err("WASAPI output wait failed".to_string());
        }

        fill_output_available(
            &client,
            &render_client,
            &output_rx,
            &cycle_tick_tx,
            &mut pending,
        )?;
    }

    unsafe {
        let _ = client.client.Stop();
    }
    Ok(())
}

fn run_input_thread(
    requested_device: String,
    sample_rate: u32,
    period_frames: usize,
    mode: WasapiMode,
    input_tx: SyncSender<Vec<f32>>,
    ready_tx: SyncSender<Result<StreamInfo, String>>,
    shutdown_event: HANDLE,
) -> Result<(), String> {
    let _com = ComApartment::new()?;
    let device = select_device(eCapture, &requested_device)?;
    let client = open_client(&device, eCapture, sample_rate, period_frames, mode)?;
    let capture_client = unsafe {
        client
            .client
            .GetService::<IAudioCaptureClient>()
            .map_err(|e| format!("Failed to get WASAPI capture client: {e}"))?
    };
    unsafe {
        client
            .client
            .SetEventHandle(client.event)
            .map_err(|e| format!("Failed to set WASAPI capture event: {e}"))?;
        client
            .client
            .Start()
            .map_err(|e| format!("Failed to start WASAPI input: {e}"))?;
    }

    let info = client.info();
    let _ = ready_tx.try_send(Ok(info));
    let chunk_samples = period_frames.saturating_mul(client.channels);
    let mut reservoir = VecDeque::<f32>::with_capacity(chunk_samples.saturating_mul(2));
    loop {
        let handles = [shutdown_event, client.event];
        let wait = unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };
        if wait == WAIT_OBJECT_0 {
            break;
        }
        if wait.0 != WAIT_OBJECT_0.0 + 1 {
            return Err("WASAPI input wait failed".to_string());
        }

        drain_input_available(&capture_client, client.channels, &mut reservoir)?;
        while reservoir.len() >= chunk_samples {
            let mut chunk = Vec::with_capacity(chunk_samples);
            for _ in 0..chunk_samples {
                if let Some(sample) = reservoir.pop_front() {
                    chunk.push(sample);
                }
            }
            let _ = input_tx.try_send(chunk);
        }
    }

    unsafe {
        let _ = client.client.Stop();
    }
    Ok(())
}

struct WasapiClient {
    client: IAudioClient,
    event: HANDLE,
    actual_buffer_frames: usize,
    sample_rate: usize,
    channels: usize,
    period_frames: usize,
    latency_frames: usize,
}

impl WasapiClient {
    fn info(&self) -> StreamInfo {
        StreamInfo {
            sample_rate: self.sample_rate,
            channels: self.channels,
            period_frames: self.period_frames,
            actual_buffer_frames: self.actual_buffer_frames,
            latency_frames: self.latency_frames,
        }
    }
}

impl Drop for WasapiClient {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.event);
        }
    }
}

fn open_client(
    device: &IMMDevice,
    flow: windows::Win32::Media::Audio::EDataFlow,
    requested_rate: u32,
    period_frames: usize,
    mode: WasapiMode,
) -> Result<WasapiClient, String> {
    let client = unsafe {
        device
            .Activate::<IAudioClient>(CLSCTX_ALL, None)
            .map_err(|e| format!("Failed to activate WASAPI client: {e}"))?
    };
    let format = build_float_mix_format(&client, requested_rate, mode)?;
    let sample_rate = format.Format.nSamplesPerSec;
    let stream_flags = match mode {
        WasapiMode::SharedLowLatency => {
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY
        }
        WasapiMode::Exclusive => AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    };

    let actual_period_frames = match mode {
        WasapiMode::SharedLowLatency => initialise_low_latency(&client, &format, period_frames)?,
        WasapiMode::Exclusive => {
            initialise_exclusive(&client, &format, period_frames, stream_flags)?
        }
    } as usize;

    let actual_buffer_frames = unsafe {
        client
            .GetBufferSize()
            .map_err(|e| format!("Failed to query WASAPI buffer size: {e}"))?
    } as usize;
    let latency_hns = unsafe { client.GetStreamLatency().unwrap_or_default() };
    let latency_frames = ref_time_to_frames(latency_hns, sample_rate) as usize;
    let event = if flow == eRender {
        create_event("WASAPI render event", false)?
    } else if flow == eCapture {
        create_event("WASAPI capture event", false)?
    } else {
        create_event("WASAPI event", false)?
    };
    let channels = format.Format.nChannels;
    warn!(
        flow = if flow == eRender { "render" } else { "capture" },
        actual_buffer_frames,
        sample_rate,
        actual_period_frames,
        channels,
        latency_frames,
        "WASAPI stream opened"
    );

    Ok(WasapiClient {
        client,
        event,
        actual_buffer_frames,
        sample_rate: sample_rate as usize,
        channels: format.Format.nChannels as usize,
        period_frames: actual_period_frames,
        latency_frames,
    })
}

fn initialise_low_latency(
    client: &IAudioClient,
    format: &WAVEFORMATEXTENSIBLE,
    period_frames: usize,
) -> Result<u32, String> {
    let client3 = client
        .cast::<IAudioClient3>()
        .map_err(|_| "WASAPI shared low-latency mode requires IAudioClient3".to_string())?;
    let mut default_period = 0;
    let mut fundamental_period = 0;
    let mut min_period = 0;
    let mut max_period = 0;
    unsafe {
        client3
            .GetSharedModeEnginePeriod(
                &format.Format,
                &mut default_period,
                &mut fundamental_period,
                &mut min_period,
                &mut max_period,
            )
            .map_err(|e| format!("Failed to query WASAPI shared low-latency periods: {e}"))?;
    }
    let period = align_low_latency_period(
        period_frames as u32,
        default_period,
        fundamental_period,
        min_period,
        max_period,
    );
    warn!(
        requested_period_frames = period_frames,
        default_period,
        fundamental_period,
        min_period,
        max_period,
        selected_period = period,
        "WASAPI shared low-latency period selection"
    );
    unsafe {
        client3
            .InitializeSharedAudioStream(
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                period,
                &format.Format,
                None,
            )
            .map_err(|e| format!("Failed to initialize WASAPI shared low-latency stream: {e}"))?;
    }
    Ok(period)
}

fn initialise_exclusive(
    client: &IAudioClient,
    format: &WAVEFORMATEXTENSIBLE,
    period_frames: usize,
    stream_flags: u32,
) -> Result<u32, String> {
    let period_hns = frames_to_ref_time(period_frames as u32, format.Format.nSamplesPerSec);
    unsafe {
        client
            .Initialize(
                AUDCLNT_SHAREMODE_EXCLUSIVE,
                stream_flags,
                period_hns,
                period_hns,
                &format.Format,
                None,
            )
            .map_err(|e| format!("Failed to initialize WASAPI exclusive stream: {e}"))?;
    }
    Ok(period_frames as u32)
}

fn build_float_mix_format(
    client: &IAudioClient,
    requested_rate: u32,
    mode: WasapiMode,
) -> Result<WAVEFORMATEXTENSIBLE, String> {
    let mix_ptr = unsafe {
        client
            .GetMixFormat()
            .map_err(|e| format!("Failed to query WASAPI mix format: {e}"))?
    };
    if mix_ptr.is_null() {
        return Err("WASAPI returned a null mix format".to_string());
    }

    let mix = unsafe { *mix_ptr };
    unsafe {
        CoTaskMemFree(Some(mix_ptr.cast::<c_void>()));
    }
    let channels = mix.nChannels.max(1);
    let sample_rate = requested_rate.max(1);
    let block_align = channels.saturating_mul(4);
    let channel_mask = if channels <= 32 {
        (1_u32 << channels) - 1
    } else {
        0
    };

    let mut format = WAVEFORMATEXTENSIBLE::default();
    format.Format.wFormatTag = WAVE_FORMAT_EXTENSIBLE as u16;
    format.Format.nChannels = channels;
    format.Format.nSamplesPerSec = sample_rate;
    format.Format.nAvgBytesPerSec = sample_rate.saturating_mul(block_align as u32);
    format.Format.nBlockAlign = block_align;
    format.Format.wBitsPerSample = 32;
    format.Format.cbSize =
        (std::mem::size_of::<WAVEFORMATEXTENSIBLE>() - std::mem::size_of::<WAVEFORMATEX>()) as u16;
    format.Samples.wValidBitsPerSample = 32;
    format.dwChannelMask = channel_mask;
    format.SubFormat = KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;

    let mut closest: *mut WAVEFORMATEX = ptr::null_mut();
    let share_mode = match mode {
        WasapiMode::SharedLowLatency => AUDCLNT_SHAREMODE_SHARED,
        WasapiMode::Exclusive => AUDCLNT_SHAREMODE_EXCLUSIVE,
    };
    let closest_out = match mode {
        WasapiMode::SharedLowLatency => Some(&mut closest as *mut *mut WAVEFORMATEX),
        WasapiMode::Exclusive => None,
    };
    let supported = unsafe { client.IsFormatSupported(share_mode, &format.Format, closest_out) };
    if !closest.is_null() {
        unsafe {
            CoTaskMemFree(Some(closest.cast::<c_void>()));
        }
    }
    if supported.is_err() {
        return Err(format!(
            "WASAPI device does not support 32-bit float at {sample_rate} Hz"
        ));
    }

    Ok(format)
}

fn fill_output_available(
    client: &WasapiClient,
    render_client: &IAudioRenderClient,
    output_rx: &Receiver<Vec<f32>>,
    cycle_tick_tx: &SyncSender<()>,
    pending: &mut VecDeque<f32>,
) -> Result<usize, String> {
    let available_frames = unsafe {
        let padding = client
            .client
            .GetCurrentPadding()
            .map_err(|e| format!("Failed to query WASAPI output padding: {e}"))?;
        client.actual_buffer_frames.saturating_sub(padding as usize)
    };
    let pending_before = pending.len();
    if available_frames == 0 {
        tracing::info!(
            available_frames,
            pending_before,
            "fill_output_available: no space"
        );
        return Ok(0);
    }

    let mut periods_received = 0usize;
    while pending.len() < available_frames.saturating_mul(client.channels) {
        match output_rx.try_recv() {
            Ok(period) => {
                let period_len = period.len();
                pending.extend(period);
                periods_received += 1;
                let tick_sent = cycle_tick_tx.try_send(()).is_ok();
                tracing::info!(
                    period_len,
                    tick_sent,
                    "fill_output_available: received period"
                );
            }
            Err(_) => break,
        }
    }

    let sample_count = available_frames.saturating_mul(client.channels);
    let buffer = unsafe {
        render_client
            .GetBuffer(available_frames as u32)
            .map_err(|e| format!("Failed to get WASAPI output buffer: {e}"))?
    };
    let dst = unsafe { std::slice::from_raw_parts_mut(buffer.cast::<f32>(), sample_count) };
    let pending_after = pending.len();
    let mut underrun = false;
    for sample in dst.iter_mut() {
        if let Some(src) = pending.pop_front() {
            *sample = src;
        } else {
            *sample = 0.0;
            underrun = true;
        }
    }
    unsafe {
        render_client
            .ReleaseBuffer(available_frames as u32, 0)
            .map_err(|e| format!("Failed to release WASAPI output buffer: {e}"))?;
    }
    tracing::info!(
        available_frames,
        channels = client.channels,
        pending_before,
        pending_after,
        periods_received,
        underrun,
        "fill_output_available: wrote buffer"
    );
    if underrun {
        debug!(available_frames, "WASAPI output underrun");
    }
    Ok(available_frames)
}

fn prime_output_with_silence(
    client: &WasapiClient,
    render_client: &IAudioRenderClient,
) -> Result<(), String> {
    let buffer = unsafe {
        render_client
            .GetBuffer(client.actual_buffer_frames as u32)
            .map_err(|e| format!("Failed to prime WASAPI output: {e}"))?
    };
    let sample_count = client.actual_buffer_frames.saturating_mul(client.channels);
    unsafe {
        ptr::write_bytes(buffer.cast::<f32>(), 0, sample_count);
        render_client
            .ReleaseBuffer(
                client.actual_buffer_frames as u32,
                AUDCLNT_BUFFERFLAGS_SILENT.0 as u32,
            )
            .map_err(|e| format!("Failed to release primed WASAPI output: {e}"))?;
    }
    Ok(())
}

fn drain_input_available(
    capture_client: &IAudioCaptureClient,
    channels: usize,
    reservoir: &mut VecDeque<f32>,
) -> Result<(), String> {
    loop {
        let mut data = ptr::null_mut();
        let mut frames = 0_u32;
        let mut flags = 0_u32;
        let result =
            unsafe { capture_client.GetBuffer(&mut data, &mut frames, &mut flags, None, None) };
        if result.is_err() || frames == 0 {
            break;
        }
        let samples = frames as usize * channels;
        if (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 {
            reservoir.extend(std::iter::repeat_n(0.0, samples));
        } else {
            let src = unsafe { std::slice::from_raw_parts(data.cast::<f32>(), samples) };
            reservoir.extend(src.iter().copied());
        }
        if (flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32) != 0 {
            debug!(frames, "WASAPI input discontinuity");
        }
        unsafe {
            capture_client
                .ReleaseBuffer(frames)
                .map_err(|e| format!("Failed to release WASAPI input buffer: {e}"))?;
        }
    }
    Ok(())
}

fn align_low_latency_period(
    requested: u32,
    default_period: u32,
    fundamental_period: u32,
    min_period: u32,
    max_period: u32,
) -> u32 {
    let requested = requested.max(min_period).min(max_period);
    if fundamental_period == 0 {
        return requested.max(default_period);
    }
    let steps = requested.div_ceil(fundamental_period);
    let aligned = steps.saturating_mul(fundamental_period);
    aligned.max(min_period).min(max_period)
}

fn frames_to_ref_time(frames: u32, sample_rate: u32) -> i64 {
    ((frames as i64) * REFTIME_PER_SEC + sample_rate as i64 - 1) / sample_rate as i64
}

fn ref_time_to_frames(ref_time: i64, sample_rate: u32) -> u32 {
    ((ref_time * sample_rate as i64 + REFTIME_PER_SEC - 1) / REFTIME_PER_SEC) as u32
}

fn create_event(label: &str, manual_reset: bool) -> Result<HANDLE, String> {
    unsafe {
        CreateEventW(None, manual_reset, false, PCWSTR::null())
            .map_err(|e| format!("Failed to create {label}: {e}"))
    }
}

struct ComApartment;

impl ComApartment {
    fn new() -> Result<Self, String> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED)
                .ok()
                .map_err(|e| format!("Failed to initialize COM: {e}"))?;
        }
        Ok(Self)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}

fn select_device(
    flow: windows::Win32::Media::Audio::EDataFlow,
    requested: &str,
) -> Result<IMMDevice, String> {
    let enumerator = unsafe {
        CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .map_err(|e| format!("Failed to create WASAPI device enumerator: {e}"))?
    };
    if requested.eq_ignore_ascii_case("default") || requested.is_empty() {
        return unsafe {
            enumerator
                .GetDefaultAudioEndpoint(flow, eConsole)
                .map_err(|e| format!("Failed to get default WASAPI device: {e}"))
        };
    }

    let devices = unsafe {
        enumerator
            .EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)
            .map_err(|e| format!("Failed to enumerate WASAPI devices: {e}"))?
    };
    find_device_in_collection(&devices, requested)
        .ok_or_else(|| format!("No matching WASAPI device for '{requested}'"))
}

fn find_device_in_collection(devices: &IMMDeviceCollection, requested: &str) -> Option<IMMDevice> {
    let requested_lc = requested.to_lowercase();
    let count = unsafe { devices.GetCount().ok()? };
    let mut fuzzy = None;
    for idx in 0..count {
        let device = unsafe { devices.Item(idx).ok()? };
        let id = device_id(&device).unwrap_or_default();
        let friendly = device_friendly_name(&device).unwrap_or_default();
        if id.eq_ignore_ascii_case(requested) || friendly.eq_ignore_ascii_case(requested) {
            return Some(device);
        }
        if fuzzy.is_none() {
            let id_lc = id.to_lowercase();
            let friendly_lc = friendly.to_lowercase();
            if id_lc.contains(&requested_lc)
                || friendly_lc.contains(&requested_lc)
                || requested_lc.contains(&friendly_lc)
            {
                fuzzy = Some(device);
            }
        }
    }
    fuzzy
}

fn strip_wasapi_prefix(device: &str) -> &str {
    device.strip_prefix(WASAPI_PREFIX).unwrap_or(device).trim()
}

fn device_id(device: &IMMDevice) -> Option<String> {
    let id = unsafe { device.GetId().ok()? };
    let value = pwstr_to_string(id);
    unsafe {
        CoTaskMemFree(Some(id.0.cast::<c_void>()));
    }
    Some(value)
}

fn device_friendly_name(device: &IMMDevice) -> Option<String> {
    let store = unsafe { device.OpenPropertyStore(STGM_READ).ok()? };
    let mut value =
        unsafe { store.GetValue(&DEVPKEY_Device_FriendlyName as *const _ as *const _) }.ok()?;
    let variant = unsafe { &value.Anonymous.Anonymous };
    if variant.vt != VT_LPWSTR {
        unsafe {
            let _ = PropVariantClear(&mut value);
        }
        return None;
    }
    let text = unsafe { pwstr_to_string(variant.Anonymous.pwszVal) };
    unsafe {
        let _ = PropVariantClear(&mut value);
    }
    Some(text)
}

fn pwstr_to_string(value: PWSTR) -> String {
    if value.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    unsafe {
        while *value.0.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(value.0, len))
    }
}

pub fn list_midi_input_devices() -> Vec<String> {
    let Ok(midi_in) = MidiInput::new("maolan-midi-list-in") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (idx, port) in midi_in.ports().iter().enumerate() {
        if let Ok(name) = midi_in.port_name(port) {
            out.push(format!("{MIDI_IN_PREFIX}{idx}:{name}"));
        }
    }
    out
}

pub fn list_midi_output_devices() -> Vec<String> {
    let Ok(midi_out) = MidiOutput::new("maolan-midi-list-out") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (idx, port) in midi_out.ports().iter().enumerate() {
        if let Ok(name) = midi_out.port_name(port) {
            out.push(format!("{MIDI_OUT_PREFIX}{idx}:{name}"));
        }
    }
    out
}

struct MidiInputDevice {
    device: String,
    connection: MidiInputConnection<()>,
}

struct MidiOutputDevice {
    device: String,
    connection: MidiOutputConnection,
}

#[derive(Default)]
pub struct MidiHub {
    inputs: Vec<MidiInputDevice>,
    outputs: Vec<MidiOutputDevice>,
    input_events: Arc<Mutex<Vec<HwMidiEvent>>>,
}

impl MidiHub {
    pub fn open_input(&mut self, device: &str) -> Result<(), String> {
        if self.inputs.iter().any(|d| d.device == device) {
            return Ok(());
        }

        let index = parse_prefixed_index(device, MIDI_IN_PREFIX)?;
        let mut midi_in = MidiInput::new("maolan-midi-in")
            .map_err(|e| format!("Failed to initialize MIDI input: {e}"))?;
        midi_in.ignore(Ignore::None);
        let ports = midi_in.ports();
        let port = ports
            .get(index)
            .ok_or_else(|| format!("MIDI input device index out of range: {index}"))?
            .clone();

        let event_device = device.to_string();
        let queue = self.input_events.clone();
        let connection = midi_in
            .connect(
                &port,
                "maolan-midi-input",
                move |_stamp, data, _| {
                    if data.is_empty() {
                        return;
                    }
                    if let Ok(mut events) = queue.lock() {
                        events.push(HwMidiEvent {
                            device: event_device.clone(),
                            event: MidiEvent::new(0, data.to_vec()),
                        });
                    }
                },
                (),
            )
            .map_err(|e| format!("Failed to open MIDI input '{device}': {e}"))?;

        self.inputs.push(MidiInputDevice {
            device: device.to_string(),
            connection,
        });
        Ok(())
    }

    pub fn open_output(&mut self, device: &str) -> Result<(), String> {
        if self.outputs.iter().any(|d| d.device == device) {
            return Ok(());
        }

        let index = parse_prefixed_index(device, MIDI_OUT_PREFIX)?;
        let midi_out = MidiOutput::new("maolan-midi-out")
            .map_err(|e| format!("Failed to initialize MIDI output: {e}"))?;
        let ports = midi_out.ports();
        let port = ports
            .get(index)
            .ok_or_else(|| format!("MIDI output device index out of range: {index}"))?
            .clone();
        let connection = midi_out
            .connect(&port, "maolan-midi-output")
            .map_err(|e| format!("Failed to open MIDI output '{device}': {e}"))?;

        self.outputs.push(MidiOutputDevice {
            device: device.to_string(),
            connection,
        });
        Ok(())
    }

    pub fn read_events_into(&mut self, out: &mut Vec<HwMidiEvent>) {
        out.clear();
        let Ok(mut queue) = self.input_events.lock() else {
            return;
        };
        out.extend(queue.drain(..));
    }

    pub fn write_events(&mut self, events: &[HwMidiEvent]) {
        if events.is_empty() {
            return;
        }
        for output in &mut self.outputs {
            for event in events {
                if event.device != output.device || event.event.data.is_empty() {
                    continue;
                }
                if let Err(err) = output.connection.send(&event.event.data) {
                    error!("MIDI write error on {}: {}", output.device, err);
                    break;
                }
            }
        }
    }

    pub fn write_events_blocking(&mut self, events: &[HwMidiEvent], _timeout: Duration) {
        self.write_events(events);
    }

    pub fn close_all(&mut self) {
        while let Some(input) = self.inputs.pop() {
            let _ = input.connection.close();
        }
        while let Some(output) = self.outputs.pop() {
            let _ = output.connection.close();
        }
    }

    pub fn output_devices(&self) -> Vec<String> {
        self.outputs
            .iter()
            .map(|output| output.device.clone())
            .collect()
    }
}

impl Drop for HwDriver {
    fn drop(&mut self) {
        self.close_fds();
    }
}

impl Drop for MidiHub {
    fn drop(&mut self) {
        while let Some(input) = self.inputs.pop() {
            let _ = input.connection.close();
        }
        while let Some(output) = self.outputs.pop() {
            let _ = output.connection.close();
        }
    }
}

fn parse_prefixed_index(device: &str, prefix: &str) -> Result<usize, String> {
    let rest = device
        .strip_prefix(prefix)
        .ok_or_else(|| format!("Unsupported MIDI device id '{device}'"))?;
    let index_str = rest.split(':').next().unwrap_or("");
    index_str
        .parse::<usize>()
        .map_err(|_| format!("Invalid MIDI device id '{device}'"))
}

impl traits::HwWorkerDriver for HwDriver {
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
        self.close_fds();
    }

    fn run_cycle_for_worker(&mut self) -> Result<(), String> {
        self.run_cycle()
    }

    fn run_assist_step_for_worker(&mut self) -> Result<bool, String> {
        self.run_assist_step()
    }
}

impl traits::HwDevice for HwDriver {
    fn input_channels(&self) -> usize {
        self.input_channels()
    }

    fn output_channels(&self) -> usize {
        self.output_channels()
    }

    fn sample_rate(&self) -> i32 {
        self.sample_rate()
    }

    fn latency_ranges(&self) -> ((usize, usize), (usize, usize)) {
        (
            (self.input_latency_frames, self.input_latency_frames),
            (self.output_latency_frames, self.output_latency_frames),
        )
    }
}

impl traits::HwMidiHub for MidiHub {
    fn read_events_into(&mut self, out: &mut Vec<HwMidiEvent>) {
        self.read_events_into(out);
    }

    fn write_events(&mut self, events: &[HwMidiEvent]) {
        self.write_events(events);
    }
}
