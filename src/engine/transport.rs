use super::*;
use crate::engine::fields::TransportFields;
use crate::track::Track;
use tracing::error;

impl Engine {
    pub(crate) fn update_global_tempo_from_map(&mut self) {
        let (bpm, num, den) = self.transport.timing_at_sample(0);
        self.transport.tempo_bpm = bpm;
        self.transport.tsig_num = num;
        self.transport.tsig_denom = den;
        self.bump_prepare_generation();
    }

    pub(crate) fn active_transport_sample(&self) -> usize {
        if self.transport.session_clip_playback_enabled && self.transport.playing {
            self.transport.session_transport_sample
        } else {
            self.transport.transport_sample
        }
    }

    pub(crate) fn scheduled_loop_wrap_for_next_cycle(&self) -> Option<(usize, usize, usize)> {
        if !self.transport.playing || !self.transport.loop_enabled {
            return None;
        }
        let (loop_start, loop_end) = self.transport.loop_range_samples?;
        if loop_end <= loop_start || self.transport.transport_sample >= loop_end {
            return None;
        }
        let cycle_samples = self.current_cycle_samples();
        if cycle_samples == 0 {
            return None;
        }
        let next = self
            .transport
            .transport_sample
            .saturating_add(cycle_samples);
        if next < loop_end {
            return None;
        }
        let after_frames = loop_end.saturating_sub(self.transport.transport_sample);
        Some((
            after_frames,
            loop_start,
            self.transport.normalize_transport_sample(next),
        ))
    }

    pub(crate) fn cycle_segments(&self, frames: usize) -> Vec<(usize, usize, usize)> {
        if frames == 0 {
            return vec![];
        }
        if !self.transport.loop_enabled {
            return vec![(
                self.transport.transport_sample,
                self.transport.transport_sample.saturating_add(frames),
                0,
            )];
        }
        let Some((loop_start, loop_end)) = self.transport.loop_range_samples else {
            return vec![(
                self.transport.transport_sample,
                self.transport.transport_sample.saturating_add(frames),
                0,
            )];
        };
        if loop_end <= loop_start {
            return vec![(
                self.transport.transport_sample,
                self.transport.transport_sample.saturating_add(frames),
                0,
            )];
        }
        let mut segments = Vec::new();
        let mut remaining = frames;
        let mut out_offset = 0usize;
        let mut current = self.transport.transport_sample;
        while remaining > 0 {
            let take = loop_end.saturating_sub(current).min(remaining);
            if take == 0 {
                current = loop_start;
                continue;
            }
            segments.push((current, current.saturating_add(take), out_offset));
            out_offset = out_offset.saturating_add(take);
            remaining -= take;
            current = if remaining > 0 {
                loop_start
            } else {
                current.saturating_add(take)
            };
        }
        segments
    }

    pub(crate) fn set_osc_enabled_with<F>(
        &mut self,
        enabled: bool,
        start_server: F,
    ) -> Result<(), String>
    where
        F: FnOnce(Sender<Message>) -> Result<OscServer, String>,
    {
        if enabled {
            if self.osc_server.is_none() {
                self.osc_server = Some(start_server(self.tx.clone())?);
            }
        } else if let Some(mut server) = self.osc_server.take() {
            server.stop();
        }
        Ok(())
    }

    #[cfg(unix)]
    pub(crate) async fn handle_hw_finished(&mut self) {
        if !self.transport.awaiting_hwfinished {
            return;
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
                    let out_events = std::mem::take(&mut self.hw_midi.pending_hw_midi_out_events);
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
        let pending_hw_in_by_device = self.hw_midi.pending_hw_midi_events_by_device.clone();
        let mut reconfigured_tracks = Vec::new();
        let state = self.state_snapshot.load_full();
        for (track_name, track) in state.tracks.iter() {
            let mut track_lock = track.lock();
            if self.jack_runtime_is_some() {
                if !self.hw_midi.pending_hw_midi_events.is_empty() {
                    track_lock.push_hw_midi_events(&self.hw_midi.pending_hw_midi_events);
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
                // The per-cycle advance reaches tracks through the mirrored
                // lock-free snapshot, so no generation bump is needed here.
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

    pub(crate) fn publish_transport_snapshot(&mut self) {
        let snapshot = self.transport.transport_snapshot_producer.write_buffer();
        snapshot.sample = self.transport.transport_sample;
        snapshot.tempo_bpm = self.transport.tempo_bpm;
        snapshot.playing = self.transport.playing;
        snapshot.transport_running = self.transport.transport_running;
        snapshot.tsig_num = self.transport.tsig_num;
        snapshot.tsig_denom = self.transport.tsig_denom;
        self.transport.transport_snapshot_producer.publish();
    }

    pub(crate) async fn ensure_metronome_track(&mut self) {
        if self.state.lock().tracks.contains_key(Self::METRONOME_TRACK) {
            return;
        }
        let (cycle_samples, sample_rate_hz, output_channels): (usize, f64, usize) =
            if let Some(info) = self.hw_driver_info {
                (
                    info.cycle_samples,
                    info.sample_rate as f64,
                    info.output_channels,
                )
            } else {
                #[cfg(unix)]
                {
                    if let Some(jack) = &self.jack_runtime {
                        (
                            jack.buffer_size,
                            jack.sample_rate as f64,
                            jack.audio_outs().len(),
                        )
                    } else {
                        return;
                    }
                }
                #[cfg(not(unix))]
                {
                    return;
                }
            };
        if output_channels == 0 {
            return;
        }
        self.state.lock().tracks.insert(
            Self::METRONOME_TRACK.to_string(),
            Arc::new(Track::new(
                Self::METRONOME_TRACK.to_string(),
                0,
                1,
                0,
                0,
                cycle_samples.max(1),
                sample_rate_hz.max(1.0),
            )),
        );
        if let Some(track) = self.state.lock().tracks.get(Self::METRONOME_TRACK).cloned() {
            track.lock().set_level(Self::METRONOME_DEFAULT_LEVEL_DB);
            track
                .lock()
                .set_metronome_enabled(self.transport.metronome_enabled);
        }
        self.notify_clients(Ok(Action::AddTrack {
            name: Self::METRONOME_TRACK.to_string(),
            audio_ins: 0,
            midi_ins: 0,
            audio_outs: 1,
            midi_outs: 0,
            folder: false,
            mixosc_addr: None,
        }))
        .await;
        self.notify_clients(Ok(Action::TrackLevel(
            Self::METRONOME_TRACK.to_string(),
            Self::METRONOME_DEFAULT_LEVEL_DB,
        )))
        .await;
    }

    pub(crate) async fn handle_play(&mut self, action: Action) -> bool {
        let Action::Play = action else {
            return false;
        };

        tracing::debug!(
            "Action::Play pressed, transport_sample={} awaiting={} handling={}",
            self.transport.transport_sample,
            self.transport.awaiting_hwfinished,
            self.transport.handling_hwfinished
        );
        self.meters.meter_decay_after_stop = None;
        self.transport.playing = true;
        self.bump_prepare_generation();
        self.transport.transport_running = true;
        self.transport.transport_restart_pending = true;
        self.transport.notified_loop_wrap_sample = None;
        self.publish_transport_snapshot();
        self.set_hw_playing(true).await;
        #[cfg(unix)]
        if let Some(jack) = &self.jack_runtime
            && let Err(e) = jack.transport_start()
        {
            self.notify_clients(Err(e)).await;
        }
        self.notify_clients(Ok(Action::Play)).await;
        self.notify_event(Event::TransportPosition(self.transport.transport_sample))
            .await;
        self.preload_track_clips().await;
        {
            let echoes = self.apply_modulators(self.active_transport_sample());
            self.dispatch_automation_echoes(echoes).await;
        }
        if !self.transport.awaiting_hwfinished && !self.transport.handling_hwfinished {
            let completed = self.start_plan_cycle().await;
            if completed {
                self.transport.transport_restart_pending = false;
            }
        }

        false
    }

    pub(crate) async fn handle_pause(&mut self, action: Action) -> bool {
        let Action::Pause = action else {
            return false;
        };

        self.transport.clip_playback_enabled = false;
        self.transport.session_clip_playback_enabled = false;
        self.bump_prepare_generation();
        for track in self.state_snapshot.load_full().tracks.values() {
            let mut t = track.lock();
            t.set_clip_playback_enabled(false);
            t.set_session_clip_playback_enabled(false);
        }
        self.transport.transport_running = false;
        self.publish_transport_snapshot();
        if !self.transport.playing {
            self.transport.playing = true;
            self.bump_prepare_generation();
            self.transport.transport_restart_pending = true;
            self.transport.notified_loop_wrap_sample = None;
            self.publish_transport_snapshot();
            self.set_hw_playing(true).await;
            #[cfg(unix)]
            if let Some(jack) = &self.jack_runtime
                && let Err(e) = jack.transport_start()
            {
                self.notify_clients(Err(e)).await;
            }
            self.preload_track_clips().await;
            if !self.transport.awaiting_hwfinished && !self.transport.handling_hwfinished {
                let completed = self.start_plan_cycle().await;
                if completed {
                    self.transport.transport_restart_pending = false;
                }
            }
        }
        self.notify_clients(Ok(Action::Pause)).await;
        self.notify_event(Event::TransportPosition(self.transport.transport_sample))
            .await;

        false
    }

    pub(crate) async fn handle_stop(&mut self, action: Action) -> bool {
        let Action::Stop = action else {
            return false;
        };

        self.transport.playing = false;
        self.bump_prepare_generation();
        self.transport.transport_running = false;
        self.transport.transport_panic_flush_pending = false;
        self.transport.transport_restart_pending = false;
        self.transport.notified_loop_wrap_sample = None;
        self.transport.clip_playback_enabled = true;
        self.transport.session_clip_playback_enabled = false;
        self.transport.session_transport_sample = 0;
        self.bump_prepare_generation();
        self.session.session_scene_queue = None;
        self.session.session_scene_queue_length_samples = 0;
        self.session.session_current_scene = None;
        self.session.session_current_scene_previous_scene = None;
        self.session.session_current_scene_start_sample = 0;
        self.session.session_current_scene_length_samples = 0;
        self.session.session_completed_clip_passes.clear();
        self.session.session_reported_clip_passes.clear();
        for track in self.state_snapshot.load_full().tracks.values() {
            let mut t = track.lock();
            t.set_clip_playback_enabled(true);
            t.set_session_clip_playback_enabled(false);
            t.stop_all_session_clips_immediate();
        }
        self.publish_transport_snapshot();
        self.set_hw_playing(false).await;
        #[cfg(unix)]
        if let Some(jack) = &self.jack_runtime
            && let Err(e) = jack.transport_stop()
        {
            self.notify_clients(Err(e)).await;
        }
        let panic_events = self.note_off_events_for_all_active_tracks();
        if let Some(worker) = &self.hw_worker {
            if !panic_events.is_empty()
                && let Err(e) = worker.tx.send(Message::HWMidiOutEvents(panic_events)).await
            {
                error!("Error sending stop MIDI panic events {e}");
            }
        } else {
            self.hw_midi
                .pending_hw_midi_out_events_by_device
                .extend(panic_events);
        }
        self.reset_meters_after_stop();
        self.flush_recordings().await;
        self.notify_event(Event::TransportPosition(self.transport.transport_sample))
            .await;

        false
    }

    pub(crate) async fn handle_session_play(&mut self, action: Action) -> bool {
        let Action::SessionPlay = action else {
            return false;
        };

        self.transport.playing = true;
        self.transport.transport_running = false;
        self.transport.transport_restart_pending = true;
        self.transport.notified_loop_wrap_sample = None;
        self.transport.clip_playback_enabled = false;
        self.transport.session_clip_playback_enabled = true;
        self.transport.session_transport_sample = 0;
        self.bump_prepare_generation();
        self.session.session_scene_queue = None;
        self.session.session_scene_queue_length_samples = 0;
        self.session.session_current_scene = None;
        self.session.session_current_scene_previous_scene = None;
        self.session.session_current_scene_start_sample = 0;
        self.session.session_current_scene_length_samples = 0;
        self.session.session_completed_clip_passes.clear();
        self.session.session_reported_clip_passes.clear();
        for track in self.state_snapshot.load_full().tracks.values() {
            track.lock().stop_all_session_clips_immediate();
        }
        self.publish_transport_snapshot();
        self.set_hw_playing(true).await;
        #[cfg(unix)]
        if let Some(jack) = &self.jack_runtime
            && let Err(e) = jack.transport_start()
        {
            self.notify_clients(Err(e)).await;
        }
        self.notify_event(Event::TransportPosition(self.transport.transport_sample))
            .await;
        self.preload_track_clips().await;
        {
            let echoes = self.apply_modulators(self.active_transport_sample());
            self.dispatch_automation_echoes(echoes).await;
        }
        if !self.transport.awaiting_hwfinished && !self.transport.handling_hwfinished {
            let completed = self.start_plan_cycle().await;
            if completed {
                self.transport.transport_restart_pending = false;
            }
        }

        false
    }

    pub(crate) async fn handle_transport_position(&mut self, a: Action) -> bool {
        let Action::TransportPosition(sample) = a else {
            return false;
        };

        self.transport.transport_sample = self.transport.normalize_transport_sample(sample);
        self.bump_prepare_generation();
        self.transport.notified_loop_wrap_sample = None;
        self.publish_transport_snapshot();
        {
            let echoes = self.apply_modulators(self.active_transport_sample());
            self.dispatch_automation_echoes(echoes).await;
        }
        #[cfg(unix)]
        if let Some(jack) = &self.jack_runtime
            && let Err(e) = jack.transport_locate(self.transport.transport_sample)
        {
            self.notify_clients(Err(e)).await;
        }
        if self.transport.playing {
            self.transport.transport_restart_pending = true;
            self.transport.transport_panic_flush_pending = self.hw_worker.is_some();
            self.clear_hw_midi_output_state(true).await;
            // The running cycle (if any) finishes naturally — at most one
            // block at the old position; the next dispatch reads the new
            // transport sample.
            if !self.transport.awaiting_hwfinished && !self.transport.handling_hwfinished {
                let completed = self.start_plan_cycle().await;
                if completed {
                    self.transport.transport_restart_pending = false;
                }
            }
        }

        false
    }

    pub(crate) async fn handle_set_loop_range(&mut self, a: Action) -> bool {
        let Action::SetLoopRange(range) = a else {
            return false;
        };

        self.transport.loop_range_samples = range.and_then(|(start, end)| {
            if end > start {
                Some((start, end))
            } else {
                None
            }
        });
        self.transport.loop_enabled = self.transport.loop_range_samples.is_some();
        self.bump_prepare_generation();
        self.transport.notified_loop_wrap_sample = None;
        if self.transport.loop_enabled
            && let Some((loop_start, loop_end)) = self.transport.loop_range_samples
            && self.transport.transport_sample >= loop_end
        {
            self.transport.transport_sample = loop_start;
            self.bump_prepare_generation();
            self.notify_event(Event::TransportPosition(self.transport.transport_sample))
                .await;
        }

        false
    }
}

impl Engine {
    /// Transport-related request arms: play/pause/stop, position, loop,
    /// punch, metronome, tempo, OSC enable.
    pub(crate) async fn handle_transport_request(&mut self, a: Action) -> bool {
        match a {
            Action::Play => {
                if Self::box_bool(self.handle_play(a.clone())).await {
                    return true;
                }
            }
            Action::Pause => {
                if Self::box_bool(self.handle_pause(a.clone())).await {
                    return true;
                }
            }
            Action::Stop => {
                if Self::box_bool(self.handle_stop(a.clone())).await {
                    return true;
                }
            }
            Action::SessionPlay => {
                if Self::box_bool(self.handle_session_play(a.clone())).await {
                    return true;
                }
            }
            Action::JumpToEnd => {
                self.transport.transport_sample = self
                    .transport
                    .normalize_transport_sample(self.session_end_sample());
                self.bump_prepare_generation();
                self.publish_transport_snapshot();
                self.notify_event(Event::TransportPosition(self.transport.transport_sample))
                    .await;
            }
            Action::TransportPosition(..) => {
                if Self::box_bool(self.handle_transport_position(a.clone())).await {
                    return true;
                }
            }
            Action::SetLoopEnabled(enabled) => {
                self.transport.loop_enabled =
                    enabled && self.transport.loop_range_samples.is_some();
                self.bump_prepare_generation();
                self.transport.notified_loop_wrap_sample = None;
            }
            Action::SetLoopRange(..) => {
                if Self::box_bool(self.handle_set_loop_range(a.clone())).await {
                    return true;
                }
            }
            Action::SetPunchEnabled(enabled) => {
                self.transport.punch_enabled =
                    enabled && self.transport.punch_range_samples.is_some();
            }
            Action::SetPunchRange(range) => {
                self.transport.punch_range_samples = range.and_then(|(start, end)| {
                    if end > start {
                        Some((start, end))
                    } else {
                        None
                    }
                });
                self.transport.punch_enabled = self.transport.punch_range_samples.is_some();
            }
            Action::SetMetronomeEnabled(enabled) => {
                self.transport.metronome_enabled = enabled;
                if enabled {
                    self.ensure_metronome_track().await;
                }
                if let Some(track) = self
                    .state_snapshot
                    .load_full()
                    .tracks
                    .get(Self::METRONOME_TRACK)
                    .cloned()
                {
                    track.lock().set_metronome_enabled(enabled);
                }
            }
            Action::SetTempo(bpm) => {
                self.transport.tempo_bpm = bpm.max(1.0);
                self.bump_prepare_generation();
                self.publish_transport_snapshot();
            }
            Action::SetTimeSignature {
                numerator,
                denominator,
            } => {
                self.transport.tsig_num = numerator.max(1);
                self.transport.tsig_denom = denominator.max(1);
            }
            Action::SetTempoMap {
                ref tempo_points,
                ref time_signature_points,
            } => {
                self.transport.tempo_points = tempo_points.clone();
                self.transport.time_signature_points = time_signature_points.clone();
                self.update_global_tempo_from_map();
                self.publish_transport_snapshot();
            }
            Action::SetOscEnabled(enabled) => {
                if let Err(err) = self.set_osc_enabled_with(enabled, OscServer::start) {
                    self.notify_clients(Err(err)).await;
                }
            }
            _ => {}
        }
        false
    }
}

impl Engine {
    pub(crate) fn mix_audio_preview_into_hw_outputs(&mut self) {
        let cycle_samples = self.current_cycle_samples();
        if cycle_samples == 0 {
            return;
        }
        let plan = self.executor.plan().clone();
        let Some(preview) = self.audio_preview.as_mut() else {
            return;
        };
        let channels = preview.channels.max(1);
        let total_frames = preview.samples.len() / channels;
        if preview.cursor >= total_frames {
            self.audio_preview = None;
            return;
        }

        for &(buffer, channel) in &plan.hw_out_map {
            // Safety: request_hw_cycle runs after all render-plan producers
            // completed for this hardware cycle and before the hardware
            // backend reads the output arena.
            let dst = unsafe { &mut *plan.buffer_ptr(buffer) };
            let frames = cycle_samples.min(dst.len());
            dst[..frames].fill(0.0);
            let source_channel = channel.min(channels - 1);
            for (frame, out) in dst.iter_mut().take(frames).enumerate() {
                let source_frame = preview.cursor + frame;
                if source_frame >= total_frames {
                    break;
                }
                let sample_index = source_frame * channels + source_channel;
                *out = preview.samples.get(sample_index).copied().unwrap_or(0.0);
            }
        }

        preview.cursor = preview.cursor.saturating_add(cycle_samples);
        if preview.cursor >= total_frames {
            self.audio_preview = None;
        }
    }

    /// Start the next audio cycle: pull any published plan, copy hardware
    /// inputs, and dispatch the seed jobs. Returns true when the cycle
    /// completed instantly (empty plan) and `on_all_tracks_finished` ran.
    /// Ensure the metronome track's source port and output wiring exist
    /// before tasks are dispatched. Runs on the dispatcher at cycle top so
    /// `AudioIO.connections` is never mutated from a worker thread; marks
    /// the plan dirty when the wiring changed.
    pub(crate) fn ensure_metronome_wiring(&mut self) {
        let Some(track) = self
            .state_snapshot
            .load_full()
            .tracks
            .get(Self::METRONOME_TRACK)
            .cloned()
        else {
            return;
        };
        let frames = self.current_cycle_samples();
        let (_, changed) = track.lock().ensure_metronome_source(frames);
        if changed {
            self.plan_builder.mark_dirty();
        }
    }
}

impl TransportFields {
    pub(crate) fn timing_at_sample(&self, sample: usize) -> (f64, u16, u16) {
        let bpm = self
            .tempo_points
            .iter()
            .filter(|p| p.sample <= sample)
            .max_by_key(|p| p.sample)
            .map(|p| p.bpm)
            .unwrap_or(self.tempo_bpm)
            .max(1.0);
        let (num, den) = self
            .time_signature_points
            .iter()
            .filter(|p| p.sample <= sample)
            .max_by_key(|p| p.sample)
            .map(|p| (p.numerator.max(1), p.denominator.max(1)))
            .unwrap_or((self.tsig_num.max(1), self.tsig_denom.max(1)));
        (bpm, num, den)
    }

    pub(crate) fn normalize_transport_sample(&self, sample: usize) -> usize {
        if self.loop_enabled
            && let Some((loop_start, loop_end)) = self.loop_range_samples
            && loop_end > loop_start
            && sample >= loop_end
        {
            let loop_len = loop_end - loop_start;
            return loop_start + (sample - loop_start) % loop_len;
        }
        sample
    }
}

// ---------- Undo/history support (colocated in Phase 4; formerly the crate::history matches) ----------
/// Whether `action` is an undoable command owned by this feature
/// (colocated from `crate::history::should_record` in Phase 4).
pub(crate) fn undo_should_record(action: &Action) -> bool {
    matches!(
        action,
        Action::SetTempo(_)
            | Action::SetLoopEnabled(_)
            | Action::SetLoopRange(_)
            | Action::SetPunchEnabled(_)
            | Action::SetPunchRange(_)
            | Action::SetMetronomeEnabled(_)
            | Action::SetTimeSignature { .. }
            | Action::SetTempoMap { .. }
    )
}

impl Engine {
    /// Engine-state inverses for transport commands (colocated from
    /// `prepare_inverse_actions` in Phase 4). Returns `None` for actions
    /// owned by other features.
    pub(crate) fn undo_engine_state_inverse_transport(
        &self,
        action: &Action,
    ) -> Option<Vec<Action>> {
        match action {
            Action::SetTempo(_) => Some(vec![Action::SetTempo(self.transport.tempo_bpm)]),
            Action::SetLoopEnabled(_) => {
                Some(vec![Action::SetLoopEnabled(self.transport.loop_enabled)])
            }
            Action::SetLoopRange(_) => Some(vec![
                Action::SetLoopRange(self.transport.loop_range_samples),
                Action::SetLoopEnabled(self.transport.loop_enabled),
            ]),
            Action::SetPunchEnabled(_) => {
                Some(vec![Action::SetPunchEnabled(self.transport.punch_enabled)])
            }
            Action::SetPunchRange(_) => Some(vec![
                Action::SetPunchRange(self.transport.punch_range_samples),
                Action::SetPunchEnabled(self.transport.punch_enabled),
            ]),
            Action::SetMetronomeEnabled(_) => Some(vec![Action::SetMetronomeEnabled(
                self.transport.metronome_enabled,
            )]),
            Action::SetTimeSignature { .. } => Some(vec![Action::SetTimeSignature {
                numerator: self.transport.tsig_num,
                denominator: self.transport.tsig_denom,
            }]),
            Action::SetTempoMap { .. } => Some(vec![Action::SetTempoMap {
                tempo_points: self.transport.tempo_points.clone(),
                time_signature_points: self.transport.time_signature_points.clone(),
            }]),
            Action::SetClipPlaybackEnabled(_) => Some(vec![Action::SetClipPlaybackEnabled(
                self.transport.clip_playback_enabled,
            )]),
            _ => None,
        }
    }
}
