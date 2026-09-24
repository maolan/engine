use super::*;
use crate::workers::worker::Worker;
use tokio::sync::mpsc::channel;
use tracing::error;

impl Engine {
    pub fn new(rx: Receiver<Message>, tx: Sender<Message>) -> Self {
        let (meter_snapshot_producer, _) =
            crate::triple_buffer::triple_buffer(crate::meter::MeterSnapshot::default());
        let (transport_snapshot_producer, _) =
            crate::triple_buffer::triple_buffer(crate::meter::TransportSnapshot::default());
        let (session_runtime_snapshot_producer, _) =
            crate::triple_buffer::triple_buffer(crate::meter::SessionRuntimeSnapshot::default());
        Self::new_with_snapshots(
            rx,
            tx,
            meter_snapshot_producer,
            transport_snapshot_producer,
            session_runtime_snapshot_producer,
        )
    }

    pub fn new_with_snapshots(
        rx: Receiver<Message>,
        tx: Sender<Message>,
        meter_snapshot_producer: crate::triple_buffer::TripleBufferProducer<
            crate::meter::MeterSnapshot,
        >,
        transport_snapshot_producer: crate::triple_buffer::TripleBufferProducer<
            crate::meter::TransportSnapshot,
        >,
        session_runtime_snapshot_producer: crate::triple_buffer::TripleBufferProducer<
            crate::meter::SessionRuntimeSnapshot,
        >,
    ) -> Self {
        let state = Arc::new(State::default());
        let initial_state_snapshot = state.lock().snapshot();
        let state_snapshot = Arc::new(crate::state::StateSlot::from_pointee(
            initial_state_snapshot.clone(),
        ));
        // Phase 2 render-plan machinery: an initial (empty-session) plan is
        // built synchronously; the builder thread republishes on demand.
        let collector = basedrop::Collector::new();
        let hw_ports = Arc::new(arc_swap::ArcSwap::from_pointee(
            crate::plan_builder::HwPorts {
                buffer_size: 1024,
                ..Default::default()
            },
        ));
        let initial_plan =
            { crate::render_plan::RenderPlan::compile(&initial_state_snapshot, &[], &[], 1024) };
        let plan_slot = Arc::new(crate::render_plan::PlanSlot::from_pointee(
            basedrop::Owned::new(&collector.handle(), initial_plan),
        ));
        let plan_builder = crate::plan_builder::PlanBuilder::spawn(
            state_snapshot.clone(),
            hw_ports.clone(),
            plan_slot.clone(),
            collector,
        );
        let executor = crate::executor::CycleExecutor::new(plan_slot.clone());
        Self {
            rx,
            tx,
            clients: vec![],
            state,
            state_snapshot,
            workers: vec![],
            hw_driver: None,
            hw_driver_info: None,
            hw_input_ports: Vec::new(),
            hw_output_ports: Vec::new(),
            #[cfg(unix)]
            jack_runtime: None,
            midi_hub: Some(MidiHub::default()),
            auto_open_midi_devices: true,
            hw_worker: None,
            osc_server: None,
            osc_reply_socket: None,
            osc_reply_target: None,
            automation: Default::default(),
            hw_midi: Default::default(),
            dispatch: Default::default(),
            recording: Default::default(),
            meters: fields::MeterFields::new(meter_snapshot_producer),
            transport: fields::TransportFields::new(transport_snapshot_producer),
            session: fields::SessionFields::new(session_runtime_snapshot_producer),
            executor,
            plan_builder,
            plan_slot,
            hw_ports,
            pending_node_jobs: VecDeque::new(),
            history: crate::engine::history::History::default(),
            history_group: None,
            history_suspended: false,
            midi_learn: Default::default(),
            audio_preview: None,
            #[cfg(target_os = "windows")]
            _windows_timer_guard: crate::enable_windows_high_resolution_timer(),
            node_result_notify: Arc::new(Notify::new()),
        }
    }

    pub async fn init(&mut self) {
        let max_threads = num_cpus::get();
        for id in 0..max_threads {
            let (tx, rx) = channel::<Message>(32);
            let tx_thread = self.tx.clone();
            let handler = tokio::spawn(async move {
                let wrk = Worker::new(id, rx, tx_thread, 8);
                wrk.await.work().await;
            });
            let (node_job_tx, mut node_job_rx) = rtrb::RingBuffer::new(64);
            let (mut node_result_tx, node_result_rx) = rtrb::RingBuffer::new(64);
            let node_quit = Arc::new(AtomicBool::new(false));
            let node_quit_thread = node_quit.clone();
            let node_result_notify = self.node_result_notify.clone();
            let node_thread_handle = std::thread::Builder::new()
                .name(format!("maolan-node-worker-{id}"))
                .spawn(move || {
                    crate::enable_flush_denormals_to_zero();
                    if let Err(e) = Worker::try_enable_realtime(8) {
                        tracing::warn!(
                            "Node worker {} realtime priority {} not enabled: {}",
                            id,
                            8,
                            e
                        );
                    }
                    while !node_quit_thread.load(std::sync::atomic::Ordering::Acquire) {
                        match node_job_rx.pop() {
                            Ok(job) => {
                                let mut result = Worker::process_node_job_result(id, job);
                                loop {
                                    match node_result_tx.push(result) {
                                        Ok(()) => {
                                            node_result_notify.notify_one();
                                            break;
                                        }
                                        Err(rtrb::PushError::Full(returned)) => {
                                            if node_quit_thread
                                                .load(std::sync::atomic::Ordering::Acquire)
                                            {
                                                break;
                                            }
                                            result = returned;
                                            std::thread::yield_now();
                                        }
                                    }
                                }
                            }
                            Err(rtrb::PopError::Empty) => {
                                std::thread::park();
                            }
                        }
                    }
                })
                .expect("failed to spawn node worker thread");
            let node_thread = node_thread_handle.thread().clone();
            std::mem::forget(node_thread_handle);
            self.workers.push(WorkerData::with_node_mailbox(
                tx.clone(),
                handler,
                node_job_tx,
                node_result_rx,
                node_thread,
                node_quit,
            ));
        }
    }
}

impl Engine {
    pub fn state(&self) -> Arc<State> {
        self.state.clone()
    }

    pub(crate) fn publish_state_snapshot(&self) {
        let snapshot = self.state.lock().snapshot();
        self.state_snapshot.store(Arc::new(snapshot));
    }

    pub(crate) fn hw_driver_cycle_samples(&self) -> Option<usize> {
        self.hw_driver_info.map(|info| info.cycle_samples)
    }

    #[cfg(unix)]
    pub(crate) fn jack_cycle_samples(&self) -> Option<usize> {
        self.jack_runtime.as_ref().map(|j| j.buffer_size)
    }

    pub(crate) fn current_cycle_samples(&self) -> usize {
        self.hw_driver_cycle_samples()
            .or_else(|| self.jack_cycle_samples())
            .unwrap_or(0)
    }

    pub(crate) fn sample_rate(&self) -> f64 {
        if let Some(info) = self.hw_driver_info {
            info.sample_rate as f64
        } else {
            #[cfg(unix)]
            {
                self.jack_runtime
                    .as_ref()
                    .map(|j| j.sample_rate as f64)
                    .unwrap_or(48_000.0)
            }
            #[cfg(not(unix))]
            {
                48_000.0
            }
        }
    }

    pub(crate) async fn set_hw_playing(&mut self, playing: bool) {
        if let Some(worker) = &self.hw_worker {
            // Await instead of try_send: a dropped HWSetPlaying(false) would
            // leave the device playing after transport stop.
            let _ = worker.tx.send(Message::HWSetPlaying(playing)).await;
        } else if let Some(driver) = self.hw_driver.as_mut() {
            driver.set_playing(playing);
        }
    }

    pub(crate) fn take_ready_worker_index(&mut self) -> Option<usize> {
        while !self.dispatch.ready_workers.is_empty() {
            let worker_index = self.dispatch.ready_workers.remove(0);
            if worker_index < self.workers.len() {
                return Some(worker_index);
            }
        }
        None
    }

    pub(crate) fn push_ready_worker(&mut self, worker_index: usize) {
        self.dispatch.ready_workers.push(worker_index);
    }

    pub(crate) async fn handle_quit(&mut self, a: Action) {
        self.flush_recordings().await;
        // Stop the HW worker before notifying the GUI so the
        // OSS audio channels are halted and closed from the
        // worker's own thread. The GUI calls exit(0) upon
        // receiving the Quit response, which skips Rust
        // destructors. Without this, the kernel's dsp_close
        // drains pending audio buffers for up to CHN_TIMEOUT
        // (5s) during process teardown.
        if let Some(mut worker) = self.hw_worker.take() {
            // Send MIDI panic (All Sound Off) for any active
            // notes before stopping the worker.
            let panic_events = self.panic_events_for_all_hw_midi_outputs();
            if !panic_events.is_empty() {
                let _ = worker.tx.send(Message::HWMidiOutEvents(panic_events)).await;
            }
            // Send Quit to the worker so it stops its audio
            // cycle loop and releases the driver.
            if let Err(e) = worker.tx.send(Message::Request(a.clone())).await {
                error!("Error sending quit message to HW worker: {e}");
            }
            if let Some(handle) = worker.handle.take() {
                handle
                    .await
                    .unwrap_or_else(|e| error!("Error waiting for HW worker to quit: {e}"));
            }
        }
        // Explicitly close audio and MIDI fds before sending
        // the Quit response. The GUI calls exit(0) upon
        // receiving it, which skips destructors — any
        // still-open device fd would trigger the kernel's
        // 5-second drain during process teardown.
        if let Some(hw) = self.hw_driver.as_mut() {
            hw.close_fds();
        }
        if let Some(midi_hub) = self.midi_hub.as_mut() {
            midi_hub.close_all();
        }
        self.hw_driver = None;
        self.hw_driver_info = None;
        self.hw_input_ports.clear();
        self.hw_output_ports.clear();
        self.notify_clients(Ok(Action::Quit)).await;
        self.dispatch.ready_workers.clear();
        while !self.workers.is_empty() {
            let mut worker = self.workers.remove(0);
            if let Err(e) = worker.tx.send(Message::Request(a.clone())).await {
                error!("Error sending quit message to worker: {e}");
            }
            if let Some(handle) = worker.handle.take() {
                handle
                    .await
                    .unwrap_or_else(|e| error!("Error waiting for worker to quit: {e}"));
            }
        }
        #[cfg(unix)]
        {
            self.jack_runtime = None;
        }
        self.osc_server = None;
    }
}
