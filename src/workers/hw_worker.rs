use crate::{
    hw::traits::{HwMidiHub, HwWorkerDriver},
    message::{HwMidiEvent, Message},
};
#[cfg(unix)]
use nix::libc;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;
use tokio::sync::mpsc::{Receiver, Sender};
use tracing::error;

/// Sentinel stored in the capture-frame mirror when the backend reports no
/// capture position (or has not run a cycle yet).
pub(crate) const CAPTURE_FRAME_UNKNOWN: i64 = -1;

pub trait Backend: Send + Sync + 'static {
    type Driver: HwWorkerDriver + Send + 'static;
    type MidiHub: HwMidiHub + Send + 'static;

    const LABEL: &'static str;
    const WORKER_THREAD_NAME: &'static str;
    const ASSIST_THREAD_NAME: &'static str;
    const ASSIST_AUTONOMOUS_ENV: &'static str;
    const ASSIST_AUTONOMOUS_DEFAULT: bool = false;
    const CYCLE_ON_WORKER_WHEN_ASSIST_AUTONOMOUS: bool = false;
    const ASSIST_STEP_REQUIRES_REQUEST_CYCLE: bool = false;
}

#[derive(Debug)]
pub struct HwWorker<B: Backend> {
    /// Owned by the worker between cycles; shuttled to the spawn_blocking
    /// thread for the duration of each audio cycle (`None` only while a
    /// cycle is in flight, during which the message channel is not polled).
    driver: Option<B::Driver>,
    /// Shared with the RT cycle thread while inline render is armed: the
    /// cycle drains MIDI input mid-cycle (after the capture read), the
    /// worker keeps using it between cycles. Both sides only lock across
    /// cycle boundaries, so the mutex is uncontended in practice.
    midi_hub: Arc<Mutex<B::MidiHub>>,
    rx: Receiver<Message>,
    tx: Sender<Message>,
    cycle_frames: u32,
    pending_midi_out_events: Vec<HwMidiEvent>,
    pending_midi_out_sorted: bool,
    midi_stop: Arc<AtomicBool>,
    /// Mirrors the last `HWSetPlaying`; while stopped, MIDI-out events are
    /// flushed on receipt (panic All-Sound-Off must not wait for a cycle)
    /// and MIDI input is drained by a periodic timer instead of per cycle.
    playing: bool,
    /// Shared with the engine dispatcher; refreshed from the driver's
    /// `current_capture_frame` after every audio cycle. `CAPTURE_FRAME_UNKNOWN`
    /// when the backend reports nothing.
    last_xrun_count: Option<u64>,
    last_xrun_report: Option<std::time::Instant>,
    capture_frame: Arc<AtomicI64>,
    /// RT-inline render context. When armed, `TracksFinished` is the Go
    /// signal for a cycle that executes the render plan on the cycle thread;
    /// the pre-cycle MIDI-in drain moves into that cycle, so the worker
    /// skips it here.
    inline_render: Option<Arc<crate::inline_render::InlineRender>>,
}

/// How often hardware MIDI input is polled when no audio cycles are running
/// (transport stopped). While playing, input is drained every cycle and this
/// timer is only a harmless extra non-blocking read.
const MIDI_INPUT_POLL_INTERVAL: Duration = Duration::from_millis(10);

impl<B: Backend> Drop for HwWorker<B> {
    fn drop(&mut self) {
        if let Some(driver) = self.driver.as_mut() {
            driver.request_stop();
        }
        self.midi_stop.store(true, Ordering::Release);
        let mut hub = self.lock_midi_hub();
        hub.wake_input_waiter();
        hub.close_all();
        drop(hub);
        if let Some(driver) = self.driver.as_mut() {
            driver.close_fds();
        }
    }
}

#[cfg(unix)]
const RT_POLICY: i32 = libc::SCHED_FIFO;
const RT_PRIORITY_WORKER: i32 = 18;

impl<B: Backend> HwWorker<B> {
    fn configure_rt_thread(name: &str, priority: i32) -> Result<(), String> {
        #[cfg(unix)]
        {
            let thread = unsafe { libc::pthread_self() };
            #[cfg(unix)]
            let c_name = std::ffi::CString::new(name).map_err(|e| e.to_string())?;
            #[cfg(target_os = "linux")]
            unsafe {
                let _ = libc::pthread_setname_np(thread, c_name.as_ptr());
            }
            #[cfg(target_os = "macos")]
            unsafe {
                // macOS names the current thread and takes no thread handle.
                let _ = libc::pthread_setname_np(c_name.as_ptr());
            }
            #[cfg(any(target_os = "freebsd", target_os = "openbsd"))]
            unsafe {
                libc::pthread_set_name_np(thread, c_name.as_ptr());
            }

            let param = unsafe {
                let mut p = std::mem::zeroed::<libc::sched_param>();
                p.sched_priority = priority;
                p
            };
            let rc = unsafe { libc::pthread_setschedparam(thread, RT_POLICY, &param) };
            if rc != 0 {
                return Err(format!(
                    "pthread_setschedparam({}, prio {}) failed with errno {}",
                    name, priority, rc
                ));
            }

            let mut actual_policy = 0_i32;
            let mut actual_param = unsafe { std::mem::zeroed::<libc::sched_param>() };
            let rc = unsafe {
                libc::pthread_getschedparam(thread, &mut actual_policy, &mut actual_param)
            };
            if rc != 0 {
                return Err(format!(
                    "pthread_getschedparam({}) failed with errno {}",
                    name, rc
                ));
            }
            if actual_policy != RT_POLICY || actual_param.sched_priority != priority {
                return Err(format!(
                    "realtime verification failed for {}: policy {}, prio {}",
                    name, actual_policy, actual_param.sched_priority
                ));
            }
            Ok(())
        }
        #[cfg(target_os = "windows")]
        {
            use std::{cell::Cell, ffi::OsStr, os::windows::ffi::OsStrExt};

            #[link(name = "avrt")]
            unsafe extern "system" {
                fn AvSetMmThreadCharacteristicsW(
                    task_name: *const u16,
                    task_index: *mut u32,
                ) -> isize;
            }

            let _ = priority;
            thread_local! {
                static MMCSS_TASK_HANDLE: Cell<isize> = const { Cell::new(0) };
            }

            MMCSS_TASK_HANDLE.with(|handle| {
                if handle.get() != 0 {
                    return Ok(());
                }

                let task_name: Vec<u16> = OsStr::new("Pro Audio")
                    .encode_wide()
                    .chain(Some(0))
                    .collect();
                let mut task_index = 0_u32;
                let mmcss_handle =
                    unsafe { AvSetMmThreadCharacteristicsW(task_name.as_ptr(), &mut task_index) };
                if mmcss_handle == 0 {
                    Err(format!(
                        "AvSetMmThreadCharacteristicsW({name}, Pro Audio) failed: {}",
                        std::io::Error::last_os_error()
                    ))
                } else {
                    handle.set(mmcss_handle);
                    Ok(())
                }
            })
        }
        #[cfg(all(not(unix), not(target_os = "windows")))]
        {
            let _ = name;
            let _ = priority;
            Err("Realtime thread priority is not supported on this platform".to_string())
        }
    }

    #[cfg(unix)]
    fn lock_memory_pages() -> Result<(), String> {
        let rc = unsafe { libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE) };
        if rc == 0 {
            Ok(())
        } else {
            Err(format!(
                "mlockall(MCL_CURRENT|MCL_FUTURE) failed: {}",
                std::io::Error::last_os_error()
            ))
        }
    }

    pub fn new(
        driver: B::Driver,
        midi_hub: B::MidiHub,
        rx: Receiver<Message>,
        tx: Sender<Message>,
        capture_frame: Arc<AtomicI64>,
        inline_render: Option<Arc<crate::inline_render::InlineRender>>,
    ) -> Self {
        let cycle_frames = driver.cycle_samples() as u32;
        let midi_hub = Arc::new(Mutex::new(midi_hub));
        if let Some(ctx) = inline_render.as_ref() {
            let source: Arc<Mutex<dyn crate::inline_render::MidiInSource + Send>> =
                midi_hub.clone();
            ctx.set_midi_source(&source);
        }
        Self {
            driver: Some(driver),
            midi_hub,
            rx,
            tx,
            cycle_frames,
            pending_midi_out_events: vec![],
            pending_midi_out_sorted: true,
            midi_stop: Arc::new(AtomicBool::new(false)),
            playing: false,
            last_xrun_count: None,
            last_xrun_report: None,
            capture_frame,
            inline_render,
        }
    }

    async fn publish_xruns(&mut self) {
        let count = self.driver.as_ref().and_then(|driver| driver.xrun_count());
        if count != self.last_xrun_count
            || self
                .last_xrun_report
                .is_none_or(|last| last.elapsed() >= Duration::from_millis(250))
        {
            self.last_xrun_report = Some(std::time::Instant::now());
            self.last_xrun_count = count;
            if let Some(count) = count {
                let _ = self
                    .tx
                    .send(Message::Event(crate::message::Event::AudioXruns { count }))
                    .await;
            }
        }
    }

    /// Refresh the shared capture-frame mirror from the driver, if it reports
    /// one. Runs on the worker thread between cycles, when the driver is back
    /// in hand.
    fn publish_capture_frame(&self) {
        if let Some(driver) = self.driver.as_ref() {
            let frame = driver
                .current_capture_frame()
                .unwrap_or(CAPTURE_FRAME_UNKNOWN);
            self.capture_frame.store(frame, Ordering::Relaxed);
        }
    }

    fn driver_mut(&mut self) -> &mut B::Driver {
        self.driver
            .as_mut()
            .expect("driver is only absent while a cycle runs on the blocking thread")
    }

    fn lock_midi_hub(&self) -> std::sync::MutexGuard<'_, B::MidiHub> {
        self.midi_hub.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Run one audio cycle on a tokio blocking thread. The blocking pool
    /// thread does not inherit the async worker thread's realtime priority,
    /// so configure it for every cycle — the pool may hand each cycle to a
    /// different thread.
    fn run_cycle_blocking(
        mut driver: B::Driver,
        inline: Option<Arc<crate::inline_render::InlineRender>>,
    ) -> (B::Driver, Result<(), String>) {
        let rt_start = std::time::Instant::now();
        if let Err(e) = Self::configure_rt_thread(B::WORKER_THREAD_NAME, RT_PRIORITY_WORKER) {
            static WARNED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                tracing::warn!(
                    "{} cycle thread realtime priority not enabled: {}",
                    B::LABEL,
                    e
                );
            }
        }
        let _rt_us = rt_start.elapsed().as_micros() as u64;
        let _cycle_start = std::time::Instant::now();
        crate::cycle_trace::begin_cycle();
        crate::cycle_trace::mark(crate::cycle_trace::TracePoint::HwCycleStart);
        let result = driver.run_cycle_for_worker();
        if let Some(inline) = inline {
            inline.finish_pending_render();
        }
        let _cycle_us = _cycle_start.elapsed().as_micros() as u64;
        (driver, result)
    }

    pub async fn work(mut self) {
        crate::enable_flush_denormals_to_zero();
        #[cfg(unix)]
        {
            if let Err(e) = Self::lock_memory_pages() {
                error!("{} worker memory lock not enabled: {}", B::LABEL, e);
            }
        }
        if let Err(e) = Self::configure_rt_thread(B::WORKER_THREAD_NAME, RT_PRIORITY_WORKER) {
            error!("{} worker realtime priority not enabled: {}", B::LABEL, e);
        }
        #[cfg(unix)]
        {
            let has_fds = self
                .driver
                .as_ref()
                .is_some_and(|d| d.capture_fd().is_some() && d.playback_fd().is_some());
            if has_fds {
                self.work_async().await;
                return;
            }
        }

        self.work_legacy().await;
    }

    /// Handle one worker message outside of a running cycle. `cycle_tx` and
    /// `cycle_running` let `TracksFinished` launch a cycle. Returns true when
    /// the worker should exit (Quit or a closed channel).
    async fn handle_async_msg(
        &mut self,
        msg: Message,
        cycle_tx: &tokio::sync::mpsc::Sender<(B::Driver, Result<(), String>)>,
        cycle_running: &mut bool,
    ) -> bool {
        match msg {
            Message::Request(crate::message::Action::Quit) => {
                self.driver_mut().request_stop();
                self.shutdown_quit();
                return true;
            }
            Message::TracksFinished => {
                crate::cycle_trace::mark(crate::cycle_trace::TracePoint::GoReceived);
                self.flush_pending_midi_out();
                // Inline render drains MIDI input on the cycle thread, right
                // after the capture read, so events reach the render in the
                // same cycle instead of one cycle late.
                if self.inline_render.is_none() {
                    self.drain_midi_input().await;
                }
                if !*cycle_running {
                    *cycle_running = true;
                    let tx = cycle_tx.clone();
                    let driver = self
                        .driver
                        .take()
                        .expect("driver is only absent while a cycle is running");
                    let inline = self.inline_render.clone();
                    tokio::task::spawn_blocking(move || {
                        let _ = tx.blocking_send(Self::run_cycle_blocking(driver, inline));
                    });
                }
            }
            Message::HWMidiOutEvents(mut events) => {
                self.pending_midi_out_events.append(&mut events);
                self.pending_midi_out_sorted = false;
                // Stopped transport means no cycles and no
                // TracksFinished to flush on; write immediately so
                // e.g. panic All-Sound-Off reaches the device.
                if !self.playing {
                    self.flush_pending_midi_out();
                }
            }
            Message::ClearHWMidiOutEvents => {
                self.pending_midi_out_events.clear();
                self.pending_midi_out_sorted = true;
            }
            Message::HWSetPlaying(playing) => {
                self.playing = playing;
                self.driver_mut().set_playing(playing);
            }
            Message::HWZeroFillBuffers => {
                self.driver_mut().zero_fill_hw_buffers();
            }
            Message::HWSetOutputGainBalance { gain, balance } => {
                self.driver_mut().set_output_gain_balance(gain, balance);
            }
            Message::HWOpenMidiInputDevice(device) => {
                let result = self.lock_midi_hub().open_input(&device);
                let action = crate::message::Action::OpenMidiInputDevice(device);
                let _ = self
                    .tx
                    .send(Message::Response(result.map(|_| action)))
                    .await;
            }
            Message::HWOpenMidiOutputDevice(device) => {
                let result = self.lock_midi_hub().open_output(&device);
                let action = crate::message::Action::OpenMidiOutputDevice(device);
                let _ = self
                    .tx
                    .send(Message::Response(result.map(|_| action)))
                    .await;
            }
            Message::HWCloseMidiDevices => {
                self.lock_midi_hub().close_all();
            }
            _ => {}
        }
        false
    }

    #[cfg(unix)]
    async fn work_async(&mut self) {
        let mut cycle_running = false;
        let mut quit_pending = false;
        // Messages received while a cycle is in flight cannot be handled (the
        // driver lives on the blocking thread), so they are buffered here and
        // replayed once the cycle returns. Dropping them instead would lose
        // e.g. MIDI device opens sent right after the audio device opens.
        let mut buffered: Vec<Message> = Vec::new();
        // Stop flag the worker can raise while a cycle owns the driver, so
        // a cycle blocked on device I/O unwinds promptly instead of
        // deadlocking shutdown (Quit cannot reach `request_stop` while the
        // message handler waits for the cycle to finish).
        let stop_signaller = self.driver.as_ref().and_then(|d| d.stop_signaller());
        let (cycle_tx, mut cycle_rx) =
            tokio::sync::mpsc::channel::<(B::Driver, Result<(), String>)>(1);
        let mut midi_input_poll = tokio::time::interval(MIDI_INPUT_POLL_INTERVAL);
        loop {
            tokio::select! {
                // While a cycle is in flight the driver lives on the blocking
                // thread, so regular messages are handled once the cycle
                // returns. Quit is the exception: it must interrupt a cycle
                // that never returns (e.g. a stalled device), so it is
                // received here and turned into the shared stop flag the
                // cycle polls.
                msg = self.rx.recv() => {
                    if cycle_running {
                        match msg {
                            Some(Message::Request(crate::message::Action::Quit)) | None => {
                                if let Some(flag) = &stop_signaller {
                                    flag.store(true, Ordering::Release);
                                }
                                quit_pending = true;
                            }
                            Some(m) => buffered.push(m),
                        }
                        continue;
                    }
                    let msg = match msg {
                        Some(m) => m,
                        None => {
                            self.driver_mut().request_stop();
                            self.shutdown_channel_closed();
                            return;
                        }
                    };
                    if self
                        .handle_async_msg(msg, &cycle_tx, &mut cycle_running)
                        .await
                    {
                        return;
                    }
                }
                result = cycle_rx.recv(), if cycle_running => {
                    cycle_running = false;
                    if let Some((driver, result)) = result {
                        self.driver = Some(driver);
                        if let Err(e) = result {
                            error!("{} cycle error: {}", B::LABEL, e);
                            let _ = self.tx.send(Message::Response(Err(format!(
                                "{} cycle error: {}", B::LABEL, e
                            )))).await;
                        }
                    }
                    if quit_pending {
                        self.shutdown_quit();
                        return;
                    }
                    self.publish_capture_frame();
                        self.publish_xruns().await;
                    crate::cycle_trace::mark(crate::cycle_trace::TracePoint::HwCycleEnd);
                    crate::cycle_trace::mark(crate::cycle_trace::TracePoint::HwFinishedSent);
                    if let Err(e) = self.tx.send(Message::HWFinished).await {
                        error!("{} worker failed to send HWFinished: {}", B::LABEL, e);
                    }
                    for m in std::mem::take(&mut buffered) {
                        if self
                            .handle_async_msg(m, &cycle_tx, &mut cycle_running)
                            .await
                        {
                            return;
                        }
                        if cycle_running {
                            // A buffered TracksFinished started a new cycle;
                            // the rest waits for it to return.
                            break;
                        }
                    }
                }
                // Hardware MIDI input must flow even while the transport is
                // stopped (MIDI learn, monitoring, external controllers);
                // while playing, input is drained every cycle on
                // TracksFinished and this is an extra non-blocking read.
                _ = midi_input_poll.tick(), if !cycle_running => {
                    self.drain_midi_input().await;
                }
            }
        }
    }

    async fn work_legacy(&mut self) {
        let mut midi_input_poll = tokio::time::interval(MIDI_INPUT_POLL_INTERVAL);
        loop {
            let msg = tokio::select! {
                msg = self.rx.recv() => match msg {
                    Some(msg) => msg,
                    None => {
                        self.driver_mut().request_stop();
                        self.shutdown_midi();
                        self.driver_mut().close_fds();
                        return;
                    }
                },
                // Keep hardware MIDI input flowing while the transport is
                // stopped; see work_async.
                _ = midi_input_poll.tick() => {
                    self.drain_midi_input().await;
                    continue;
                }
            };
            if self.handle_legacy_msg(msg).await {
                return;
            }
        }
    }

    /// Handle one worker message in the legacy loop. Returns true when the
    /// worker should exit (Quit or a failed cycle task).
    async fn handle_legacy_msg(&mut self, msg: Message) -> bool {
        match msg {
            Message::Request(crate::message::Action::Quit) => {
                self.driver_mut().request_stop();
                self.flush_pending_midi_out();
                self.shutdown_midi();
                self.driver_mut().close_fds();
                self.driver_mut().request_stop();
                return true;
            }
            Message::TracksFinished => {
                crate::cycle_trace::mark(crate::cycle_trace::TracePoint::GoReceived);
                self.flush_pending_midi_out();
                if self.inline_render.is_none() {
                    self.drain_midi_input().await;
                }
                // The cycle blocks for a full audio period; run it on a
                // blocking thread with per-cycle RT priority instead of
                // stalling the async worker task (see work_async).
                let stop_signaller = self.driver.as_ref().and_then(|d| d.stop_signaller());
                let driver = self
                    .driver
                    .take()
                    .expect("driver is only absent while a cycle is running");
                let inline = self.inline_render.clone();
                let mut cycle =
                    tokio::task::spawn_blocking(move || Self::run_cycle_blocking(driver, inline));
                // Watch for Quit while the cycle runs: a cycle blocked on
                // device I/O would otherwise deadlock shutdown, because
                // request_stop is only reachable from message handling.
                // Non-Quit messages are buffered and handled once the
                // cycle returns.
                let mut quit_pending = false;
                let mut buffered: Vec<Message> = Vec::new();
                let outcome = loop {
                    tokio::select! {
                        res = &mut cycle => break res,
                        msg = self.rx.recv() => match msg {
                            Some(m) => {
                                if matches!(m, Message::Request(crate::message::Action::Quit)) {
                                    if !quit_pending
                                        && let Some(flag) = &stop_signaller {
                                            flag.store(true, Ordering::Release);
                                        }
                                    quit_pending = true;
                                }
                                buffered.push(m);
                            }
                            None => {
                                if !quit_pending
                                    && let Some(flag) = &stop_signaller {
                                        flag.store(true, Ordering::Release);
                                    }
                                quit_pending = true;
                            }
                        },
                    }
                };
                match outcome {
                    Ok((driver, result)) => {
                        self.driver = Some(driver);
                        self.publish_capture_frame();
                        self.publish_xruns().await;
                        if let Err(e) = result {
                            error!("{} assist cycle error: {}", B::LABEL, e);
                            let _ = self
                                .tx
                                .send(Message::Response(Err(format!(
                                    "{} assist cycle error: {}",
                                    B::LABEL,
                                    e
                                ))))
                                .await;
                        }
                    }
                    Err(e) => {
                        error!("{} cycle task failed: {}", B::LABEL, e);
                        return true;
                    }
                }
                if quit_pending {
                    self.driver_mut().request_stop();
                    self.flush_pending_midi_out();
                    self.shutdown_midi();
                    self.driver_mut().close_fds();
                    self.driver_mut().request_stop();
                    return true;
                }
                crate::cycle_trace::mark(crate::cycle_trace::TracePoint::HwCycleEnd);
                crate::cycle_trace::mark(crate::cycle_trace::TracePoint::HwFinishedSent);
                if let Err(e) = self.tx.send(Message::HWFinished).await {
                    error!(
                        "{} worker failed to send HWFinished to engine: {}",
                        B::LABEL,
                        e
                    );
                }
                for m in buffered {
                    // Boxed to allow the recursive call (async fn).
                    if Box::pin(self.handle_legacy_msg(m)).await {
                        return true;
                    }
                }
            }
            Message::HWMidiOutEvents(mut events) => {
                self.pending_midi_out_events.append(&mut events);
                self.pending_midi_out_sorted = false;
                // Stopped transport means no cycles and no TracksFinished
                // to flush on; write immediately (panic All-Sound-Off).
                if !self.playing {
                    self.flush_pending_midi_out();
                }
            }
            Message::ClearHWMidiOutEvents => {
                self.pending_midi_out_events.clear();
                self.pending_midi_out_sorted = true;
            }
            Message::HWSetPlaying(playing) => {
                self.playing = playing;
                self.driver_mut().set_playing(playing);
            }
            Message::HWZeroFillBuffers => {
                self.driver_mut().zero_fill_hw_buffers();
            }
            Message::HWSetOutputGainBalance { gain, balance } => {
                self.driver_mut().set_output_gain_balance(gain, balance);
            }
            Message::HWOpenMidiInputDevice(device) => {
                let result = self.lock_midi_hub().open_input(&device);
                let action = crate::message::Action::OpenMidiInputDevice(device);
                let _ = self
                    .tx
                    .send(Message::Response(result.map(|_| action)))
                    .await;
            }
            Message::HWOpenMidiOutputDevice(device) => {
                let result = self.lock_midi_hub().open_output(&device);
                let action = crate::message::Action::OpenMidiOutputDevice(device);
                let _ = self
                    .tx
                    .send(Message::Response(result.map(|_| action)))
                    .await;
            }
            Message::HWCloseMidiDevices => {
                self.lock_midi_hub().close_all();
            }
            _ => {}
        }
        false
    }

    fn flush_pending_midi_out(&mut self) {
        if self.pending_midi_out_events.is_empty() {
            return;
        }
        if !self.pending_midi_out_sorted {
            self.pending_midi_out_events.sort_by(|a, b| {
                a.event
                    .frame
                    .cmp(&b.event.frame)
                    .then_with(|| a.device.cmp(&b.device))
            });
            self.pending_midi_out_sorted = true;
        }
        self.lock_midi_hub()
            .write_events(&self.pending_midi_out_events);
        self.pending_midi_out_events.clear();
    }

    async fn drain_midi_input(&mut self) {
        let mut midi_in_events = Vec::with_capacity(64);
        self.lock_midi_hub().read_events_into(&mut midi_in_events);
        if midi_in_events.is_empty() {
            return;
        }
        spread_hw_event_frames(&mut midi_in_events, self.cycle_frames);
        let _ = self.tx.send(Message::HWMidiEvents(midi_in_events)).await;
    }

    fn shutdown_midi(&mut self) {
        self.midi_stop.store(true, Ordering::Release);
        let mut hub = self.lock_midi_hub();
        hub.wake_input_waiter();
        hub.close_all();
    }

    #[cfg(unix)]
    fn shutdown_quit(&mut self) {
        self.driver_mut().request_stop();
        self.flush_pending_midi_out();
        self.shutdown_midi();
        self.driver_mut().close_fds();
        self.driver_mut().request_stop();
    }

    #[cfg(unix)]
    fn shutdown_channel_closed(&mut self) {
        self.driver_mut().request_stop();
        self.shutdown_midi();
        self.driver_mut().close_fds();
        self.driver_mut().request_stop();
    }
}

fn spread_hw_event_frames(events: &mut [HwMidiEvent], frames: u32) {
    if events.len() <= 1 || frames <= 1 {
        return;
    }
    let n = events.len() as u32;
    for (idx, event) in events.iter_mut().enumerate() {
        let pos = idx as u32;
        event.event.frame = ((pos as u64 * (frames - 1) as u64) / n as u64) as u32;
    }
}

#[cfg(test)]
mod tests {
    use super::{Backend, CAPTURE_FRAME_UNKNOWN, HwWorker};
    use crate::hw::traits::{HwMidiHub, HwWorkerDriver};
    use crate::message::{Action, HwMidiEvent, Message};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
    use std::time::Duration;
    use tokio::sync::mpsc::channel;

    /// Driver whose cycle blocks until the shared stop flag is set,
    /// simulating a device stalled mid-cycle (e.g. capture that never
    /// becomes readable).
    #[derive(Debug)]
    struct StallingDriver {
        stop: Arc<AtomicBool>,
    }

    impl HwWorkerDriver for StallingDriver {
        fn cycle_samples(&self) -> usize {
            1024
        }

        fn sample_rate(&self) -> i32 {
            48_000
        }

        fn run_cycle_for_worker(&mut self) -> Result<(), String> {
            while !self.stop.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(())
        }

        fn run_assist_step_for_worker(&mut self) -> Result<bool, String> {
            Ok(false)
        }

        fn stop_signaller(&self) -> Option<Arc<AtomicBool>> {
            Some(self.stop.clone())
        }

        #[cfg(unix)]
        fn capture_fd(&self) -> Option<std::os::fd::RawFd> {
            use std::os::fd::AsRawFd;
            Some(std::fs::File::open("/dev/null").ok()?.as_raw_fd())
        }

        #[cfg(unix)]
        fn playback_fd(&self) -> Option<std::os::fd::RawFd> {
            use std::os::fd::AsRawFd;
            Some(std::fs::File::open("/dev/null").ok()?.as_raw_fd())
        }
    }

    #[derive(Debug, Default)]
    struct NoopMidiHub;

    impl HwMidiHub for NoopMidiHub {
        fn read_events_into(&mut self, _out: &mut Vec<HwMidiEvent>) {}

        fn write_events(&mut self, _events: &[HwMidiEvent]) {}
    }

    #[derive(Debug)]
    struct StallingBackend;

    impl Backend for StallingBackend {
        type Driver = StallingDriver;
        type MidiHub = NoopMidiHub;

        const LABEL: &'static str = "stalling";
        const WORKER_THREAD_NAME: &'static str = "stalling-worker";
        const ASSIST_THREAD_NAME: &'static str = "stalling-assist";
        const ASSIST_AUTONOMOUS_ENV: &'static str = "MAOLAN_STALLING_ASSIST";
    }

    /// Regression test for the shutdown deadlock: a `Quit` received while a
    /// hardware cycle is blocked on device I/O must interrupt the cycle via
    /// the driver's stop flag and shut the worker down. Before the fix the
    /// message loop could not process `Quit` until the cycle returned, so a
    /// stalled cycle deadlocked process exit forever.
    #[tokio::test]
    async fn quit_interrupts_stalled_cycle() {
        let driver = StallingDriver {
            stop: Arc::new(AtomicBool::new(false)),
        };
        let (msg_tx, msg_rx) = channel::<Message>(32);
        let (engine_tx, _engine_rx) = channel::<Message>(32);
        let worker = HwWorker::<StallingBackend>::new(
            driver,
            NoopMidiHub,
            msg_rx,
            engine_tx,
            Arc::new(AtomicI64::new(CAPTURE_FRAME_UNKNOWN)),
            None,
        );
        let handle = tokio::spawn(worker.work());
        // Start a cycle, let it stall, then quit while it is in flight.
        msg_tx.send(Message::TracksFinished).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        msg_tx.send(Message::Request(Action::Quit)).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("worker did not shut down after Quit during a stalled cycle")
            .expect("worker task panicked");
    }
}
