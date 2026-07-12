use super::*;
#[cfg(target_os = "linux")]
use crate::hw::alsa::{HwDriver, HwOptions, MidiHub};
#[cfg(target_os = "macos")]
use crate::hw::coreaudio::{HwDriver, HwOptions, MidiHub};
#[cfg(target_os = "windows")]
use crate::hw::options::HwOptions;
#[cfg(target_os = "openbsd")]
use crate::hw::sndio::{HwDriver, HwOptions, MidiHub};
#[cfg(target_os = "windows")]
use crate::hw::wasapi::{self, HwDriver, MidiHub};
#[cfg(target_os = "linux")]
use crate::workers::alsa_worker::HwWorker;
#[cfg(target_os = "macos")]
use crate::workers::coreaudio_worker::HwWorker;
#[cfg(target_os = "openbsd")]
use crate::workers::sndio_worker::HwWorker;
#[cfg(target_os = "windows")]
use crate::workers::wasapi_worker::HwWorker;
use crate::{
    audio::clip::AudioClip,
    audio::io::AudioIO,
    history::{UndoEntry, create_inverse_actions, should_record},
    kind::Kind,
    message::Action,
    midi::clip::MIDIClip,
    midi::io::{MIDIIO, MidiEvent},
    routing,
    track::Track,
};
use std::sync::Arc;
use std::sync::atomic::Ordering;

impl Engine {
    pub(crate) fn is_track_frozen(&self, track_name: &str) -> bool {
        self.state
            .lock()
            .tracks
            .get(track_name)
            .map(|track| track.lock().frozen())
            .unwrap_or(false)
    }

    pub(crate) async fn reject_if_track_frozen(
        &mut self,
        track_name: &str,
        operation: &str,
    ) -> bool {
        if self.is_track_frozen(track_name) {
            self.notify_clients(Err(format!(
                "Track '{track_name}' is frozen; {operation} is blocked"
            )))
            .await;
            true
        } else {
            false
        }
    }

    #[cfg(unix)]
    pub(crate) fn audio_ports_connected(source: &Arc<AudioIO>, target: &Arc<AudioIO>) -> bool {
        source
            .connections
            .lock()
            .iter()
            .any(|conn| Arc::ptr_eq(conn, target))
    }

    pub(crate) fn resolve_audio_route_ports(
        &self,
        from_track: &str,
        from_port: usize,
        to_track: &str,
        to_port: usize,
    ) -> (Option<Arc<AudioIO>>, Option<Arc<AudioIO>>) {
        let state = self.state.lock();
        let from_is_child_of_to = state
            .tracks
            .get(from_track)
            .and_then(|t| t.lock().parent_track.clone())
            .as_deref()
            == Some(to_track);
        let to_is_child_of_from = state
            .tracks
            .get(to_track)
            .and_then(|t| t.lock().parent_track.clone())
            .as_deref()
            == Some(from_track);

        let from_audio_io = if from_track == "hw:in" {
            self.hw_input_audio_port(from_port)
        } else {
            state.tracks.get(from_track).and_then(|t| {
                let t = t.lock();
                if t.is_folder {
                    if to_is_child_of_from {
                        // Folder input -> child input.
                        t.audio.ins.get(from_port).cloned()
                    } else {
                        // Folder output -> external target.
                        t.audio.outs.get(from_port).cloned()
                    }
                } else {
                    t.audio.outs.get(from_port).cloned()
                }
            })
        };
        let to_audio_io = if to_track == "hw:out" {
            self.hw_output_audio_port(to_port)
        } else {
            state.tracks.get(to_track).and_then(|t| {
                let t = t.lock();
                if t.is_folder {
                    if from_is_child_of_to {
                        // Child output -> folder output.
                        t.audio.outs.get(to_port).cloned()
                    } else {
                        // External source -> folder input.
                        t.audio.ins.get(to_port).cloned()
                    }
                } else {
                    t.audio.ins.get(to_port).cloned()
                }
            })
        };
        (from_audio_io, to_audio_io)
    }

    pub(crate) async fn disconnect_audio_route_and_notify(
        &mut self,
        action: Action,
    ) -> Result<(), String> {
        let Action::Disconnect {
            from_track,
            from_port,
            to_track,
            to_port,
            kind,
        } = &action
        else {
            return Err("disconnect_audio_route_and_notify requires Disconnect action".to_string());
        };
        if *kind != Kind::Audio {
            return Err("disconnect_audio_route_and_notify only supports audio routes".to_string());
        }
        let (from_audio_io, to_audio_io) =
            self.resolve_audio_route_ports(from_track, *from_port, to_track, *to_port);
        match (from_audio_io, to_audio_io) {
            (Some(source), Some(target)) => {
                crate::audio::io::AudioIO::disconnect(&source, &target)
                    .map_err(|e| format!("Disconnect failed: {e}"))?;
                self.notify_clients(Ok(action)).await;
                Ok(())
            }
            _ => Err(format!(
                "Disconnect failed: Port not found ({} -> {})",
                from_track, to_track
            )),
        }
    }

    #[cfg(unix)]
    pub(crate) fn disconnect_actions_for_removed_hw_input(
        &self,
        removed_port: usize,
        removed_io: &Arc<AudioIO>,
    ) -> Vec<Action> {
        let mut actions = Vec::new();
        {
            let state = self.state.lock();
            for (track_name, track) in &state.tracks {
                let track = track.lock();
                for (to_port, target) in track.audio.ins.iter().enumerate() {
                    if Self::audio_ports_connected(removed_io, target) {
                        actions.push(Action::Disconnect {
                            from_track: "hw:in".to_string(),
                            from_port: removed_port,
                            to_track: track_name.clone(),
                            to_port,
                            kind: Kind::Audio,
                        });
                    }
                }
            }
        }
        for (to_port, target) in self.all_hw_output_audio_ports().into_iter().enumerate() {
            if Self::audio_ports_connected(removed_io, &target) {
                actions.push(Action::Disconnect {
                    from_track: "hw:in".to_string(),
                    from_port: removed_port,
                    to_track: "hw:out".to_string(),
                    to_port,
                    kind: Kind::Audio,
                });
            }
        }
        actions
    }

    #[cfg(unix)]
    pub(crate) fn disconnect_actions_for_removed_hw_output(
        &self,
        removed_port: usize,
        removed_io: &Arc<AudioIO>,
    ) -> Vec<Action> {
        let mut actions = Vec::new();
        {
            let state = self.state.lock();
            for (track_name, track) in &state.tracks {
                let track = track.lock();
                for (from_port, source) in track.audio.outs.iter().enumerate() {
                    if Self::audio_ports_connected(source, removed_io) {
                        actions.push(Action::Disconnect {
                            from_track: track_name.clone(),
                            from_port,
                            to_track: "hw:out".to_string(),
                            to_port: removed_port,
                            kind: Kind::Audio,
                        });
                    }
                }
            }
        }
        #[cfg(unix)]
        if let Some(jack) = &self.jack_runtime {
            for (from_port, source) in jack.audio_ins().into_iter().enumerate() {
                if Self::audio_ports_connected(&source, removed_io) {
                    actions.push(Action::Disconnect {
                        from_track: "hw:in".to_string(),
                        from_port,
                        to_track: "hw:out".to_string(),
                        to_port: removed_port,
                        kind: Kind::Audio,
                    });
                }
            }
        }
        actions
    }

    #[cfg(unix)]
    pub(crate) fn reindex_notifications_for_removed_hw_input(
        &self,
        removed_port: usize,
    ) -> Vec<Action> {
        let mut actions = Vec::new();
        #[cfg(unix)]
        if let Some(jack) = &self.jack_runtime {
            for from_port in (removed_port + 1)..jack.input_channels() {
                let Some(source) = jack.input_audio_port(from_port) else {
                    continue;
                };
                {
                    let state = self.state.lock();
                    for (track_name, track) in &state.tracks {
                        let track = track.lock();
                        for (to_port, target) in track.audio.ins.iter().enumerate() {
                            if Self::audio_ports_connected(&source, target) {
                                actions.push(Action::Disconnect {
                                    from_track: "hw:in".to_string(),
                                    from_port,
                                    to_track: track_name.clone(),
                                    to_port,
                                    kind: Kind::Audio,
                                });
                                actions.push(Action::Connect {
                                    from_track: "hw:in".to_string(),
                                    from_port: from_port - 1,
                                    to_track: track_name.clone(),
                                    to_port,
                                    kind: Kind::Audio,
                                });
                            }
                        }
                    }
                }
                for (to_port, target) in self.all_hw_output_audio_ports().into_iter().enumerate() {
                    if Self::audio_ports_connected(&source, &target) {
                        actions.push(Action::Disconnect {
                            from_track: "hw:in".to_string(),
                            from_port,
                            to_track: "hw:out".to_string(),
                            to_port,
                            kind: Kind::Audio,
                        });
                        actions.push(Action::Connect {
                            from_track: "hw:in".to_string(),
                            from_port: from_port - 1,
                            to_track: "hw:out".to_string(),
                            to_port,
                            kind: Kind::Audio,
                        });
                    }
                }
            }
        }
        actions
    }

    #[cfg(unix)]
    pub(crate) fn reindex_notifications_for_removed_hw_output(
        &self,
        removed_port: usize,
    ) -> Vec<Action> {
        let mut actions = Vec::new();
        #[cfg(unix)]
        if let Some(jack) = &self.jack_runtime {
            for to_port in (removed_port + 1)..jack.output_channels() {
                let Some(target) = jack.output_audio_port(to_port) else {
                    continue;
                };
                {
                    let state = self.state.lock();
                    for (track_name, track) in &state.tracks {
                        let track = track.lock();
                        for (from_port, source) in track.audio.outs.iter().enumerate() {
                            if Self::audio_ports_connected(source, &target) {
                                actions.push(Action::Disconnect {
                                    from_track: track_name.clone(),
                                    from_port,
                                    to_track: "hw:out".to_string(),
                                    to_port,
                                    kind: Kind::Audio,
                                });
                                actions.push(Action::Connect {
                                    from_track: track_name.clone(),
                                    from_port,
                                    to_track: "hw:out".to_string(),
                                    to_port: to_port - 1,
                                    kind: Kind::Audio,
                                });
                            }
                        }
                    }
                }
                for (from_port, source) in jack.audio_ins().into_iter().enumerate() {
                    if Self::audio_ports_connected(&source, &target) {
                        actions.push(Action::Disconnect {
                            from_track: "hw:in".to_string(),
                            from_port,
                            to_track: "hw:out".to_string(),
                            to_port,
                            kind: Kind::Audio,
                        });
                        actions.push(Action::Connect {
                            from_track: "hw:in".to_string(),
                            from_port,
                            to_track: "hw:out".to_string(),
                            to_port: to_port - 1,
                            kind: Kind::Audio,
                        });
                    }
                }
            }
        }
        actions
    }

    pub(crate) fn upstream_audio_track_names(
        &self,
        seeds: &std::collections::HashSet<String>,
    ) -> std::collections::HashSet<String> {
        let state = self.state.lock();
        let mut output_to_track: std::collections::HashMap<
            *const crate::audio::io::AudioIO,
            String,
        > = std::collections::HashMap::new();
        for (name, track) in &state.tracks {
            let t = track.lock();
            for out in &t.audio.outs {
                output_to_track.insert(std::sync::Arc::as_ptr(out), name.clone());
            }
        }
        let mut upstream = std::collections::HashSet::new();
        let mut to_process: Vec<String> = seeds.iter().cloned().collect();
        let mut processed = std::collections::HashSet::new();
        while let Some(target_name) = to_process.pop() {
            if !processed.insert(target_name.clone()) {
                continue;
            }
            if let Some(target_track) = state.tracks.get(&target_name) {
                let tt = target_track.lock();
                for input in &tt.audio.ins {
                    for conn in input.connections.lock().iter() {
                        let conn_ptr = std::sync::Arc::as_ptr(conn);
                        if let Some(source_name) = output_to_track.get(&conn_ptr)
                            && source_name != &target_name
                            && !seeds.contains(source_name)
                        {
                            upstream.insert(source_name.clone());
                            to_process.push(source_name.clone());
                        }
                    }
                }
            }
        }
        upstream
    }

    pub(crate) fn is_track_in_soloed_folder(
        &self,
        track: &Track,
        tracks: &std::collections::HashMap<String, crate::state::TrackHandle>,
    ) -> bool {
        let mut current = track.parent_track.clone();
        while let Some(parent_name) = current {
            if let Some(parent) = tracks.get(&parent_name) {
                let p = parent.lock();
                if p.soloed() {
                    return true;
                }
                current = p.parent_track.clone();
            } else {
                break;
            }
        }
        false
    }

    pub(crate) fn folder_has_soloed_descendant(
        &self,
        folder_name: &str,
        tracks: &std::collections::HashMap<String, crate::state::TrackHandle>,
    ) -> bool {
        for track in tracks.values() {
            let t = track.lock();
            if !t.soloed() {
                continue;
            }
            let mut current = t.parent_track.clone();
            while let Some(parent_name) = current {
                if parent_name == folder_name {
                    return true;
                }
                if let Some(parent) = tracks.get(&parent_name) {
                    current = parent.lock().parent_track.clone();
                } else {
                    break;
                }
            }
        }
        false
    }

    pub(crate) fn refresh_realtime_infection(&self) {
        let state = self.state.lock();
        let live_seeds: std::collections::HashSet<String> = state
            .tracks
            .iter()
            .filter_map(|(name, track)| {
                let t = track.lock();
                if t.armed() && t.input_monitor().iter().any(|&m| m) {
                    Some(name.clone())
                } else {
                    None
                }
            })
            .collect();
        let mut output_owner: std::collections::HashMap<*const crate::audio::io::AudioIO, String> =
            std::collections::HashMap::new();
        for (name, track) in state.tracks.iter() {
            let t = track.lock();
            for out in &t.audio.outs {
                output_owner.insert(std::sync::Arc::as_ptr(out), name.clone());
            }
        }

        let mut infected = live_seeds.clone();
        let mut mixed_nodes = std::collections::HashSet::new();
        loop {
            let mut changed = false;
            for (name, track) in state.tracks.iter() {
                let t = track.lock();
                let mut upstream_owners = std::collections::HashSet::new();
                for input in &t.audio.ins {
                    for conn in input.connections.lock().iter() {
                        if let Some(owner) = output_owner.get(&std::sync::Arc::as_ptr(conn)) {
                            upstream_owners.insert(owner.clone());
                        }
                    }
                }
                if upstream_owners.is_empty() {
                    continue;
                }
                let has_realtime = upstream_owners
                    .iter()
                    .any(|owner| infected.contains(owner) || live_seeds.contains(owner));
                let has_playback = upstream_owners
                    .iter()
                    .any(|owner| !infected.contains(owner) && !live_seeds.contains(owner));
                if has_realtime && has_playback {
                    mixed_nodes.insert(name.clone());
                }
                if has_realtime && infected.insert(name.clone()) {
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        for (name, track) in state.tracks.iter() {
            let forced = infected.contains(name) && !live_seeds.contains(name);
            let mut t = track.lock();
            t.set_shared_realtime_mixed(mixed_nodes.contains(name));
            t.set_force_realtime_domain(forced);
        }
    }

    pub(crate) fn apply_mute_solo_policy(&mut self) {
        let mut newly_disabled_tracks: Vec<String> = Vec::new();
        {
            let tracks = &self.state.lock().tracks;
            let soloed: std::collections::HashSet<String> = tracks
                .iter()
                .filter_map(|(name, t)| {
                    if t.lock().soloed() {
                        Some(name.clone())
                    } else {
                        None
                    }
                })
                .collect();
            let any_soloed = !soloed.is_empty();
            let upstream = if any_soloed {
                self.upstream_audio_track_names(&soloed)
            } else {
                std::collections::HashSet::new()
            };
            for track in tracks.values() {
                let t = track.lock();
                let was_enabled = t.output_enabled();
                let in_soloed_folder = self.is_track_in_soloed_folder(&t, tracks);
                let folder_with_soloed_child =
                    t.is_folder && self.folder_has_soloed_descendant(&t.name, tracks);
                let enabled = if t.is_master() {
                    !t.muted()
                } else if any_soloed {
                    (t.soloed()
                        || upstream.contains(&t.name)
                        || in_soloed_folder
                        || folder_with_soloed_child)
                        && !t.muted()
                } else {
                    !t.muted()
                };
                t.set_output_enabled(enabled);
                if was_enabled && !enabled {
                    newly_disabled_tracks.push(t.name.clone());
                }
            }
        }
        let mut note_off_events = Vec::new();
        for track_name in newly_disabled_tracks {
            note_off_events.extend(self.note_off_events_for_track(&track_name));
        }
        if !note_off_events.is_empty() {
            self.pending_hw_midi_out_events_by_device
                .extend(note_off_events);
        }
    }

    pub(crate) fn track_handle_by_name(
        &self,
        track_name: &str,
    ) -> Option<crate::state::TrackHandle> {
        self.state.lock().tracks.get(track_name).cloned()
    }

    pub(crate) fn track_handle_or_err(
        &self,
        track_name: &str,
    ) -> Result<crate::state::TrackHandle, String> {
        self.track_handle_by_name(track_name)
            .ok_or_else(|| format!("Track not found: {track_name}"))
    }

    pub(crate) fn add_clip_to_track(&self, request: ClipAddRequest<'_>) {
        if let Some(track) = self.state.lock().tracks.get(request.track_name) {
            let mut track = track.lock();
            if track.is_master() || track.is_folder {
                return;
            }
            match request.kind {
                Kind::Audio => {
                    let mut clip = AudioClip::new(
                        request.name.to_string(),
                        request.start,
                        request.start.saturating_add(request.length.max(1)),
                    );
                    clip.id = request.clip_id.to_string();
                    clip.offset = request.offset;
                    let max_lane = track.audio.ins.len().saturating_sub(1);
                    clip.input_channel = request.input_channel.min(max_lane);
                    clip.muted = request.muted;
                    clip.peaks_file = request.peaks_file;
                    clip.fade_enabled = request.fade_enabled;
                    clip.fade_in_samples = request.fade_in_samples;
                    clip.fade_out_samples = request.fade_out_samples;
                    clip.pitch_correction_preview_name = request.preview_name;
                    clip.pitch_correction_source_name = request.source_name;
                    clip.pitch_correction_source_offset = request.source_offset;
                    clip.pitch_correction_source_length = request.source_length;
                    clip.pitch_correction_points = request.pitch_correction_points;
                    clip.pitch_correction_frame_likeness = request.pitch_correction_frame_likeness;
                    clip.pitch_correction_inertia_ms = request.pitch_correction_inertia_ms;
                    clip.pitch_correction_formant_compensation =
                        request.pitch_correction_formant_compensation;
                    clip.plugin_graph_json = request.plugin_graph_json;
                    track.audio.push_clip(clip);
                    #[cfg(unix)]
                    track.rt.clip_pitch_shifters.clear();
                }
                Kind::MIDI => {
                    let mut clip = MIDIClip::new(
                        request.name.to_string(),
                        request.start,
                        request.start.saturating_add(request.length.max(1)),
                    );
                    clip.id = request.clip_id.to_string();
                    clip.offset = request.offset;
                    let max_lane = track.midi.ins.len().saturating_sub(1);
                    clip.input_channel = request.input_channel.min(max_lane);
                    clip.muted = request.muted;
                    track.midi.push_clip(clip);
                }
            }
        }
    }

    pub(crate) fn audio_clip_from_data(data: &crate::message::AudioClipData) -> AudioClip {
        let mut clip = AudioClip::new(
            data.name.clone(),
            data.start,
            data.start.saturating_add(data.length.max(1)),
        );
        clip.id = data.id.clone();
        clip.offset = data.offset;
        clip.input_channel = data.input_channel;
        clip.muted = data.muted;
        clip.peaks_file = data.peaks_file.clone();
        clip.fade_enabled = data.fade_enabled;
        clip.fade_in_samples = data.fade_in_samples;
        clip.fade_out_samples = data.fade_out_samples;
        clip.pitch_correction_preview_name = data.preview_name.clone();
        clip.pitch_correction_source_name = data.source_name.clone();
        clip.pitch_correction_source_offset = data.source_offset;
        clip.pitch_correction_source_length = data.source_length;
        clip.pitch_correction_points = data.pitch_correction_points.clone();
        clip.pitch_correction_frame_likeness = data.pitch_correction_frame_likeness;
        clip.pitch_correction_inertia_ms = data.pitch_correction_inertia_ms;
        clip.pitch_correction_formant_compensation = data.pitch_correction_formant_compensation;
        clip.plugin_graph_json = data.plugin_graph_json.clone();
        clip.grouped_clips = data
            .grouped_clips
            .iter()
            .map(Self::audio_clip_from_data)
            .collect();
        for child in &mut clip.grouped_clips {
            child.fade_enabled = false;
            child.fade_in_samples = 0;
            child.fade_out_samples = 0;
        }
        clip
    }

    pub(crate) fn midi_clip_from_data(data: &crate::message::MidiClipData) -> MIDIClip {
        let mut clip = MIDIClip::new(
            data.name.clone(),
            data.start,
            data.start.saturating_add(data.length.max(1)),
        );
        clip.id = data.id.clone();
        clip.offset = data.offset;
        clip.input_channel = data.input_channel;
        clip.muted = data.muted;
        clip.grouped_clips = data
            .grouped_clips
            .iter()
            .map(Self::midi_clip_from_data)
            .collect();
        clip
    }

    pub(crate) fn add_grouped_clip_to_track(
        &self,
        track_name: &str,
        kind: Kind,
        audio_clip: Option<crate::message::AudioClipData>,
        midi_clip: Option<crate::message::MidiClipData>,
    ) {
        if let Some(track) = self.state.lock().tracks.get(track_name) {
            let mut track = track.lock();
            if track.is_master() {
                return;
            }
            match kind {
                Kind::Audio => {
                    if let Some(mut clip) = audio_clip.map(|clip| Self::audio_clip_from_data(&clip))
                    {
                        let max_lane = track.audio.ins.len().saturating_sub(1);
                        clip.input_channel = clip.input_channel.min(max_lane);
                        track.audio.push_clip(clip);
                        #[cfg(unix)]
                        track.rt.clip_pitch_shifters.clear();
                    }
                }
                Kind::MIDI => {
                    if let Some(mut clip) = midi_clip.map(|clip| Self::midi_clip_from_data(&clip)) {
                        let max_lane = track.midi.ins.len().saturating_sub(1);
                        clip.input_channel = clip.input_channel.min(max_lane);
                        track.midi.push_clip(clip);
                    }
                }
            }
        }
    }

    pub(crate) fn remove_clips_from_track(
        &self,
        track_name: &str,
        kind: Kind,
        clip_indices: &[usize],
    ) {
        if let Some(track) = self.state.lock().tracks.get(track_name) {
            let mut track = track.lock();
            let mut indices = clip_indices.to_vec();
            indices.sort_unstable();
            indices.dedup();
            match kind {
                Kind::Audio => {
                    for idx in indices.into_iter().rev() {
                        if idx < track.audio.clips().len() {
                            track.audio.remove_clip(idx);
                        }
                    }
                    #[cfg(unix)]
                    track.rt.clip_pitch_shifters.clear();
                }
                Kind::MIDI => {
                    for idx in indices.into_iter().rev() {
                        if idx < track.midi.clips().len() {
                            track.midi.remove_clip(idx);
                        }
                    }
                }
            }
        }
    }

    pub(crate) fn rename_clip_references(
        &self,
        track_name: &str,
        kind: Kind,
        clip_index: usize,
        new_name: &str,
    ) {
        let Some(track) = self.state.lock().tracks.get(track_name).cloned() else {
            return;
        };
        let track = track.lock();
        let old_name = match kind {
            Kind::Audio => {
                if clip_index >= track.audio.clips().len() {
                    return;
                }
                track.audio.clips().get(clip_index).unwrap().name.clone()
            }
            Kind::MIDI => {
                if clip_index >= track.midi.clips().len() {
                    return;
                }
                track.midi.clips().get(clip_index).unwrap().name.clone()
            }
        };

        let new_file_name = match kind {
            Kind::Audio => format!("audio/{}.wav", new_name),
            Kind::MIDI => {
                let ext = std::path::Path::new(&old_name)
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|s| s.to_ascii_lowercase())
                    .filter(|e| e == "mid" || e == "midi")
                    .unwrap_or_else(|| "mid".to_string());
                format!("midi/{}.{}", new_name, ext)
            }
        };
        let _ = track;

        for (_, other_track) in self.state.lock().tracks.iter() {
            let other_track = other_track.lock();
            match kind {
                Kind::Audio => {
                    let clips = other_track.audio.clips();
                    for idx in 0..clips.len() {
                        other_track.audio.update_clip(idx, |clip| {
                            if clip.name == old_name {
                                clip.name = new_file_name.clone();
                            }
                            if clip.pitch_correction_source_name.as_deref()
                                == Some(old_name.as_str())
                            {
                                clip.pitch_correction_source_name = Some(new_file_name.clone());
                            }
                        });
                    }
                }
                Kind::MIDI => {
                    let clips = other_track.midi.clips();
                    for idx in 0..clips.len() {
                        other_track.midi.update_clip(idx, |clip| {
                            if clip.name == old_name {
                                clip.name = new_file_name.clone();
                            }
                        });
                    }
                }
            }
        }
    }

    pub(crate) fn set_clip_fade(
        &self,
        track_name: &str,
        clip_index: usize,
        kind: Kind,
        fade_enabled: bool,
        fade_in_samples: usize,
        fade_out_samples: usize,
    ) {
        let Some(track) = self.state.lock().tracks.get(track_name).cloned() else {
            return;
        };
        let track = track.lock();
        match kind {
            Kind::Audio => {
                track.audio.update_clip(clip_index, |clip| {
                    clip.fade_enabled = fade_enabled;
                    clip.fade_in_samples = fade_in_samples;
                    clip.fade_out_samples = fade_out_samples;
                });
            }
            Kind::MIDI => {}
        }
    }

    pub(crate) fn set_clip_bounds(
        &self,
        track_name: &str,
        clip_index: usize,
        kind: Kind,
        start: usize,
        length: usize,
        offset: usize,
    ) {
        let Some(track) = self.state.lock().tracks.get(track_name).cloned() else {
            return;
        };
        let mut track = track.lock();
        match kind {
            Kind::Audio => {
                track.audio.update_clip(clip_index, |clip| {
                    clip.start = start;
                    clip.end = start.saturating_add(length.max(1));
                    clip.offset = offset;
                    clip.pitch_correction_preview_name = None;
                    clip.pitch_correction_source_name = None;
                    clip.pitch_correction_source_offset = None;
                    clip.pitch_correction_source_length = None;
                    clip.pitch_correction_points.clear();
                    clip.pitch_correction_frame_likeness = None;
                    clip.pitch_correction_inertia_ms = None;
                    clip.pitch_correction_formant_compensation = None;
                });
                #[cfg(unix)]
                track.rt.clip_pitch_shifters.clear();
            }
            Kind::MIDI => {
                track.midi.update_clip(clip_index, |clip| {
                    clip.start = start;
                    clip.end = start.saturating_add(length.max(1));
                    clip.offset = offset;
                });
            }
        }
    }

    pub(crate) fn set_clip_source_name(
        &self,
        track_name: &str,
        clip_index: usize,
        kind: Kind,
        name: String,
    ) {
        let Some(track) = self.state.lock().tracks.get(track_name).cloned() else {
            return;
        };
        let mut track = track.lock();
        match kind {
            Kind::Audio => {
                track.audio.update_clip(clip_index, |clip| {
                    clip.name = name;
                });
                #[cfg(unix)]
                track.rt.clip_pitch_shifters.clear();
            }
            Kind::MIDI => {
                track.midi.update_clip(clip_index, |clip| {
                    clip.name = name;
                });
            }
        }
    }

    pub(crate) fn set_clip_muted(
        &self,
        track_name: &str,
        clip_index: usize,
        kind: Kind,
        muted: bool,
    ) {
        let Some(track) = self.state.lock().tracks.get(track_name).cloned() else {
            return;
        };
        let track = track.lock();
        match kind {
            Kind::Audio => {
                track.audio.update_clip(clip_index, |clip| {
                    clip.muted = muted;
                });
            }
            Kind::MIDI => {
                track.midi.update_clip(clip_index, |clip| {
                    clip.muted = muted;
                });
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn set_clip_pitch_correction(
        &self,
        track_name: &str,
        clip_index: usize,
        preview_name: Option<String>,
        source_name: Option<String>,
        source_offset: Option<usize>,
        source_length: Option<usize>,
        pitch_correction_points: Vec<crate::message::PitchCorrectionPointData>,
        pitch_correction_frame_likeness: Option<f32>,
        pitch_correction_inertia_ms: Option<u16>,
        pitch_correction_formant_compensation: Option<bool>,
    ) {
        if let Some(track) = self.state.lock().tracks.get(track_name) {
            let mut track = track.lock();
            track.audio.update_clip(clip_index, |clip| {
                clip.pitch_correction_preview_name = preview_name;
                clip.pitch_correction_source_name = source_name;
                clip.pitch_correction_source_offset = source_offset;
                clip.pitch_correction_source_length = source_length;
                clip.pitch_correction_points = pitch_correction_points;
                clip.pitch_correction_frame_likeness = pitch_correction_frame_likeness;
                clip.pitch_correction_inertia_ms = pitch_correction_inertia_ms;
                clip.pitch_correction_formant_compensation = pitch_correction_formant_compensation;
            });
            #[cfg(unix)]
            track.rt.clip_pitch_shifters.clear();
        }
    }

    pub fn check_if_leads_to_kind(
        &self,
        kind: Kind,
        current_track_name: &str,
        target_track_name: &str,
    ) -> bool {
        routing::would_create_cycle(
            &target_track_name.to_string(),
            &current_track_name.to_string(),
            |track_name| self.connected_neighbors(kind, track_name),
        )
    }

    pub(crate) fn connected_neighbors(&self, kind: Kind, current_track_name: &str) -> Vec<String> {
        let state = self.state.lock();
        let mut found_neighbors = Vec::new();

        if let Some(current_track_handle) = state.tracks.get(current_track_name) {
            let current_track = current_track_handle.lock();

            match kind {
                Kind::Audio => {
                    for out_port in &current_track.audio.outs {
                        let conns = out_port.connections.lock();
                        for conn in conns.iter() {
                            for (name, next_track_handle) in &state.tracks {
                                let next_track = next_track_handle.lock();
                                let is_connected =
                                    next_track.audio.ins.iter().any(|ins_port| {
                                        Arc::ptr_eq(&ins_port.buffer, &conn.buffer)
                                    });

                                if is_connected {
                                    found_neighbors.push(name.clone());
                                }
                            }
                        }
                    }
                }
                Kind::MIDI => {
                    for out_port in &current_track.midi.outs {
                        let conns = out_port.connections();
                        for conn in conns.iter() {
                            for (name, next_track_handle) in &state.tracks {
                                let next_track = next_track_handle.lock();
                                let is_connected = next_track
                                    .midi
                                    .ins
                                    .iter()
                                    .any(|ins_port| Arc::ptr_eq(ins_port, conn));

                                if is_connected {
                                    found_neighbors.push(name.clone());
                                }
                            }
                        }
                    }
                }
            }
        }
        found_neighbors
    }

    pub(crate) fn find_audio_io_owner(
        &self,
        state: &crate::state::State,
        io: &std::sync::Arc<crate::audio::io::AudioIO>,
    ) -> Option<(String, usize)> {
        for (name, track) in &state.tracks {
            let t = track.lock();
            for (i, out) in t.audio.outs.iter().enumerate() {
                if std::sync::Arc::ptr_eq(out, io) {
                    return Some((name.clone(), i));
                }
            }
            for (i, inp) in t.audio.ins.iter().enumerate() {
                if std::sync::Arc::ptr_eq(inp, io) {
                    return Some((name.clone(), i));
                }
            }
        }
        None
    }

    pub(crate) fn find_midi_io_owner(
        &self,
        state: &crate::state::State,
        io: &std::sync::Arc<crate::midi::io::MIDIIO>,
    ) -> Option<(String, usize, bool)> {
        for (name, track) in &state.tracks {
            let t = track.lock();
            for (i, out) in t.midi.outs.iter().enumerate() {
                if std::sync::Arc::ptr_eq(out, io) {
                    return Some((name.clone(), i, false));
                }
            }
            for (i, inp) in t.midi.ins.iter().enumerate() {
                if std::sync::Arc::ptr_eq(inp, io) {
                    return Some((name.clone(), i, true));
                }
            }
        }
        None
    }

    pub(crate) fn collect_descendant_track_names(&self, name: &str, out: &mut Vec<String>) {
        // Clone the child arcs while briefly holding the parent lock, then release it before
        // recursing so we never nest locks on the same thread.
        let child_arcs: Vec<crate::state::TrackHandle> = {
            let state = self.state.lock();
            if let Some(track) = state.tracks.get(name) {
                track.lock().child_tracks.clone()
            } else {
                Vec::new()
            }
        };
        for child in child_arcs {
            let child_name = { child.lock().name.clone() };
            self.collect_descendant_track_names(&child_name, out);
            out.push(child_name);
        }
    }

    pub(crate) async fn remove_single_track(&mut self, name: &str) {
        let children: Vec<crate::state::TrackHandle> = {
            let state = self.state.lock();
            if let Some(removed) = state.tracks.get(name).cloned() {
                removed.lock().child_tracks.clone()
            } else {
                Vec::new()
            }
        };
        let parent_name: Option<String> = {
            let state = self.state.lock();
            state
                .tracks
                .get(name)
                .map(|t| t.lock().parent_track.clone())
                .unwrap_or(None)
        };
        if let Some(parent_name) = parent_name {
            let state = self.state.lock();
            if let Some(parent) = state.tracks.get(&parent_name).cloned() {
                let mut parent = parent.lock();
                parent.child_tracks.retain(|c| c.lock().name != *name);
            }
        }
        if let Some(removed_track) = self.state.lock().tracks.get(name).cloned() {
            for child in children {
                let removed = removed_track.lock();
                child.lock().disconnect_from_parent(&removed);
                child.lock().parent_track = None;
            }
        }
        self.state.lock().tracks.remove(name);
        self.audio_recordings.remove(name);
        self.midi_recordings.remove(name);
        self.midi_hw_in_routes.retain(|r| r.to_track != *name);
        self.midi_hw_out_routes.retain(|r| r.from_track != *name);
        if self
            .pending_midi_learn
            .as_ref()
            .is_some_and(|(track_name, _, _)| track_name == name)
        {
            self.pending_midi_learn = None;
        }
    }

    pub(crate) fn prepare_inverse_actions(
        &self,
        action_to_process: &Action,
        record_history: bool,
        suppress_timing_history: bool,
    ) -> Option<Vec<Action>> {
        let mut extra_inverse_actions: Vec<Action> = Vec::new();
        if record_history
            && !self.history_suspended
            && let Action::RemoveTrack(track_name) = action_to_process
        {
            for route in self
                .midi_hw_in_routes
                .iter()
                .filter(|route| &route.to_track == track_name)
            {
                extra_inverse_actions.push(Action::Connect {
                    from_track: format!("midi:hw:in:{}", route.device),
                    from_port: 0,
                    to_track: route.to_track.clone(),
                    to_port: route.to_port,
                    kind: Kind::MIDI,
                });
            }
            for route in self
                .midi_hw_out_routes
                .iter()
                .filter(|route| &route.from_track == track_name)
            {
                extra_inverse_actions.push(Action::Connect {
                    from_track: route.from_track.clone(),
                    from_port: route.from_port,
                    to_track: format!("midi:hw:out:{}", route.device),
                    to_port: 0,
                    kind: Kind::MIDI,
                });
            }
        }
        if record_history
            && !self.history_suspended
            && matches!(action_to_process, Action::ClearAllMidiLearnBindings)
        {
            if let Some(binding) = self.global_midi_learn_play_pause.clone() {
                extra_inverse_actions.push(Action::SetGlobalMidiLearnBinding {
                    target: crate::message::GlobalMidiLearnTarget::PlayPause,
                    binding: Some(binding),
                });
            }
            if let Some(binding) = self.global_midi_learn_stop.clone() {
                extra_inverse_actions.push(Action::SetGlobalMidiLearnBinding {
                    target: crate::message::GlobalMidiLearnTarget::Stop,
                    binding: Some(binding),
                });
            }
            if let Some(binding) = self.global_midi_learn_record_toggle.clone() {
                extra_inverse_actions.push(Action::SetGlobalMidiLearnBinding {
                    target: crate::message::GlobalMidiLearnTarget::RecordToggle,
                    binding: Some(binding),
                });
            }
            for (key, binding) in self.session_midi_learn_slots.clone() {
                extra_inverse_actions.push(Action::SetSessionMidiLearnBinding {
                    target: crate::message::SessionMidiLearnTarget::Slot {
                        track_name: key.0,
                        scene_index: key.1,
                    },
                    binding: Some(binding),
                });
            }
            for (scene_index, binding) in self.session_midi_learn_scenes.clone() {
                extra_inverse_actions.push(Action::SetSessionMidiLearnBinding {
                    target: crate::message::SessionMidiLearnTarget::Scene(scene_index),
                    binding: Some(binding),
                });
            }
            for (track_name, binding) in self.session_midi_learn_stop_track.clone() {
                extra_inverse_actions.push(Action::SetSessionMidiLearnBinding {
                    target: crate::message::SessionMidiLearnTarget::StopTrack(track_name),
                    binding: Some(binding),
                });
            }
            if let Some(binding) = self.session_midi_learn_stop_all.clone() {
                extra_inverse_actions.push(Action::SetSessionMidiLearnBinding {
                    target: crate::message::SessionMidiLearnTarget::StopAll,
                    binding: Some(binding),
                });
            }
        }
        let mut inverse_actions = if record_history
            && !suppress_timing_history
            && should_record(action_to_process)
            && !self.history_suspended
        {
            let state = self.state.lock();
            create_inverse_actions(action_to_process, &state).map(|mut actions| {
                actions.extend(extra_inverse_actions);
                actions
            })
        } else {
            None
        };
        if record_history && !suppress_timing_history && !self.history_suspended {
            match action_to_process {
                Action::SetTempo(_) => {
                    inverse_actions = Some(vec![Action::SetTempo(self.tempo_bpm)]);
                }
                Action::SetLoopEnabled(_) => {
                    inverse_actions = Some(vec![Action::SetLoopEnabled(self.loop_enabled)]);
                }
                Action::SetLoopRange(_) => {
                    inverse_actions = Some(vec![
                        Action::SetLoopRange(self.loop_range_samples),
                        Action::SetLoopEnabled(self.loop_enabled),
                    ]);
                }
                Action::SetPunchEnabled(_) => {
                    inverse_actions = Some(vec![Action::SetPunchEnabled(self.punch_enabled)]);
                }
                Action::SetPunchRange(_) => {
                    inverse_actions = Some(vec![
                        Action::SetPunchRange(self.punch_range_samples),
                        Action::SetPunchEnabled(self.punch_enabled),
                    ]);
                }
                Action::SetMetronomeEnabled(_) => {
                    inverse_actions =
                        Some(vec![Action::SetMetronomeEnabled(self.metronome_enabled)]);
                }
                Action::SetTimeSignature { .. } => {
                    inverse_actions = Some(vec![Action::SetTimeSignature {
                        numerator: self.tsig_num,
                        denominator: self.tsig_denom,
                    }]);
                }
                Action::SetTempoMap { .. } => {
                    inverse_actions = Some(vec![Action::SetTempoMap {
                        tempo_points: self.tempo_points.clone(),
                        time_signature_points: self.time_signature_points.clone(),
                    }]);
                }
                Action::SetClipPlaybackEnabled(_) => {
                    inverse_actions = Some(vec![Action::SetClipPlaybackEnabled(
                        self.clip_playback_enabled,
                    )]);
                }
                Action::SetRecordEnabled(_) => {
                    inverse_actions = Some(vec![Action::SetRecordEnabled(self.record_enabled)]);
                }
                Action::SetGlobalMidiLearnBinding { target, .. } => {
                    let binding = match target {
                        crate::message::GlobalMidiLearnTarget::PlayPause => {
                            self.global_midi_learn_play_pause.clone()
                        }
                        crate::message::GlobalMidiLearnTarget::Stop => {
                            self.global_midi_learn_stop.clone()
                        }
                        crate::message::GlobalMidiLearnTarget::RecordToggle => {
                            self.global_midi_learn_record_toggle.clone()
                        }
                    };
                    inverse_actions = Some(vec![Action::SetGlobalMidiLearnBinding {
                        target: *target,
                        binding,
                    }]);
                }
                Action::SetModulators(_) => {
                    inverse_actions = Some(vec![Action::SetModulators(self.modulators.clone())]);
                }
                Action::SetSessionMidiLearnBinding { target, .. } => {
                    let binding = match target {
                        crate::message::SessionMidiLearnTarget::Slot {
                            track_name,
                            scene_index,
                        } => self
                            .session_midi_learn_slots
                            .get(&(track_name.clone(), *scene_index))
                            .cloned(),
                        crate::message::SessionMidiLearnTarget::Scene(scene_index) => {
                            self.session_midi_learn_scenes.get(scene_index).cloned()
                        }
                        crate::message::SessionMidiLearnTarget::StopTrack(track_name) => {
                            self.session_midi_learn_stop_track.get(track_name).cloned()
                        }
                        crate::message::SessionMidiLearnTarget::StopAll => {
                            self.session_midi_learn_stop_all.clone()
                        }
                    };
                    inverse_actions = Some(vec![Action::SetSessionMidiLearnBinding {
                        target: target.clone(),
                        binding,
                    }]);
                }
                _ => {}
            }
        }
        inverse_actions
    }

    pub(crate) async fn handle_add_track(
        &mut self,
        name: String,
        audio_ins: usize,
        midi_ins: usize,
        audio_outs: usize,
        midi_outs: usize,
        folder: bool,
    ) {
        let tracks = &mut self.state.lock().tracks;
        if tracks.contains_key(&name) {
            self.notify_clients(Err(format!("Track {} already exists", name)))
                .await;
            return;
        }
        let maybe_hw = if let Some(info) = self.hw_driver_info {
            Some((info.cycle_samples, info.sample_rate as f64))
        } else {
            #[cfg(unix)]
            if let Some(jack) = &self.jack_runtime {
                let j = jack;
                Some((j.buffer_size, j.sample_rate as f64))
            } else {
                None
            }
            #[cfg(not(unix))]
            None
        };

        if let Some((chsamples, sample_rate)) = maybe_hw {
            let track = if folder {
                Track::new_folder(
                    name.clone(),
                    audio_ins,
                    audio_outs,
                    midi_ins,
                    midi_outs,
                    chsamples,
                    sample_rate,
                )
            } else {
                Track::new(
                    name.clone(),
                    audio_ins,
                    audio_outs,
                    midi_ins,
                    midi_outs,
                    chsamples,
                    sample_rate,
                )
            };
            tracks.insert(name.clone(), Arc::new(track));
            if let Some(track) = tracks.get(&name) {
                let mut t = track.lock();
                t.set_clip_playback_enabled(self.clip_playback_enabled);
                t.set_transport_timing(self.tempo_bpm, self.tsig_num, self.tsig_denom);
                t.set_session_base_dir(self.session_dir.clone());
            }
        } else {
            self.notify_clients(Err(
                "Engine needs to open audio device before adding audio track".to_string(),
            ))
            .await;
        }
    }

    pub(crate) async fn handle_remove_track(&mut self, name: String, record_history: bool) {
        let mut descendant_names = Vec::new();
        self.collect_descendant_track_names(&name, &mut descendant_names);
        let names_to_remove: Vec<String> = descendant_names
            .iter()
            .cloned()
            .chain(std::iter::once(name.clone()))
            .collect();

        let combined_inverse = if record_history && !self.history_suspended {
            let state = self.state.lock();
            let mut inv = Vec::new();
            for n in &names_to_remove {
                if let Some(mut actions) =
                    create_inverse_actions(&Action::RemoveTrack(n.clone()), &state)
                {
                    inv.append(&mut actions);
                }
                for route in self.midi_hw_in_routes.iter().filter(|r| &r.to_track == n) {
                    inv.push(Action::Connect {
                        from_track: format!("midi:hw:in:{}", route.device),
                        from_port: 0,
                        to_track: route.to_track.clone(),
                        to_port: route.to_port,
                        kind: Kind::MIDI,
                    });
                }
                for route in self
                    .midi_hw_out_routes
                    .iter()
                    .filter(|r| &r.from_track == n)
                {
                    inv.push(Action::Connect {
                        from_track: route.from_track.clone(),
                        from_port: route.from_port,
                        to_track: format!("midi:hw:out:{}", route.device),
                        to_port: 0,
                        kind: Kind::MIDI,
                    });
                }
            }

            // Reorder so all AddTrack actions come first, then everything else, then
            // explicit Connect actions. This mirrors EndHistoryGroup and guarantees that
            // tracks are recreated before they are re-parented or reconnected.
            let mut add_tracks = Vec::new();
            let mut connections = Vec::new();
            let mut rest = Vec::new();
            for action in inv {
                match action {
                    Action::AddTrack { .. } => add_tracks.push(action),
                    Action::Connect { .. } => connections.push(action),
                    _ => rest.push(action),
                }
            }
            let mut ordered = add_tracks;
            ordered.extend(rest);
            ordered.extend(connections);
            ordered
        } else {
            Vec::new()
        };

        for n in &descendant_names {
            self.remove_single_track(n).await;
            self.notify_clients(Ok(Action::RemoveTrack(n.clone())))
                .await;
        }
        self.remove_single_track(&name).await;

        if record_history && !self.history_suspended && !combined_inverse.is_empty() {
            self.history.record(UndoEntry {
                forward_actions: vec![Action::RemoveTrack(name.clone())],
                inverse_actions: combined_inverse,
            });
        }
    }

    pub(crate) async fn handle_connect(
        &mut self,
        from_track: &str,
        from_port: usize,
        to_track: &str,
        to_port: usize,
        kind: Kind,
    ) {
        match kind {
            Kind::Audio => {
                let (from_audio_io, to_audio_io) =
                    self.resolve_audio_route_ports(from_track, from_port, to_track, to_port);
                match (from_audio_io, to_audio_io) {
                    (Some(source), Some(target)) => {
                        if from_track != "hw:in"
                            && to_track != "hw:out"
                            && self.check_if_leads_to_kind(Kind::Audio, to_track, from_track)
                        {
                            self.notify_clients(Err("Circular routing is not allowed!".into()))
                                .await;
                            return;
                        }
                        crate::audio::io::AudioIO::connect(&source, &target);
                    }
                    (None, _) => {
                        self.notify_clients(Err(format!(
                            "Source track '{}' not found",
                            from_track
                        )))
                        .await;
                    }
                    (_, None) => {
                        self.notify_clients(Err(format!(
                            "Destination track '{}' not found",
                            to_track
                        )))
                        .await;
                    }
                }
            }
            Kind::MIDI => {
                let from_hw_in_device = Self::midi_hw_in_device(from_track);
                let to_hw_out_device = Self::midi_hw_out_device(to_track);
                let from_is_invalid_hw = Self::midi_hw_out_device(from_track).is_some();
                let to_is_invalid_hw = Self::midi_hw_in_device(to_track).is_some();

                if from_is_invalid_hw || to_is_invalid_hw {
                    self.notify_clients(Err(
                        "Invalid MIDI hardware connection direction".to_string()
                    ))
                    .await;
                    return;
                }

                if from_hw_in_device.is_none()
                    && to_hw_out_device.is_none()
                    && self.check_if_leads_to_kind(Kind::MIDI, to_track, from_track)
                {
                    self.notify_clients(Err("Circular routing is not allowed!".into()))
                        .await;
                    return;
                }

                let state = self.state.lock();
                let from_track_handle = state.tracks.get(from_track);
                let to_track_handle = state.tracks.get(to_track);

                if let (Some(from_device), Some(to_device)) = (from_hw_in_device, to_hw_out_device)
                {
                    let route = MidiHwThruRoute {
                        from_device: from_device.to_string(),
                        to_device: to_device.to_string(),
                    };
                    if !self.midi_hw_thru_routes.iter().any(|r| r == &route) {
                        self.midi_hw_thru_routes.push(route);
                    }
                } else if let Some(device) = from_hw_in_device {
                    if let Some(t_t) = to_track_handle {
                        if t_t.lock().midi.ins.get(to_port).is_none() {
                            self.notify_clients(Err(format!(
                                "MIDI input port {} not found on track '{}'",
                                to_port, to_track
                            )))
                            .await;
                            return;
                        }
                        let route = MidiHwInRoute {
                            device: device.to_string(),
                            to_track: to_track.to_string(),
                            to_port,
                        };
                        if !self.midi_hw_in_routes.iter().any(|r| r == &route) {
                            self.midi_hw_in_routes.push(route);
                        }
                    } else {
                        self.notify_clients(Err(format!(
                            "MIDI destination track not found: {}",
                            to_track
                        )))
                        .await;
                    }
                } else if let Some(device) = to_hw_out_device {
                    if let Some(f_t) = from_track_handle {
                        if f_t.lock().midi.outs.get(from_port).is_none() {
                            self.notify_clients(Err(format!(
                                "MIDI output port {} not found on track '{}'",
                                from_port, from_track
                            )))
                            .await;
                            return;
                        }
                        let route = MidiHwOutRoute {
                            from_track: from_track.to_string(),
                            from_port,
                            device: device.to_string(),
                        };
                        if !self.midi_hw_out_routes.iter().any(|r| r == &route) {
                            self.midi_hw_out_routes.push(route);
                        }
                    } else {
                        self.notify_clients(Err(format!(
                            "MIDI source track not found: {}",
                            from_track
                        )))
                        .await;
                    }
                } else {
                    match (from_track_handle, to_track_handle) {
                        (Some(f_t), Some(t_t)) => {
                            let to_in_res = t_t.lock().midi.ins.get(to_port).cloned();
                            if let Some(to_in) = to_in_res {
                                let mut from_track = f_t.lock();
                                if let Err(e) = from_track.midi.connect_out(from_port, to_in) {
                                    self.notify_clients(Err(e)).await;
                                    return;
                                }
                                from_track.invalidate_midi_route_cache();
                            } else {
                                self.notify_clients(Err(format!(
                                    "MIDI input port {} not found on track '{}'",
                                    to_port, to_track
                                )))
                                .await;
                            }
                        }
                        _ => {
                            self.notify_clients(Err(format!(
                                "MIDI tracks not found: {} or {}",
                                from_track, to_track
                            )))
                            .await;
                        }
                    }
                }
            }
        };
    }

    pub(crate) async fn handle_disconnect(&mut self, action: Action) {
        let Action::Disconnect {
            ref from_track,
            from_port,
            ref to_track,
            to_port,
            kind,
        } = action
        else {
            return;
        };

        if kind == Kind::Audio {
            if let Err(e) = self.disconnect_audio_route_and_notify(action.clone()).await {
                self.notify_clients(Err(e)).await;
            }
        } else if kind == Kind::MIDI {
            let from_hw_in_device = Self::midi_hw_in_device(from_track);
            let to_hw_out_device = Self::midi_hw_out_device(to_track);

            if let (Some(from_device), Some(to_device)) = (from_hw_in_device, to_hw_out_device) {
                let before = self.midi_hw_thru_routes.len();
                self.midi_hw_thru_routes
                    .retain(|r| !(r.from_device == from_device && r.to_device == to_device));
                if self.midi_hw_thru_routes.len() < before {
                    self.notify_clients(Ok(action.clone())).await;
                } else {
                    self.notify_clients(Err(format!(
                        "Disconnect failed: MIDI route not found ({} -> {})",
                        from_track, to_track
                    )))
                    .await;
                }
                return;
            }

            if let Some(device) = from_hw_in_device {
                let before = self.midi_hw_in_routes.len();
                self.midi_hw_in_routes.retain(|r| {
                    !(r.device == device && r.to_track == *to_track && r.to_port == to_port)
                });
                if self.midi_hw_in_routes.len() < before {
                    self.notify_clients(Ok(action.clone())).await;
                } else {
                    self.notify_clients(Err(format!(
                        "Disconnect failed: MIDI route not found ({} -> {})",
                        from_track, to_track
                    )))
                    .await;
                }
                return;
            }

            if let Some(device) = to_hw_out_device {
                let before = self.midi_hw_out_routes.len();
                self.midi_hw_out_routes.retain(|r| {
                    !(r.from_track == *from_track && r.from_port == from_port && r.device == device)
                });
                if self.midi_hw_out_routes.len() < before {
                    self.notify_clients(Ok(action.clone())).await;
                } else {
                    self.notify_clients(Err(format!(
                        "Disconnect failed: MIDI route not found ({} -> {})",
                        from_track, to_track
                    )))
                    .await;
                }
                return;
            }

            let state = self.state.lock();
            if let (Some(f_t), Some(t_t)) =
                (state.tracks.get(from_track), state.tracks.get(to_track))
                && let Some(to_in) = t_t.lock().midi.ins.get(to_port).cloned()
            {
                let mut from_track = f_t.lock();
                if let Err(e) = from_track.midi.disconnect_out(from_port, &to_in) {
                    self.notify_clients(Err(e)).await;
                } else {
                    from_track.invalidate_midi_route_cache();
                    self.notify_clients(Ok(action.clone())).await;
                }
            } else {
                self.notify_clients(Err(format!(
                    "Disconnect failed: MIDI ports not found ({} -> {})",
                    from_track, to_track
                )))
                .await;
            }
        }
    }

    pub(crate) async fn handle_track_set_parent(
        &mut self,
        track_name: &str,
        parent_name: Option<&str>,
    ) {
        let track = match self.track_handle_or_err(track_name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return;
            }
        };
        if parent_name == Some(track_name) {
            self.notify_clients(Err("Track cannot be its own parent".to_string()))
                .await;
            return;
        }

        // Validate the new parent is a folder (if any).
        if let Some(parent_name) = parent_name {
            let state = self.state.lock();
            let parent = state.tracks.get(parent_name);
            if parent.is_none() {
                self.notify_clients(Err(format!(
                    "Parent track '{}' does not exist",
                    parent_name
                )))
                .await;
                return;
            }
            if !parent.unwrap().lock().is_folder {
                self.notify_clients(Err(format!("Track '{}' is not a folder", parent_name)))
                    .await;
                return;
            }
        }

        // Disconnect from the old parent and update its child list.
        {
            let old_parent_name = track.lock().parent_track.clone();
            if let Some(old_parent_name) = old_parent_name {
                let state = self.state.lock();
                if let (Some(parent_arc), Some(child_arc)) = (
                    state.tracks.get(&old_parent_name).cloned(),
                    state.tracks.get(track_name).cloned(),
                ) {
                    {
                        let mut parent = parent_arc.lock();
                        parent.child_tracks.retain(|c| c.lock().name != *track_name);
                    }
                    {
                        let mut child = child_arc.lock();
                        let parent = parent_arc.lock();
                        child.disconnect_from_parent(&parent);
                    }
                }
            }
        }

        let mut disconnect_actions = Vec::new();

        // Remove all existing audio and MIDI connections involving this track.
        {
            let state = self.state.lock();
            let hw_inputs = self.all_hw_input_audio_ports();
            let hw_outputs = self.all_hw_output_audio_ports();
            if let Some(child_arc) = state.tracks.get(track_name).cloned() {
                let child = child_arc.lock();
                for (port_idx, inp) in child.audio.ins.iter().enumerate() {
                    let sources = inp.connections.lock().clone();
                    for src in sources {
                        let _ = AudioIO::disconnect(&src, inp);
                        if let Some((src_name, src_port)) = self.find_audio_io_owner(&state, &src) {
                            disconnect_actions.push(Action::Disconnect {
                                from_track: src_name,
                                from_port: src_port,
                                to_track: track_name.to_string(),
                                to_port: port_idx,
                                kind: Kind::Audio,
                            });
                        } else if let Some(src_port) = hw_inputs
                            .iter()
                            .position(|hw_in| std::sync::Arc::ptr_eq(hw_in, &src))
                        {
                            disconnect_actions.push(Action::Disconnect {
                                from_track: "hw:in".to_string(),
                                from_port: src_port,
                                to_track: track_name.to_string(),
                                to_port: port_idx,
                                kind: Kind::Audio,
                            });
                        }
                    }
                }
                for (port_idx, out) in child.audio.outs.iter().enumerate() {
                    let targets = out.connections.lock().clone();
                    for tgt in targets {
                        let _ = AudioIO::disconnect(out, &tgt);
                        if let Some((tgt_name, tgt_port)) = self.find_audio_io_owner(&state, &tgt) {
                            disconnect_actions.push(Action::Disconnect {
                                from_track: track_name.to_string(),
                                from_port: port_idx,
                                to_track: tgt_name,
                                to_port: tgt_port,
                                kind: Kind::Audio,
                            });
                        } else if let Some(tgt_port) = hw_outputs
                            .iter()
                            .position(|hw_out| std::sync::Arc::ptr_eq(hw_out, &tgt))
                        {
                            disconnect_actions.push(Action::Disconnect {
                                from_track: track_name.to_string(),
                                from_port: port_idx,
                                to_track: "hw:out".to_string(),
                                to_port: tgt_port,
                                kind: Kind::Audio,
                            });
                        }
                    }
                }

                // Remove MIDI hardware routes.
                for route in self
                    .midi_hw_in_routes
                    .iter()
                    .filter(|r| r.to_track == track_name)
                {
                    disconnect_actions.push(Action::Disconnect {
                        from_track: format!("midi:hw:in:{}", route.device),
                        from_port: 0,
                        to_track: track_name.to_string(),
                        to_port: route.to_port,
                        kind: Kind::MIDI,
                    });
                }
                self.midi_hw_in_routes.retain(|r| r.to_track != track_name);

                for route in self
                    .midi_hw_out_routes
                    .iter()
                    .filter(|r| r.from_track == track_name)
                {
                    disconnect_actions.push(Action::Disconnect {
                        from_track: track_name.to_string(),
                        from_port: route.from_port,
                        to_track: format!("midi:hw:out:{}", route.device),
                        to_port: 0,
                        kind: Kind::MIDI,
                    });
                }
                self.midi_hw_out_routes
                    .retain(|r| r.from_track != track_name);

                // Remove track-to-track MIDI connections where this track is the source.
                for (port_idx, out) in child.midi.outs.iter().enumerate() {
                    let targets = out.connections();
                    for tgt in targets {
                        if let Some((tgt_name, tgt_port, _)) = self.find_midi_io_owner(&state, &tgt)
                        {
                            let _ = MIDIIO::disconnect(out, &tgt);
                            disconnect_actions.push(Action::Disconnect {
                                from_track: track_name.to_string(),
                                from_port: port_idx,
                                to_track: tgt_name,
                                to_port: tgt_port,
                                kind: Kind::MIDI,
                            });
                        }
                    }
                }
            }

            // Remove track-to-track MIDI connections where this track is the target.
            let child_input_arcs: Vec<_> =
                if let Some(child_arc) = state.tracks.get(track_name).cloned() {
                    let child = child_arc.lock();
                    child.midi.ins.clone()
                } else {
                    Vec::new()
                };
            for (other_name, other_track) in &state.tracks {
                if other_name.as_str() == track_name {
                    continue;
                }
                let other = other_track.lock();
                for (out_port, out) in other.midi.outs.iter().enumerate() {
                    let targets = out.connections();
                    for tgt in targets {
                        if let Some(to_port) = child_input_arcs
                            .iter()
                            .position(|inp| std::sync::Arc::ptr_eq(inp, &tgt))
                        {
                            let _ = MIDIIO::disconnect(out, &tgt);
                            disconnect_actions.push(Action::Disconnect {
                                from_track: other_name.clone(),
                                from_port: out_port,
                                to_track: track_name.to_string(),
                                to_port,
                                kind: Kind::MIDI,
                            });
                        }
                    }
                }
            }
        }

        // Apply the parent change.
        {
            track.lock().parent_track = parent_name.map(str::to_string);
        }

        // Connect to the new parent and add to its child list.
        if let Some(parent_name) = parent_name {
            let state = self.state.lock();
            if let (Some(parent_arc), Some(child_arc)) = (
                state.tracks.get(parent_name).cloned(),
                state.tracks.get(track_name).cloned(),
            ) {
                {
                    let mut parent = parent_arc.lock();
                    parent.child_tracks.push(child_arc.clone());
                }
                {
                    let mut child = child_arc.lock();
                    let mut parent = parent_arc.lock();
                    // Folder input -> child input (one-to-one when counts match).
                    if parent.audio.ins.len() == child.audio.ins.len() {
                        for (parent_in, child_in) in
                            parent.audio.ins.iter().zip(child.audio.ins.iter())
                        {
                            Track::connect_directed_audio(parent_in, child_in);
                        }
                    }
                    // Child output -> folder output (one-to-one when counts match).
                    if parent.audio.outs.len() == child.audio.outs.len() {
                        for (child_out, parent_out) in
                            child.audio.outs.iter().zip(parent.audio.outs.iter())
                        {
                            AudioIO::connect(child_out, parent_out);
                        }
                    }
                    // Folder MIDI input -> child MIDI input (one-to-one when counts match).
                    if parent.midi.ins.len() == child.midi.ins.len() {
                        for (parent_in, child_in) in
                            parent.midi.ins.iter().zip(child.midi.ins.iter())
                        {
                            child_in.add_connection(parent_in);
                        }
                    }
                    // Child MIDI output -> folder MIDI output (one-to-one when counts match).
                    if parent.midi.outs.len() == child.midi.outs.len() {
                        for (child_out, parent_out) in
                            child.midi.outs.iter().zip(parent.midi.outs.iter())
                        {
                            child_out.add_connection(parent_out);
                        }
                    }
                    child.invalidate_audio_route_cache();
                    parent.invalidate_audio_route_cache();
                    child.invalidate_midi_route_cache();
                    parent.invalidate_midi_route_cache();
                }
            }
        }

        // Restore default input->output passthrough so audio/MIDI can flow
        // through the track whether it is a root track or a folder child.
        {
            let state = self.state.lock();
            if let Some(child_arc) = state.tracks.get(track_name).cloned() {
                let mut child = child_arc.lock();
                child.ensure_default_audio_passthrough();
                child.ensure_default_midi_passthrough();
            }
        }

        for action in disconnect_actions {
            self.notify_clients(Ok(action)).await;
        }

        self.notify_clients(Ok(Action::TrackSetParent {
            track_name: track_name.to_string(),
            parent_name: parent_name.map(str::to_string),
        }))
        .await;
    }

    pub(crate) async fn handle_clip_move(&mut self, action: Action) {
        let Action::ClipMove {
            ref kind,
            ref from,
            ref to,
            copy,
        } = action
        else {
            return;
        };
        if let Some(from_track_handle) = self.state.lock().tracks.get(&from.track_name)
            && let Some(to_track_handle) = self.state.lock().tracks.get(&to.track_name)
        {
            let from_track = from_track_handle.lock();
            let to_track = to_track_handle.lock();
            match kind {
                Kind::Audio => {
                    if from.clip_index >= from_track.audio.clips().len() {
                        self.notify_clients(Err(format!(
                            "Clip index {} is too high, as track {} has only {} clips!",
                            from.clip_index,
                            from_track.name.clone(),
                            from_track.audio.clips().len(),
                        )))
                        .await;
                        return;
                    }
                    if from_track.audio.ins.len() != to_track.audio.ins.len() {
                        self.notify_clients(Err(format!(
                            "Cannot move/copy audio clip from '{}' ({} inputs) to '{}' ({} inputs)",
                            from_track.name,
                            from_track.audio.ins.len(),
                            to_track.name,
                            to_track.audio.ins.len()
                        )))
                        .await;
                        return;
                    }
                    let Some(clip_copy) = from_track.audio.clips().get(from.clip_index).cloned()
                    else {
                        return;
                    };
                    if !copy {
                        from_track.audio.remove_clip(from.clip_index);
                    }
                    let mut clip_copy = (*clip_copy).clone();
                    clip_copy.start = to.sample_offset;
                    let max_lane = to_track.audio.ins.len().saturating_sub(1);
                    clip_copy.input_channel = to.input_channel.min(max_lane);
                    to_track.audio.push_clip(clip_copy);
                }
                Kind::MIDI => {
                    if from.clip_index >= from_track.midi.clips().len() {
                        self.notify_clients(Err(format!(
                            "Clip index {} is too high, as track {} has only {} clips!",
                            from.clip_index,
                            from_track.name.clone(),
                            from_track.midi.clips().len(),
                        )))
                        .await;
                        return;
                    }
                    let Some(clip_copy) = from_track.midi.clips().get(from.clip_index).cloned()
                    else {
                        return;
                    };
                    if !copy {
                        from_track.midi.remove_clip(from.clip_index);
                    }
                    let mut clip_copy = (*clip_copy).clone();
                    clip_copy.start = to.sample_offset;
                    let max_lane = to_track.midi.ins.len().saturating_sub(1);
                    clip_copy.input_channel = to.input_channel.min(max_lane);
                    to_track.midi.push_clip(clip_copy);
                }
            }
        }
    }

    pub(crate) async fn handle_rename_track(&mut self, action: Action) -> bool {
        let Action::RenameTrack {
            ref old_name,
            ref new_name,
        } = action
        else {
            return false;
        };

        if self.state.lock().tracks.contains_key(new_name) {
            self.notify_clients(Err(format!("Track '{}' already exists", new_name)))
                .await;
            return true;
        }

        let Some(track) = self.state.lock().tracks.remove(old_name) else {
            self.notify_clients(Err(format!("Track '{}' not found", old_name)))
                .await;
            return true;
        };

        track.lock().name = new_name.clone();
        self.state.lock().tracks.insert(new_name.clone(), track);
        for other in self.state.lock().tracks.values() {
            let mut other = other.lock();
            if other.parent_track.as_deref() == Some(old_name.as_str()) {
                other.parent_track = Some(new_name.clone());
            }
        }

        if let Some(recording) = self.audio_recordings.remove(old_name) {
            self.audio_recordings.insert(new_name.clone(), recording);
        }
        if let Some(recording) = self.midi_recordings.remove(old_name) {
            self.midi_recordings.insert(new_name.clone(), recording);
        }

        for route in &mut self.midi_hw_in_routes {
            if route.to_track == *old_name {
                route.to_track = new_name.clone();
            }
        }
        for route in &mut self.midi_hw_out_routes {
            if route.from_track == *old_name {
                route.from_track = new_name.clone();
            }
        }
        if let Some((armed_track, target, device)) = self.pending_midi_learn.clone()
            && armed_track == *old_name
        {
            self.pending_midi_learn = Some((new_name.clone(), target, device));
        }

        self.notify_clients(Ok(Action::RenameTrack {
            old_name: old_name.clone(),
            new_name: new_name.clone(),
        }))
        .await;

        false
    }

    pub(crate) async fn handle_add_clip(&mut self, action: Action) -> bool {
        let Action::AddClip {
            ref clip_id,
            ref name,
            ref track_name,
            start,
            length,
            offset,
            input_channel,
            muted,
            ref peaks_file,
            kind,
            fade_enabled,
            fade_in_samples,
            fade_out_samples,
            ref source_name,
            source_offset,
            source_length,
            ref preview_name,
            ref pitch_correction_points,
            pitch_correction_frame_likeness,
            pitch_correction_inertia_ms,
            pitch_correction_formant_compensation,
            ref plugin_graph_json,
        } = action
        else {
            return false;
        };

        self.add_clip_to_track(ClipAddRequest {
            clip_id,
            name,
            track_name,
            start,
            length,
            offset,
            input_channel,
            muted,
            peaks_file: peaks_file.clone(),
            kind,
            fade_enabled,
            fade_in_samples,
            fade_out_samples,
            source_name: source_name.clone(),
            source_offset,
            source_length,
            preview_name: preview_name.clone(),
            pitch_correction_points: pitch_correction_points.clone(),
            pitch_correction_frame_likeness,
            pitch_correction_inertia_ms,
            pitch_correction_formant_compensation,
            plugin_graph_json: plugin_graph_json.clone(),
        });
        if let Some(track) = self.state.lock().tracks.get(track_name).cloned() {
            let track_name = track_name.clone();
            tokio::task::spawn_blocking(move || {
                track.lock().preload_clips();
                tracing::debug!("Preloaded clips for track '{}' after AddClip", track_name);
            });
        }

        false
    }

    pub(crate) async fn handle_track_set_folder(&mut self, a: Action) -> bool {
        let Action::TrackSetFolder {
            ref track_name,
            is_folder,
        } = a
        else {
            return false;
        };

        let track = match self.track_handle_or_err(track_name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        if is_folder {
            let is_master = track.lock().is_master();
            if is_master {
                self.notify_clients(Err(format!(
                    "Track '{}' is the master track and cannot be made a folder",
                    track_name
                )))
                .await;
                return true;
            }
        }
        {
            let mut track = track.lock();
            track.is_folder = is_folder;
            track.ensure_default_audio_passthrough();
            track.ensure_default_midi_passthrough();
        }
        self.notify_clients(Ok(Action::TrackSetFolder {
            track_name: track_name.clone(),
            is_folder,
        }))
        .await;

        false
    }

    pub(crate) async fn handle_track_connect_audio(&mut self, a: Action) -> bool {
        let Action::TrackConnectAudio {
            ref track_name,
            ref from,
            from_port,
            ref to,
            to_port,
        } = a
        else {
            return false;
        };

        if self
            .reject_if_track_frozen(track_name, "routing changes")
            .await
        {
            return true;
        }
        let track = match self.track_handle_or_err(track_name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        if let Err(e) =
            track
                .lock()
                .connect_audio_connectable(from.clone(), from_port, to.clone(), to_port)
        {
            self.notify_clients(Err(e)).await;
            return true;
        }

        false
    }

    pub(crate) async fn handle_track_disconnect_audio(&mut self, a: Action) -> bool {
        let Action::TrackDisconnectAudio {
            ref track_name,
            ref from,
            from_port,
            ref to,
            to_port,
        } = a
        else {
            return false;
        };

        if self
            .reject_if_track_frozen(track_name, "routing changes")
            .await
        {
            return true;
        }
        let track = match self.track_handle_or_err(track_name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        if let Err(e) =
            track
                .lock()
                .disconnect_audio_connectable(from.clone(), from_port, to.clone(), to_port)
        {
            self.notify_clients(Err(e)).await;
            return true;
        }

        false
    }

    pub(crate) async fn handle_track_connect_midi(&mut self, a: Action) -> bool {
        let Action::TrackConnectMidi {
            ref track_name,
            ref from,
            from_port,
            ref to,
            to_port,
        } = a
        else {
            return false;
        };

        if self
            .reject_if_track_frozen(track_name, "routing changes")
            .await
        {
            return true;
        }
        let track = match self.track_handle_or_err(track_name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        if let Err(e) =
            track
                .lock()
                .connect_midi_connectable(from.clone(), from_port, to.clone(), to_port)
        {
            self.notify_clients(Err(e)).await;
            return true;
        }

        false
    }

    pub(crate) async fn handle_track_disconnect_midi(&mut self, a: Action) -> bool {
        let Action::TrackDisconnectMidi {
            ref track_name,
            ref from,
            from_port,
            ref to,
            to_port,
        } = a
        else {
            return false;
        };

        if self
            .reject_if_track_frozen(track_name, "routing changes")
            .await
        {
            return true;
        }
        let track = match self.track_handle_or_err(track_name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        if let Err(e) =
            track
                .lock()
                .disconnect_midi_connectable(from.clone(), from_port, to.clone(), to_port)
        {
            self.notify_clients(Err(e)).await;
            return true;
        }

        false
    }

    pub(crate) async fn handle_set_clip_pitch_correction(&mut self, a: Action) -> bool {
        let Action::SetClipPitchCorrection {
            ref track_name,
            clip_index,
            ref preview_name,
            ref source_name,
            source_offset,
            source_length,
            ref pitch_correction_points,
            pitch_correction_frame_likeness,
            pitch_correction_inertia_ms,
            pitch_correction_formant_compensation,
        } = a
        else {
            return false;
        };

        self.set_clip_pitch_correction(
            track_name,
            clip_index,
            preview_name.clone(),
            source_name.clone(),
            source_offset,
            source_length,
            pitch_correction_points.clone(),
            pitch_correction_frame_likeness,
            pitch_correction_inertia_ms,
            pitch_correction_formant_compensation,
        );

        false
    }

    pub(crate) async fn handle_track_remove_audio_output(&mut self, a: Action) -> bool {
        let Action::TrackRemoveAudioOutput(ref name) = a else {
            return false;
        };

        let track = match self.track_handle_or_err(name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        let (hw_outputs, track_inputs) = {
            let state = self.state.lock();
            let hw_outputs = self.all_hw_output_audio_ports();
            let track_inputs = state
                .tracks
                .iter()
                .filter(|(track_name, _)| *track_name != name)
                .flat_map(|(_, handle)| handle.lock().audio.ins.clone())
                .collect::<Vec<_>>();
            (hw_outputs, track_inputs)
        };
        if let Err(e) = track.lock().remove_audio_output(&hw_outputs, &track_inputs) {
            self.notify_clients(Err(e)).await;
            return true;
        }

        false
    }

    pub(crate) async fn handle_track_toggle_folder(&mut self, a: Action) -> bool {
        let Action::TrackToggleFolder { ref track_name } = a else {
            return false;
        };

        let track = match self.track_handle_or_err(track_name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        {
            let t = track.lock();
            t.folder_open.fetch_not(Ordering::Relaxed);
        }
        self.notify_clients(Ok(Action::TrackToggleFolder {
            track_name: track_name.clone(),
        }))
        .await;

        self.notify_clients(Ok(Action::TrackSetFolder {
            track_name: track_name.clone(),
            is_folder: track.lock().is_folder,
        }))
        .await;

        false
    }

    pub(crate) async fn handle_add_grouped_clip(&mut self, a: Action) -> bool {
        let Action::AddGroupedClip {
            ref track_name,
            kind,
            ref audio_clip,
            ref midi_clip,
        } = a
        else {
            return false;
        };

        self.add_grouped_clip_to_track(track_name, kind, audio_clip.clone(), midi_clip.clone());
        if let Some(track) = self.state.lock().tracks.get(track_name).cloned() {
            let track_name = track_name.clone();
            tokio::task::spawn_blocking(move || {
                track.lock().preload_clips();
                tracing::debug!(
                    "Preloaded clips for track '{}' after AddGroupedClip",
                    track_name
                );
            });
        }

        false
    }

    pub(crate) async fn handle_track_add_audio_input(&mut self, a: Action) -> bool {
        let Action::TrackAddAudioInput(ref name) = a else {
            return false;
        };

        let track = match self.track_handle_or_err(name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        if let Err(e) = track.lock().add_audio_input() {
            self.notify_clients(Err(e)).await;
            return true;
        }

        false
    }

    pub(crate) async fn handle_track_add_audio_output(&mut self, a: Action) -> bool {
        let Action::TrackAddAudioOutput(ref name) = a else {
            return false;
        };

        let track = match self.track_handle_or_err(name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        if let Err(e) = track.lock().add_audio_output() {
            self.notify_clients(Err(e)).await;
            return true;
        }

        false
    }

    pub(crate) async fn handle_track_remove_audio_input(&mut self, a: Action) -> bool {
        let Action::TrackRemoveAudioInput(ref name) = a else {
            return false;
        };

        let track = match self.track_handle_or_err(name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        if let Err(e) = track.lock().remove_audio_input() {
            self.notify_clients(Err(e)).await;
            return true;
        }

        false
    }

    pub(crate) async fn handle_track_midi_cc(&mut self, a: Action) -> bool {
        let Action::TrackMidiCc {
            ref track_name,
            channel,
            cc,
            value,
        } = a
        else {
            return false;
        };

        if let Some(track) = self.state.lock().tracks.get(track_name) {
            track
                .lock()
                .rt
                .pending_automation_midi_events
                .push(MidiEvent::new(
                    0,
                    vec![0xB0 | channel.min(15), cc.min(127), value.min(127)],
                ));
        }

        false
    }

    pub(crate) async fn handle_track_toggle_arm(&mut self, a: Action) -> bool {
        let Action::TrackToggleArm(ref name) = a else {
            return false;
        };

        if self.reject_if_track_frozen(name, "arming/disarming").await {
            return true;
        }
        if let Some(track) = self.state.lock().tracks.get(name).cloned() {
            track.lock().arm();
            let armed = track.lock().armed();
            if !armed && self.audio_recordings.contains_key(name) {
                self.flush_track_recording(name).await;
            }
        } else {
            tracing::warn!(
                "TrackToggleArm for '{}' but track not found in engine",
                name
            );
        }

        false
    }

    pub(crate) async fn handle_track_set_midi_lane_channel(&mut self, a: Action) -> bool {
        let Action::TrackSetMidiLaneChannel {
            ref track_name,
            lane,
            channel,
        } = a
        else {
            return false;
        };

        let track = match self.track_handle_or_err(track_name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        track.lock().set_midi_lane_channel(lane, channel);

        false
    }

    pub(crate) async fn handle_track_set_frozen(&mut self, a: Action) -> bool {
        let Action::TrackSetFrozen {
            ref track_name,
            frozen,
        } = a
        else {
            return false;
        };

        let track = match self.track_handle_or_err(track_name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        track.lock().set_frozen(frozen);

        false
    }

    pub(crate) async fn handle_track_clear_default_passthrough(&mut self, a: Action) -> bool {
        let Action::TrackClearDefaultPassthrough { ref track_name } = a else {
            return false;
        };

        if self
            .reject_if_track_frozen(track_name, "plugin graph editing")
            .await
        {
            return true;
        }
        let track = match self.track_handle_or_err(track_name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        track.lock().clear_default_passthrough();

        false
    }

    pub(crate) async fn handle_set_clip_fade(&mut self, a: Action) -> bool {
        let Action::SetClipFade {
            ref track_name,
            clip_index,
            kind,
            fade_enabled,
            fade_in_samples,
            fade_out_samples,
        } = a
        else {
            return false;
        };

        self.set_clip_fade(
            track_name,
            clip_index,
            kind,
            fade_enabled,
            fade_in_samples,
            fade_out_samples,
        );

        false
    }
}
