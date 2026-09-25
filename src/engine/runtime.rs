use super::*;
#[cfg(target_os = "linux")]
use crate::hw::alsa::MidiHub;
#[cfg(target_os = "macos")]
use crate::hw::coremidi::MidiHub;
#[cfg(target_os = "openbsd")]
use crate::hw::sndio::{HwDriver, HwOptions, MidiHub};
#[cfg(target_os = "windows")]
use crate::hw::wasapi::MidiHub;
#[cfg(target_os = "openbsd")]
use crate::workers::sndio_worker::HwWorker;
use crate::{
    history::UndoEntry,
    message::{Action, HwMidiEvent, Message, ProcessTask},
};
use std::{
    sync::atomic::Ordering,
    time::{Duration, Instant},
};
use tracing::error;

impl Engine {
    pub(crate) const TRACK_PROCESS_TIMEOUT: Duration = Duration::from_millis(250);

    #[cfg(not(unix))]
    pub(crate) fn jack_cycle_samples(&self) -> Option<usize> {
        None
    }

    pub(crate) async fn dispatch_request(&mut self, a: Action) {
        match a {
            Action::TrackOfflineBounceCancel { track_name } => {
                if let Some(job) = self.dispatch.offline_bounce_jobs.get(&track_name) {
                    job.cancel.store(true, Ordering::Relaxed);
                }
            }
            Action::TrackOfflineBounceCancelAll => {
                for job in self.dispatch.offline_bounce_jobs.values() {
                    job.cancel.store(true, Ordering::Relaxed);
                }
            }
            _ if !self.dispatch.offline_bounce_jobs.is_empty() => {
                self.dispatch.pending_requests.push_back(a);
            }
            Action::OpenAudioDevice { .. }
            | Action::OpenMidiInputDevice(_)
            | Action::OpenMidiOutputDevice(_)
            | Action::RequestMeterSnapshot
            | Action::RequestTrackList
            | Action::RequestTransportState
            | Action::Quit
            | Action::Play
            | Action::Pause
            | Action::Stop
            | Action::TransportPosition(_)
            | Action::JumpToEnd
            | Action::SetLoopEnabled(_)
            | Action::SetLoopRange(_)
            | Action::SetPunchEnabled(_)
            | Action::SetPunchRange(_)
            | Action::SetMetronomeEnabled(_)
            | Action::SetTempo(_)
            | Action::SetTimeSignature { .. }
            | Action::SetTempoMap { .. }
            | Action::SetOscEnabled(_)
            | Action::SetClipPlaybackEnabled(_)
            | Action::SetRecordEnabled(_)
            | Action::SetStepRecording(_)
            | Action::SetClipIdentity { .. }
            | Action::SetClipGainDb { .. }
            | Action::SetSessionPath(_)
            | Action::ClearHistory
            | Action::BeginSessionRestore
            | Action::PianoKey { .. }
            | Action::ModifyMidiNotes { .. }
            | Action::ModifyMidiControllers { .. }
            | Action::DeleteMidiControllers { .. }
            | Action::InsertMidiControllers { .. }
            | Action::DeleteMidiNotes { .. }
            | Action::InsertMidiNotes { .. }
            | Action::SetMidiSysExEvents { .. }
            | Action::Session(_) => {
                self.handle_request(a).await;
            }
            #[cfg(unix)]
            Action::ListLv2Plugins => {
                self.handle_request(a).await;
            }
            Action::ListVst3Plugins => {
                self.handle_request(a).await;
            }
            Action::ListClapPlugins => {
                self.handle_request(a).await;
            }
            Action::ListClapPluginsWithCapabilities => {
                self.handle_request(a).await;
            }
            _ => {
                self.dispatch.pending_requests.push_back(a);
                if self.can_schedule_hw_cycle() {
                    self.request_hw_cycle().await;
                } else {
                    while let Some(next) = self.dispatch.pending_requests.pop_front() {
                        self.handle_request(next).await;
                    }
                }
            }
        };
        self.publish_clap_state_dirty().await;
    }

    pub(crate) async fn request_hw_cycle(&mut self) {
        if self.transport.awaiting_hwfinished {
            tracing::debug!(
                playing = self.transport.playing,
                transport_running = self.transport.transport_running,
                transport_sample = self.transport.transport_sample,
                session_transport_sample = self.transport.session_transport_sample,
                cycle_samples = self.current_cycle_samples(),
                "request_hw_cycle skipped because HWFinished is still pending"
            );
            return;
        }
        self.mix_audio_preview_into_hw_outputs();
        tracing::debug!(
            playing = self.transport.playing,
            transport_running = self.transport.transport_running,
            transport_sample = self.transport.transport_sample,
            session_transport_sample = self.transport.session_transport_sample,
            cycle_samples = self.current_cycle_samples(),
            "request_hw_cycle sending TracksFinished"
        );
        self.apply_hw_out_gain_and_meter().await;
        self.publish_meter_snapshot_if_due();
        if let Some((after_frames, loop_start, cycle_end_sample)) =
            self.scheduled_loop_wrap_for_next_cycle()
        {
            self.transport.notified_loop_wrap_sample = Some(cycle_end_sample);
            self.notify_event(Event::TransportPositionAt {
                sample: loop_start,
                after_frames,
            })
            .await;
        } else {
            self.transport.notified_loop_wrap_sample = None;
        }
        if let Some(worker) = &self.hw_worker {
            if !self.hw_midi.pending_hw_midi_out_events_by_device.is_empty() {
                let out_events =
                    std::mem::take(&mut self.hw_midi.pending_hw_midi_out_events_by_device);
                if let Err(e) = worker.tx.send(Message::HWMidiOutEvents(out_events)).await {
                    error!("Error sending HWMidiOutEvents {e}");
                }
            }
            match worker.tx.send(Message::TracksFinished).await {
                Ok(_) => {
                    self.transport.awaiting_hwfinished = true;
                }
                Err(e) => {
                    error!("Error sending TracksFinished {e}");
                }
            }
        }
    }

    pub(crate) fn task_track_name(task: &ProcessTask) -> String {
        match task {
            ProcessTask::Track(t) | ProcessTask::FolderInput(t) | ProcessTask::FolderOutput(t) => {
                t.lock().name.clone()
            }
            ProcessTask::Plugin { track, .. } => track.lock().name.clone(),
        }
    }

    /// Pushes the current transport state into the task's track before the
    /// task is dispatched.
    ///
    /// The transport sample itself is NOT pushed here: it travels through the
    /// shared lock-free `transport_sample_snapshot`, which is mirrored from
    /// `transport_sample`/`session_transport_sample` on every dispatch (also
    /// on the generation fast path) and read by the worker at task start.
    /// Only the session-vs-main choice and the remaining pushed fields are
    /// generation-gated.
    ///
    /// INVARIANT: `self.transport.prepare_generation` must be bumped (via
    /// `bump_prepare_generation`) at every mutation of any field pushed
    /// here — `session_clip_playback_enabled`, `playing`,
    /// `loop_enabled`, `loop_range_samples`, `tempo_bpm`, `tsig_num`,
    /// `tsig_denom`, `clip_playback_enabled`, and `record_enabled`. A missed
    /// bump leaves tracks holding stale transport state until some other
    /// bump; an extra bump only costs one redundant push per track. A
    /// per-cycle advance of `transport_sample`/`session_transport_sample`
    /// does NOT bump the generation; the mirrored snapshot carries it.
    ///
    /// The atomic loads/stores below and the stores inside the lock all run
    /// on the dispatcher thread; workers only reach `last_prepare_generation`
    /// through this same path, so a stale comparison (e.g. a track added
    /// mid-generation) self-heals on the next dispatch.
    pub(crate) fn prepare_task_track(&self, task: &ProcessTask) {
        let track = match task {
            ProcessTask::Track(t) | ProcessTask::FolderInput(t) | ProcessTask::FolderOutput(t) => t,
            ProcessTask::Plugin { track, .. } => track,
        };
        // Mirror the dispatcher-thread transport positions into the shared
        // snapshot before this dispatch's tasks can run. Release stores; the
        // worker's Acquire load at task start is ordered by the node-job
        // channel handoff.
        self.transport.transport_sample_snapshot.mirror(
            self.transport.transport_sample,
            self.transport.session_transport_sample,
        );
        if track.transport_sample_snapshot().is_none() {
            // First dispatch after an explicit detach (offline bounce or
            // freeze render repositioned the track): re-attach. Cheap atomic
            // check on the fast path.
            track
                .attach_transport_sample_snapshot(self.transport.transport_sample_snapshot.clone());
        }
        if track.last_prepare_generation() == self.transport.prepare_generation {
            // Already pushed for this generation; skip the track lock. The
            // RT worker marks the track `processing` at task start, so the
            // `set_processing(true)` below (and the lock) are unnecessary
            // here.
            return;
        }
        let mut t = track.lock();
        self.transport.transport_sample_snapshot.set_use_session(
            self.transport.session_clip_playback_enabled && self.transport.playing,
        );
        t.set_loop_config(
            self.transport.loop_enabled,
            self.transport.loop_range_samples,
        );
        t.set_transport_timing(
            self.transport.tempo_bpm,
            self.transport.tsig_num,
            self.transport.tsig_denom,
        );
        t.set_clip_playback_enabled(self.transport.clip_playback_enabled && self.transport.playing);
        t.set_session_clip_playback_enabled(
            self.transport.session_clip_playback_enabled && self.transport.playing,
        );
        t.set_record_tap_enabled(self.transport.playing && self.recording.record_enabled);
        t.audio.set_processing(true);
        t.mark_prepare_pushed(self.transport.prepare_generation);
    }

    pub(crate) fn bump_prepare_generation(&mut self) {
        self.transport.prepare_generation += 1;
    }

    /// Dispatch queued node jobs to ready workers, buffering the rest until
    /// a worker reports `Ready`. Returns true if the cycle completed while
    /// dispatching (only possible via abandoned nodes).
    pub(crate) async fn dispatch_node_jobs(&mut self, jobs: Vec<crate::executor::NodeJob>) -> bool {
        self.pending_node_jobs.extend(jobs);
        let mut cycle_complete = false;
        while !self.pending_node_jobs.is_empty() {
            let Some(worker_index) = self.take_ready_worker_index() else {
                break;
            };
            let Some(job) = self.pending_node_jobs.pop_front() else {
                break;
            };
            if let Some(crate::render_plan::Op::Task { task, .. }) =
                job.plan.nodes.get(job.node as usize)
            {
                self.prepare_task_track(task);
            }
            let worker = &mut self.workers[worker_index];
            if let Some(node_job_tx) = worker.node_job_tx.as_mut() {
                match node_job_tx.push(job) {
                    Ok(()) => {
                        if let Some(thread) = &worker.node_thread {
                            thread.unpark();
                        }
                    }
                    Err(rtrb::PushError::Full(job)) => {
                        self.pending_node_jobs.push_front(job);
                        self.push_ready_worker(worker_index);
                        break;
                    }
                }
            } else {
                let node = job.node;
                error!("Worker {worker_index} has no node-job mailbox");
                let outcome = self.executor.abandon_node(node, Instant::now());
                self.log_silenced_nodes(&outcome.silenced);
                cycle_complete |= outcome.cycle_complete;
                self.pending_node_jobs.extend(outcome.jobs);
            }
        }
        cycle_complete
    }

    pub(crate) async fn start_plan_cycle(&mut self) -> bool {
        // While a bounce job exists, plan cycles are suspended: the bounce
        // worker renders through live track bodies and must be their only
        // mutator (LOCKLESS.md Phase 5, 5b-iii).
        if !self.transport.playing
            || !self.executor.cycle_complete()
            || !self.dispatch.offline_bounce_jobs.is_empty()
        {
            return false;
        }
        self.refresh_realtime_infection();
        self.ensure_metronome_wiring();
        let jobs = self.executor.start_cycle(Instant::now());
        if self.dispatch_node_jobs(jobs).await {
            self.on_all_tracks_finished().await;
            return true;
        }
        false
    }

    /// Handle a worker completion: cascade the executor, publish meters and
    /// parameter echoes, force timed-out nodes, and finish the cycle when
    /// all nodes are done.
    pub(crate) async fn on_node_done(
        &mut self,
        worker_id: usize,
        epoch: u64,
        node: u32,
        output_linear: Vec<f32>,
        parameter_updates: Vec<Action>,
        latency_changed: bool,
    ) {
        self.push_ready_worker(worker_id);
        let mut complete = self.dispatch_node_jobs(Vec::new()).await;
        if epoch != self.executor.epoch() {
            tracing::debug!(
                "dropping stale NodeDone (epoch {} vs {}) for node {}",
                epoch,
                self.executor.epoch(),
                node
            );
            return;
        }
        if latency_changed {
            self.publish_state_snapshot();
            self.plan_builder.mark_dirty();
        }
        let plan = self.executor.plan().clone();
        if let Some(crate::render_plan::Op::Task { task, .. }) = plan.nodes.get(node as usize) {
            let track_name = Self::task_track_name(task);
            self.meters
                .track_meter_linear_by_track
                .insert(track_name, output_linear);
        }
        for action in parameter_updates {
            self.notify_clients(Ok(action)).await;
        }
        let now = Instant::now();
        let (jobs, done) = self.executor.on_node_done(epoch, node, now);
        complete |= done;
        complete |= self.dispatch_node_jobs(jobs).await;
        let outcome = self
            .executor
            .force_timeouts(now, Self::TRACK_PROCESS_TIMEOUT);
        self.log_silenced_nodes(&outcome.silenced);
        complete |= self.dispatch_node_jobs(outcome.jobs).await;
        complete |= outcome.cycle_complete;
        if complete {
            self.on_all_tracks_finished().await;
        }
    }

    pub(crate) async fn poll_node_worker_results(&mut self) {
        let mut results = Vec::new();
        for worker in &mut self.workers {
            if let Some(rx) = worker.node_result_rx.as_mut() {
                while let Ok(result) = rx.pop() {
                    results.push(result);
                }
            }
        }
        for result in results {
            self.on_node_done(
                result.worker_id,
                result.epoch,
                result.node,
                result.output_linear,
                result.parameter_updates,
                result.latency_changed,
            )
            .await;
        }
    }

    pub(crate) async fn poll_jack_hw_finished(&mut self) {
        #[cfg(unix)]
        {
            let finished = self
                .jack_runtime
                .as_ref()
                .map(|jack| jack.take_hw_finished_count())
                .unwrap_or(0);
            if finished > 0 {
                self.handle_hw_finished().await;
            }
        }
    }

    /// Periodic tick (called from the engine loop's interval): force
    /// timed-out nodes even when no worker message arrives.
    pub(crate) async fn on_executor_tick(&mut self) {
        if self.executor.cycle_complete() {
            return;
        }
        let outcome = self
            .executor
            .force_timeouts(Instant::now(), Self::TRACK_PROCESS_TIMEOUT);
        self.log_silenced_nodes(&outcome.silenced);
        let mut complete = self.dispatch_node_jobs(outcome.jobs).await;
        complete |= outcome.cycle_complete;
        if complete {
            self.on_all_tracks_finished().await;
        }
    }

    pub(crate) fn log_silenced_nodes(&self, silenced: &[u32]) {
        for &node in silenced {
            let plan = self.executor.plan();
            let name = match plan.nodes.get(node as usize) {
                Some(crate::render_plan::Op::Task { task, .. }) => Self::task_track_name(task),
                _ => format!("node {node}"),
            };
            tracing::warn!(
                "Node {} ('{}') exceeded process timeout ({} ms); forced silent completion for cycle",
                node,
                name,
                Self::TRACK_PROCESS_TIMEOUT.as_millis()
            );
        }
    }

    pub(crate) async fn on_all_tracks_finished(&mut self) {
        // Hand deferred bounce jobs to their workers now that no plan cycle
        // is in flight (see handle_track_offline_bounce).
        let pending = std::mem::take(&mut self.dispatch.pending_bounce_starts);
        for (worker_index, job) in pending {
            self.send_bounce_job(worker_index, job).await;
        }
        if self.transport.transport_restart_pending {
            let state = self.state_snapshot.load_full();
            for track in state.tracks.values() {
                track.lock().take_hw_midi_out_events();
            }
        } else if self.hw_worker.is_some() {
            self.hw_midi.active_hw_notes_cycle_start =
                self.hw_midi.active_hw_notes_by_track.clone();
            let mut out_events = self.collect_hw_midi_output_events_by_device();
            if self.transport.loop_enabled
                && let Some((_, loop_end)) = self.transport.loop_range_samples
            {
                let cycle_end = self
                    .transport
                    .transport_sample
                    .saturating_add(self.current_cycle_samples());
                if self.transport.transport_sample < loop_end && cycle_end >= loop_end {
                    let wrap_frame = loop_end
                        .saturating_sub(self.transport.transport_sample)
                        .min(self.current_cycle_samples())
                        as u32;
                    out_events.extend(self.note_off_events_for_active_snapshot(
                        &self.hw_midi.active_hw_notes_cycle_start,
                        wrap_frame,
                    ));
                    out_events.sort_by(|a, b| {
                        a.event
                            .frame
                            .cmp(&b.event.frame)
                            .then_with(|| a.device.cmp(&b.device))
                    });
                }
            }
            self.hw_midi
                .pending_hw_midi_out_events_by_device
                .extend(out_events);
        } else {
            self.hw_midi.pending_hw_midi_out_events = self.collect_hw_midi_output_events();
        }
        self.request_hw_cycle().await;
    }

    pub(crate) async fn handle_request(&mut self, a: Action) {
        match a {
            Action::Log { source, message } => {
                self.notify_event(Event::Log { source, message }).await;
            }
            Action::Undo => {
                let actions = match self.history.undo() {
                    Some(actions) => actions,
                    None => {
                        self.notify_clients(Ok(Action::Undo)).await;
                        self.notify_event(Event::HistoryState {
                            dirty: self.history.is_dirty(),
                        })
                        .await;
                        return;
                    }
                };

                let was_suspended = self.history_suspended;
                self.history_suspended = true;
                for action in actions {
                    self.handle_request_inner(action, false).await;
                }
                self.history_suspended = was_suspended;
                self.notify_clients(Ok(Action::Undo)).await;
                self.notify_event(Event::HistoryState {
                    dirty: self.history.is_dirty(),
                })
                .await;
            }
            Action::Redo => {
                let actions = match self.history.redo() {
                    Some(actions) => actions,
                    None => {
                        self.notify_clients(Ok(Action::Redo)).await;
                        self.notify_event(Event::HistoryState {
                            dirty: self.history.is_dirty(),
                        })
                        .await;
                        return;
                    }
                };

                let was_suspended = self.history_suspended;
                self.history_suspended = true;
                for action in actions {
                    self.handle_request_inner(action, false).await;
                }
                self.history_suspended = was_suspended;
                self.notify_clients(Ok(Action::Redo)).await;
                self.notify_event(Event::HistoryState {
                    dirty: self.history.is_dirty(),
                })
                .await;
            }
            Action::ApplyGroupedActions(actions) => {
                self.handle_request_inner(Action::BeginHistoryGroup, true)
                    .await;
                for action in actions {
                    self.handle_request_inner(action, true).await;
                }
                self.handle_request_inner(Action::EndHistoryGroup, true)
                    .await;
            }
            Action::Session(_) => {
                self.handle_request_inner(a, false).await;
            }
            other => {
                self.handle_request_inner(other, true).await;
            }
        }
        self.publish_state_snapshot();
    }

    #[inline]
    pub(crate) fn box_bool<'a>(
        fut: impl std::future::Future<Output = bool> + Send + 'a,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(fut)
    }

    pub(crate) async fn handle_request_inner(
        &mut self,
        mut action_to_process: Action,
        record_history: bool,
    ) {
        let a = action_to_process.clone();
        let suppress_timing_history = self.transport.playing
            && matches!(
                &action_to_process,
                Action::SetTempo(_) | Action::SetTimeSignature { .. } | Action::SetTempoMap { .. }
            );
        let mut inverse_actions = self.prepare_inverse_actions(
            &action_to_process,
            record_history,
            suppress_timing_history,
        );

        match action_to_process {
            // Transport: play/pause/stop, position, loop, punch, metronome,
            // tempo/tempo map, OSC enable.
            Action::Play
            | Action::Pause
            | Action::Stop
            | Action::SessionPlay
            | Action::JumpToEnd
            | Action::TransportPosition(..)
            | Action::SetLoopEnabled(_)
            | Action::SetLoopRange(..)
            | Action::SetPunchEnabled(_)
            | Action::SetPunchRange(_)
            | Action::SetMetronomeEnabled(_)
            | Action::SetTempo(_)
            | Action::SetTimeSignature { .. }
            | Action::SetTempoMap { .. }
            | Action::SetOscEnabled(_) => {
                if self.handle_transport_request(a.clone()).await {
                    return;
                }
            }
            // Recording: record enable, step recording, input arming.
            Action::SetRecordEnabled(..)
            | Action::SetStepRecording(_)
            | Action::TrackToggleArm(..) => {
                if self.handle_recording_request(a.clone()).await {
                    return;
                }
            }
            // Session view: clip/scene runtime, session path, session slots.
            Action::Session(_)
            | Action::SetClipPlaybackEnabled(_)
            | Action::SetSessionClipPlaybackEnabled(_)
            | Action::SetSessionPath(_)
            | Action::SessionArmMidiLearn { .. }
            | Action::TrackSetSessionSlot { .. }
            | Action::TrackSetSessionSlotPlayEnabled { .. }
            | Action::TrackSetSessionSlotStopEnabled { .. } => {
                if self.handle_session_request(a.clone()).await {
                    return;
                }
            }
            // Offline bounce.
            Action::TrackOfflineBounce { .. }
            | Action::TrackOfflineBounceCancel { .. }
            | Action::TrackOfflineBounceCancelAll
            | Action::TrackOfflineBounceCanceled { .. }
            | Action::TrackOfflineBounceProgress { .. } => {
                if self.handle_bounce_request(a.clone()).await {
                    return;
                }
            }
            // Meters.
            Action::RequestMeterSnapshot => {
                if self.handle_meter_request(a.clone()).await {
                    return;
                }
            }
            // Modulators and track automation.
            Action::SetModulators(_)
            | Action::SetTrackAutomationLanes { .. }
            | Action::TrackAutomationToggleLane { .. }
            | Action::TrackAutomationInsertPoint { .. }
            | Action::TrackAutomationDeletePoint { .. }
            | Action::TrackAutomationSetMode { .. }
            | Action::TrackAutomationLevel(..)
            | Action::TrackAutomationBalance(..) => {
                if self.handle_automation_request(a.clone()).await {
                    return;
                }
            }
            // Read-only queries.
            Action::RequestTrackList
            | Action::RequestTransportState
            | Action::RequestSessionDiagnostics
            | Action::RequestMidiLearnMappingsReport => {
                if self.handle_query_request(a.clone()).await {
                    return;
                }
            }
            // History and session restore.
            Action::BeginHistoryGroup
            | Action::EndHistoryGroup
            | Action::MarkHistorySavePoint
            | Action::ClearHistory
            | Action::BeginSessionRestore
            | Action::EndSessionRestore
            | Action::Undo
            | Action::Redo
            | Action::ApplyGroupedActions(_) => {
                if self.handle_history_request(a.clone()).await {
                    return;
                }
            }
            // MIDI: piano key input, MIDI clip edits, MIDI learn, hardware
            // MIDI device open, panic.
            Action::Panic
            | Action::TrackMidiCc { .. }
            | Action::TrackArmMidiLearn { .. }
            | Action::GlobalArmMidiLearn { .. }
            | Action::TrackSetMidiLearnBinding { .. }
            | Action::SetGlobalMidiLearnBinding { .. }
            | Action::SetSessionMidiLearnBinding { .. }
            | Action::PianoKey { .. }
            | Action::ModifyMidiNotes { .. }
            | Action::ModifyMidiControllers { .. }
            | Action::DeleteMidiControllers { .. }
            | Action::InsertMidiControllers { .. }
            | Action::DeleteMidiNotes { .. }
            | Action::InsertMidiNotes { .. }
            | Action::SetMidiSysExEvents { .. }
            | Action::OpenMidiInputDevice(_)
            | Action::OpenMidiOutputDevice(_)
            | Action::ClearAllMidiLearnBindings => {
                if self.handle_midi_request(a.clone()).await {
                    return;
                }
            }
            // Topology: tracks, routing, connections, clips.
            Action::AddTrack { .. }
            | Action::TrackAddAudioInput(..)
            | Action::TrackAddAudioOutput(..)
            | Action::TrackRemoveAudioInput(..)
            | Action::TrackRemoveAudioOutput(..)
            | Action::RenameTrack { .. }
            | Action::TrackLevel(..)
            | Action::TrackBalance(..)
            | Action::TrackToggleMute(..)
            | Action::TrackTogglePhase(..)
            | Action::TrackToggleSolo(..)
            | Action::TrackToggleMaster(..)
            | Action::TrackToggleInputMonitor { .. }
            | Action::TrackToggleDiskMonitor { .. }
            | Action::TrackToggleMidiInputMonitor { .. }
            | Action::TrackToggleMidiDiskMonitor { .. }
            | Action::TrackSetColor { .. }
            | Action::TrackSetFolder { .. }
            | Action::TrackSetParent { .. }
            | Action::TrackToggleFolder { .. }
            | Action::TrackSetMidiLaneChannel { .. }
            | Action::TrackSetMpeZone { .. }
            | Action::TrackSetMpePitchBendSensitivity { .. }
            | Action::TrackSetFrozen { .. }
            | Action::TrackClearDefaultPassthrough { .. }
            | Action::ClipMove { .. }
            | Action::AddClip { .. }
            | Action::AddGroupedClip { .. }
            | Action::RemoveClip { .. }
            | Action::MoveClipToUnused { .. }
            | Action::DeleteUnusedClips { .. }
            | Action::SetUnusedClips { .. }
            | Action::RenameClip { .. }
            | Action::SetClipIdentity { .. }
            | Action::SetClipSourceName { .. }
            | Action::SetClipFade { .. }
            | Action::SetClipBounds { .. }
            | Action::SyncClipBounds { .. }
            | Action::SetClipMuted { .. }
            | Action::SetClipReversed { .. }
            | Action::SetClipGainDb { .. }
            | Action::SetClipPluginGraphJson { .. }
            | Action::SetClipPitchCorrection { .. }
            | Action::Connect { .. }
            | Action::Disconnect { .. } => {
                if self.handle_topology_request(a.clone()).await {
                    return;
                }
            }
            // Plugins (non-LV2).
            Action::TrackClearPlugins { .. }
            | Action::TrackGetClapNoteNames { .. }
            | Action::TrackGetPluginGraph { .. }
            | Action::TrackConnectPluginAudio { .. }
            | Action::TrackConnectPluginMidi { .. }
            | Action::TrackDisconnectPluginAudio { .. }
            | Action::TrackDisconnectPluginMidi { .. }
            | Action::TrackConnectAudio { .. }
            | Action::TrackDisconnectAudio { .. }
            | Action::TrackConnectMidi { .. }
            | Action::TrackDisconnectMidi { .. }
            | Action::ListVst3Plugins
            | Action::ListClapPlugins
            | Action::ListClapPluginsWithCapabilities
            | Action::TrackLoadClapPlugin { .. }
            | Action::TrackUnloadClapPlugin { .. }
            | Action::TrackUnloadClapPluginInstance { .. }
            | Action::TrackShowClapGui { .. }
            | Action::ClipShowClapGui { .. }
            | Action::TrackLoadVst3Plugin { .. }
            | Action::TrackUnloadVst3Plugin { .. }
            | Action::TrackUnloadVst3PluginInstance { .. }
            | Action::TrackShowVst3Gui { .. }
            | Action::ClipShowVst3Gui { .. }
            | Action::TrackSetPluginResourceDir { .. }
            | Action::TrackClapCollectResources { .. }
            | Action::ClipSetPluginResourceDir { .. }
            | Action::ClipClapCollectResources { .. }
            | Action::TrackSetClapParameter { .. }
            | Action::ClipSetClapParameter { .. }
            | Action::ClipGetClapParameters { .. }
            | Action::TrackSetClapParameterAt { .. }
            | Action::TrackBeginClapParameterEdit { .. }
            | Action::TrackEndClapParameterEdit { .. }
            | Action::TrackGetClapParameters { .. }
            | Action::TrackClapSnapshotState { .. }
            | Action::ClipClapSnapshotState { .. }
            | Action::TrackClapRestoreState { .. }
            | Action::ClipClapRestoreState { .. }
            | Action::TrackSnapshotAllClapStates { .. }
            | Action::TrackGetVst3Graph { .. }
            | Action::TrackSetVst3Parameter { .. }
            | Action::ClipSetVst3Parameter { .. }
            | Action::TrackSetPluginBypassed { .. }
            | Action::TrackGetVst3Parameters { .. }
            | Action::ClipGetVst3Parameters { .. }
            | Action::TrackVst3SnapshotState { .. }
            | Action::ClipVst3SnapshotState { .. }
            | Action::TrackVst3RestoreState { .. }
            | Action::TrackConnectVst3Audio { .. }
            | Action::TrackDisconnectVst3Audio { .. } => {
                if self.handle_plugin_request(a.clone()).await {
                    return;
                }
            }
            // Plugins (LV2, Unix only).
            #[cfg(unix)]
            Action::TrackSetLv2PluginState { .. }
            | Action::ClipSetLv2PluginState { .. }
            | Action::TrackGetLv2Midnam { .. }
            | Action::ListLv2Plugins
            | Action::TrackLoadLv2Plugin { .. }
            | Action::TrackUnloadLv2Plugin { .. }
            | Action::TrackUnloadLv2PluginInstance { .. }
            | Action::TrackShowLv2Gui { .. }
            | Action::ClipShowLv2Gui { .. }
            | Action::TrackSetLv2ControlValue { .. }
            | Action::ClipSetLv2ControlValue { .. }
            | Action::TrackGetLv2PluginControls { .. }
            | Action::ClipGetLv2PluginControls { .. }
            | Action::TrackLv2SnapshotState { .. }
            | Action::ClipLv2SnapshotState { .. } => {
                if self.handle_plugin_request(a.clone()).await {
                    return;
                }
            }
            // Hardware: JACK graph operations.
            Action::JackAddAudioInputPort
            | Action::JackRemoveAudioInputPort(..)
            | Action::JackAddAudioOutputPort
            | Action::JackRemoveAudioOutputPort(..)
            | Action::JackGetGraph
            | Action::JackConnect { .. }
            | Action::JackDisconnect { .. } => {
                if self.handle_hardware_request(a.clone()).await {
                    return;
                }
            }
            Action::Quit => {
                self.handle_quit(a.clone()).await;
                return;
            }
            Action::RemoveTrack(ref name) => {
                self.handle_remove_track(name.clone(), record_history).await;
                inverse_actions = None;
            }
            Action::OpenAudioDevice { .. } => {
                let (done, updated) = self.handle_open_audio_device(a.clone()).await;
                if done {
                    return;
                }
                if let Some(action) = updated {
                    action_to_process = action;
                }
            }
            _ => {}
        }

        if let Some(inverse) = inverse_actions {
            if let Some(group) = self.history_group.as_mut() {
                group.forward_actions.push(action_to_process.clone());
                group.inverse_actions.splice(0..0, inverse);
            } else {
                self.history.record(UndoEntry {
                    forward_actions: vec![action_to_process.clone()],
                    inverse_actions: inverse,
                });
            }
        }

        self.notify_clients(Ok(action_to_process)).await;
    }
    pub async fn work(&mut self) {
        // Wake immediately when a fixed-thread node worker pushes a result;
        // a slower periodic tick still force-completes timed-out nodes.
        let mut timeout_tick = tokio::time::interval(Duration::from_millis(10));
        timeout_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let message = tokio::select! {
                message = self.rx.recv() => {
                    let Some(message) = message else {
                        break;
                    };
                    tracing::debug!(?message, "engine work loop received message");
                    Some(message)
                }
                _ = self.node_result_notify.notified() => {
                    tracing::trace!("engine work loop woken by node result");
                    None
                }
                _ = timeout_tick.tick() => {
                    tracing::trace!("engine work loop timeout tick");
                    None
                }
            };
            self.poll_node_worker_results().await;
            self.poll_jack_hw_finished().await;
            self.poll_stopped_plugin_parameter_echoes().await;
            if !self.transport.playing && !self.transport.transport_running {
                self.publish_clap_state_dirty().await;
            }
            self.on_executor_tick().await;
            let Some(message) = message else {
                continue;
            };
            match message {
                Message::Ready(id) => {
                    // A bounce worker's terminal Ready cleans up its job even
                    // when the OfflineBounceFinished payload was an Err
                    // without a track name.
                    if let Some(track_name) = self.dispatch.bounce_worker_tracks.remove(&id) {
                        self.dispatch.offline_bounce_jobs.remove(&track_name);
                    }
                    self.push_ready_worker(id);
                    if self.dispatch_node_jobs(Vec::new()).await {
                        self.on_all_tracks_finished().await;
                    }
                    self.drain_pending_requests_if_idle().await;
                }
                Message::NodeDone {
                    worker_id,
                    epoch,
                    node,
                    output_linear,
                    parameter_updates,
                    latency_changed,
                } => {
                    self.on_node_done(
                        worker_id,
                        epoch,
                        node,
                        output_linear,
                        parameter_updates,
                        latency_changed,
                    )
                    .await;
                }
                Message::Channel(s) => {
                    self.clients.push(s);
                }
                Message::Response(result) => {
                    self.notify_clients(result).await;
                }

                Message::Request(a) => {
                    self.dispatch_request(a).await;
                    // Any request may have changed the topology; the builder
                    // coalesces bursts into a single plan rebuild.
                    self.plan_builder.mark_dirty();
                }
                Message::OscRequest { action, reply_to } => {
                    tracing::debug!(%reply_to, ?action, "engine received OscRequest");
                    self.osc_reply_target = Some(reply_to);
                    self.dispatch_request(action).await;
                    self.osc_reply_target = None;
                    self.plan_builder.mark_dirty();
                }
                Message::OfflineBounceFinished { result } => {
                    if let Ok(Action::TrackOfflineBounce { track_name, .. })
                    | Ok(Action::TrackOfflineBounceCanceled { track_name, .. }) = &result
                    {
                        self.dispatch.offline_bounce_jobs.remove(track_name);
                    }
                    self.notify_clients(result).await;
                    self.drain_pending_requests_if_idle().await;
                }
                Message::HWFinished => {
                    if !self.transport.awaiting_hwfinished {
                        tracing::debug!(
                            playing = self.transport.playing,
                            transport_running = self.transport.transport_running,
                            transport_sample = self.transport.transport_sample,
                            session_transport_sample = self.transport.session_transport_sample,
                            cycle_samples = self.current_cycle_samples(),
                            "HWFinished ignored because engine was not awaiting it"
                        );
                        continue;
                    }
                    tracing::debug!(
                        playing = self.transport.playing,
                        transport_running = self.transport.transport_running,
                        transport_sample = self.transport.transport_sample,
                        session_transport_sample = self.transport.session_transport_sample,
                        cycle_samples = self.current_cycle_samples(),
                        "HWFinished handling"
                    );
                    self.transport.handling_hwfinished = true;
                    self.transport.awaiting_hwfinished = false;
                    #[cfg(unix)]
                    {
                        if let Some(jack) = self.jack_runtime.as_mut() {
                            if !self.hw_midi.pending_hw_midi_out_events.is_empty() {
                                let out_events =
                                    std::mem::take(&mut self.hw_midi.pending_hw_midi_out_events);
                                jack.write_events(&out_events);
                            }
                            let mut in_events = vec![];
                            jack.read_events_into(&mut in_events);
                            if !in_events.is_empty() {
                                self.hw_midi.pending_hw_midi_events.extend(in_events);
                            }
                            let dropped = jack.take_midi_events_dropped();
                            if dropped > 0 {
                                tracing::warn!(
                                    "JACK MIDI ring full; {dropped} events dropped since last cycle"
                                );
                            }
                        }
                    }
                    #[cfg(unix)]
                    if self.jack_runtime.is_some() {
                        self.sync_from_jack_transport().await;
                    }
                    while let Some(a) = self.dispatch.pending_requests.pop_front() {
                        self.handle_request(a).await;
                    }
                    self.apply_mute_solo_policy();
                    self.append_recorded_cycle();
                    self.flush_completed_recordings().await;
                    let hw_in_routes = self.hw_midi.midi_hw_in_routes.clone();
                    let pending_hw_in_by_device =
                        self.hw_midi.pending_hw_midi_events_by_device.clone();
                    let mut reconfigured_tracks = Vec::new();
                    let state = self.state_snapshot.load_full();
                    for (track_name, track) in state.tracks.iter() {
                        let mut track_lock = track.lock();
                        if self.jack_runtime_is_some() {
                            if !self.hw_midi.pending_hw_midi_events.is_empty() {
                                track_lock
                                    .push_hw_midi_events(&self.hw_midi.pending_hw_midi_events);
                            }
                        } else {
                            for route in hw_in_routes.iter().filter(|r| &r.to_track == track_name) {
                                if let Some(events) = pending_hw_in_by_device.get(&route.device) {
                                    track_lock.push_hw_midi_events_to_port(route.to_port, events);
                                }
                            }
                        }
                        if track_lock.setup() {
                            reconfigured_tracks.push(track_name.clone());
                        }
                    }
                    self.publish_track_meters().await;
                    self.publish_session_runtime_reports().await;
                    self.publish_clap_state_dirty().await;
                    for track_name in reconfigured_tracks {
                        let track = state.tracks.get(&track_name).cloned();
                        if let Some(track) = track {
                            let (plugins, connections, connectable_connections) = {
                                let track_lock = track.lock();
                                (
                                    track_lock.plugin_graph_plugins(false),
                                    track_lock.plugin_graph_connections(),
                                    track_lock.connectable_connections(),
                                )
                            };
                            self.notify_query_reply(QueryReply::TrackPluginGraph {
                                track_name: track_name.clone(),
                                plugins,
                                connections,
                                connectable_connections,
                            })
                            .await;
                        }
                    }
                    self.hw_midi.pending_hw_midi_events.clear();
                    self.hw_midi.pending_hw_midi_events_by_device.clear();
                    let cycle_samples = self.current_cycle_samples();
                    if self.transport.transport_running {
                        if self.transport.transport_panic_flush_pending {
                            self.transport.transport_panic_flush_pending = false;
                        } else if self.transport.transport_restart_pending {
                            self.transport.transport_restart_pending = false;
                        } else {
                            let before = self.transport.transport_sample;
                            let next = self
                                .transport
                                .transport_sample
                                .saturating_add(cycle_samples);
                            let normalized = self.transport.normalize_transport_sample(next);
                            let wrapped = normalized != next;
                            self.transport.transport_sample = normalized;
                            // The per-cycle advance reaches tracks through
                            // the mirrored lock-free snapshot; see
                            // `handle_hw_finished`.
                            tracing::debug!(
                                before,
                                delta = cycle_samples,
                                next,
                                normalized,
                                wrapped,
                                "transport advanced after HWFinished"
                            );
                            self.publish_transport_snapshot();
                            if wrapped {
                                if self.transport.notified_loop_wrap_sample
                                    == Some(self.transport.transport_sample)
                                {
                                    self.transport.notified_loop_wrap_sample = None;
                                } else {
                                    self.notify_event(Event::TransportPosition(
                                        self.transport.transport_sample,
                                    ))
                                    .await;
                                }
                            }
                        }
                    } else {
                        tracing::debug!(
                            playing = self.transport.playing,
                            cycle_samples,
                            "transport not advanced because transport_running is false"
                        );
                    }
                    if self.transport.session_clip_playback_enabled && self.transport.playing {
                        let before = self.transport.session_transport_sample;
                        self.transport.session_transport_sample = self
                            .transport
                            .session_transport_sample
                            .saturating_add(cycle_samples);
                        tracing::debug!(
                            before,
                            delta = cycle_samples,
                            after = self.transport.session_transport_sample,
                            "session transport advanced after HWFinished"
                        );
                    }
                    {
                        let echoes = self.apply_modulators(self.active_transport_sample());
                        self.dispatch_automation_echoes(echoes).await;
                    }
                    self.apply_mixosc_automation(self.active_transport_sample());
                    let cycle_started = self.start_plan_cycle().await;
                    // If a plan cycle is still running, its completion will request the
                    // hardware cycle. Requesting here would replay stale arena buffers.
                    if self.hw_worker.is_some()
                        && !cycle_started
                        && (self.transport.playing || self.audio_preview.is_some())
                        && self.executor.cycle_complete()
                    {
                        self.request_hw_cycle().await;
                    }
                    tracing::debug!(
                        cycle_started,
                        hw_worker = self.hw_worker.is_some(),
                        awaiting_hwfinished = self.transport.awaiting_hwfinished,
                        executor_complete = self.executor.cycle_complete(),
                        "HWFinished rearm decision"
                    );
                    #[cfg(unix)]
                    {
                        if self.jack_runtime.is_some() {
                            self.transport.awaiting_hwfinished = true;
                        }
                    }
                    self.transport.handling_hwfinished = false;
                }
                Message::HWMidiEvents(events) => {
                    for hw_event in events {
                        let thru_targets: Vec<String> = self
                            .hw_midi
                            .midi_hw_thru_routes
                            .iter()
                            .filter(|route| route.from_device == hw_event.device)
                            .map(|route| route.to_device.clone())
                            .collect();
                        for device in thru_targets {
                            self.hw_midi
                                .pending_hw_midi_out_events_by_device
                                .push(HwMidiEvent {
                                    device,
                                    event: hw_event.event.clone(),
                                });
                        }
                        if hw_event.event.data.len() >= 3 {
                            let status = hw_event.event.data[0];
                            if status & 0xF0 == 0xB0 {
                                let channel = status & 0x0F;
                                let cc = hw_event.event.data[1];
                                let value = hw_event.event.data[2];
                                self.handle_incoming_hw_cc(&hw_event.device, channel, cc, value)
                                    .await;
                            }
                            if self.recording.step_recording_enabled && status & 0xF0 == 0x90 {
                                let channel = status & 0x0F;
                                let pitch = hw_event.event.data[1];
                                let velocity = hw_event.event.data[2];
                                if velocity > 0 {
                                    self.notify_event(Event::StepRecordMidiNote {
                                        device: hw_event.device.clone(),
                                        channel,
                                        pitch,
                                        velocity,
                                    })
                                    .await;
                                }
                            }
                        }
                        self.hw_midi
                            .pending_hw_midi_events_by_device
                            .entry(hw_event.device)
                            .or_default()
                            .push(hw_event.event);
                    }
                }
                Message::StartAudioPreview {
                    samples,
                    channels,
                    start_sample,
                } => {
                    self.audio_preview = Some(AudioPreviewPlayback {
                        samples,
                        channels: channels.max(1),
                        cursor: start_sample,
                    });
                    self.meters.meter_decay_after_stop = None;
                    self.set_hw_playing(true).await;
                    if !self.transport.awaiting_hwfinished && self.executor.cycle_complete() {
                        self.request_hw_cycle().await;
                    }
                }
                Message::StopAudioPreview => {
                    self.audio_preview = None;
                }
                _ => {}
            }
        }
    }
}
