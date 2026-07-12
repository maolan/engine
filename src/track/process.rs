use crate::audio::track::AudioTrack;
#[cfg(target_os = "macos")]
use crate::clap::ClapMidiOutputEvent;
use crate::message::{PluginGraphNode, PluginKind};
use crate::midi::track::MIDITrack;

use super::*;
use crate::{audio::io::AudioIO, midi::io::MIDIIO};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, atomic::Ordering},
};

impl Track {
    const METRONOME_DEFAULT_LEVEL_DB: f32 = -10.0;

    pub(crate) fn new_raw(
        name: String,
        io: TrackIoCounts,
        buffer_size: usize,
        sample_rate: f64,
        is_folder: bool,
    ) -> Self {
        Self {
            rt: TrackRtCell::new(TrackRt::new(io.audio_outs, buffer_size)),
            name,
            level: AtomicU32::new(0.0_f32.to_bits()),
            balance: AtomicU32::new(0.0_f32.to_bits()),
            armed: AtomicBool::new(false),
            muted: AtomicBool::new(false),
            phase_inverted: AtomicBool::new(false),
            soloed: AtomicBool::new(false),
            is_master: AtomicBool::new(false),
            input_monitor: ArcSwap::from_pointee(vec![false; io.audio_ins]),
            disk_monitor: ArcSwap::from_pointee(vec![true; io.audio_ins]),
            midi_input_monitor: ArcSwap::from_pointee(vec![false; io.midi_ins]),
            midi_disk_monitor: ArcSwap::from_pointee(vec![true; io.midi_ins]),
            color: None,
            midi_learn_volume: None,
            midi_learn_balance: None,
            midi_learn_mute: None,
            midi_learn_solo: None,
            midi_learn_arm: None,
            midi_learn_input_monitor: None,
            midi_learn_disk_monitor: None,
            is_folder,
            folder_open: AtomicBool::new(true),
            parent_track: None,
            child_tracks: Vec::new(),
            automation_lanes: serde_json::Value::Array(vec![]),
            automation_mode: AtomicU8::new(crate::message::TrackAutomationMode::Read.as_u8()),
            frozen: AtomicBool::new(false),
            midi_lane_channels: ArcSwap::from_pointee(vec![None; io.midi_ins]),
            primary_audio_ins: io.audio_ins,
            primary_audio_outs: io.audio_outs,
            audio: AudioTrack::new(io.audio_ins, io.audio_outs, buffer_size),
            midi: MIDITrack::new(io.midi_ins, io.midi_outs),
            clap_plugins: Vec::new(),
            vst3_plugins: Vec::new(),
            #[cfg(all(unix, not(target_os = "macos")))]
            lv2_plugins: Vec::new(),
            plugin_midi_connections: Vec::new(),
            next_clap_instance_id: AtomicUsize::new(0),
            next_vst3_instance_id: AtomicUsize::new(0),
            #[cfg(all(unix, not(target_os = "macos")))]
            next_lv2_instance_id: AtomicUsize::new(0),
            next_plugin_instance_id: AtomicUsize::new(0),
            sample_rate,
            process_block_size: AtomicUsize::new(buffer_size.max(1)),
            force_realtime_domain: false,
            shared_realtime_mixed: false,
            output_enabled: AtomicBool::new(true),
            metronome_enabled: AtomicBool::new(false),
            session_base_dir: None,
            metronome_source: ArcSwapOption::empty(),
        }
    }

    pub fn new(
        name: String,
        audio_ins: usize,
        audio_outs: usize,
        midi_ins: usize,
        midi_outs: usize,
        buffer_size: usize,
        sample_rate: f64,
    ) -> Self {
        Self::new_raw(
            name,
            TrackIoCounts {
                audio_ins,
                audio_outs,
                midi_ins,
                midi_outs,
            },
            buffer_size,
            sample_rate,
            false,
        )
        .with_default_passthrough()
    }

    pub fn new_folder(
        name: String,
        audio_ins: usize,
        audio_outs: usize,
        midi_ins: usize,
        midi_outs: usize,
        buffer_size: usize,
        sample_rate: f64,
    ) -> Self {
        Self::new_raw(
            name,
            TrackIoCounts {
                audio_ins,
                audio_outs,
                midi_ins,
                midi_outs,
            },
            buffer_size,
            sample_rate,
            true,
        )
        .with_default_passthrough()
    }

    pub(crate) fn alloc_plugin_instance_id(&self) -> usize {
        // Returns the pre-increment value, matching the old read-then-write.
        self.next_plugin_instance_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_add(1))
            })
            .expect("closure always returns Some")
    }

    pub fn setup(&mut self) -> bool {
        self.audio.setup();
        let mut reconfigured = false;
        for runtime in self.rt.clip_plugin_tracks.values() {
            for instance in &runtime.clap_plugins {
                instance.processor.run_host_callbacks_main_thread();
                match instance.processor.reconfigure_ports_if_needed() {
                    Ok(true) => reconfigured = true,
                    Err(e) => {
                        tracing::warn!(
                            "CLAP port reconfiguration failed for '{}': {}",
                            instance.processor.name(),
                            e
                        );
                    }
                    Ok(false) => {}
                }
            }
        }
        reconfigured
    }

    pub fn connect_directed_audio(from: &Arc<AudioIO>, to: &Arc<AudioIO>) {
        let new_len = {
            let mut conns = to.connections.lock();
            if !conns.iter().any(|conn| Arc::ptr_eq(conn, from)) {
                conns.push(from.clone());
            }
            conns.len()
        };
        to.connection_count.store(new_len, Ordering::Relaxed);
    }

    pub fn invalidate_audio_route_cache(&mut self) {
        self.rt.audio_route_cache_dirty = true;
    }

    pub fn primary_audio_ins(&self) -> usize {
        self.primary_audio_ins.min(self.audio.ins.len())
    }

    pub fn primary_audio_outs(&self) -> usize {
        self.primary_audio_outs.min(self.audio.outs.len())
    }

    pub(crate) fn ensure_audio_route_cache(&mut self) {
        if !self.rt.audio_route_cache_dirty
            && self.rt.internal_output_routes_cache.len() == self.audio.outs.len()
        {
            return;
        }
        let internal_sources = self.internal_audio_sources();
        let mut routes = Vec::with_capacity(self.audio.outs.len());
        for audio_out in &self.audio.outs {
            let connections = audio_out.connections.lock();
            let mut route_sources = Vec::new();
            for source in connections.iter() {
                if internal_sources
                    .iter()
                    .any(|candidate| Arc::ptr_eq(candidate, source))
                {
                    route_sources.push(source.clone());
                }
            }
            routes.push(route_sources);
        }
        self.rt.internal_output_routes_cache = routes;
        self.rt.audio_route_cache_dirty = false;
    }

    pub fn invalidate_midi_route_cache(&mut self) {
        self.rt.midi_route_cache_dirty = true;
    }

    pub(crate) fn ensure_midi_route_cache(&mut self) {
        if !self.rt.midi_route_cache_dirty
            && self.rt.midi_input_to_out_routes_cache.len() == self.midi.ins.len()
            && self.rt.midi_out_external_targets_cache.len() == self.midi.outs.len()
        {
            return;
        }

        let mut input_to_out = vec![Vec::<usize>::new(); self.midi.ins.len()];
        let mut out_external_targets = vec![Vec::<Arc<MIDIIO>>::new(); self.midi.outs.len()];

        for (out_idx, out) in self.midi.outs.iter().enumerate() {
            for source in out.sources() {
                if let Some(input_idx) = self
                    .midi
                    .ins
                    .iter()
                    .position(|input| Arc::ptr_eq(input, &source))
                {
                    input_to_out[input_idx].push(out_idx);
                }
            }
            for target in out.connections() {
                out_external_targets[out_idx].push(target);
            }
        }

        self.rt.midi_input_to_out_routes_cache = input_to_out;
        self.rt.midi_out_external_targets_cache = out_external_targets;
        self.rt.midi_route_cache_dirty = false;
    }

    #[inline(always)]
    pub(crate) fn copy_unity_with_zero_tail(dst: &mut [f32], src: &[f32]) {
        let len = dst.len().min(src.len());
        unsafe {
            std::ptr::copy_nonoverlapping(src.as_ptr(), dst.as_mut_ptr(), len);
        }
        if len < dst.len() {
            dst[len..].fill(0.0);
        }
    }

    #[inline(always)]
    pub(crate) fn copy_scaled_with_zero_tail(dst: &mut [f32], src: &[f32], gain: f32) {
        let len = dst.len().min(src.len());
        crate::simd::copy_scaled_inplace(&mut dst[..len], &src[..len], gain);
        if len < dst.len() {
            dst[len..].fill(0.0);
        }
    }

    #[inline(always)]
    pub(crate) fn add_unity(dst: &mut [f32], src: &[f32]) {
        crate::simd::add_inplace(dst, src);
    }

    #[inline(always)]
    pub(crate) fn add_scaled(dst: &mut [f32], src: &[f32], gain: f32) {
        crate::simd::add_scaled_inplace(dst, src, gain);
    }

    /// Ensure the metronome source port exists, is large enough, and is
    /// connected to every audio output. Returns the source and whether the
    /// wiring changed (so the caller can mark the plan dirty).
    ///
    /// Called from the dispatcher at cycle top only — never from a worker
    /// thread: mutating `connections` from a worker races the plan builder
    /// (LOCKLESS.md Phase 5).
    pub(crate) fn ensure_metronome_source(
        &mut self,
        frames: usize,
    ) -> (Option<Arc<AudioIO>>, bool) {
        if self.name != "metronome" || self.audio.outs.is_empty() {
            return (None, false);
        }
        let mut changed = false;
        let needed = frames.max(1);
        let needs_new = self
            .metronome_source
            .load_full()
            .as_ref()
            .is_none_or(|src| src.buffer.lock().len() < needed);
        if needs_new {
            self.metronome_source
                .store(Some(Arc::new(AudioIO::new(needed))));
            changed = true;
        }
        let Some(src) = self.metronome_source.load_full() else {
            return (None, changed);
        };
        for out in &self.audio.outs {
            if !out
                .connections
                .lock()
                .iter()
                .any(|conn| Arc::ptr_eq(conn, &src))
            {
                Self::connect_directed_audio(&src, out);
                changed = true;
            }
        }
        if changed {
            self.invalidate_audio_route_cache();
        }
        (Some(src), changed)
    }

    pub(crate) fn synthesize_metronome_into(&mut self, dst: &Arc<AudioIO>, frames: usize) {
        let mut buf = dst.buffer.lock();
        buf.fill(0.0);
        if !self.metronome_enabled() || !self.rt.clip_playback_enabled || frames == 0 {
            return;
        }
        let metronome_gain = 10.0_f32.powf((-Self::METRONOME_DEFAULT_LEVEL_DB) / 20.0);
        let sample_rate = self.sample_rate.max(1.0);
        let denom = self.rt.tsig_denom.max(1) as f64;
        let beats_per_bar = self.rt.tsig_num.max(1) as u64;
        let samples_per_beat = ((sample_rate * 60.0) / self.rt.tempo_bpm.max(1.0)) * (4.0 / denom);
        if !samples_per_beat.is_finite() || samples_per_beat <= 1.0 {
            return;
        }
        let segments = self.cycle_segments(frames);
        for (seg_start, seg_end, frame_offset) in segments {
            if seg_end <= seg_start {
                continue;
            }
            let mut beat_idx = ((seg_start as f64) / samples_per_beat).ceil() as u64;
            loop {
                let beat_sample = (beat_idx as f64 * samples_per_beat).round() as usize;
                if beat_sample >= seg_end {
                    break;
                }
                if beat_sample >= seg_start {
                    let hit_frame = frame_offset + (beat_sample - seg_start);
                    if hit_frame < frames {
                        let accented = beat_idx.is_multiple_of(beats_per_bar);
                        let freq = if accented { 1_760.0_f32 } else { 1_320.0_f32 };
                        let amp = if accented { 0.30_f32 } else { 0.22_f32 } * metronome_gain;
                        let click_len = ((sample_rate as usize) / 50).max(64);
                        let phase_step = core::f32::consts::TAU * (freq / sample_rate as f32);
                        let end = (hit_frame + click_len).min(frames).min(buf.len());
                        for (n, buf_n) in buf
                            .iter_mut()
                            .enumerate()
                            .skip(hit_frame)
                            .take(end - hit_frame)
                        {
                            let t = (n - hit_frame) as f32;
                            let env = (-t / (click_len as f32 * 0.28)).exp();
                            let s = (t * phase_step).sin() * amp * env;
                            *buf_n = (*buf_n + s).clamp(-1.0, 1.0);
                        }
                    }
                }
                beat_idx = beat_idx.saturating_add(1);
            }
        }
    }

    pub(crate) fn process_render_block(&mut self) -> usize {
        let live_mode = self.is_realtime_domain();
        let t0 = std::time::Instant::now();
        self.process_folder_input();
        let t5 = std::time::Instant::now();
        let frames = self.compute_process_frames();

        {
            let track_name = self.name.clone();
            let track_input_events = self.rt.folder_input_midi_events.clone();
            let can_skip_plugins = !live_mode
                && self.rt.last_render_block_silent
                && track_input_events.is_empty()
                && self.audio.ins.iter().all(|audio_in| {
                    let buf = audio_in.buffer.lock();
                    buf.iter().all(|&s| s == 0.0)
                });
            if can_skip_plugins {
                for instance in &self.clap_plugins {
                    for output in instance.processor.audio_outputs() {
                        output.buffer.lock().fill(0.0);
                    }
                }
                for instance in &self.vst3_plugins {
                    for output in instance.processor.audio_outputs() {
                        output.buffer.lock().fill(0.0);
                    }
                }
                #[cfg(all(unix, not(target_os = "macos")))]
                for instance in &self.lv2_plugins {
                    for output in instance.processor.audio_outputs() {
                        output.buffer.lock().fill(0.0);
                    }
                }
                self.rt.echoed_parameter_updates.clear();
                self.rt.folder_plugin_midi_node_events.clear();
                self.rt.folder_processed_midi_plugins.clear();
            } else {
                self.process_track_plugins_in_graph_order(frames);
            }
            let t6 = std::time::Instant::now();
            let t9 = std::time::Instant::now();
            let total = t9.duration_since(t0).as_secs_f64() * 1000.0;
            if total > 20.0 {
                let _clap_count = self.clap_plugins.len();
                let _vst3_count = self.vst3_plugins.len();
                #[cfg(all(unix, not(target_os = "macos")))]
                let _lv2_count = self.lv2_plugins.len();
                #[cfg(not(all(unix, not(target_os = "macos"))))]
                let _lv2_count = 0;
                tracing::warn!(
                    "Track '{}' process breakdown: total={:.1}ms clip_mix={:.1}ms plugins={:.1}ms midi_route={:.1}ms",
                    track_name,
                    total,
                    t5.duration_since(t0).as_secs_f64() * 1000.0,
                    t6.duration_since(t5).as_secs_f64() * 1000.0,
                    t9.duration_since(t6).as_secs_f64() * 1000.0,
                );
            }
        }

        self.process_folder_output();
        frames
    }

    pub fn process(&mut self) {
        let _ = self.process_render_block();
    }

    pub fn process_folder_input(&mut self) {
        for audio_in in &self.audio.ins {
            audio_in.process();
        }
        let frames = self
            .audio
            .ins
            .first()
            .map(|audio_in| audio_in.buffer.lock().len())
            .or_else(|| {
                self.audio
                    .outs
                    .first()
                    .map(|audio_out| audio_out.buffer.lock().len())
            })
            .unwrap_or(self.process_block_size());
        // The metronome source and its wiring are ensured by the dispatcher
        // at cycle top (never from a worker — see ensure_metronome_source);
        // here we only synthesize into the existing source.
        if let Some(source) = self.metronome_source.load_full() {
            self.synthesize_metronome_into(&source, frames);
        }
        let input_monitor = self.input_monitor();
        let disk_monitor = self.disk_monitor();
        let midi_disk_monitor = self.midi_disk_monitor();
        let audio_disk_active = disk_monitor.iter().any(|&m| m);
        let midi_disk_active = midi_disk_monitor.iter().any(|&m| m);
        self.rt.folder_clip_playback_active =
            (audio_disk_active || midi_disk_active) && self.rt.clip_playback_enabled;
        tracing::debug!(
            "process_folder_input for '{}' active={} disk={:?} clip_enabled={}",
            self.name,
            self.rt.folder_clip_playback_active,
            disk_monitor,
            self.rt.clip_playback_enabled
        );
        self.rt.folder_record_tap_input_snapshots = if self.armed() && self.rt.record_tap_enabled {
            self.audio
                .ins
                .iter()
                .map(|audio_in| audio_in.buffer.lock().to_vec())
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        let mut track_input_midi_events = self.collect_track_input_midi_events();
        let cycle_start = self.rt.transport_sample;
        let cycle_end = cycle_start.saturating_add(frames);
        let arrangement_active =
            self.rt.clip_playback_enabled && self.rt.folder_clip_playback_active;
        let session_active = (self.rt.clip_playback_enabled
            || self.rt.session_clip_playback_enabled)
            && (!self.rt.playing_session_clips.is_empty()
                || !self.rt.pending_session_launches.is_empty());
        if arrangement_active || session_active {
            self.process_session_clips(cycle_start, cycle_end, frames);
            if arrangement_active {
                self.mix_clip_midi_into_inputs(&mut track_input_midi_events, frames);
            }
            if session_active {
                self.mix_session_midi_into_inputs(&mut track_input_midi_events, frames);
            }
            for (lane, input) in self.midi.ins.iter().enumerate() {
                // Safety: plan single-writer invariant — this task is the sole
                // writer of its own ports this cycle; sources it reads were
                // produced by earlier plan nodes (LOCKLESS.md Phase 3).
                let mut buffer = unsafe { input.buffer_mut() };
                buffer.clear();
                if let Some(events) = track_input_midi_events.get(lane) {
                    buffer.extend_from_slice(events);
                }
                buffer.sort_by_key(|event| event.frame);
                input.mark_finished();
            }
            for (lane, audio_in) in self.audio.ins.iter().enumerate() {
                if !input_monitor.get(lane).copied().unwrap_or(false) {
                    audio_in.buffer.lock().fill(0.0);
                }
            }
            let mix_start = std::time::Instant::now();
            if arrangement_active {
                self.mix_clip_audio_into_inputs();
            }
            if session_active {
                self.mix_session_audio_into_inputs();
            }
            let mix_elapsed = mix_start.elapsed().as_secs_f64() * 1000.0;
            if mix_elapsed > 1.0 {
                tracing::warn!(
                    "mix session/clip audio into inputs for '{}' took {:.2}ms",
                    self.name,
                    mix_elapsed
                );
            }
        }

        self.rt.folder_input_midi_events = track_input_midi_events.clone();

        // Folder children receive the same input MIDI events as the folder.
        if !self.child_tracks.is_empty() {
            for child in &self.child_tracks {
                let child = child.lock();
                for (i, events) in track_input_midi_events.iter().enumerate() {
                    if let Some(child_in) = child.midi.ins.get(i) {
                        // Safety: plan edge folder-input → child serializes
                        // this write before the child's task runs.
                        unsafe { child_in.buffer_mut() }.extend_from_slice(events);
                    }
                }
            }
        }

        self.rt.folder_plugin_midi_node_events.clear();
        self.rt.folder_processed_midi_plugins.clear();
    }

    pub(crate) fn compute_process_frames(&self) -> usize {
        self.audio
            .ins
            .first()
            .map(|audio_in| audio_in.buffer.lock().len())
            .or_else(|| {
                self.audio
                    .outs
                    .first()
                    .map(|audio_out| audio_out.buffer.lock().len())
            })
            .unwrap_or(self.process_block_size())
    }

    pub fn process_plugin(&mut self, kind: PluginKind, index: usize) {
        let frames = self.compute_process_frames();
        let track_input_events = self.rt.folder_input_midi_events.clone();

        match kind {
            PluginKind::Clap => {
                if index >= self.clap_plugins.len() {
                    return;
                }
                let processor = self.clap_plugins[index].processor.clone();
                let ready = processor.audio_inputs().iter().all(|input| input.ready());
                let node = PluginGraphNode::ClapPluginInstance(self.clap_plugins[index].id);
                if !ready || !self.plugin_midi_ready(&node, &self.rt.folder_processed_midi_plugins)
                {
                    return;
                }
                for input in processor.audio_inputs() {
                    input.process();
                }
                self.plugin_midi_input_events(
                    &node,
                    processor.midi_input_count(),
                    &track_input_events,
                    &self.rt.folder_plugin_midi_node_events,
                );
                let outputs = processor.process_with_midi(
                    frames,
                    &[],
                    crate::plugins::types::ClapTransportInfo {
                        transport_sample: self.rt.transport_sample,
                        playing: (self.disk_monitor().iter().any(|&m| m)
                            || self.midi_disk_monitor().iter().any(|&m| m))
                            && self.rt.clip_playback_enabled,
                        loop_enabled: self.rt.loop_enabled,
                        loop_range_samples: self.rt.loop_range_samples,
                        bpm: self.rt.tempo_bpm,
                        tsig_num: self.rt.tsig_num,
                        tsig_denom: self.rt.tsig_denom,
                    },
                );
                let track_name = self.name.clone();
                for ev in processor.drain_echoed_parameters() {
                    self.rt.echoed_parameter_updates.push(
                        crate::message::Action::TrackSetClapParameter {
                            track_name: track_name.clone(),
                            instance_id: self.clap_plugins[index].id,
                            param_id: ev.param_index,
                            value: ev.value as f64,
                        },
                    );
                }
                for evt in outputs {
                    self.rt
                        .folder_plugin_midi_node_events
                        .entry((node.clone(), evt.port))
                        .or_default()
                        .push(evt.event);
                }
                self.rt.folder_processed_midi_plugins.insert(node);
            }
            PluginKind::Vst3 => {
                if index >= self.vst3_plugins.len() {
                    return;
                }
                let processor = self.vst3_plugins[index].processor.clone();
                let ready = processor.audio_inputs().iter().all(|input| input.ready());
                let node = PluginGraphNode::Vst3PluginInstance(self.vst3_plugins[index].id);
                if !ready || !self.plugin_midi_ready(&node, &self.rt.folder_processed_midi_plugins)
                {
                    return;
                }
                for input in processor.audio_inputs() {
                    input.process();
                }
                let midi_inputs = self.plugin_midi_input_events(
                    &node,
                    processor.midi_input_count(),
                    &track_input_events,
                    &self.rt.folder_plugin_midi_node_events,
                );
                let vst3_input = midi_inputs.first().cloned().unwrap_or_default();
                let outputs = processor.process_with_midi(frames, &vst3_input);
                let track_name = self.name.clone();
                for ev in processor.drain_echoed_parameters() {
                    self.rt.echoed_parameter_updates.push(
                        crate::message::Action::TrackSetVst3Parameter {
                            track_name: track_name.clone(),
                            instance_id: self.vst3_plugins[index].id,
                            param_id: ev.param_index,
                            value: ev.value,
                        },
                    );
                }
                if !outputs.is_empty() {
                    self.rt
                        .folder_plugin_midi_node_events
                        .insert((node.clone(), 0), outputs);
                }
                self.rt.folder_processed_midi_plugins.insert(node);
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            PluginKind::Lv2 => {
                if index >= self.lv2_plugins.len() {
                    return;
                }
                let processor = self.lv2_plugins[index].processor.clone();
                let ready = processor.audio_inputs().iter().all(|input| input.ready());
                let node = PluginGraphNode::Lv2PluginInstance(self.lv2_plugins[index].id);
                if !ready || !self.plugin_midi_ready(&node, &self.rt.folder_processed_midi_plugins)
                {
                    return;
                }
                for input in processor.audio_inputs() {
                    input.process();
                }
                let midi_inputs = self.plugin_midi_input_events(
                    &node,
                    processor.midi_input_count(),
                    &track_input_events,
                    &self.rt.folder_plugin_midi_node_events,
                );
                let lv2_input = midi_inputs.first().cloned().unwrap_or_default();
                let outputs = processor.process_with_midi(frames, &lv2_input);
                let track_name = self.name.clone();
                for ev in processor.drain_echoed_parameters() {
                    self.rt.echoed_parameter_updates.push(
                        crate::message::Action::TrackSetLv2ControlValue {
                            track_name: track_name.clone(),
                            instance_id: self.lv2_plugins[index].id,
                            index: ev.param_index,
                            value: ev.value,
                        },
                    );
                }
                if !outputs.is_empty() {
                    self.rt
                        .folder_plugin_midi_node_events
                        .insert((node.clone(), 0), outputs);
                }
                self.rt.folder_processed_midi_plugins.insert(node);
            }
        }
    }

    pub fn process_folder_output(&mut self) {
        let track_input_events = self.rt.folder_input_midi_events.clone();
        let midi_node_events = self.rt.folder_plugin_midi_node_events.clone();

        self.ensure_midi_route_cache();
        self.route_track_inputs_to_track_outputs(&track_input_events);
        self.route_plugin_midi_to_track_outputs_graph(&track_input_events, &midi_node_events);

        // Sum child-track MIDI outputs into the folder's MIDI outputs.
        for child in &self.child_tracks {
            let child = child.lock();
            for (out_idx, child_out) in child.midi.outs.iter().enumerate() {
                if let Some(folder_out) = self.midi.outs.get(out_idx) {
                    // Safety: plan edge child → folder-output serializes the
                    // child's write before this read.
                    let events = unsafe { child_out.buffer() }.to_vec();
                    if !events.is_empty() {
                        // Safety: plan single-writer invariant — the
                        // folder-output task is the sole writer of its own
                        // ports this cycle (LOCKLESS.md Phase 3).
                        unsafe { folder_out.buffer_mut() }.extend_from_slice(&events);
                    }
                }
            }
        }

        self.route_modulator_midi_to_track_outputs();
        self.route_automation_midi_to_track_outputs();
        self.collect_hw_midi_output_events();
        self.clear_local_midi_inputs();

        let linear_gain = 10.0_f32.powf(self.level() / 20.0);
        let phase_multiplier = if self.phase_inverted() { -1.0 } else { 1.0 };
        let (left_balance, right_balance) = if self.audio.outs.len() == 2 {
            let b = self.balance().clamp(-1.0, 1.0);
            ((1.0 - b).clamp(0.0, 1.0), (1.0 + b).clamp(0.0, 1.0))
        } else {
            (1.0, 1.0)
        };

        self.ensure_audio_route_cache();
        if self.rt.output_meter_linear_cache.len() != self.audio.outs.len() {
            self.rt
                .output_meter_linear_cache
                .resize(self.audio.outs.len(), 0.0);
        }
        if self.rt.meter_peak_hold_linear.len() != self.audio.outs.len() {
            self.rt
                .meter_peak_hold_linear
                .resize(self.audio.outs.len(), 0.0);
        }
        let clip_playback_active = self.rt.folder_clip_playback_active;
        let session_active = self.rt.session_clip_playback_enabled
            && (!self.rt.playing_session_clips.is_empty()
                || !self.rt.pending_session_launches.is_empty());
        let child_session_active = self.is_folder
            && self.child_tracks.iter().any(|child| {
                let c = child.lock();
                c.rt.session_clip_playback_enabled
                    && (!c.rt.playing_session_clips.is_empty()
                        || !c.rt.pending_session_launches.is_empty())
            });
        let record_tap_input_snapshots = self.rt.folder_record_tap_input_snapshots.clone();
        let input_monitor = self.input_monitor();
        let mut all_outputs_zero = true;
        for out_idx in 0..self.audio.outs.len() {
            let audio_out = self.audio.outs[out_idx].clone();
            let mut out_samples = audio_out.buffer.lock();
            let capture_record_tap = self.armed() && self.rt.record_tap_enabled;
            if capture_record_tap {
                if self.rt.record_tap_outs.len() <= out_idx {
                    self.rt.record_tap_outs.push(vec![0.0; out_samples.len()]);
                }
                if self.rt.record_tap_outs[out_idx].len() != out_samples.len() {
                    self.rt.record_tap_outs[out_idx].resize(out_samples.len(), 0.0);
                }
            }
            let balance_gain = if self.audio.outs.len() == 2 {
                if out_idx == 0 {
                    left_balance
                } else {
                    right_balance
                }
            } else {
                1.0
            };
            let output_gain = linear_gain * balance_gain * phase_multiplier;
            let unity_output_gain = (output_gain - 1.0).abs() <= f32::EPSILON;
            let sources = self.rt.internal_output_routes_cache.get(out_idx).cloned();
            let has_sources = sources.as_ref().is_some_and(|s| !s.is_empty());
            let mut wrote_output = false;
            if self.output_enabled()
                && let Some(sources) = &sources
            {
                let mut seeded = false;
                for source in sources {
                    let source_input_monitor = self
                        .audio
                        .ins
                        .iter()
                        .position(|input| Arc::ptr_eq(input, source))
                        .and_then(|idx| input_monitor.get(idx).copied())
                        .unwrap_or(false);
                    if !source_input_monitor
                        && !clip_playback_active
                        && !session_active
                        && !child_session_active
                        && self.is_track_input_source(source)
                    {
                        continue;
                    }
                    let source_buf = source.buffer.lock();
                    if !seeded {
                        if unity_output_gain {
                            Self::copy_unity_with_zero_tail(&mut out_samples, &source_buf);
                        } else {
                            Self::copy_scaled_with_zero_tail(
                                &mut out_samples,
                                &source_buf,
                                output_gain,
                            );
                        }
                        seeded = true;
                        wrote_output = true;
                    } else if unity_output_gain {
                        Self::add_unity(&mut out_samples, &source_buf);
                    } else {
                        Self::add_scaled(&mut out_samples, &source_buf, output_gain);
                    }
                }
            }
            if !wrote_output {
                out_samples.fill(0.0);
            }

            if capture_record_tap {
                let tap = &mut self.rt.record_tap_outs[out_idx];
                if has_sources {
                    if let Some(sources) = &sources {
                        let first_idx = self
                            .audio
                            .ins
                            .iter()
                            .position(|input| Arc::ptr_eq(input, &sources[0]));
                        if let Some(idx) = first_idx
                            .filter(|idx| !input_monitor.get(*idx).copied().unwrap_or(false))
                        {
                            Self::copy_unity_with_zero_tail(tap, &record_tap_input_snapshots[idx]);
                        } else {
                            let first = sources[0].buffer.lock();
                            Self::copy_unity_with_zero_tail(tap, &first);
                        }
                        for source in &sources[1..] {
                            if let Some(idx) = self
                                .audio
                                .ins
                                .iter()
                                .position(|input| Arc::ptr_eq(input, source))
                                .filter(|idx| !input_monitor.get(*idx).copied().unwrap_or(false))
                            {
                                Self::add_unity(tap, &record_tap_input_snapshots[idx]);
                            } else {
                                let source_buf = source.buffer.lock();
                                Self::add_unity(tap, &source_buf);
                            }
                        }
                    }
                } else if let Some(source) = record_tap_input_snapshots
                    .get(out_idx.min(record_tap_input_snapshots.len().saturating_sub(1)))
                {
                    Self::copy_unity_with_zero_tail(tap, source);
                } else {
                    tap.fill(0.0);
                }
            }
            let peak_now = crate::simd::peak_abs(&out_samples);
            if peak_now > 0.0 {
                all_outputs_zero = false;
            }

            let held = self.rt.meter_peak_hold_linear[out_idx] * 0.92;
            let next = peak_now.max(held);
            self.rt.meter_peak_hold_linear[out_idx] = next;
            self.rt.output_meter_linear_cache[out_idx] = next;
            audio_out.finished.store(true, Ordering::Release);
        }

        self.rt.last_render_block_silent = all_outputs_zero;
        self.audio.set_finished(true);
        self.audio.set_processing(false);
    }

    pub fn name(&self) -> String {
        self.name.clone()
    }
    pub fn set_name(&mut self, name: String) {
        self.name = name;
    }

    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }
    pub fn set_level(&self, level: f32) {
        self.level.store(level.to_bits(), Ordering::Relaxed);
    }
    pub fn balance(&self) -> f32 {
        f32::from_bits(self.balance.load(Ordering::Relaxed))
    }
    pub fn set_balance(&self, balance: f32) {
        self.balance
            .store(balance.clamp(-1.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    pub fn output_meter_linear(&self) -> Vec<f32> {
        self.rt.output_meter_linear_cache.clone()
    }

    pub fn clear_output_meters(&mut self) {
        for value in &mut self.rt.output_meter_linear_cache {
            *value = 0.0;
        }
        for value in &mut self.rt.meter_peak_hold_linear {
            *value = 0.0;
        }
    }

    pub fn arm(&self) {
        self.armed.fetch_not(Ordering::Relaxed);
    }
    pub fn armed(&self) -> bool {
        self.armed.load(Ordering::Relaxed)
    }

    pub fn set_output_enabled(&self, enabled: bool) {
        self.output_enabled.store(enabled, Ordering::Relaxed);
    }
    pub fn output_enabled(&self) -> bool {
        self.output_enabled.load(Ordering::Relaxed)
    }
    pub fn set_transport_sample(&mut self, sample: usize) {
        self.rt.with_rt(|rt| rt.transport_sample = sample);
    }
    pub fn set_loop_config(&mut self, enabled: bool, range: Option<(usize, usize)>) {
        self.rt.with_rt(|rt| {
            rt.loop_enabled = enabled;
            rt.loop_range_samples = range;
        });
    }
    pub fn set_transport_timing(&mut self, tempo_bpm: f64, tsig_num: u16, tsig_denom: u16) {
        self.rt.with_rt(|rt| {
            rt.tempo_bpm = tempo_bpm.max(1.0);
            rt.tsig_num = tsig_num.max(1);
            rt.tsig_denom = tsig_denom.max(1);
        });
    }
    pub fn set_clip_playback_enabled(&mut self, enabled: bool) {
        self.rt.with_rt(|rt| rt.clip_playback_enabled = enabled);
    }
    pub fn set_session_clip_playback_enabled(&mut self, enabled: bool) {
        self.rt
            .with_rt(|rt| rt.session_clip_playback_enabled = enabled);
    }
    pub fn set_metronome_enabled(&self, enabled: bool) {
        self.metronome_enabled.store(enabled, Ordering::Relaxed);
    }
    pub fn metronome_enabled(&self) -> bool {
        self.metronome_enabled.load(Ordering::Relaxed)
    }
    pub fn set_record_tap_enabled(&mut self, enabled: bool) {
        self.rt.with_rt(|rt| rt.record_tap_enabled = enabled);
    }

    pub fn set_midi_lane_channel(&mut self, lane: usize, channel: Option<u8>) {
        let mut lanes = self.midi_lane_channels();
        if let Some(slot) = Arc::make_mut(&mut lanes).get_mut(lane) {
            *slot = channel.map(|channel| channel.min(15));
            self.midi_lane_channels.store(lanes);
        }
    }

    pub fn input_monitor(&self) -> Arc<Vec<bool>> {
        self.input_monitor.load_full()
    }

    pub fn set_input_monitor(&self, monitor: Vec<bool>) {
        self.input_monitor.store(Arc::new(monitor));
    }

    pub fn disk_monitor(&self) -> Arc<Vec<bool>> {
        self.disk_monitor.load_full()
    }

    pub fn set_disk_monitor(&self, monitor: Vec<bool>) {
        self.disk_monitor.store(Arc::new(monitor));
    }

    pub fn midi_input_monitor(&self) -> Arc<Vec<bool>> {
        self.midi_input_monitor.load_full()
    }

    pub fn set_midi_input_monitor(&self, monitor: Vec<bool>) {
        self.midi_input_monitor.store(Arc::new(monitor));
    }

    pub fn midi_disk_monitor(&self) -> Arc<Vec<bool>> {
        self.midi_disk_monitor.load_full()
    }

    pub fn set_midi_disk_monitor(&self, monitor: Vec<bool>) {
        self.midi_disk_monitor.store(Arc::new(monitor));
    }

    pub fn midi_lane_channels(&self) -> Arc<Vec<Option<u8>>> {
        self.midi_lane_channels.load_full()
    }
    pub fn mute(&self) {
        self.muted.fetch_not(Ordering::Relaxed);
    }
    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }
    pub fn muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }
    pub fn invert_phase(&self) {
        self.phase_inverted.fetch_not(Ordering::Relaxed);
    }
    pub fn set_phase_inverted(&self, phase_inverted: bool) {
        self.phase_inverted.store(phase_inverted, Ordering::Relaxed);
    }
    pub fn phase_inverted(&self) -> bool {
        self.phase_inverted.load(Ordering::Relaxed)
    }
    pub fn solo(&self) {
        self.soloed.fetch_not(Ordering::Relaxed);
    }
    pub fn soloed(&self) -> bool {
        self.soloed.load(Ordering::Relaxed)
    }
    pub fn toggle_master(&self) {
        // A folder track can never become master; an already-master folder
        // is allowed to toggle off to recover from invalid legacy states.
        if !self.is_master() && self.is_folder {
            return;
        }
        self.is_master.fetch_not(Ordering::Relaxed);
    }
    pub fn set_master(&self, master: bool) {
        if master && self.is_folder {
            return;
        }
        self.is_master.store(master, Ordering::Relaxed);
    }
    pub fn is_master(&self) -> bool {
        self.is_master.load(Ordering::Relaxed)
    }
    pub fn toggle_input_monitor(&mut self, lane: usize) {
        let mut monitors = self.input_monitor();
        if let Some(monitor) = Arc::make_mut(&mut monitors).get_mut(lane) {
            *monitor = !*monitor;
            self.input_monitor.store(monitors);
        }
    }
    pub fn toggle_disk_monitor(&mut self, lane: usize) {
        let mut monitors = self.disk_monitor();
        if let Some(monitor) = Arc::make_mut(&mut monitors).get_mut(lane) {
            *monitor = !*monitor;
            self.disk_monitor.store(monitors);
        }
    }
    pub fn toggle_midi_input_monitor(&mut self, lane: usize) {
        let mut monitors = self.midi_input_monitor();
        if let Some(monitor) = Arc::make_mut(&mut monitors).get_mut(lane) {
            *monitor = !*monitor;
            self.midi_input_monitor.store(monitors);
        }
    }
    pub fn toggle_midi_disk_monitor(&mut self, lane: usize) {
        let mut monitors = self.midi_disk_monitor();
        if let Some(monitor) = Arc::make_mut(&mut monitors).get_mut(lane) {
            *monitor = !*monitor;
            self.midi_disk_monitor.store(monitors);
        }
    }

    pub fn set_session_base_dir(&mut self, base_dir: Option<PathBuf>) {
        if self.session_base_dir != base_dir {
            tracing::warn!(
                "Clearing clip caches for track '{}' because session base dir changed from {:?} to {:?}",
                self.name,
                self.session_base_dir,
                base_dir
            );
            self.session_base_dir = base_dir;

            self.rt.audio_clip_cache.clear();
            self.rt.midi_clip_cache.clear();
        }
    }

    pub fn frozen(&self) -> bool {
        self.frozen.load(Ordering::Relaxed)
    }

    pub fn set_frozen(&self, frozen: bool) {
        self.frozen.store(frozen, Ordering::Relaxed);
    }

    pub fn folder_open(&self) -> bool {
        self.folder_open.load(Ordering::Relaxed)
    }

    pub fn automation_mode(&self) -> crate::message::TrackAutomationMode {
        crate::message::TrackAutomationMode::from_u8(self.automation_mode.load(Ordering::Relaxed))
    }

    pub fn set_automation_mode(&self, mode: crate::message::TrackAutomationMode) {
        self.automation_mode.store(mode.as_u8(), Ordering::Relaxed);
    }

    pub fn process_block_size(&self) -> usize {
        self.process_block_size.load(Ordering::Relaxed)
    }

    pub fn offline_bounce_interleaved(
        &mut self,
        start_sample: usize,
        length_samples: usize,
    ) -> (usize, Vec<f32>) {
        let channels = self.audio.outs.len().max(1);
        if length_samples == 0 {
            return (channels, vec![]);
        }
        let block_size = self
            .audio
            .outs
            .first()
            .map(|io| io.buffer.lock().len())
            .or_else(|| self.audio.ins.first().map(|io| io.buffer.lock().len()))
            .unwrap_or(0)
            .max(1);

        let saved_transport = self.rt.transport_sample;
        let saved_disk_monitor = self.disk_monitor();
        let saved_input_monitor = self.input_monitor();
        let saved_midi_disk_monitor = self.midi_disk_monitor();
        let saved_midi_input_monitor = self.midi_input_monitor();
        let saved_clip_playback_enabled = self.rt.clip_playback_enabled;
        let saved_record_tap_enabled = self.rt.record_tap_enabled;
        let saved_armed = self.armed();
        let saved_output_enabled = self.output_enabled();
        let saved_loop_enabled = self.rt.loop_enabled;
        let saved_loop_range = self.rt.loop_range_samples;
        let saved_pending_hw = self.rt.pending_hw_midi_out_events.clone();

        let audio_in_count = self.audio.ins.len();
        let midi_in_count = self.midi.ins.len();
        self.set_disk_monitor(vec![true; audio_in_count]);
        self.set_input_monitor(vec![false; audio_in_count]);
        self.set_midi_disk_monitor(vec![true; midi_in_count]);
        self.set_midi_input_monitor(vec![false; midi_in_count]);
        self.rt.clip_playback_enabled = true;
        self.rt.record_tap_enabled = false;
        self.armed.store(false, Ordering::Relaxed);
        self.output_enabled.store(true, Ordering::Relaxed);
        self.rt.loop_enabled = false;
        self.rt.loop_range_samples = None;
        self.rt.pending_hw_midi_out_events.clear();

        let mut rendered = vec![0.0_f32; length_samples.saturating_mul(channels)];
        let mut cursor = 0usize;
        while cursor < length_samples {
            self.rt.transport_sample = start_sample.saturating_add(cursor);
            self.process();
            let step = (length_samples - cursor).min(block_size);
            if channels == 2 {
                let out_l = self.audio.outs[0].buffer.lock();
                let out_r = self.audio.outs[1].buffer.lock();
                let copy_len = step.min(out_l.len()).min(out_r.len());
                #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
                unsafe {
                    if std::arch::is_x86_feature_detected!("sse") {
                        let n = copy_len / 4;
                        for i in 0..n {
                            let l = std::arch::x86_64::_mm_loadu_ps(out_l.as_ptr().add(i * 4));
                            let r = std::arch::x86_64::_mm_loadu_ps(out_r.as_ptr().add(i * 4));
                            let lr0 = std::arch::x86_64::_mm_unpacklo_ps(l, r);
                            let lr1 = std::arch::x86_64::_mm_unpackhi_ps(l, r);
                            let dst = (cursor + i * 4) * 2;
                            std::arch::x86_64::_mm_storeu_ps(rendered.as_mut_ptr().add(dst), lr0);
                            std::arch::x86_64::_mm_storeu_ps(
                                rendered.as_mut_ptr().add(dst + 4),
                                lr1,
                            );
                        }
                        for i in n * 4..copy_len {
                            let dst = (cursor + i) * 2;
                            rendered[dst] = out_l[i];
                            rendered[dst + 1] = out_r[i];
                        }
                    } else {
                        for i in 0..copy_len {
                            let dst = (cursor + i) * 2;
                            rendered[dst] = out_l[i];
                            rendered[dst + 1] = out_r[i];
                        }
                    }
                }
                #[cfg(not(any(target_arch = "x86_64", target_arch = "x86")))]
                {
                    for i in 0..copy_len {
                        let dst = (cursor + i) * 2;
                        rendered[dst] = out_l[i];
                        rendered[dst + 1] = out_r[i];
                    }
                }
            } else {
                for ch in 0..channels {
                    let out = self.audio.outs[ch].buffer.lock();
                    let copy_len = step.min(out.len());
                    for (i, out_i) in out.iter().enumerate().take(copy_len) {
                        let dst = (cursor + i) * channels + ch;
                        rendered[dst] = *out_i;
                    }
                }
            }
            cursor = cursor.saturating_add(step);
            self.rt.pending_hw_midi_out_events.clear();
        }

        self.rt.transport_sample = saved_transport;
        self.disk_monitor.store(saved_disk_monitor);
        self.input_monitor.store(saved_input_monitor);
        self.midi_disk_monitor.store(saved_midi_disk_monitor);
        self.midi_input_monitor.store(saved_midi_input_monitor);
        self.rt.clip_playback_enabled = saved_clip_playback_enabled;
        self.rt.record_tap_enabled = saved_record_tap_enabled;
        self.armed.store(saved_armed, Ordering::Relaxed);
        self.output_enabled
            .store(saved_output_enabled, Ordering::Relaxed);
        self.rt.loop_enabled = saved_loop_enabled;
        self.rt.loop_range_samples = saved_loop_range;
        self.rt.pending_hw_midi_out_events = saved_pending_hw;

        (channels, rendered)
    }

    pub(crate) fn resolve_clip_path(&self, clip_name: &str) -> PathBuf {
        let clip_path = Path::new(clip_name);
        if clip_path.is_absolute() {
            clip_path.to_path_buf()
        } else {
            if let Some(base) = &self.session_base_dir {
                let candidate = base.join(clip_path);
                if candidate.exists() {
                    return candidate;
                }
            }

            let cwd_candidate = clip_path.to_path_buf();
            if cwd_candidate.exists() {
                return cwd_candidate;
            }

            if let Ok(session_root) = std::env::var("MAOLAN_SESSION_PATH") {
                let candidate = Path::new(&session_root).join(clip_path);
                if candidate.exists() {
                    return candidate;
                }
            }

            if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
                let candidate = Path::new(&home).join("recordings").join(clip_path);
                if candidate.exists() {
                    return candidate;
                }
            }

            if let Some(base) = &self.session_base_dir {
                base.join(clip_path)
            } else {
                cwd_candidate
            }
        }
    }
}
