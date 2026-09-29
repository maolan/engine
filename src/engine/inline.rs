//! RT-inline cycle path (ARCHITECTURE.md Phase 2): the dispatcher side of
//! running the render plan on the hardware cycle thread.
//!
//! The Go sender ([`Engine::request_inline_cycle`]) replaces the pool
//! pipeline's `request_hw_cycle` when `rt_inline_enabled` is set: it prepares
//! the plan's task tracks (transport mirroring), republishes the shared
//! MIDI-in route table, arms the render flag, and sends `TracksFinished` —
//! the Go signal. The cycle thread then executes the plan between the
//! capture fill and the playback drain (see `inline_render.rs`), and the
//! `HWFinished` handler picks the per-cycle outcome up via
//! [`Engine::drain_inline_outcome`].

use super::*;
use crate::render_plan::Op;
use tracing::error;

impl Engine {
    /// Go sender for the RT-inline path. Mirrors the pool path's
    /// `request_hw_cycle` ordering (preview mix happens on the cycle thread
    /// post-render): gain/loop-wrap notification and MIDI-out flush first,
    /// then the render preparation, then the Go signal itself.
    pub(crate) async fn request_inline_cycle(&mut self) {
        if self.transport.awaiting_hwfinished {
            tracing::debug!(
                playing = self.transport.playing,
                transport_running = self.transport.transport_running,
                transport_sample = self.transport.transport_sample,
                "request_inline_cycle skipped because HWFinished is still pending"
            );
            return;
        }
        let playable = self.transport.playing || self.inline_render.preview_active();
        if !playable {
            return;
        }
        // While an offline bounce job exists the plan cycle is suspended (the
        // bounce worker must be the only track-body mutator); the hardware
        // cycle still runs, replaying the arena exactly like the pool path.
        let render = self.transport.playing && self.dispatch.offline_bounce_jobs.is_empty();

        self.send_hw_out_gain_balance().await;
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
        if let Some(worker) = &self.hw_worker
            && !self.hw_midi.pending_hw_midi_out_events_by_device.is_empty()
        {
            let out_events = std::mem::take(&mut self.hw_midi.pending_hw_midi_out_events_by_device);
            if let Err(e) = worker.tx.send(Message::HWMidiOutEvents(out_events)).await {
                error!("Error sending HWMidiOutEvents {e}");
            }
        }

        // Hardware MIDI-in events buffered between cycles (the hw worker's
        // poll drain): deliver them to the routed track ports now so the
        // upcoming render sees them. Events drained on the cycle thread were
        // already delivered there.
        self.push_pending_hw_midi_in_to_ports();

        // Resolve the current routes to track handles for the cycle thread.
        let routes = {
            let state = self.state_snapshot.load_full();
            self.hw_midi
                .midi_hw_in_routes
                .iter()
                .filter_map(|route| {
                    state.tracks.get(&route.to_track).map(|track| {
                        crate::inline_render::MidiInRoute {
                            device: route.device.clone(),
                            track: track.clone(),
                            port: route.to_port,
                        }
                    })
                })
                .collect()
        };
        self.inline_render.publish_midi_routes(routes);
        self.inline_render.request_render(render);
        // Phase 4: tag the cycle with the transport position it renders for;
        // the cycle thread compares it against the device's capture progress
        // and silences + reports a skip when the render would be stale.
        self.inline_render.publish_go(
            self.transport.transport_sample as i64,
            self.transport.transport_restart_pending,
        );

        if render {
            self.refresh_realtime_infection();
            self.ensure_metronome_wiring();
            if self.transport.clear_processing_buffers_pending {
                // No cycle is in flight (the previous HWFinished was
                // processed before this Go), so zeroing track/plan buffers is
                // race-free — the same invariant as `start_plan_cycle`.
                self.transport.clear_processing_buffers_pending = false;
                self.clear_processing_buffers_before_playback();
            }
            // Transport mirroring and the generation-gated track push must
            // land before the cycle thread's render reads them.
            let plan = self.plan_slot.load_full();
            for op in &plan.nodes {
                if let Op::Task { task, .. } = op {
                    self.prepare_task_track(task);
                }
            }
        }

        if let Some(worker) = &self.hw_worker {
            match worker.tx.send(Message::TracksFinished).await {
                Ok(_) => {
                    self.transport.awaiting_hwfinished = true;
                    crate::cycle_trace::mark(crate::cycle_trace::TracePoint::GoSent);
                }
                Err(e) => {
                    error!("Error sending TracksFinished {e}");
                }
            }
        }
    }

    /// `HWFinished` handler, inline branch: collect the cycle thread's
    /// outcome — per-node meters and parameter echoes, the plan that ran
    /// (for the hardware-output meter), and hardware MIDI-in events for
    /// thru-routing / MIDI-learn / step-record (already delivered to track
    /// ports by the cycle thread, so they are not buffered again).
    pub(crate) async fn drain_inline_outcome(&mut self) {
        let Some(outcome) = self.inline_render.take_outcome() else {
            return;
        };
        if outcome.stale {
            // Phase 4 back-pressure: the cycle thread silenced the playback
            // drain (the driver never emitted the stale render). Skip the
            // gap in the transport so subsequent tags realign with the
            // device, drop the anchor, and surface the xrun. The ring
            // positions resync via the driver's existing xrun jump.
            tracing::warn!(
                transport_rendered = outcome.transport_rendered,
                skipped_frames = outcome.skipped_frames,
                "RT-inline xrun: render stale by a full period; cycle silenced, transport skipping the gap"
            );
            self.transport.transport_sample = self
                .transport
                .transport_sample
                .saturating_add(outcome.skipped_frames as usize);
            self.inline_render.invalidate_anchor();
        }
        if let Some(plan) = outcome.plan {
            for result in outcome.results {
                if let Some(Op::Task { task, .. }) = plan.nodes.get(result.node as usize) {
                    let track_name = Self::task_track_name(task);
                    self.meters
                        .track_meter_linear_by_track
                        .insert(track_name, result.output_linear);
                }
                if result.latency_changed {
                    self.publish_state_snapshot();
                    self.plan_builder.mark_dirty();
                }
                for action in result.parameter_updates {
                    self.notify_clients(Ok(action)).await;
                }
            }
            // The arena still holds this cycle's render output; meter it
            // before the next render overwrites it.
            self.apply_hw_out_meter_from_plan(&plan).await;
        }
        if !outcome.midi_in_events.is_empty() {
            self.handle_hw_midi_in_events(outcome.midi_in_events, false)
                .await;
        }
    }

    /// Hardware MIDI-in event handling shared by the `HWMidiEvents` message
    /// (events from the hw worker's between-cycle drains, buffered for the
    /// track-port push when `buffer_for_ports`) and the inline cycle thread's
    /// forward (already delivered to ports; only thru/learn/step-record here).
    pub(crate) async fn handle_hw_midi_in_events(
        &mut self,
        events: Vec<HwMidiEvent>,
        buffer_for_ports: bool,
    ) {
        for hw_event in events {
            let thru_targets: Vec<String> = self
                .hw_midi
                .midi_hw_thru_routes
                .iter()
                .filter(|route| route.from_device == hw_event.device)
                .map(|route| route.to_device.clone())
                .collect();
            for device in thru_targets {
                self.hw_midi.pending_hw_midi_out_events_by_device.push(
                    crate::message::HwMidiEvent {
                        device,
                        event: hw_event.event.clone(),
                    },
                );
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
            if buffer_for_ports {
                self.hw_midi
                    .pending_hw_midi_events_by_device
                    .entry(hw_event.device)
                    .or_default()
                    .push(hw_event.event);
            }
        }
    }

    /// Deliver hardware MIDI-in events buffered between cycles to the routed
    /// track ports. On the pool path this runs inside `handle_hw_finished`;
    /// on the inline path the Go sender runs it (the cycle thread may
    /// concurrently hold no track lock — cycles only start at Go).
    pub(crate) fn push_pending_hw_midi_in_to_ports(&mut self) {
        if self.hw_midi.pending_hw_midi_events_by_device.is_empty() {
            return;
        }
        let routes = self.hw_midi.midi_hw_in_routes.clone();
        let pending = std::mem::take(&mut self.hw_midi.pending_hw_midi_events_by_device);
        let state = self.state_snapshot.load_full();
        for (track_name, track) in state.tracks.iter() {
            let mut track_lock = track.lock();
            for route in routes.iter().filter(|r| &r.to_track == track_name) {
                if let Some(events) = pending.get(&route.device) {
                    track_lock.push_hw_midi_events_to_port(route.to_port, events);
                }
            }
        }
    }

    /// Collect the hardware MIDI-out events produced by the cycle that just
    /// rendered, buffer them for the Go sender, and add loop-wrap note-offs.
    /// Extracted from `on_all_tracks_finished` so the inline `HWFinished`
    /// handler can run it at the same point in the cycle (before the
    /// transport advance, which the wrap math is relative to).
    pub(crate) fn collect_hw_midi_out_for_next_cycle(&mut self) {
        if self.transport.transport_restart_pending {
            let state = self.state_snapshot.load_full();
            for track in state.tracks.values() {
                track.lock().take_hw_midi_out_events();
            }
            return;
        }
        if self.hw_worker.is_none() {
            self.hw_midi.pending_hw_midi_out_events = self.collect_hw_midi_output_events();
            return;
        }
        self.hw_midi.active_hw_notes_cycle_start = self.hw_midi.active_hw_notes_by_track.clone();
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
                    .min(self.current_cycle_samples()) as u32;
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
    }

    /// Hand deferred bounce jobs to their workers at a cycle boundary (no
    /// render running). Shared by `on_all_tracks_finished` (pool path) and
    /// the inline `HWFinished` handler.
    pub(crate) async fn handoff_pending_bounce_starts(&mut self) {
        let pending = std::mem::take(&mut self.dispatch.pending_bounce_starts);
        for (worker_index, job) in pending {
            self.send_bounce_job(worker_index, job).await;
        }
    }
}
