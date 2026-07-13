use crate::{
    hw::traits::{HwMidiHub, HwWorkerDriver},
    message::{HwMidiEvent, Message},
};
#[cfg(unix)]
use nix::libc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::mpsc::{Receiver, Sender};
use tracing::error;

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
    midi_hub: B::MidiHub,
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
        self.midi_hub.wake_input_waiter();
        self.midi_hub.close_all();
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
            #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "openbsd"))]
            let c_name = std::ffi::CString::new(name).map_err(|e| e.to_string())?;
            #[cfg(target_os = "linux")]
            unsafe {
                let _ = libc::pthread_setname_np(thread, c_name.as_ptr());
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
        #[cfg(not(unix))]
        {
            let _ = name;
            let _ = priority;
            Err("Realtime thread priority is not supported on this platform".to_string())
        }
    }

    fn lock_memory_pages() -> Result<(), String> {
        #[cfg(unix)]
        {
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
        #[cfg(not(unix))]
        {
            Err("mlockall is not supported on this platform".to_string())
        }
    }

    pub fn new(
        driver: B::Driver,
        midi_hub: B::MidiHub,
        rx: Receiver<Message>,
        tx: Sender<Message>,
    ) -> Self {
        let cycle_frames = driver.cycle_samples() as u32;
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
        }
    }

    fn driver_mut(&mut self) -> &mut B::Driver {
        self.driver
            .as_mut()
            .expect("driver is only absent while a cycle runs on the blocking thread")
    }

    /// Run one audio cycle on a tokio blocking thread. The blocking pool
    /// thread does not inherit the async worker thread's realtime priority,
    /// so configure it for every cycle — the pool may hand each cycle to a
    /// different thread.
    fn run_cycle_blocking(mut driver: B::Driver) -> (B::Driver, Result<(), String>) {
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
        let result = driver.run_cycle_for_worker();
        (driver, result)
    }

    pub async fn work(mut self) {
        crate::enable_flush_denormals_to_zero();
        if let Err(e) = Self::lock_memory_pages() {
            error!("{} worker memory lock not enabled: {}", B::LABEL, e);
        }
        if let Err(e) = Self::configure_rt_thread(B::WORKER_THREAD_NAME, RT_PRIORITY_WORKER) {
            error!("{} worker realtime priority not enabled: {}", B::LABEL, e);
        }
        #[cfg(target_os = "macos")]
        unsafe {
            libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
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

    #[cfg(unix)]
    async fn work_async(&mut self) {
        let mut cycle_running = false;
        let (cycle_tx, mut cycle_rx) =
            tokio::sync::mpsc::channel::<(B::Driver, Result<(), String>)>(1);
        let mut midi_input_poll = tokio::time::interval(MIDI_INPUT_POLL_INTERVAL);
        loop {
            tokio::select! {
                // While a cycle is in flight the driver lives on the blocking
                // thread, so the message channel is not polled; queued
                // messages (including Quit) are handled once the cycle
                // returns — bounded by one audio period.
                msg = self.rx.recv(), if !cycle_running => {
                    let msg = match msg {
                        Some(m) => m,
                        None => {
                            self.driver_mut().request_stop();
                            self.shutdown_channel_closed();
                            return;
                        }
                    };
                    match msg {
                        Message::Request(crate::message::Action::Quit) => {
                            self.driver_mut().request_stop();
                            self.shutdown_quit();
                            return;
                        }
                        Message::TracksFinished => {
                            self.flush_pending_midi_out();
                            self.drain_midi_input().await;
                            if !cycle_running {
                                cycle_running = true;
                                let tx = cycle_tx.clone();
                                let driver = self.driver.take().expect(
                                    "driver is only absent while a cycle is running",
                                );
                                tokio::task::spawn_blocking(move || {
                                    let _ = tx.blocking_send(Self::run_cycle_blocking(driver));
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
                        Message::HWSetOutputGainBalance { gain, balance } => {
                            self.driver_mut().set_output_gain_balance(gain, balance);
                        }
                        Message::HWOpenMidiInputDevice(device) => {
                            let result = self.midi_hub.open_input(&device);
                            let action = crate::message::Action::OpenMidiInputDevice(device);
                            let _ = self.tx.send(Message::Response(result.map(|_| action))).await;
                        }
                        Message::HWOpenMidiOutputDevice(device) => {
                            let result = self.midi_hub.open_output(&device);
                            let action = crate::message::Action::OpenMidiOutputDevice(device);
                            let _ = self.tx.send(Message::Response(result.map(|_| action))).await;
                        }
                        Message::HWCloseMidiDevices => {
                            self.midi_hub.close_all();
                        }
                        _ => {}
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
                    if let Err(e) = self.tx.send(Message::HWFinished).await {
                        error!("{} worker failed to send HWFinished: {}", B::LABEL, e);
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
            match msg {
                Message::Request(crate::message::Action::Quit) => {
                    self.driver_mut().request_stop();
                    self.flush_pending_midi_out();
                    self.shutdown_midi();
                    self.driver_mut().close_fds();
                    self.driver_mut().request_stop();
                    return;
                }
                Message::TracksFinished => {
                    self.flush_pending_midi_out();
                    self.drain_midi_input().await;
                    // The cycle blocks for a full audio period; run it on a
                    // blocking thread with per-cycle RT priority instead of
                    // stalling the async worker task (see work_async).
                    let driver = self
                        .driver
                        .take()
                        .expect("driver is only absent while a cycle is running");
                    let cycle =
                        tokio::task::spawn_blocking(move || Self::run_cycle_blocking(driver));
                    match cycle.await {
                        Ok((driver, result)) => {
                            self.driver = Some(driver);
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
                            return;
                        }
                    }
                    if let Err(e) = self.tx.send(Message::HWFinished).await {
                        error!(
                            "{} worker failed to send HWFinished to engine: {}",
                            B::LABEL,
                            e
                        );
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
                Message::HWSetOutputGainBalance { gain, balance } => {
                    self.driver_mut().set_output_gain_balance(gain, balance);
                }
                Message::HWOpenMidiInputDevice(device) => {
                    let result = self.midi_hub.open_input(&device);
                    let action = crate::message::Action::OpenMidiInputDevice(device);
                    let _ = self
                        .tx
                        .send(Message::Response(result.map(|_| action)))
                        .await;
                }
                Message::HWOpenMidiOutputDevice(device) => {
                    let result = self.midi_hub.open_output(&device);
                    let action = crate::message::Action::OpenMidiOutputDevice(device);
                    let _ = self
                        .tx
                        .send(Message::Response(result.map(|_| action)))
                        .await;
                }
                Message::HWCloseMidiDevices => {
                    self.midi_hub.close_all();
                }
                _ => {}
            }
        }
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
        self.midi_hub.write_events(&self.pending_midi_out_events);
        self.pending_midi_out_events.clear();
    }

    async fn drain_midi_input(&mut self) {
        let mut midi_in_events = Vec::with_capacity(64);
        self.midi_hub.read_events_into(&mut midi_in_events);
        if midi_in_events.is_empty() {
            return;
        }
        spread_hw_event_frames(&mut midi_in_events, self.cycle_frames);
        let _ = self.tx.send(Message::HWMidiEvents(midi_in_events)).await;
    }

    fn shutdown_midi(&mut self) {
        self.midi_stop.store(true, Ordering::Release);
        self.midi_hub.wake_input_waiter();
        self.midi_hub.close_all();
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
