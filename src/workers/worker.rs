use crate::{
    audio::io::AudioIO,
    executor::NodeJob,
    message::{
        Action, Message, OfflineAutomationLane, OfflineAutomationPoint, OfflineAutomationTarget,
        OfflineBounceWork, PluginKind, ProcessTask,
    },
    midi::io::MidiEvent,
    render_plan::Op,
    track::Track,
};
#[cfg(unix)]
use nix::libc;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{Receiver, Sender};

#[derive(Debug)]
pub struct Worker {
    id: usize,
    rx: Receiver<Message>,
    tx: Sender<Message>,
    realtime_priority: i32,
}

impl Worker {
    fn automation_lane_value_at(points: &[OfflineAutomationPoint], sample: usize) -> Option<f32> {
        if points.is_empty() {
            return None;
        }
        if sample <= points[0].sample {
            return Some(points[0].value.clamp(0.0, 1.0));
        }
        if sample >= points[points.len().saturating_sub(1)].sample {
            return Some(points[points.len().saturating_sub(1)].value.clamp(0.0, 1.0));
        }
        for segment in points.windows(2) {
            let left = &segment[0];
            let right = &segment[1];
            if sample < left.sample || sample > right.sample {
                continue;
            }
            let span = right.sample.saturating_sub(left.sample).max(1) as f32;
            let t = (sample.saturating_sub(left.sample) as f32 / span).clamp(0.0, 1.0);
            return Some((left.value + (right.value - left.value) * t).clamp(0.0, 1.0));
        }
        None
    }

    fn apply_freeze_automation_at_sample(
        track: &mut crate::track::Track,
        sample: usize,
        lanes: &[OfflineAutomationLane],
    ) {
        for lane in lanes {
            if matches!(
                lane.target,
                OfflineAutomationTarget::Volume | OfflineAutomationTarget::Balance
            ) {
                continue;
            }
            let Some(value) = Self::automation_lane_value_at(&lane.points, sample) else {
                continue;
            };
            match lane.target {
                OfflineAutomationTarget::Volume | OfflineAutomationTarget::Balance => {}
                OfflineAutomationTarget::MidiCc { channel, cc } => {
                    let cc_value = (value * 127.0).round() as u8;
                    track.rt.pending_automation_midi_events.push(MidiEvent::new(
                        0,
                        vec![0xB0 | channel.min(15), cc.min(127), cc_value],
                    ));
                }
                #[cfg(all(unix, not(target_os = "macos")))]
                OfflineAutomationTarget::Lv2Parameter {
                    instance_id,
                    index,
                    min,
                    max,
                } => {
                    let lo = min.min(max);
                    let hi = max.max(min);
                    let param_value = (lo + value * (hi - lo)).clamp(lo, hi);
                    let _ = track.set_lv2_control_value(
                        instance_id,
                        index as usize,
                        param_value as f64,
                    );
                }
                OfflineAutomationTarget::Vst3Parameter {
                    instance_id,
                    param_id,
                } => {
                    let _ = track.set_vst3_parameter(instance_id, param_id, value.clamp(0.0, 1.0));
                }
                OfflineAutomationTarget::ClapParameter {
                    instance_id,
                    param_id,
                    min,
                    max,
                } => {
                    let lo = min.min(max);
                    let hi = max.max(min);
                    let param_value = (lo + value as f64 * (hi - lo)).clamp(lo, hi);
                    let _ = track.set_clap_parameter_at(instance_id, param_id, param_value, 0);
                }
            }
        }
    }

    fn prepare_track_for_freeze_render(track: &mut crate::track::Track) -> (f32, f32) {
        let original_level = track.level();
        let original_balance = track.balance();
        track.set_level(0.0);
        track.set_balance(0.0);
        (original_level, original_balance)
    }

    fn restore_track_after_freeze_render(
        track: &mut crate::track::Track,
        original_level: f32,
        original_balance: f32,
    ) {
        track.set_level(original_level);
        track.set_balance(original_balance);
    }

    async fn process_offline_bounce(&self, job: OfflineBounceWork) {
        let track_handle = job.state.tracks.get(&job.track_name).cloned();
        let Some(target_track) = track_handle else {
            let _ = self
                .tx
                .send(Message::OfflineBounceFinished {
                    result: Err(format!("Track not found: {}", job.track_name)),
                })
                .await;
            let _ = self.tx.send(Message::Ready(self.id)).await;
            return;
        };
        let (channels, block_size, sample_rate) = {
            let t = target_track.lock();
            let block_size = t
                .audio
                .outs
                .first()
                .map(|io| io.buffer.lock().len())
                .or_else(|| t.audio.ins.first().map(|io| io.buffer.lock().len()))
                .unwrap_or(0)
                .max(1);
            (
                t.audio.outs.len().max(1),
                block_size,
                t.sample_rate.round().max(1.0) as i32,
            )
        };
        let freeze_state = if job.apply_fader {
            None
        } else {
            let mut t = target_track.lock();
            Some(Self::prepare_track_for_freeze_render(&mut t))
        };

        let all_tracks: Vec<_> = job.state.tracks.values().cloned().collect();
        let mut output_to_track: std::collections::HashMap<usize, String> =
            std::collections::HashMap::new();
        for handle in &all_tracks {
            let t = handle.lock();
            for out in &t.audio.outs {
                output_to_track.insert(Arc::as_ptr(out) as usize, t.name.clone());
            }
        }
        let mut relevant_names = HashSet::new();
        let mut queue = vec![job.track_name.clone()];
        while let Some(name) = queue.pop() {
            if !relevant_names.insert(name.clone()) {
                continue;
            }
            if let Some(handle) = all_tracks.iter().find(|h| h.lock().name == name) {
                let t = handle.lock();
                for input in &t.audio.ins {
                    for conn in input.connections.lock().iter() {
                        if let Some(source_name) =
                            output_to_track.get(&(Arc::as_ptr(conn) as usize))
                        {
                            queue.push(source_name.clone());
                        }
                    }
                }
            }
        }
        let relevant_tracks: Vec<_> = all_tracks
            .into_iter()
            .filter(|h| relevant_names.contains(&h.lock().name))
            .collect();

        let mut output_samples =
            Vec::<f32>::with_capacity(job.length_samples.saturating_mul(channels.max(1)));

        let mut cursor = 0usize;
        let mut last_reported_progress = 0.0_f32;
        let mut total_process_time = Duration::ZERO;
        let mut total_write_time = Duration::ZERO;
        while cursor < job.length_samples {
            if job.cancel.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = std::fs::remove_file(&job.output_path);
                if let Some((original_level, original_balance)) = freeze_state {
                    let mut t = target_track.lock();
                    Self::restore_track_after_freeze_render(
                        &mut t,
                        original_level,
                        original_balance,
                    );
                }
                let _ = self
                    .tx
                    .send(Message::OfflineBounceFinished {
                        result: Ok(Action::TrackOfflineBounceCanceled {
                            track_name: job.track_name.clone(),
                        }),
                    })
                    .await;
                let _ = self.tx.send(Message::Ready(self.id)).await;
                return;
            }

            let step = (job.length_samples - cursor).min(block_size);
            for handle in &relevant_tracks {
                let mut t = handle.lock();
                t.audio.set_finished(false);
                t.audio.set_processing(false);
                t.set_transport_sample(job.start_sample.saturating_add(cursor));
                t.set_loop_config(false, None);
                t.set_transport_timing(job.tempo_bpm, job.tsig_num, job.tsig_denom);
                t.set_clip_playback_enabled(true);
                t.set_record_tap_enabled(false);
            }

            loop {
                let mut all_finished = true;
                let mut progressed = false;
                for handle in &relevant_tracks {
                    let mut t = handle.lock();
                    if t.audio.finished() {
                        continue;
                    }
                    all_finished = false;
                    if !t.audio.processing() && t.audio.ready() {
                        if t.name == job.track_name {
                            Self::apply_freeze_automation_at_sample(
                                &mut t,
                                job.start_sample.saturating_add(cursor),
                                &job.automation_lanes,
                            );
                        }
                        t.audio.set_processing(true);
                        let p_start = Instant::now();
                        t.process();
                        total_process_time += p_start.elapsed();
                        t.audio.set_processing(false);
                        progressed = true;
                    }
                }
                if all_finished {
                    break;
                }
                if !progressed {
                    for handle in &relevant_tracks {
                        let mut t = handle.lock();
                        if t.audio.finished() {
                            continue;
                        }
                        if t.name == job.track_name {
                            Self::apply_freeze_automation_at_sample(
                                &mut t,
                                job.start_sample.saturating_add(cursor),
                                &job.automation_lanes,
                            );
                        }
                        t.audio.set_processing(true);
                        let p_start = Instant::now();
                        t.process();
                        total_process_time += p_start.elapsed();
                        t.audio.set_processing(false);
                    }
                    break;
                }
            }

            let write_start = Instant::now();
            {
                let t = target_track.lock();
                let outs: Vec<_> = (0..channels)
                    .map(|ch| t.audio.outs[ch].buffer.lock())
                    .collect();
                for i in 0..step {
                    for out in outs.iter().take(channels) {
                        let sample = out.get(i).copied().unwrap_or(0.0);
                        output_samples.push(sample);
                    }
                }
            }
            total_write_time += write_start.elapsed();

            cursor = cursor.saturating_add(step);
            let progress = (cursor as f32 / job.length_samples as f32).clamp(0.0, 1.0);

            if progress - last_reported_progress >= 0.01 || cursor >= job.length_samples {
                last_reported_progress = progress;
                let _ = self
                    .tx
                    .send(Message::OfflineBounceFinished {
                        result: Ok(Action::TrackOfflineBounceProgress {
                            track_name: job.track_name.clone(),
                            progress,
                            operation: Some("Rendering freeze".to_string()),
                        }),
                    })
                    .await;
            }
        }

        if let Err(e) = crate::audio_codec::write_wav_f32(
            std::path::Path::new(&job.output_path),
            &output_samples,
            channels,
            sample_rate as u32,
        ) {
            let _ = std::fs::remove_file(&job.output_path);
            if let Some((original_level, original_balance)) = freeze_state {
                let mut t = target_track.lock();
                Self::restore_track_after_freeze_render(&mut t, original_level, original_balance);
            }
            let _ = self
                .tx
                .send(Message::OfflineBounceFinished {
                    result: Err(format!(
                        "Failed to write offline bounce '{}': {e}",
                        job.output_path
                    )),
                })
                .await;
            let _ = self.tx.send(Message::Ready(self.id)).await;
            return;
        }

        if let Some((original_level, original_balance)) = freeze_state {
            let mut t = target_track.lock();
            Self::restore_track_after_freeze_render(&mut t, original_level, original_balance);
        }

        let _ = self
            .tx
            .send(Message::OfflineBounceFinished {
                result: Ok(Action::TrackOfflineBounce {
                    track_name: job.track_name,
                    output_path: job.output_path,
                    start_sample: job.start_sample,
                    length_samples: job.length_samples,
                    automation_lanes: vec![],
                    apply_fader: job.apply_fader,
                }),
            })
            .await;
        let _ = self.tx.send(Message::Ready(self.id)).await;
    }

    #[cfg(unix)]
    fn try_enable_realtime(priority: i32) -> Result<(), String> {
        let thread = unsafe { libc::pthread_self() };
        let policy = libc::SCHED_FIFO;
        let param = unsafe {
            let mut p = std::mem::zeroed::<libc::sched_param>();
            p.sched_priority = priority;
            p
        };
        let rc = unsafe { libc::pthread_setschedparam(thread, policy, &param) };
        if rc == 0 {
            Ok(())
        } else {
            Err(format!("pthread_setschedparam failed with errno {}", rc))
        }
    }

    #[cfg(not(unix))]
    fn try_enable_realtime(_priority: i32) -> Result<(), String> {
        Err("Realtime thread priority is not supported on this platform".to_string())
    }

    pub async fn new(
        id: usize,
        rx: Receiver<Message>,
        tx: Sender<Message>,
        realtime_priority: i32,
    ) -> Worker {
        let worker = Worker {
            id,
            rx,
            tx,
            realtime_priority,
        };
        worker.send(Message::Ready(id)).await;
        worker
    }

    pub async fn send(&self, message: Message) {
        self.tx
            .send(message)
            .await
            .expect("Failed to send message from worker");
    }

    /// The output ports a plan task writes, in the same order the plan
    /// compiler used for the node's `outs` buffers.
    fn task_output_ports(t: &Track, task: &ProcessTask) -> Vec<Arc<AudioIO>> {
        match task {
            ProcessTask::Track(_) | ProcessTask::FolderOutput(_) => t.audio.outs.clone(),
            ProcessTask::FolderInput(_) => Vec::new(),
            ProcessTask::Plugin { kind, index, .. } => match kind {
                PluginKind::Clap => t
                    .clap_plugins
                    .get(*index)
                    .map(|p| p.processor.audio_outputs().to_vec())
                    .unwrap_or_default(),
                PluginKind::Vst3 => t
                    .vst3_plugins
                    .get(*index)
                    .map(|p| p.processor.audio_outputs().to_vec())
                    .unwrap_or_default(),
                #[cfg(all(unix, not(target_os = "macos")))]
                PluginKind::Lv2 => t
                    .lv2_plugins
                    .get(*index)
                    .map(|p| p.processor.audio_outputs().to_vec())
                    .unwrap_or_default(),
            },
        }
    }

    /// Execute one node of a render plan (Phase 2, see `LOCKLESS.md`).
    ///
    /// `Sum`/`Zero` are pure arena ops. Task nodes run the legacy track body
    /// (it still re-sums its inputs from the port graph — identical to the
    /// plan's `Sum` result) and then copy their output ports into the arena
    /// so downstream `Sum` nodes and the hardware drain see the result.
    async fn process_node_job(&self, job: NodeJob) {
        let NodeJob { epoch, plan, node } = job;
        let (output_linear, parameter_updates) = match &plan.nodes[node as usize] {
            Op::Zero { output } => {
                // Safety: this worker executes this node; the plan's
                // single-producer-chain invariant guarantees exclusive
                // access to the output buffer.
                unsafe { &mut *plan.buffer_ptr(*output) }.fill(0.0);
                (Vec::new(), Vec::new())
            }
            Op::Sum { inputs, output } => {
                // Safety: see `Op::Zero`; additionally, every input buffer's
                // producer completed before this node was dispatched.
                let out = unsafe { &mut *plan.buffer_ptr(*output) };
                out.fill(0.0);
                let mut sources = inputs.iter();
                if let Some(&first) = sources.next() {
                    let src = unsafe { plan.buffer(first) };
                    crate::simd::copy_sanitized_inplace(out, src);
                    if src.len() < out.len() {
                        out[src.len()..].fill(0.0);
                    }
                }
                for &input in sources {
                    let src = unsafe { plan.buffer(input) };
                    crate::simd::add_sanitized_inplace(out, src);
                }
                (Vec::new(), Vec::new())
            }
            Op::HwInput { .. } => {
                // The hardware driver wrote this buffer before the cycle
                // started; nothing to do.
                (Vec::new(), Vec::new())
            }
            Op::Task { task, outs, .. } => {
                let track = match task {
                    ProcessTask::Track(t)
                    | ProcessTask::FolderInput(t)
                    | ProcessTask::FolderOutput(t) => t,
                    ProcessTask::Plugin { track, .. } => track,
                };
                let mut t = track.lock();
                match task {
                    ProcessTask::Track(_) => t.process(),
                    ProcessTask::FolderInput(_) => t.process_folder_input(),
                    ProcessTask::FolderOutput(_) => t.process_folder_output(),
                    ProcessTask::Plugin { kind, index, .. } => t.process_plugin(*kind, *index),
                }
                t.audio.set_processing(false);
                let updates = std::mem::take(&mut t.rt.echoed_parameter_updates);
                let meter = t.output_meter_linear();
                // Copy task outputs port -> arena for downstream Sum nodes
                // and the hardware output drain.
                for (port, &buf) in Self::task_output_ports(&t, task).iter().zip(outs.iter()) {
                    let src = port.buffer.lock();
                    // Safety: this worker executes this node, the unique
                    // producer of `buf` (see `Op::Zero`).
                    let dst = unsafe { &mut *plan.buffer_ptr(buf) };
                    let n = src.len().min(dst.len());
                    dst[..n].copy_from_slice(&src[..n]);
                    if n < dst.len() {
                        dst[n..].fill(0.0);
                    }
                }
                (meter, updates)
            }
        };
        let _ = self
            .tx
            .send(Message::NodeDone {
                worker_id: self.id,
                epoch,
                node,
                output_linear,
                parameter_updates,
            })
            .await;
    }

    pub async fn work(&mut self) {
        crate::enable_flush_denormals_to_zero();
        if let Err(e) = Self::try_enable_realtime(self.realtime_priority) {
            tracing::warn!(
                "Worker {} realtime priority {} not enabled: {}",
                self.id,
                self.realtime_priority,
                e
            );
        }
        while let Some(message) = self.rx.recv().await {
            match message {
                Message::Request(Action::Quit) => {
                    return;
                }
                Message::ProcessOfflineBounce(job) => {
                    self.process_offline_bounce(job).await;
                }
                Message::NodeJob(job) => {
                    self.process_node_job(job).await;
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Worker;
    use crate::message::{
        Action, Message, OfflineAutomationLane, OfflineAutomationPoint, OfflineAutomationTarget,
        OfflineBounceWork,
    };
    use crate::state::State;
    use crate::track::Track;
    use std::path::PathBuf;
    use std::sync::{Arc, atomic::AtomicBool};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::sync::mpsc::channel;

    fn make_state_with_track(track: Track) -> State {
        let mut state = State::default();
        state.tracks.insert(track.name.clone(), Arc::new(track));
        state
    }

    fn unique_temp_wav(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!("maolan_{name}_{nanos}.wav"))
    }

    #[test]
    fn prepare_track_for_freeze_render_neutralizes_level_and_balance() {
        let mut track = Track::new("track".to_string(), 1, 2, 0, 0, 64, 48_000.0);
        track.set_level(-6.0);
        track.set_balance(0.35);

        let (level, balance) = Worker::prepare_track_for_freeze_render(&mut track);

        assert_eq!(level, -6.0);
        assert_eq!(balance, 0.35);
        assert_eq!(track.level(), 0.0);
        assert_eq!(track.balance(), 0.0);

        Worker::restore_track_after_freeze_render(&mut track, level, balance);
        assert_eq!(track.level(), -6.0);
        assert_eq!(track.balance(), 0.35);
    }

    #[test]
    fn freeze_automation_ignores_volume_and_balance_lanes() {
        let mut track = Track::new("track".to_string(), 1, 2, 0, 1, 64, 48_000.0);
        let lanes = vec![
            OfflineAutomationLane {
                target: OfflineAutomationTarget::Volume,
                visible: true,
                points: vec![OfflineAutomationPoint {
                    sample: 0,
                    value: 0.0,
                }],
            },
            OfflineAutomationLane {
                target: OfflineAutomationTarget::Balance,
                visible: true,
                points: vec![OfflineAutomationPoint {
                    sample: 0,
                    value: 1.0,
                }],
            },
            OfflineAutomationLane {
                target: OfflineAutomationTarget::MidiCc { channel: 0, cc: 7 },
                visible: true,
                points: vec![OfflineAutomationPoint {
                    sample: 0,
                    value: 1.0,
                }],
            },
        ];

        Worker::apply_freeze_automation_at_sample(&mut track, 0, &lanes);

        assert_eq!(track.level(), 0.0);
        assert_eq!(track.balance(), 0.0);
        assert_eq!(track.rt.pending_automation_midi_events.len(), 1);
        assert_eq!(
            track.rt.pending_automation_midi_events[0].data,
            vec![0xB0, 7, 127]
        );
    }

    #[test]
    fn automation_lane_value_at_interpolates_between_points() {
        let value = Worker::automation_lane_value_at(
            &[
                OfflineAutomationPoint {
                    sample: 10,
                    value: 0.25,
                },
                OfflineAutomationPoint {
                    sample: 20,
                    value: 0.75,
                },
            ],
            15,
        )
        .expect("value");

        assert!((value - 0.5).abs() < 1.0e-6);
    }

    #[test]
    fn freeze_automation_applies_interpolated_midi_cc_lane() {
        let mut track = Track::new("track".to_string(), 1, 1, 0, 1, 64, 48_000.0);
        let lanes = vec![OfflineAutomationLane {
            target: OfflineAutomationTarget::MidiCc { channel: 0, cc: 7 },
            visible: true,
            points: vec![
                OfflineAutomationPoint {
                    sample: 0,
                    value: 0.0,
                },
                OfflineAutomationPoint {
                    sample: 10,
                    value: 1.0,
                },
            ],
        }];

        Worker::apply_freeze_automation_at_sample(&mut track, 5, &lanes);
        assert_eq!(track.rt.pending_automation_midi_events.len(), 1);
        assert_eq!(track.rt.pending_automation_midi_events[0].data[2], 64);

        track.rt.pending_automation_midi_events.clear();
        Worker::apply_freeze_automation_at_sample(&mut track, 2, &lanes);
        assert_eq!(track.rt.pending_automation_midi_events.len(), 1);
        assert_eq!(track.rt.pending_automation_midi_events[0].data[2], 25);
    }

    #[tokio::test]
    async fn process_node_job_sums_arena_buffers() {
        use crate::render_plan::{Op, RenderPlan};
        use std::cell::UnsafeCell;
        use std::collections::HashMap;

        let (_rx_unused_tx, rx_unused) = channel(1);
        let (tx, mut out_rx) = channel(8);
        let worker = Worker {
            id: 4,
            rx: rx_unused,
            tx,
            realtime_priority: 0,
        };
        let collector = basedrop::Collector::new();
        let plan = RenderPlan {
            buffer_size: 4,
            buffers: (0..3).map(|_| UnsafeCell::new(vec![0.0; 4])).collect(),
            nodes: vec![Op::Sum {
                inputs: vec![0, 1],
                output: 2,
            }],
            indegree: vec![0],
            dependents: vec![vec![]],
            sources: vec![0],
            hw_in_map: vec![],
            hw_in_ports: vec![],
            hw_out_map: vec![],
            port_map: HashMap::new(),
            midi_edges: vec![],
            forced: vec![],
        };
        // Safety: test thread, no node is executing yet.
        unsafe {
            (&mut *plan.buffer_ptr(0)).copy_from_slice(&[0.25, 0.5, 0.75, 1.0]);
            (&mut *plan.buffer_ptr(1)).copy_from_slice(&[0.75, 0.5, 0.25, f32::NAN]);
        }
        let shared = std::sync::Arc::new(basedrop::Owned::new(&collector.handle(), plan));

        worker
            .process_node_job(crate::executor::NodeJob {
                epoch: 1,
                plan: shared.clone(),
                node: 0,
            })
            .await;

        // Safety: the job completed, so the Sum node's writes are done.
        // The NaN in the second source is sanitized to 0 before adding.
        unsafe {
            assert_eq!(
                &*shared.buffer_ptr(2),
                &vec![1.0, 1.0, 1.0, 1.0],
                "sanitized sum in the arena"
            );
        }
        match out_rx.recv().await.expect("message") {
            Message::NodeDone {
                worker_id,
                epoch,
                node,
                output_linear,
                ..
            } => {
                assert_eq!(worker_id, 4);
                assert_eq!(epoch, 1);
                assert_eq!(node, 0);
                assert!(output_linear.is_empty());
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[tokio::test]
    async fn process_offline_bounce_errors_when_track_is_missing() {
        let (_rx_unused_tx, rx_unused) = channel(1);
        let (tx, mut out_rx) = channel(8);
        let worker = Worker {
            id: 7,
            rx: rx_unused,
            tx,
            realtime_priority: 0,
        };
        let job = OfflineBounceWork {
            state: Arc::new(State::default().snapshot()),
            track_name: "missing".to_string(),
            output_path: unique_temp_wav("missing").to_string_lossy().to_string(),
            start_sample: 0,
            length_samples: 8,
            tempo_bpm: 120.0,
            tsig_num: 4,
            tsig_denom: 4,
            automation_lanes: vec![],
            cancel: Arc::new(AtomicBool::new(false)),
            apply_fader: false,
        };

        worker.process_offline_bounce(job).await;

        match out_rx.recv().await.expect("message") {
            Message::OfflineBounceFinished { result: Err(err) } => {
                assert!(err.contains("Track not found: missing"));
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[tokio::test]
    async fn process_offline_bounce_cancels_and_restores_track_state() {
        let (_rx_unused_tx, rx_unused) = channel(1);
        let (tx, mut out_rx) = channel(8);
        let worker = Worker {
            id: 5,
            rx: rx_unused,
            tx,
            realtime_priority: 0,
        };
        let track = Track::new("track".to_string(), 1, 2, 0, 0, 4, 48_000.0);
        track.set_level(-9.0);
        track.set_balance(-0.3);
        let state = make_state_with_track(track);
        let job = OfflineBounceWork {
            state: Arc::new(state.lock().snapshot()),
            track_name: "track".to_string(),
            output_path: unique_temp_wav("cancel").to_string_lossy().to_string(),
            start_sample: 0,
            length_samples: 8,
            tempo_bpm: 120.0,
            tsig_num: 4,
            tsig_denom: 4,
            automation_lanes: vec![],
            cancel: Arc::new(AtomicBool::new(true)),
            apply_fader: false,
        };

        worker.process_offline_bounce(job).await;

        match out_rx.recv().await.expect("message") {
            Message::OfflineBounceFinished {
                result: Ok(Action::TrackOfflineBounceCanceled { track_name }),
            } => assert_eq!(track_name, "track"),
            other => panic!("unexpected message: {other:?}"),
        }
        assert!(matches!(out_rx.recv().await, Some(Message::Ready(5))));
        let track = state.lock().tracks.get("track").expect("track").lock();
        assert_eq!(track.level(), -9.0);
        assert_eq!(track.balance(), -0.3);
    }

    #[tokio::test]
    async fn process_offline_bounce_restores_track_state_on_write_failure() {
        let (_rx_unused_tx, rx_unused) = channel(1);
        let (tx, mut out_rx) = channel(8);
        let worker = Worker {
            id: 3,
            rx: rx_unused,
            tx,
            realtime_priority: 0,
        };
        let track = Track::new("track".to_string(), 1, 2, 0, 0, 4, 48_000.0);
        track.set_level(-4.0);
        track.set_balance(0.25);
        let state = make_state_with_track(track);
        let output_path = std::env::temp_dir().to_string_lossy().to_string();
        let job = OfflineBounceWork {
            state: Arc::new(state.lock().snapshot()),
            track_name: "track".to_string(),
            output_path,
            start_sample: 0,
            length_samples: 4,
            tempo_bpm: 120.0,
            tsig_num: 4,
            tsig_denom: 4,
            automation_lanes: vec![],
            cancel: Arc::new(AtomicBool::new(false)),
            apply_fader: false,
        };

        worker.process_offline_bounce(job).await;

        let mut saw_error = false;
        while let Some(message) = out_rx.recv().await {
            match message {
                Message::OfflineBounceFinished {
                    result: Ok(Action::TrackOfflineBounceProgress { .. }),
                } => {}
                Message::OfflineBounceFinished { result: Err(err) } => {
                    assert!(
                        err.contains("Failed to create offline bounce")
                            || err.contains("Failed to write offline bounce")
                            || err.contains("Failed to finalize offline bounce")
                    );
                    saw_error = true;
                }
                Message::Ready(3) => break,
                other => panic!("unexpected message: {other:?}"),
            }
        }
        assert!(saw_error);
        let track = state.lock().tracks.get("track").expect("track").lock();
        assert_eq!(track.level(), -4.0);
        assert_eq!(track.balance(), 0.25);
    }

    #[tokio::test]
    async fn process_offline_bounce_emits_progress_and_completion() {
        let (_rx_unused_tx, rx_unused) = channel(1);
        let (tx, mut out_rx) = channel(16);
        let worker = Worker {
            id: 2,
            rx: rx_unused,
            tx,
            realtime_priority: 0,
        };
        let track = Track::new("track".to_string(), 1, 1, 0, 0, 4, 48_000.0);
        track.set_level(-3.0);
        track.set_balance(0.4);
        let state = make_state_with_track(track);
        let output = unique_temp_wav("success");
        let job = OfflineBounceWork {
            state: Arc::new(state.lock().snapshot()),
            track_name: "track".to_string(),
            output_path: output.to_string_lossy().to_string(),
            start_sample: 0,
            length_samples: 8,
            tempo_bpm: 120.0,
            tsig_num: 4,
            tsig_denom: 4,
            automation_lanes: vec![],
            cancel: Arc::new(AtomicBool::new(false)),
            apply_fader: false,
        };

        worker.process_offline_bounce(job).await;

        let mut saw_progress = false;
        let mut saw_complete = false;
        let mut saw_ready = false;
        while let Some(message) = out_rx.recv().await {
            match message {
                Message::OfflineBounceFinished {
                    result:
                        Ok(Action::TrackOfflineBounceProgress {
                            track_name,
                            progress,
                            ..
                        }),
                } => {
                    assert_eq!(track_name, "track");
                    assert!(progress > 0.0);
                    saw_progress = true;
                }
                Message::OfflineBounceFinished {
                    result:
                        Ok(Action::TrackOfflineBounce {
                            track_name,
                            output_path,
                            ..
                        }),
                } => {
                    assert_eq!(track_name, "track");
                    assert_eq!(output_path, output.to_string_lossy());
                    saw_complete = true;
                }
                Message::Ready(2) => {
                    saw_ready = true;
                    break;
                }
                other => panic!("unexpected message: {other:?}"),
            }
        }

        assert!(saw_progress);
        assert!(saw_complete);
        assert!(saw_ready);
        assert!(output.exists());
        std::fs::remove_file(&output).expect("remove temp wav");
        let track = state.lock().tracks.get("track").expect("track").lock();
        assert_eq!(track.level(), -3.0);
        assert_eq!(track.balance(), 0.4);
        assert!(!track.muted());
    }
}
