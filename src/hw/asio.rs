//! Windows ASIO backend on top of the pure-Rust `maolan-asio` host crate.
//!
//! Callback-driven like CoreAudio: the engine's cycle is paced by the ASIO
//! buffer-switch callback consuming interleaved f32 periods from bounded
//! rtrb SPSC rings (backpressure mirrors `wasapi::HwDriver`). The producer
//! half of the output ring lives in this driver; the consumer half lives in
//! the ASIO callback. For input the halves are swapped. Tiny mutex+condvar
//! gates cover wake/backpressure signalling only; sample data itself is
//! moved through the lock-free rings.
//!
//! # Threading model (imposed by COM)
//!
//! ASIO drivers are in-process COM servers initialized in a
//! single-threaded apartment, and the `maolan_asio::Driver`/`Stream` are
//! bound to the thread that created them (most drivers additionally require
//! every vtable call on that thread). The engine's HwWorkerDriver, by
//! contrast, shuttles the driver across worker threads for every cycle. To
//! reconcile the two, a dedicated `maolan-asio` thread owns the COM guard,
//! the opened driver, and the stream for the driver's whole lifetime:
//!
//! 1. The constructor spawns that thread and blocks on an open report.
//! 2. The thread initializes COM, opens the driver selected by the device
//!    string (`asio:<driver name>`), creates the SPSC rings, installs the
//!    buffer-switch callback (which only ever touches the rings), starts
//!    the stream, and ships the ring halves back in the report.
//!    Device opening waits for the first callback before reporting readiness.
//! 3. `run_cycle` (on whatever worker thread the engine assigns) drains the
//!    input ring and fills the output ring — no COM, no driver pointers,
//!    so the struct is naturally `Send`.
//! 4. The thread then polls `Stream::take_events()` (OVERLOAD -> xrun
//!    counter; reset/resync/size/rate changes are logged) until `close_fds`
//!    orders a shutdown, at which point the stream is stopped and disposed
//!    and COM is uninitialized, all on the owning thread.
//!
//! # One stream per process
//!
//! The ASIO callback table has no user-data pointer, so `maolan-asio`
//! allows only one stream per process. The engine opens exactly one audio
//! device, which keeps the backend within that limit; opening a second
//! ASIO device fails with `StreamError::Busy` while the first is alive.
//!
//! # Real-time rules (the buffer-switch callback must never block)
//!
//! The callback runs on the driver's own real-time thread; blocking it for
//! roughly a buffer period or more stalls drivers like ASIO4ALL, which then
//! manifests as the producer-side "Timed out waiting for ASIO buffer
//! switch" watchdog. Guarantees:
//!
//! - Output: pops under the gate mutex (held only across ring pops and the
//!   copy into the driver's buffers) and, when starved while playing, waits
//!   on the condvar for at most [`STARVED_GRACE`] — never a full period.
//! - Input: pushes lock-free; when the ring cannot hold a whole switch,
//!   the oldest complete frames are dropped in ONE bounded mutex
//!   acquisition (newest data wins, frame alignment preserved). There is
//!   no per-sample locking and no retry loop.
//! - No allocation in steady state (the scratch Vecs are pre-reserved).
//! - `run_cycle` never holds the input consumer mutex across an allocation
//!   (it drains into a reusable scratch) and never blocks while holding
//!   either mutex; `queue_output_period`'s condvar wait releases the gate,
//!   so the callback's and the producer's waits stay mutually exclusive
//!   (empty ring vs. full ring) with no lost wakeup.
//!
//! # Requested vs. negotiated sizing
//!
//! The rings and scratch buffers are created before `Driver::stream` and
//! are therefore sized from the *requested* period, while the callback
//! paces by the *negotiated* `ctx.frames()`. Every `frames`-dependent
//! consumer (`run_cycle`, `output_period`, the port surface) uses
//! `period_frames` from the open report, i.e. the negotiated size, so a
//! negotiation that comes out smaller than requested (e.g. ASIO4ALL
//! clamping 4096 to its 2048 maximum) is fully consistent — the rings are
//! merely over-capacious. A negotiation that comes out *larger* than
//! requested (only possible with a stepped buffer-size granularity) can
//! need more than one switch per engine cycle when `nperiods == 1`; it
//! still cannot deadlock (the callback drains the ring every switch), but
//! `nperiods >= 2` (the engine default) should be preferred.
//!
//! # Limitations
//!
//! - Control panel: `Driver::control_panel()` is not wired to a UI action
//!   yet (most drivers also require a valid HWND at `init`, which
//!   `maolan-asio` does not pass).
//! - Reset requests (`kAsioResetRequest`) are logged but not acted on; the
//!   engine keeps streaming with the negotiated parameters.
//! - ASIO is inherently exclusive: the `exclusive` HwOptions flag has no
//!   meaning here and is ignored.
//! - Latency reporting uses the driver's `getLatencies()` values plus the
//!   user-supplied `hw_opts` offsets; run `maolan-calibrate` to correct
//!   the residual per-driver difference.

use crate::audio::io::AudioIO;
use crate::hw::{common, options::HwOptions, traits};
use rtrb::{Consumer, Producer, RingBuffer};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use tracing::{debug, warn};

pub(crate) const ASIO_PREFIX: &str = "asio:";

/// Poll interval for `Stream::take_events()` on the ASIO owner thread.
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Driver startup can take substantially longer than a running buffer switch:
/// ASIO4ALL on USB hardware takes over a second at large periods. Do not start
/// the steady-state watchdog or calibration until callbacks actually arrive.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the buffer-switch callback may block waiting for engine data
/// while the transport is playing, before falling back to silence.
///
/// Keep this independent of period size so a starved engine does not hold
/// the driver's callback thread for an entire audio period.
const STARVED_GRACE: Duration = Duration::from_millis(4);

struct OutputGate {
    stopped: bool,
    started: bool,
}

struct OutputShared {
    gate: Mutex<OutputGate>,
    condvar: Condvar,
    playing: AtomicBool,
    xruns: AtomicUsize,
    idle_timeout: Duration,
}

struct InputShared {
    consumer: Mutex<Consumer<f32>>,
    channels: usize,
    /// Full-period drops caused by input overflow (oldest frames dropped).
    overflow_drops: AtomicUsize,
    xruns: AtomicUsize,
}

enum ControlMsg {
    Shutdown,
}

/// Opened-stream facts and the engine-side ring halves, reported back from
/// the ASIO owner thread once the stream is running.
struct OpenReport {
    output_producer: Producer<f32>,
    output_shared: Arc<OutputShared>,
    input_shared: Option<Arc<InputShared>>,
    sample_rate: usize,
    period_frames: usize,
    input_channels: usize,
    output_channels: usize,
    input_latency_frames: usize,
    output_latency_frames: usize,
}

/// Stream parameters handed to the ASIO owner thread.
struct StreamParams {
    requested_rate: f64,
    requested_period: usize,
    nperiods: usize,
    min_inputs: usize,
    min_outputs: usize,
}

/// Number of channels to open on one stream direction, mirroring the ALSA
/// convention for `HwOptions`: an explicit request wins (capped by what the
/// device exposes); zero opens the stereo default, capped by the device.
fn choose_channels(device_channels: usize, requested: usize) -> usize {
    if requested > 0 {
        return requested.min(device_channels);
    }
    device_channels.min(2)
}

fn strip_asio_prefix(device: &str) -> &str {
    device.strip_prefix(ASIO_PREFIX).unwrap_or(device).trim()
}

/// Resolve a user-supplied driver name (already prefix-stripped) to an
/// installed ASIO driver: exact name match first, then substring.
fn resolve_driver_info(requested: &str) -> Result<maolan_asio::DriverInfo, String> {
    let drivers =
        maolan_asio::drivers().map_err(|e| format!("Failed to enumerate ASIO drivers: {e}"))?;
    if requested.is_empty() || requested.eq_ignore_ascii_case("default") {
        return drivers
            .into_iter()
            .next()
            .ok_or_else(|| "No ASIO drivers installed".to_string());
    }
    let requested_lc = requested.to_lowercase();
    let mut fuzzy = None;
    for info in drivers {
        if info.name.eq_ignore_ascii_case(requested) {
            return Ok(info);
        }
        if fuzzy.is_none() && info.name.to_lowercase().contains(&requested_lc) {
            fuzzy = Some(info);
        }
    }
    fuzzy.ok_or_else(|| format!("No matching ASIO driver for '{requested}'"))
}

/// Public ASIO driver descriptor for device listings.
pub struct AsioDeviceDescriptor {
    /// Engine device id: `asio:<driver name>`.
    pub id: String,
    /// Human-readable label (`DriverInfo` display: name plus description).
    pub label: String,
}

/// Enumerate installed ASIO drivers as engine device ids. Empty on any
/// failure (including non-Windows hosts, where the registry list is empty).
pub fn discover_asio_audio_devices() -> Vec<AsioDeviceDescriptor> {
    maolan_asio::drivers()
        .map(|drivers| {
            drivers
                .into_iter()
                .map(|info| AsioDeviceDescriptor {
                    id: format!("{ASIO_PREFIX}{}", info.name),
                    label: info.to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Whether a device string selects the ASIO backend.
pub(crate) fn is_asio_device_id(device: &str) -> bool {
    device.starts_with(ASIO_PREFIX)
}

/// How long `queue_output_period` waits for the buffer-switch callback to
/// drain the ring before declaring the device stalled: at least two
/// negotiated periods (a healthy driver consumes one period per switch),
/// with a 500 ms floor. This wait is a dead-driver detector, not a pacing
/// mechanism — a live driver under load (debug build, engine hiccup) can
/// pause for tens of milliseconds per cycle, so the floor must comfortably
/// outlast worst-case scheduling stalls.
fn queue_wait_timeout(period_frames: usize, sample_rate: usize) -> Duration {
    let period_ms = (period_frames as u64).saturating_mul(1_000) / sample_rate.max(1) as u64;
    Duration::from_millis((period_ms.max(1) * 2).max(500))
}

/// Wait on a predicate protected by the callback's gate so a first switch
/// arriving before this wait cannot be missed. This runs only on the owner
/// thread; the callback never waits for startup acknowledgement.
fn wait_for_stream_start(shared: &OutputShared, timeout: Duration) -> Result<(), String> {
    let gate = shared
        .gate
        .lock()
        .map_err(|_| "ASIO output gate poisoned".to_string())?;
    let (gate, _) = shared
        .condvar
        .wait_timeout_while(gate, timeout, |gate| !gate.started)
        .map_err(|_| "ASIO output gate poisoned".to_string())?;
    if !gate.started {
        return Err("Timed out waiting for ASIO stream startup (no buffer switch)".to_string());
    }
    Ok(())
}

/// Body of the dedicated ASIO owner thread: owns the COM guard, the driver,
/// and the stream; bridges them to the engine through SPSC rings.
fn asio_thread_main(
    info: maolan_asio::DriverInfo,
    params: StreamParams,
    report_tx: SyncSender<Result<OpenReport, String>>,
    control_rx: Receiver<ControlMsg>,
) {
    if let Err(e) = asio_thread_run(info, params, &report_tx, control_rx)
        && report_tx.send(Err(e)).is_err()
    {
        // The constructor went away; nothing left to notify.
    }
}

fn asio_thread_run(
    info: maolan_asio::DriverInfo,
    params: StreamParams,
    report_tx: &SyncSender<Result<OpenReport, String>>,
    control_rx: Receiver<ControlMsg>,
) -> Result<(), String> {
    let guard = maolan_asio::com::initialize()
        .map_err(|e| format!("Failed to initialize COM for ASIO: {e:?}"))?;
    let mut driver = maolan_asio::Driver::open(&info, &guard)
        .map_err(|e| format!("Failed to open ASIO driver '{}': {e}", info.name))?;
    let (device_inputs, device_outputs) = driver
        .channels()
        .map_err(|e| format!("ASIO driver '{}' channel query failed: {e}", info.name))?;
    let num_inputs = choose_channels(device_inputs as usize, params.min_inputs);
    let num_outputs = choose_channels(device_outputs as usize, params.min_outputs);
    if num_outputs == 0 {
        return Err(format!(
            "ASIO driver '{}' reports no output channels",
            info.name
        ));
    }
    let (driver_in_latency, driver_out_latency) = driver.latencies().unwrap_or((0, 0));

    // The rings carry sample streams independent of the negotiated period
    // size, so they are sized from the request; the callback paces itself
    // by the negotiated `ctx.frames()`.
    let out_samples = params
        .requested_period
        .saturating_mul(num_outputs)
        .max(num_outputs);
    let out_capacity = params
        .nperiods
        .saturating_mul(out_samples)
        .max(out_samples)
        .max(1);
    let (output_producer, mut output_consumer) = RingBuffer::<f32>::new(out_capacity);
    let input = if num_inputs > 0 {
        let in_samples = params
            .requested_period
            .saturating_mul(num_inputs)
            .max(num_inputs);
        let in_capacity = in_samples.saturating_mul(4).max(in_samples).max(1);
        let (input_producer, input_consumer) = RingBuffer::<f32>::new(in_capacity);
        let shared = Arc::new(InputShared {
            consumer: Mutex::new(input_consumer),
            channels: num_inputs,
            overflow_drops: AtomicUsize::new(0),
            xruns: AtomicUsize::new(0),
        });
        Some((input_producer, shared))
    } else {
        None
    };
    let output_shared = Arc::new(OutputShared {
        gate: Mutex::new(OutputGate {
            stopped: false,
            started: false,
        }),
        condvar: Condvar::new(),
        playing: AtomicBool::new(false),
        xruns: AtomicUsize::new(0),
        idle_timeout: Duration::from_millis(2),
    });

    let stream_config = maolan_asio::StreamConfig {
        sample_rate: params.requested_rate,
        buffer_size: params.requested_period,
        num_inputs,
        num_outputs,
    };
    let callback_shared = output_shared.clone();
    let report_input_shared = input.as_ref().map(|(_, shared)| shared.clone());
    let mut callback_input = input;
    let mut out_scratch: Vec<f32> = Vec::with_capacity(out_samples);
    let mut in_scratch: Vec<f32> = Vec::with_capacity(
        params
            .requested_period
            .saturating_mul(num_inputs)
            .max(num_inputs),
    );
    let mut stream = driver
        .stream(&stream_config, move |mut ctx| {
            let frames = ctx.frames();
            // Outputs: backpressured drain of the interleaved ring into the
            // per-channel driver buffers. While the transport is playing,
            // block briefly for the engine to catch up (a small fixed
            // grace — the callback must return well within the buffer
            // period or the driver stalls); once stopped (or at startup),
            // only wait a short jitter grace and then emit silence.
            let out_shared = &callback_shared;
            let channels = ctx.num_outputs();
            let needed = frames.saturating_mul(channels);
            out_scratch.clear();
            let popped = {
                let Ok(mut gate) = out_shared.gate.lock() else {
                    return;
                };
                gate.started = true;
                loop {
                    while out_scratch.len() < needed
                        && let Ok(sample) = output_consumer.pop()
                    {
                        out_scratch.push(sample);
                    }
                    if out_scratch.len() >= needed || gate.stopped {
                        break;
                    }
                    let wait = if out_shared.playing.load(Ordering::Acquire) {
                        STARVED_GRACE
                    } else {
                        out_shared.idle_timeout
                    };
                    if wait.is_zero() {
                        break;
                    }
                    let (guard, timeout) = match out_shared.condvar.wait_timeout(gate, wait) {
                        Ok(pair) => pair,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    gate = guard;
                    if timeout.timed_out() {
                        break;
                    }
                }
                out_scratch.len()
            };
            let mut copied = 0_usize;
            for frame in 0..frames {
                for ch in 0..channels {
                    let sample = if copied < popped {
                        out_scratch.get(copied).copied().unwrap_or(0.0)
                    } else {
                        0.0
                    };
                    if let Some(dst) = ctx.output(ch).get_mut(frame) {
                        *dst = sample;
                    }
                    copied += 1;
                }
            }
            if copied < needed {
                out_shared.xruns.fetch_add(1, Ordering::Relaxed);
            }
            // Wake a producer blocked on a full ring.
            out_shared.condvar.notify_all();

            // Inputs: interleave the per-channel driver buffers into the
            // ring. The callback must never block: if the ring cannot hold
            // the whole switch, drop the oldest complete frames in ONE
            // bounded mutex acquisition (keeping the newest data and frame
            // alignment), then push without any retry loop. A single
            // producer (this callback) and the room just made guarantee the
            // pushes succeed; the residual failures only defend against a
            // poisoned mutex and drop the newest sample rather than wait.
            if let Some((producer, in_shared)) = callback_input.as_mut()
                && in_shared.channels > 0
            {
                in_scratch.clear();
                for frame in 0..frames {
                    for ch in 0..in_shared.channels {
                        in_scratch.push(ctx.input(ch).get(frame).copied().unwrap_or(0.0));
                    }
                }
                let deficit = in_scratch.len().saturating_sub(producer.slots());
                if deficit > 0 {
                    let dropped_frames = deficit.div_ceil(in_shared.channels);
                    let dropped_samples = dropped_frames.saturating_mul(in_shared.channels);
                    if let Ok(mut consumer) = in_shared.consumer.lock() {
                        for _ in 0..dropped_samples {
                            if consumer.pop().is_err() {
                                break;
                            }
                        }
                    }
                    in_shared
                        .overflow_drops
                        .fetch_add(dropped_frames, Ordering::Relaxed);
                    in_shared.xruns.fetch_add(dropped_frames, Ordering::Relaxed);
                }
                for sample in &in_scratch {
                    if producer.push(*sample).is_err() {
                        // Unreachable while the mutex above succeeded; drop
                        // the newest sample rather than block the switch.
                        in_shared.overflow_drops.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        })
        .map_err(|e| format!("Failed to create ASIO stream on '{}': {e}", info.name))?;
    let period_frames = stream.buffer_size;
    let sample_rate = stream.current_sample_rate().round().max(1.0) as usize;
    stream
        .start()
        .map_err(|e| format!("Failed to start ASIO stream on '{}': {e}", info.name))?;

    wait_for_stream_start(&output_shared, STARTUP_TIMEOUT)?;

    let report = OpenReport {
        output_producer,
        output_shared: output_shared.clone(),
        input_shared: report_input_shared,
        sample_rate,
        period_frames,
        input_channels: num_inputs,
        output_channels: num_outputs,
        input_latency_frames: driver_in_latency.max(0) as usize,
        output_latency_frames: driver_out_latency.max(0) as usize,
    };
    if report_tx.send(Ok(report)).is_err() {
        // Constructor went away mid-open; unwind and dispose on this
        // thread (the stream drop below stops and disposes).
        return Ok(());
    }

    // Event pump: the buffer-switch callback runs on the driver's own
    // thread; here we only drain accumulated driver events until shutdown.
    loop {
        match control_rx.recv_timeout(EVENT_POLL_INTERVAL) {
            Ok(ControlMsg::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let events = stream.take_events();
                if events & maolan_asio::stream::event::OVERLOAD != 0 {
                    output_shared.xruns.fetch_add(1, Ordering::Relaxed);
                }
                if events & maolan_asio::stream::event::RESET_REQUESTED != 0 {
                    warn!(
                        driver = info.name,
                        "ASIO driver requested a reset; continuing with current parameters"
                    );
                }
                if events & maolan_asio::stream::event::BUFFER_SIZE_CHANGED != 0 {
                    warn!(
                        driver = info.name,
                        "ASIO driver changed its buffer size; continuing with current parameters"
                    );
                }
                if events & maolan_asio::stream::event::RESYNC_REQUESTED != 0 {
                    debug!(driver = info.name, "ASIO resync request");
                }
                if events & maolan_asio::stream::event::SAMPLE_RATE_CHANGED != 0 {
                    warn!(
                        driver = info.name,
                        rate = stream.current_sample_rate(),
                        "ASIO sample rate changed mid-stream"
                    );
                }
            }
        }
    }
    let _ = stream.stop();
    Ok(())
}

/// Windows ASIO `HwWorkerDriver` implementation.
pub struct HwDriver {
    control_tx: Option<mpsc::Sender<ControlMsg>>,
    owner_thread: Option<JoinHandle<()>>,
    output_producer: Producer<f32>,
    output_shared: Arc<OutputShared>,
    input_shared: Option<Arc<InputShared>>,
    audio_ins: Vec<Arc<AudioIO>>,
    audio_outs: Vec<Arc<AudioIO>>,
    input_queue: Vec<f32>,
    /// Reusable ring-drain scratch: draining never allocates the retained
    /// queue while the consumer mutex is held.
    input_drain: Vec<f32>,
    /// Reusable interleaved output period; fed to the ring without any
    /// intermediate VecDeque copy.
    output_period: Vec<f32>,
    output_gain_linear: f32,
    output_balance: f32,
    sample_rate: usize,
    period_frames: usize,
    input_channels: usize,
    output_channels: usize,
    input_latency_frames: usize,
    output_latency_frames: usize,
    playing: bool,
    closed: bool,
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
        let requested_name = strip_asio_prefix(device);
        let info = resolve_driver_info(requested_name)?;
        if let Some(input_device) = input_device {
            let input_name = strip_asio_prefix(input_device);
            if !input_name.eq_ignore_ascii_case(requested_name) {
                debug!(
                    device,
                    input_device,
                    "ASIO is a single full-duplex device; ignoring the separate input device"
                );
            }
        }
        let params = StreamParams {
            requested_rate: f64::from(rate.max(1)),
            requested_period: options.period_frames.max(1),
            nperiods: options.nperiods.max(1),
            min_inputs: options.input_channels,
            min_outputs: options.output_channels,
        };
        let (report_tx, report_rx) = mpsc::sync_channel::<Result<OpenReport, String>>(1);
        let (control_tx, control_rx) = mpsc::channel::<ControlMsg>();
        let owner_thread = std::thread::Builder::new()
            .name("maolan-asio".to_string())
            .spawn(move || asio_thread_main(info, params, report_tx, control_rx))
            .map_err(|e| format!("Failed to spawn ASIO thread: {e}"))?;
        let report = match report_rx.recv() {
            Ok(Ok(report)) => report,
            Ok(Err(e)) => {
                let _ = owner_thread.join();
                return Err(e);
            }
            Err(_) => {
                let _ = owner_thread.join();
                return Err("ASIO thread terminated during device open".to_string());
            }
        };

        let audio_outs = (0..report.output_channels)
            .map(|_| Arc::new(AudioIO::new(report.period_frames)))
            .collect();
        let audio_ins = (0..report.input_channels)
            .map(|_| Arc::new(AudioIO::new(report.period_frames)))
            .collect();
        let input_latency_frames = report
            .input_latency_frames
            .saturating_add(options.input_latency_frames);
        let output_latency_frames = report
            .output_latency_frames
            .saturating_add(options.output_latency_frames);

        debug!(
            driver = requested_name,
            input_channels = report.input_channels,
            output_channels = report.output_channels,
            sample_rate = report.sample_rate,
            period_frames = report.period_frames,
            "ASIO backend opened"
        );

        Ok(Self {
            control_tx: Some(control_tx),
            owner_thread: Some(owner_thread),
            output_producer: report.output_producer,
            output_shared: report.output_shared,
            input_shared: report.input_shared,
            audio_ins,
            audio_outs,
            input_queue: Vec::new(),
            input_drain: Vec::with_capacity(
                report
                    .period_frames
                    .saturating_mul(report.input_channels.max(1))
                    .saturating_mul(4),
            ),
            output_period: vec![
                0.0_f32;
                report.period_frames.saturating_mul(report.output_channels)
            ],
            output_gain_linear: 1.0,
            output_balance: 0.0,
            sample_rate: report.sample_rate,
            period_frames: report.period_frames,
            input_channels: report.input_channels,
            output_channels: report.output_channels,
            input_latency_frames,
            output_latency_frames,
            playing: false,
            closed: false,
            stop_requested: Arc::new(AtomicBool::new(false)),
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
        let input_frames = self.period_frames;
        let input_channels = self.input_channels.max(1);
        let needed_samples = input_frames.saturating_mul(input_channels);
        if let Some(shared) = &self.input_shared {
            // Drain into a reusable scratch so no allocation of the
            // retained queue (or anything else) happens while the consumer
            // mutex is held — the callback takes the same lock on overflow
            // and must never wait on a heap lock.
            self.input_drain.clear();
            if let Ok(mut consumer) = shared.consumer.lock() {
                while let Ok(sample) = consumer.pop() {
                    self.input_drain.push(sample);
                }
            }
            self.input_queue.extend_from_slice(&self.input_drain);
            // Keep at most the freshest period: a device that outpaces the
            // engine must not grow the queue without bound, and the arena
            // must always see recent input, not an ever-growing delay.
            if self.input_queue.len() > needed_samples {
                let dropped_frames = (self.input_queue.len() - needed_samples) / input_channels;
                self.input_queue
                    .drain(..self.input_queue.len() - needed_samples);
                shared.xruns.fetch_add(dropped_frames, Ordering::Relaxed);
            }
        }

        let have_samples = self.input_queue.len();
        let consume_frames = (have_samples / input_channels).min(input_frames);
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
        self.output_period.fill(0.0);
        if self.playing
            && let Some(slot) = &self.plan_slot
        {
            let plan = slot.load();
            crate::hw::ports::write_interleaved_from_arena(
                &plan,
                frames,
                gain,
                balance,
                |ch, frame, sample| {
                    let idx = frame * channels + ch;
                    if let Some(dst) = self.output_period.get_mut(idx) {
                        *dst = sample;
                    }
                },
            );
        }

        let period = std::mem::take(&mut self.output_period);
        let result = self.queue_output_period(&period);
        self.output_period = period;
        result
    }

    fn queue_output_period(&mut self, period: &[f32]) -> Result<(), String> {
        let shared = &self.output_shared;
        let producer = &mut self.output_producer;
        let mut gate = shared
            .gate
            .lock()
            .map_err(|_| "ASIO output gate poisoned".to_string())?;
        let mut offset = 0_usize;
        while offset < period.len() {
            if self.stop_requested.load(Ordering::Acquire) || gate.stopped {
                return Ok(());
            }
            while offset < period.len() {
                match producer.push(period[offset]) {
                    Ok(()) => offset += 1,
                    // Ring full: the buffer-switch callback paces the engine
                    // cycle; wait for it to drain below capacity.
                    Err(_) => break,
                }
            }
            if offset >= period.len() {
                break;
            }
            let (guard, timeout) = shared
                .condvar
                .wait_timeout(
                    gate,
                    queue_wait_timeout(self.period_frames, self.sample_rate),
                )
                .map_err(|_| "ASIO output gate poisoned".to_string())?;
            gate = guard;
            if timeout.timed_out() {
                return Err("Timed out waiting for ASIO buffer switch".to_string());
            }
        }
        shared.condvar.notify_all();
        Ok(())
    }

    pub fn run_assist_step(&mut self) -> Result<bool, String> {
        Ok(false)
    }

    pub fn channel(&mut self) -> &mut Self {
        self
    }

    pub fn set_playing(&mut self, playing: bool) {
        self.playing = playing;
        // The buffer-switch callback uses this flag to decide whether it may
        // block waiting for engine data (playing) or must fall back to
        // silence immediately (stopped), so the driver's callback thread
        // never overruns the buffer deadline on a starved queue.
        self.output_shared.playing.store(playing, Ordering::Release);
    }

    pub fn close_fds(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.stop_requested.store(true, Ordering::Release);
        if let Ok(mut gate) = self.output_shared.gate.lock() {
            gate.stopped = true;
        }
        self.output_shared.condvar.notify_all();
        if let Some(tx) = self.control_tx.take() {
            let _ = tx.send(ControlMsg::Shutdown);
        }
        if let Some(handle) = self.owner_thread.take() {
            let _ = handle.join();
        }
    }

    pub fn latency_ranges(&self) -> ((usize, usize), (usize, usize)) {
        (
            (self.input_latency_frames, self.input_latency_frames),
            (self.output_latency_frames, self.output_latency_frames),
        )
    }

    pub fn xrun_count(&self) -> u64 {
        let output = self.output_shared.xruns.load(Ordering::Relaxed) as u64;
        let input = self
            .input_shared
            .as_ref()
            .map_or(0, |shared| shared.xruns.load(Ordering::Relaxed) as u64);
        output + input
    }
}

impl Drop for HwDriver {
    fn drop(&mut self) {
        self.close_fds();
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
        Some(self.xrun_count())
    }

    fn close_fds(&mut self) {
        self.close_fds();
    }

    fn set_playing(&mut self, playing: bool) {
        self.set_playing(playing)
    }

    fn set_output_gain_balance(&mut self, gain: f32, balance: f32) {
        self.set_output_gain_balance(gain, balance)
    }

    fn run_cycle_for_worker(&mut self) -> Result<(), String> {
        self.channel().run_cycle()
    }

    fn run_assist_step_for_worker(&mut self) -> Result<bool, String> {
        self.run_assist_step()
    }

    fn set_plan_slot(&mut self, slot: Arc<crate::render_plan::PlanSlot>) {
        self.set_plan_slot(slot);
    }

    fn request_stop(&mut self) {
        self.stop_requested.store(true, Ordering::Release);
        self.close_fds();
    }

    fn stop_signaller(&self) -> Option<Arc<AtomicBool>> {
        Some(self.stop_requested.clone())
    }
}

crate::impl_hw_device_for_driver!(HwDriver);

#[cfg(test)]
mod tests {
    use super::*;

    fn startup_shared(started: bool) -> Arc<OutputShared> {
        Arc::new(OutputShared {
            gate: Mutex::new(OutputGate {
                stopped: false,
                started,
            }),
            condvar: Condvar::new(),
            playing: AtomicBool::new(false),
            xruns: AtomicUsize::new(0),
            idle_timeout: Duration::ZERO,
        })
    }

    #[test]
    fn startup_accepts_callback_before_wait() {
        let shared = startup_shared(true);
        assert!(wait_for_stream_start(&shared, Duration::ZERO).is_ok());
    }

    #[test]
    fn startup_reports_missing_callback() {
        let shared = startup_shared(false);
        assert_eq!(
            wait_for_stream_start(&shared, Duration::ZERO).unwrap_err(),
            "Timed out waiting for ASIO stream startup (no buffer switch)"
        );
    }

    #[test]
    fn startup_allows_callback_after_running_watchdog_interval() {
        let shared = startup_shared(false);
        let callback_shared = shared.clone();
        let callback = std::thread::spawn(move || {
            std::thread::sleep(queue_wait_timeout(1024, 48_000) + Duration::from_millis(100));
            callback_shared.gate.lock().unwrap().started = true;
            callback_shared.condvar.notify_all();
        });
        let result = wait_for_stream_start(&shared, STARTUP_TIMEOUT);
        callback.join().unwrap();
        assert!(result.is_ok());
    }

    #[test]
    fn strip_prefix_only_when_present() {
        assert_eq!(strip_asio_prefix("asio:ASIO4ALL v2"), "ASIO4ALL v2");
        assert_eq!(strip_asio_prefix("ASIO4ALL v2"), "ASIO4ALL v2");
        assert_eq!(strip_asio_prefix("asio:"), "");
    }

    #[test]
    fn asio_device_id_detection() {
        assert!(is_asio_device_id("asio:Foo"));
        assert!(!is_asio_device_id("wasapi:Foo"));
        assert!(!is_asio_device_id("default"));
    }

    #[test]
    fn choose_channels_mirrors_alsa_convention() {
        // Explicit request wins, capped by the device.
        assert_eq!(choose_channels(8, 4), 4);
        assert_eq!(choose_channels(2, 4), 2);
        // Zero requests the stereo default, capped by the device.
        assert_eq!(choose_channels(8, 0), 2);
        assert_eq!(choose_channels(1, 0), 1);
        assert_eq!(choose_channels(0, 0), 0);
    }

    #[test]
    fn queue_wait_timeout_scales_with_period() {
        // Below the floor: everyday periods get the fixed 500 ms floor.
        assert_eq!(queue_wait_timeout(512, 48_000), Duration::from_millis(500));
        assert_eq!(queue_wait_timeout(8192, 48_000), Duration::from_millis(500));
        // Rate is part of the computation: 2 * 42.6 ms stays floored.
        assert_eq!(queue_wait_timeout(2048, 96_000), Duration::from_millis(500));
        // Huge periods: 2 * 341 ms exceeds the floor and wins.
        assert_eq!(
            queue_wait_timeout(16_384, 48_000),
            Duration::from_millis(682)
        );
    }
}
