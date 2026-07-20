use super::*;
#[cfg(target_os = "macos")]
use crate::hw::coreaudio::{HwDriver, HwOptions, MidiHub};
#[cfg(target_os = "windows")]
use crate::hw::options::HwOptions;
#[cfg(target_os = "openbsd")]
use crate::hw::sndio::{HwDriver, HwOptions, MidiHub};
#[cfg(target_os = "windows")]
use crate::hw::wasapi::{self, HwDriver, MidiHub};
#[cfg(target_os = "macos")]
use crate::workers::coreaudio_worker::HwWorker;
#[cfg(target_os = "openbsd")]
use crate::workers::sndio_worker::HwWorker;
#[cfg(target_os = "windows")]
use crate::workers::wasapi_worker::HwWorker;
use crate::{
    message::{Action, HwMidiEvent, Message, MidiControllerData, MidiNoteData},
    midi::io::MidiEvent,
};
use midly::{MetaMessage, Smf, Timing, TrackEventKind, live::LiveEvent};
use std::{collections::HashMap, path::Path, time::Duration};
use tracing::error;

impl Engine {
    pub(crate) fn note_off_events_for_track(&mut self, track_name: &str) -> Vec<HwMidiEvent> {
        let Some(active) = self.active_hw_notes_by_track.remove(track_name) else {
            return vec![];
        };
        let mut channels = std::collections::HashSet::<(String, u8)>::new();
        let mut events = Vec::with_capacity(active.len() * 2);
        for (device, channel, pitch) in active {
            channels.insert((device.clone(), channel));
            events.push(HwMidiEvent {
                device,
                event: MidiEvent::new(0, vec![0x80 | channel.min(15), pitch.min(127), 64]),
            });
        }
        for (device, channel) in channels {
            events.push(HwMidiEvent {
                device,
                event: MidiEvent::new(
                    0,
                    vec![0xB0 | channel.min(15), Self::MIDI_CC_SUSTAIN_PEDAL, 0],
                ),
            });
        }
        events
    }

    pub(crate) fn update_active_hw_notes_for_track(
        &mut self,
        track_name: &str,
        device: &str,
        data: &[u8],
    ) {
        let Some(status) = data.first().copied() else {
            return;
        };
        let channel = status & 0x0F;
        match status & 0xF0 {
            0x80 => {
                if let Some(&pitch) = data.get(1)
                    && let Some(active) = self.active_hw_notes_by_track.get_mut(track_name)
                {
                    active.remove(&(device.to_string(), channel, pitch));
                    if active.is_empty() {
                        self.active_hw_notes_by_track.remove(track_name);
                    }
                }
            }
            0x90 => {
                let Some(&pitch) = data.get(1) else {
                    return;
                };
                let velocity = data.get(2).copied().unwrap_or(0);
                if velocity == 0 {
                    if let Some(active) = self.active_hw_notes_by_track.get_mut(track_name) {
                        active.remove(&(device.to_string(), channel, pitch));
                        if active.is_empty() {
                            self.active_hw_notes_by_track.remove(track_name);
                        }
                    }
                } else {
                    self.active_hw_notes_by_track
                        .entry(track_name.to_string())
                        .or_default()
                        .insert((device.to_string(), channel, pitch));
                }
            }
            _ => {}
        }
    }

    pub(crate) fn note_off_events_for_all_active_tracks(&mut self) -> Vec<HwMidiEvent> {
        let track_names: Vec<String> = self.active_hw_notes_by_track.keys().cloned().collect();
        let mut events = Vec::new();
        for track_name in track_names {
            events.extend(self.note_off_events_for_track(&track_name));
        }
        events
    }

    pub(crate) fn panic_events_for_all_hw_midi_outputs(&self) -> Vec<HwMidiEvent> {
        let mut active_channels = std::collections::HashSet::<(String, u8)>::new();
        for active in self.active_hw_notes_by_track.values() {
            for (device, channel, _pitch) in active {
                active_channels.insert((device.clone(), *channel));
            }
        }
        let mut events = Vec::with_capacity(active_channels.len());
        for (device, channel) in active_channels {
            events.push(HwMidiEvent {
                device,
                event: MidiEvent::new(0, vec![0xB0 | channel, Self::MIDI_CC_ALL_SOUND_OFF, 0]),
            });
        }
        events
    }

    pub(crate) fn note_off_events_for_active_snapshot(
        &self,
        snapshot: &HashMap<String, std::collections::HashSet<(String, u8, u8)>>,
        frame: u32,
    ) -> Vec<HwMidiEvent> {
        let mut channels = std::collections::HashSet::<(String, u8)>::new();
        let mut events = Vec::new();
        for active in snapshot.values() {
            for (device, channel, pitch) in active {
                channels.insert((device.clone(), *channel));
                events.push(HwMidiEvent {
                    device: device.clone(),
                    event: MidiEvent::new(
                        frame,
                        vec![0x80 | (*channel).min(15), (*pitch).min(127), 64],
                    ),
                });
            }
        }
        for (device, channel) in channels {
            events.push(HwMidiEvent {
                device,
                event: MidiEvent::new(
                    frame,
                    vec![0xB0 | channel.min(15), Self::MIDI_CC_SUSTAIN_PEDAL, 0],
                ),
            });
        }
        events
    }

    pub(crate) fn parse_midi_clip_for_edit(
        path: &Path,
        sample_rate: f64,
        clip_start: usize,
    ) -> Result<MidiEditParseResult, String> {
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        let smf = Smf::parse(&bytes).map_err(|e| e.to_string())?;
        let Timing::Metrical(ppq) = smf.header.timing else {
            return Ok((vec![], vec![], vec![]));
        };
        let ppq = u64::from(ppq.as_int().max(1));

        let mut tempo_changes: Vec<(u64, u32)> = vec![(0, 500_000)];
        for track in &smf.tracks {
            let mut tick = 0_u64;
            for event in track {
                tick = tick.saturating_add(event.delta.as_int() as u64);
                if let TrackEventKind::Meta(MetaMessage::Tempo(us_per_q)) = event.kind {
                    tempo_changes.push((tick, us_per_q.as_int()));
                }
            }
        }
        tempo_changes.sort_by_key(|(tick, _)| *tick);
        let mut normalized_tempos: Vec<(u64, u32)> = Vec::with_capacity(tempo_changes.len());
        for (tick, tempo) in tempo_changes {
            if let Some(last) = normalized_tempos.last_mut()
                && last.0 == tick
            {
                last.1 = tempo;
            } else {
                normalized_tempos.push((tick, tempo));
            }
        }
        let tempo_changes = normalized_tempos;

        let ticks_to_samples = |tick: u64| -> usize {
            let mut total_us: u128 = 0;
            let mut prev_tick = 0_u64;
            let mut current_tempo_us = 500_000_u32;
            for (change_tick, tempo_us) in &tempo_changes {
                if *change_tick > tick {
                    break;
                }
                let seg_ticks = change_tick.saturating_sub(prev_tick);
                total_us = total_us.saturating_add(
                    u128::from(seg_ticks).saturating_mul(u128::from(current_tempo_us))
                        / u128::from(ppq),
                );
                prev_tick = *change_tick;
                current_tempo_us = *tempo_us;
            }
            let rem = tick.saturating_sub(prev_tick);
            total_us = total_us.saturating_add(
                u128::from(rem).saturating_mul(u128::from(current_tempo_us)) / u128::from(ppq),
            );
            ((total_us as f64 / 1_000_000.0) * sample_rate).round() as usize
        };

        let mut notes = Vec::<MidiNoteData>::new();
        let mut controllers = Vec::<MidiControllerData>::new();
        let mut passthrough_events = Vec::<(u64, Vec<u8>)>::new();
        let mut active_notes: HashMap<(u8, u8), Vec<(u64, u8)>> = HashMap::new();

        for track in &smf.tracks {
            let mut tick = 0_u64;
            for event in track {
                tick = tick.saturating_add(event.delta.as_int() as u64);
                match event.kind {
                    TrackEventKind::Midi { channel, message } => {
                        let channel_u8 = channel.as_int();
                        match message {
                            midly::MidiMessage::NoteOn { key, vel } => {
                                let pitch = key.as_int();
                                let velocity = vel.as_int();
                                if velocity == 0 {
                                    if let Some(starts) = active_notes.get_mut(&(channel_u8, pitch))
                                        && let Some((start_tick, start_vel)) = starts.pop()
                                    {
                                        let start_sample = ticks_to_samples(start_tick);
                                        let end_sample = ticks_to_samples(tick);
                                        notes.push(MidiNoteData {
                                            start_sample,
                                            length_samples: end_sample
                                                .saturating_sub(start_sample)
                                                .max(1),
                                            pitch,
                                            velocity: start_vel,
                                            channel: channel_u8,
                                        });
                                    }
                                } else {
                                    active_notes
                                        .entry((channel_u8, pitch))
                                        .or_default()
                                        .push((tick, velocity));
                                }
                            }
                            midly::MidiMessage::NoteOff { key, .. } => {
                                let pitch = key.as_int();
                                if let Some(starts) = active_notes.get_mut(&(channel_u8, pitch))
                                    && let Some((start_tick, start_vel)) = starts.pop()
                                {
                                    let start_sample = ticks_to_samples(start_tick);
                                    let end_sample = ticks_to_samples(tick);
                                    notes.push(MidiNoteData {
                                        start_sample,
                                        length_samples: end_sample
                                            .saturating_sub(start_sample)
                                            .max(1),
                                        pitch,
                                        velocity: start_vel,
                                        channel: channel_u8,
                                    });
                                }
                            }
                            midly::MidiMessage::Controller { controller, value } => {
                                controllers.push(MidiControllerData {
                                    sample: ticks_to_samples(tick),
                                    controller: controller.as_int(),
                                    value: value.as_int(),
                                    channel: channel_u8,
                                });
                            }
                            _ => {
                                let mut data = Vec::with_capacity(3);
                                if (LiveEvent::Midi { channel, message })
                                    .write(&mut data)
                                    .is_ok()
                                {
                                    passthrough_events.push((ticks_to_samples(tick) as u64, data));
                                }
                            }
                        }
                    }
                    TrackEventKind::SysEx(payload) => {
                        let mut data = Vec::with_capacity(payload.len() + 2);
                        data.push(0xF0);
                        data.extend_from_slice(payload);
                        if data.last().copied() != Some(0xF7) {
                            data.push(0xF7);
                        }
                        passthrough_events.push((ticks_to_samples(tick) as u64, data));
                    }
                    TrackEventKind::Escape(payload) => {
                        let mut data = Vec::with_capacity(payload.len() + 1);
                        data.push(0xF7);
                        data.extend_from_slice(payload);
                        passthrough_events.push((ticks_to_samples(tick) as u64, data));
                    }
                    _ => {}
                }
            }
        }

        for ((channel, pitch), starts) in active_notes {
            for (start_tick, velocity) in starts {
                let start_sample = ticks_to_samples(start_tick);
                let end_sample = ticks_to_samples(start_tick.saturating_add(ppq / 8));
                notes.push(MidiNoteData {
                    start_sample,
                    length_samples: end_sample.saturating_sub(start_sample).max(1),
                    pitch,
                    velocity,
                    channel,
                });
            }
        }

        notes.sort_by_key(|n| (n.start_sample, n.pitch));
        controllers.sort_by_key(|c| (c.sample, c.controller));
        passthrough_events.sort_by_key(|(sample, _)| *sample);

        let min_sample = notes
            .iter()
            .map(|n| n.start_sample)
            .chain(controllers.iter().map(|c| c.sample))
            .chain(passthrough_events.iter().map(|(s, _)| *s as usize))
            .min()
            .unwrap_or(0);
        if min_sample >= clip_start && clip_start > 0 {
            for note in &mut notes {
                note.start_sample = note.start_sample.saturating_sub(clip_start);
            }
            for ctrl in &mut controllers {
                ctrl.sample = ctrl.sample.saturating_sub(clip_start);
            }
            for (sample, _) in &mut passthrough_events {
                *sample = sample.saturating_sub(clip_start as u64);
            }
        }

        Ok((notes, controllers, passthrough_events))
    }

    pub(crate) fn midi_events_from_notes_and_controllers(
        notes: &[MidiNoteData],
        controllers: &[MidiControllerData],
    ) -> Vec<(u64, Vec<u8>)> {
        let mut events: Vec<(u64, u8, Vec<u8>)> = Vec::new();
        for note in notes {
            let channel = note.channel.min(15);
            let pitch = note.pitch.min(127);
            let velocity = note.velocity.min(127);
            let start = note.start_sample as u64;
            let end = note.start_sample.saturating_add(note.length_samples).max(1) as u64;
            events.push((start, 2, vec![0x90 | channel, pitch, velocity]));
            events.push((end, 0, vec![0x80 | channel, pitch, 64]));
        }
        for ctrl in controllers {
            let channel = ctrl.channel.min(15);
            let controller = ctrl.controller.min(127);
            let value = ctrl.value.min(127);
            events.push((
                ctrl.sample as u64,
                1,
                vec![0xB0 | channel, controller, value],
            ));
        }
        events.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        events
            .into_iter()
            .map(|(sample, _, data)| (sample, data))
            .collect()
    }

    pub(crate) fn apply_midi_edit_action(&mut self, action: &Action) -> Result<(), String> {
        let (track_name, clip_index) = match action {
            Action::ModifyMidiNotes {
                track_name,
                clip_index,
                ..
            }
            | Action::InsertMidiNotes {
                track_name,
                clip_index,
                ..
            }
            | Action::DeleteMidiNotes {
                track_name,
                clip_index,
                ..
            }
            | Action::ModifyMidiControllers {
                track_name,
                clip_index,
                ..
            }
            | Action::InsertMidiControllers {
                track_name,
                clip_index,
                ..
            }
            | Action::DeleteMidiControllers {
                track_name,
                clip_index,
                ..
            }
            | Action::SetMidiSysExEvents {
                track_name,
                clip_index,
                ..
            } => (track_name, *clip_index),
            _ => return Ok(()),
        };

        let track_handle = self
            .state
            .lock()
            .tracks
            .get(track_name)
            .cloned()
            .ok_or_else(|| format!("Track not found: {track_name}"))?;
        let (clip_name, clip_path, sample_rate, clip_start) = {
            let track = track_handle.lock();
            if clip_index >= track.midi.clips().len() {
                return Err(format!(
                    "Invalid MIDI clip index {clip_index} for '{track_name}'"
                ));
            }
            let Some(clip) = track.midi.clips().get(clip_index).cloned() else {
                return Err(format!(
                    "Invalid MIDI clip index {clip_index} for '{track_name}'"
                ));
            };
            let clip_name = clip.name.clone();
            let clip_path = track.resolve_clip_path(&clip_name);
            (clip_name, clip_path, track.sample_rate, clip.start)
        };

        let (mut notes, mut controllers, mut passthrough_events) =
            Self::parse_midi_clip_for_edit(&clip_path, sample_rate, clip_start)?;

        match action {
            Action::ModifyMidiNotes {
                note_indices,
                new_notes,
                ..
            } => {
                for (idx, new_note) in note_indices.iter().zip(new_notes.iter()) {
                    if let Some(note) = notes.get_mut(*idx) {
                        *note = new_note.clone();
                    }
                }
            }
            Action::DeleteMidiNotes { note_indices, .. } => {
                let mut indices = note_indices.clone();
                indices.sort_unstable();
                indices.dedup();
                for idx in indices.into_iter().rev() {
                    if idx < notes.len() {
                        notes.remove(idx);
                    }
                }
            }
            Action::InsertMidiNotes {
                notes: inserted, ..
            } => {
                let mut sorted = inserted.clone();
                sorted.sort_unstable_by_key(|(idx, _)| *idx);
                for (idx, note) in sorted {
                    let at = idx.min(notes.len());
                    notes.insert(at, note);
                }
            }
            Action::ModifyMidiControllers {
                controller_indices,
                new_controllers,
                ..
            } => {
                for (idx, new_ctrl) in controller_indices.iter().zip(new_controllers.iter()) {
                    if let Some(ctrl) = controllers.get_mut(*idx) {
                        *ctrl = new_ctrl.clone();
                    }
                }
            }
            Action::DeleteMidiControllers {
                controller_indices, ..
            } => {
                let mut indices = controller_indices.clone();
                indices.sort_unstable();
                indices.dedup();
                for idx in indices.into_iter().rev() {
                    if idx < controllers.len() {
                        controllers.remove(idx);
                    }
                }
            }
            Action::InsertMidiControllers {
                controllers: inserted,
                ..
            } => {
                let mut sorted = inserted.clone();
                sorted.sort_unstable_by_key(|(idx, _)| *idx);
                for (idx, ctrl) in sorted {
                    let at = idx.min(controllers.len());
                    controllers.insert(at, ctrl);
                }
            }
            Action::SetMidiSysExEvents {
                new_sysex_events, ..
            } => {
                passthrough_events
                    .retain(|(_, data)| !matches!(data.first(), Some(0xF0) | Some(0xF7)));
                passthrough_events.extend(
                    new_sysex_events
                        .iter()
                        .map(|ev| (ev.sample as u64, ev.data.clone())),
                );
            }
            _ => {}
        }

        notes.sort_by_key(|n| (n.start_sample, n.pitch));
        controllers.sort_by_key(|c| (c.sample, c.controller));
        passthrough_events.sort_by_key(|(sample, _)| *sample);
        let mut events = Self::midi_events_from_notes_and_controllers(&notes, &controllers);
        events.extend(passthrough_events);
        events.sort_by_key(|(sample, _)| *sample);
        Self::write_midi_file(&clip_path, sample_rate.max(1.0) as u32, &events)?;
        track_handle.lock().invalidate_midi_clip_cache(&clip_name);
        Ok(())
    }

    pub(crate) fn midi_hw_in_device(track: &str) -> Option<&str> {
        track.strip_prefix("midi:hw:in:")
    }

    pub(crate) fn midi_hw_out_device(track: &str) -> Option<&str> {
        track.strip_prefix("midi:hw:out:")
    }

    pub(crate) fn midi_binding_matches(
        a: &crate::message::MidiLearnBinding,
        b: &crate::message::MidiLearnBinding,
    ) -> bool {
        if a.channel != b.channel || a.cc != b.cc {
            return false;
        }
        match (&a.device, &b.device) {
            (Some(ad), Some(bd)) => ad == bd,
            _ => true,
        }
    }

    pub(crate) fn midi_learn_slot_conflicts(
        &self,
        binding: &crate::message::MidiLearnBinding,
        ignore: Option<MidiLearnSlot>,
    ) -> Vec<String> {
        let mut conflicts = Vec::<String>::new();
        let state = self.state_snapshot.load_full();
        let mut push_conflict = |slot: MidiLearnSlot, label: String| {
            if ignore.as_ref().is_some_and(|i| i == &slot) {
                return;
            }
            conflicts.push(label);
        };
        let check_global =
            |current: &Option<crate::message::MidiLearnBinding>,
             target: crate::message::GlobalMidiLearnTarget,
             label: &str,
             push_conflict: &mut dyn FnMut(MidiLearnSlot, String)| {
                if let Some(existing) = current
                    && Self::midi_binding_matches(binding, existing)
                {
                    push_conflict(MidiLearnSlot::Global(target), format!("Global {label}"));
                }
            };
        check_global(
            &self.global_midi_learn_play_pause,
            crate::message::GlobalMidiLearnTarget::PlayPause,
            "PlayPause",
            &mut push_conflict,
        );
        check_global(
            &self.global_midi_learn_stop,
            crate::message::GlobalMidiLearnTarget::Stop,
            "Stop",
            &mut push_conflict,
        );
        check_global(
            &self.global_midi_learn_record_toggle,
            crate::message::GlobalMidiLearnTarget::RecordToggle,
            "RecordToggle",
            &mut push_conflict,
        );
        for (track_name, track) in state.tracks.iter() {
            let t = track.lock();
            let mut check_track = |current: &Option<crate::message::MidiLearnBinding>,
                                   target: crate::message::TrackMidiLearnTarget,
                                   label: &str| {
                if let Some(existing) = current
                    && Self::midi_binding_matches(binding, existing)
                {
                    push_conflict(
                        MidiLearnSlot::Track(track_name.clone(), target),
                        format!("{track_name} {label}"),
                    );
                }
            };
            check_track(
                &t.midi_learn_volume,
                crate::message::TrackMidiLearnTarget::Volume,
                "Volume",
            );
            check_track(
                &t.midi_learn_balance,
                crate::message::TrackMidiLearnTarget::Balance,
                "Balance",
            );
            check_track(
                &t.midi_learn_mute,
                crate::message::TrackMidiLearnTarget::Mute,
                "Mute",
            );
            check_track(
                &t.midi_learn_solo,
                crate::message::TrackMidiLearnTarget::Solo,
                "Solo",
            );
            check_track(
                &t.midi_learn_arm,
                crate::message::TrackMidiLearnTarget::Arm,
                "Arm",
            );
            check_track(
                &t.midi_learn_input_monitor,
                crate::message::TrackMidiLearnTarget::InputMonitor,
                "InputMonitor",
            );
            check_track(
                &t.midi_learn_disk_monitor,
                crate::message::TrackMidiLearnTarget::DiskMonitor,
                "DiskMonitor",
            );
        }
        for (key, existing) in &self.session_midi_learn_slots {
            if Self::midi_binding_matches(binding, existing) {
                push_conflict(
                    MidiLearnSlot::Session(crate::message::SessionMidiLearnTarget::Slot {
                        track_name: key.0.clone(),
                        scene_index: key.1,
                    }),
                    format!("{} Slot {}", key.0, key.1 + 1),
                );
            }
        }
        for (scene_index, existing) in &self.session_midi_learn_scenes {
            if Self::midi_binding_matches(binding, existing) {
                push_conflict(
                    MidiLearnSlot::Session(crate::message::SessionMidiLearnTarget::Scene(
                        *scene_index,
                    )),
                    format!("Scene {}", scene_index + 1),
                );
            }
        }
        for (track_name, existing) in &self.session_midi_learn_stop_track {
            if Self::midi_binding_matches(binding, existing) {
                push_conflict(
                    MidiLearnSlot::Session(crate::message::SessionMidiLearnTarget::StopTrack(
                        track_name.clone(),
                    )),
                    format!("{track_name} Stop"),
                );
            }
        }
        if let Some(existing) = self.session_midi_learn_stop_all.as_ref()
            && Self::midi_binding_matches(binding, existing)
        {
            push_conflict(
                MidiLearnSlot::Session(crate::message::SessionMidiLearnTarget::StopAll),
                "Stop All Clips".to_string(),
            );
        }
        conflicts
    }

    pub(crate) async fn handle_incoming_hw_cc(
        &mut self,
        device: &str,
        channel: u8,
        cc: u8,
        value: u8,
    ) {
        let gate_key = (device.to_string(), channel, cc);
        let high = value >= 64;
        let prev_high = self.midi_cc_gate.get(&gate_key).copied().unwrap_or(false);
        self.midi_cc_gate.insert(gate_key, high);
        let rising = high && !prev_high;

        if let Some((track_name, target, armed_device)) = self.pending_midi_learn.clone() {
            let binding = crate::message::MidiLearnBinding {
                device: armed_device.or(Some(device.to_string())),
                channel,
                cc,
            };
            let conflicts = self.midi_learn_slot_conflicts(
                &binding,
                Some(MidiLearnSlot::Track(track_name.clone(), target)),
            );
            if !conflicts.is_empty() {
                self.pending_midi_learn = None;
                self.notify_clients(Err(format!(
                    "MIDI learn conflict for '{}' {:?}: {}",
                    track_name,
                    target,
                    conflicts.join(", ")
                )))
                .await;
                return;
            }
            if let Some(track) = self.state_snapshot.load_full().tracks.get(&track_name) {
                match target {
                    crate::message::TrackMidiLearnTarget::Volume => {
                        track.lock().midi_learn_volume = Some(binding.clone());
                    }
                    crate::message::TrackMidiLearnTarget::Balance => {
                        track.lock().midi_learn_balance = Some(binding.clone());
                    }
                    crate::message::TrackMidiLearnTarget::Mute => {
                        track.lock().midi_learn_mute = Some(binding.clone());
                    }
                    crate::message::TrackMidiLearnTarget::Solo => {
                        track.lock().midi_learn_solo = Some(binding.clone());
                    }
                    crate::message::TrackMidiLearnTarget::Arm => {
                        track.lock().midi_learn_arm = Some(binding.clone());
                    }
                    crate::message::TrackMidiLearnTarget::InputMonitor => {
                        track.lock().midi_learn_input_monitor = Some(binding.clone());
                    }
                    crate::message::TrackMidiLearnTarget::DiskMonitor => {
                        track.lock().midi_learn_disk_monitor = Some(binding.clone());
                    }
                }
                self.pending_midi_learn = None;
                self.notify_clients(Ok(Action::TrackSetMidiLearnBinding {
                    track_name: track_name.clone(),
                    target,
                    binding: Some(binding),
                }))
                .await;
            } else {
                self.pending_midi_learn = None;
            }
        }
        if let Some(target) = self.pending_global_midi_learn.take() {
            let binding = crate::message::MidiLearnBinding {
                device: Some(device.to_string()),
                channel,
                cc,
            };
            let conflicts =
                self.midi_learn_slot_conflicts(&binding, Some(MidiLearnSlot::Global(target)));
            if !conflicts.is_empty() {
                self.notify_clients(Err(format!(
                    "Global MIDI learn conflict for {:?}: {}",
                    target,
                    conflicts.join(", ")
                )))
                .await;
                return;
            }
            match target {
                crate::message::GlobalMidiLearnTarget::PlayPause => {
                    self.global_midi_learn_play_pause = Some(binding.clone());
                }
                crate::message::GlobalMidiLearnTarget::Stop => {
                    self.global_midi_learn_stop = Some(binding.clone());
                }
                crate::message::GlobalMidiLearnTarget::RecordToggle => {
                    self.global_midi_learn_record_toggle = Some(binding.clone());
                }
            }
            self.notify_clients(Ok(Action::SetGlobalMidiLearnBinding {
                target,
                binding: Some(binding),
            }))
            .await;
        }
        if let Some(target) = self.pending_session_midi_learn.take() {
            let binding = crate::message::MidiLearnBinding {
                device: Some(device.to_string()),
                channel,
                cc,
            };
            let conflicts = self
                .midi_learn_slot_conflicts(&binding, Some(MidiLearnSlot::Session(target.clone())));
            if !conflicts.is_empty() {
                self.notify_clients(Err(format!(
                    "Session MIDI learn conflict for {:?}: {}",
                    target,
                    conflicts.join(", ")
                )))
                .await;
                return;
            }
            match target {
                crate::message::SessionMidiLearnTarget::Slot {
                    ref track_name,
                    scene_index,
                } => {
                    self.session_midi_learn_slots
                        .insert((track_name.clone(), scene_index), binding.clone());
                }
                crate::message::SessionMidiLearnTarget::Scene(scene_index) => {
                    self.session_midi_learn_scenes
                        .insert(scene_index, binding.clone());
                }
                crate::message::SessionMidiLearnTarget::StopTrack(ref track_name) => {
                    self.session_midi_learn_stop_track
                        .insert(track_name.clone(), binding.clone());
                }
                crate::message::SessionMidiLearnTarget::StopAll => {
                    self.session_midi_learn_stop_all = Some(binding.clone());
                }
            }
            self.notify_clients(Ok(Action::SetSessionMidiLearnBinding {
                target,
                binding: Some(binding),
            }))
            .await;
        }

        let mut mapped_actions = Vec::<Action>::new();
        let state = self.state_snapshot.load_full();
        for (track_name, track) in state.tracks.iter() {
            let t = track.lock();
            if let Some(binding) = t.midi_learn_volume.as_ref() {
                let device_matches = binding.device.as_ref().is_none_or(|d| d.as_str() == device);
                if device_matches && binding.channel == channel && binding.cc == cc {
                    let level = -90.0 + (value as f32 / 127.0) * 110.0;
                    mapped_actions.push(Action::TrackLevel(track_name.clone(), level));
                }
            }
            if let Some(binding) = t.midi_learn_balance.as_ref() {
                let device_matches = binding.device.as_ref().is_none_or(|d| d.as_str() == device);
                if device_matches && binding.channel == channel && binding.cc == cc {
                    let balance = (value as f32 / 127.0) * 2.0 - 1.0;
                    mapped_actions.push(Action::TrackBalance(track_name.clone(), balance));
                }
            }
            if let Some(binding) = t.midi_learn_mute.as_ref() {
                let device_matches = binding.device.as_ref().is_none_or(|d| d.as_str() == device);
                if device_matches && binding.channel == channel && binding.cc == cc {
                    let wanted = value >= 64;
                    if t.muted() != wanted {
                        mapped_actions.push(Action::TrackToggleMute(track_name.clone()));
                    }
                }
            }
            if let Some(binding) = t.midi_learn_solo.as_ref() {
                let device_matches = binding.device.as_ref().is_none_or(|d| d.as_str() == device);
                if device_matches && binding.channel == channel && binding.cc == cc {
                    let wanted = value >= 64;
                    if t.soloed() != wanted {
                        mapped_actions.push(Action::TrackToggleSolo(track_name.clone()));
                    }
                }
            }
            if let Some(binding) = t.midi_learn_arm.as_ref() {
                let device_matches = binding.device.as_ref().is_none_or(|d| d.as_str() == device);
                if device_matches && binding.channel == channel && binding.cc == cc {
                    let wanted = value >= 64;
                    if t.armed() != wanted {
                        mapped_actions.push(Action::TrackToggleArm(track_name.clone()));
                    }
                }
            }
            if let Some(binding) = t.midi_learn_input_monitor.as_ref() {
                let device_matches = binding.device.as_ref().is_none_or(|d| d.as_str() == device);
                if device_matches && binding.channel == channel && binding.cc == cc {
                    let wanted = value >= 64;
                    if t.input_monitor().first() != Some(&wanted) {
                        mapped_actions.push(Action::TrackToggleInputMonitor {
                            track_name: track_name.clone(),
                            lane: 0,
                        });
                    }
                }
            }
            if let Some(binding) = t.midi_learn_disk_monitor.as_ref() {
                let device_matches = binding.device.as_ref().is_none_or(|d| d.as_str() == device);
                if device_matches && binding.channel == channel && binding.cc == cc {
                    let wanted = value >= 64;
                    if t.disk_monitor().first() != Some(&wanted) {
                        mapped_actions.push(Action::TrackToggleDiskMonitor {
                            track_name: track_name.clone(),
                            lane: 0,
                        });
                    }
                }
            }
        }
        let device_matches =
            |binding: &crate::message::MidiLearnBinding| binding.device.as_deref() == Some(device);
        let mut mapped_global_actions = Vec::<Action>::new();
        if let Some(binding) = self.global_midi_learn_play_pause.as_ref()
            && device_matches(binding)
            && binding.channel == channel
            && binding.cc == cc
            && rising
        {
            mapped_global_actions.push(if self.playing {
                Action::Stop
            } else {
                Action::Play
            });
        }
        if let Some(binding) = self.global_midi_learn_stop.as_ref()
            && device_matches(binding)
            && binding.channel == channel
            && binding.cc == cc
            && rising
            && self.playing
        {
            mapped_global_actions.push(Action::Stop);
        }
        if let Some(binding) = self.global_midi_learn_record_toggle.as_ref()
            && device_matches(binding)
            && binding.channel == channel
            && binding.cc == cc
            && rising
        {
            mapped_global_actions.push(Action::SetRecordEnabled(!self.record_enabled));
        }
        if rising {
            for (key, binding) in &self.session_midi_learn_slots {
                let device_matches = binding.device.as_ref().is_none_or(|d| d.as_str() == device);
                if device_matches && binding.channel == channel && binding.cc == cc {
                    mapped_global_actions.push(Action::SessionMidiLearnTriggered {
                        target: crate::message::SessionMidiLearnTarget::Slot {
                            track_name: key.0.clone(),
                            scene_index: key.1,
                        },
                    });
                }
            }
            for (scene_index, binding) in &self.session_midi_learn_scenes {
                let device_matches = binding.device.as_ref().is_none_or(|d| d.as_str() == device);
                if device_matches && binding.channel == channel && binding.cc == cc {
                    mapped_global_actions.push(Action::SessionMidiLearnTriggered {
                        target: crate::message::SessionMidiLearnTarget::Scene(*scene_index),
                    });
                }
            }
            for (track_name, binding) in &self.session_midi_learn_stop_track {
                let device_matches = binding.device.as_ref().is_none_or(|d| d.as_str() == device);
                if device_matches && binding.channel == channel && binding.cc == cc {
                    mapped_global_actions.push(Action::SessionMidiLearnTriggered {
                        target: crate::message::SessionMidiLearnTarget::StopTrack(
                            track_name.clone(),
                        ),
                    });
                }
            }
            if let Some(binding) = self.session_midi_learn_stop_all.as_ref() {
                let device_matches = binding.device.as_ref().is_none_or(|d| d.as_str() == device);
                if device_matches && binding.channel == channel && binding.cc == cc {
                    mapped_global_actions.push(Action::SessionMidiLearnTriggered {
                        target: crate::message::SessionMidiLearnTarget::StopAll,
                    });
                }
            }
        }
        let state = self.state_snapshot.load_full();
        for action in mapped_actions {
            match action {
                Action::TrackLevel(ref track_name, level) => {
                    if let Some(track) = state.tracks.get(track_name) {
                        track.lock().set_level(level);
                        self.notify_clients(Ok(Action::TrackLevel(track_name.clone(), level)))
                            .await;
                    }
                }
                Action::TrackBalance(ref track_name, balance) => {
                    if let Some(track) = state.tracks.get(track_name) {
                        track.lock().set_balance(balance);
                        self.notify_clients(Ok(Action::TrackBalance(track_name.clone(), balance)))
                            .await;
                    }
                }
                Action::TrackToggleMute(ref track_name) => {
                    if let Some(track) = state.tracks.get(track_name) {
                        track.lock().mute();
                        self.notify_clients(Ok(Action::TrackToggleMute(track_name.clone())))
                            .await;
                    }
                }
                Action::TrackTogglePhase(ref track_name) => {
                    if let Some(track) = state.tracks.get(track_name) {
                        track.lock().invert_phase();
                        self.notify_clients(Ok(Action::TrackTogglePhase(track_name.clone())))
                            .await;
                    }
                }
                Action::TrackToggleSolo(ref track_name) => {
                    if let Some(track) = state.tracks.get(track_name) {
                        track.lock().solo();
                        self.notify_clients(Ok(Action::TrackToggleSolo(track_name.clone())))
                            .await;
                    }
                }
                Action::TrackToggleMaster(ref track_name) => {
                    if let Some(track) = state.tracks.get(track_name) {
                        let can_toggle = {
                            let t = track.lock();
                            t.is_master() || (!t.is_folder && t.parent_track.is_none())
                        };
                        if can_toggle {
                            track.lock().toggle_master();
                            self.notify_clients(Ok(Action::TrackToggleMaster(track_name.clone())))
                                .await;
                        }
                    }
                }
                Action::TrackToggleArm(ref track_name) => {
                    if let Some(track) = state.tracks.get(track_name) {
                        track.lock().arm();
                        self.notify_clients(Ok(Action::TrackToggleArm(track_name.clone())))
                            .await;
                    }
                }
                Action::TrackToggleInputMonitor {
                    ref track_name,
                    lane,
                } => {
                    if let Some(track) = state.tracks.get(track_name) {
                        track.lock().toggle_input_monitor(lane);
                        self.notify_clients(Ok(Action::TrackToggleInputMonitor {
                            track_name: track_name.clone(),
                            lane,
                        }))
                        .await;
                    }
                }
                Action::TrackToggleDiskMonitor {
                    ref track_name,
                    lane,
                } => {
                    if let Some(track) = state.tracks.get(track_name) {
                        track.lock().toggle_disk_monitor(lane);
                        self.notify_clients(Ok(Action::TrackToggleDiskMonitor {
                            track_name: track_name.clone(),
                            lane,
                        }))
                        .await;
                    }
                }
                Action::TrackToggleMidiInputMonitor {
                    ref track_name,
                    lane,
                } => {
                    if let Some(track) = state.tracks.get(track_name) {
                        track.lock().toggle_midi_input_monitor(lane);
                        self.notify_clients(Ok(Action::TrackToggleMidiInputMonitor {
                            track_name: track_name.clone(),
                            lane,
                        }))
                        .await;
                    }
                }
                Action::TrackToggleMidiDiskMonitor {
                    ref track_name,
                    lane,
                } => {
                    if let Some(track) = state.tracks.get(track_name) {
                        track.lock().toggle_midi_disk_monitor(lane);
                        self.notify_clients(Ok(Action::TrackToggleMidiDiskMonitor {
                            track_name: track_name.clone(),
                            lane,
                        }))
                        .await;
                    }
                }
                _ => {}
            }
        }
        for action in mapped_global_actions {
            self.handle_request_inner(action, false).await;
        }
    }

    pub(crate) async fn clear_hw_midi_output_state(&mut self, send_panic: bool) {
        self.pending_hw_midi_out_events.clear();
        self.pending_hw_midi_out_events_by_device.clear();
        {
            let state = self.state_snapshot.load_full();
            for track in state.tracks.values() {
                track.lock().take_hw_midi_out_events();
            }
        }

        let panic_events = if send_panic {
            self.note_off_events_for_all_active_tracks()
        } else {
            vec![]
        };

        if let Some(worker) = &self.hw_worker {
            if let Err(e) = worker.tx.send(Message::ClearHWMidiOutEvents).await {
                error!("Error clearing pending HWMidiOutEvents {e}");
            }
            if !panic_events.is_empty()
                && let Err(e) = worker.tx.send(Message::HWMidiOutEvents(panic_events)).await
            {
                error!("Error sending transport restart MIDI panic events {e}");
            }
        } else if !panic_events.is_empty() {
            self.pending_hw_midi_out_events_by_device
                .extend(panic_events);
        }
    }

    pub(crate) async fn handle_request_midi_learn_mappings_report(&mut self) {
        let mut lines = Vec::<String>::new();
        let fmt_binding = |b: &crate::message::MidiLearnBinding| {
            let device = b.device.as_deref().unwrap_or("*");
            format!("{device} CH{} CC{}", b.channel + 1, b.cc)
        };
        if let Some(b) = self.global_midi_learn_play_pause.as_ref() {
            lines.push(format!("Global PlayPause: {}", fmt_binding(b)));
        }
        if let Some(b) = self.global_midi_learn_stop.as_ref() {
            lines.push(format!("Global Stop: {}", fmt_binding(b)));
        }
        if let Some(b) = self.global_midi_learn_record_toggle.as_ref() {
            lines.push(format!("Global RecordToggle: {}", fmt_binding(b)));
        }
        let state = self.state_snapshot.load_full();
        for (track_name, track) in state.tracks.iter() {
            let t = track.lock();
            if let Some(b) = t.midi_learn_volume.as_ref() {
                lines.push(format!("{} Volume: {}", track_name, fmt_binding(b)));
            }
            if let Some(b) = t.midi_learn_balance.as_ref() {
                lines.push(format!("{} Balance: {}", track_name, fmt_binding(b)));
            }
            if let Some(b) = t.midi_learn_mute.as_ref() {
                lines.push(format!("{} Mute: {}", track_name, fmt_binding(b)));
            }
            if let Some(b) = t.midi_learn_solo.as_ref() {
                lines.push(format!("{} Solo: {}", track_name, fmt_binding(b)));
            }
            if let Some(b) = t.midi_learn_arm.as_ref() {
                lines.push(format!("{} Arm: {}", track_name, fmt_binding(b)));
            }
            if let Some(b) = t.midi_learn_input_monitor.as_ref() {
                lines.push(format!("{} InputMonitor: {}", track_name, fmt_binding(b)));
            }
            if let Some(b) = t.midi_learn_disk_monitor.as_ref() {
                lines.push(format!("{} DiskMonitor: {}", track_name, fmt_binding(b)));
            }
        }
        for ((track_name, scene_index), binding) in &self.session_midi_learn_slots {
            lines.push(format!(
                "{} Slot {}: {}",
                track_name,
                scene_index + 1,
                fmt_binding(binding)
            ));
        }
        for (scene_index, binding) in &self.session_midi_learn_scenes {
            lines.push(format!(
                "Scene {}: {}",
                scene_index + 1,
                fmt_binding(binding)
            ));
        }
        for (track_name, binding) in &self.session_midi_learn_stop_track {
            lines.push(format!("{} Stop: {}", track_name, fmt_binding(binding)));
        }
        if let Some(binding) = self.session_midi_learn_stop_all.as_ref() {
            lines.push(format!("Stop All Clips: {}", fmt_binding(binding)));
        }
        if lines.is_empty() {
            lines.push("No MIDI learn mappings configured".to_string());
        }
        self.notify_clients(Ok(Action::MidiLearnMappingsReport { lines }))
            .await;
    }

    pub(crate) async fn handle_track_set_midi_learn_binding(&mut self, action: Action) -> bool {
        let Action::TrackSetMidiLearnBinding {
            ref track_name,
            target,
            ref binding,
        } = action
        else {
            return false;
        };

        if let Some(binding) = binding.as_ref() {
            let conflicts = self.midi_learn_slot_conflicts(
                binding,
                Some(MidiLearnSlot::Track(track_name.clone(), target)),
            );
            if !conflicts.is_empty() {
                self.notify_clients(Err(format!(
                    "MIDI learn conflict for '{}' {:?}: {}",
                    track_name,
                    target,
                    conflicts.join(", ")
                )))
                .await;
                return true;
            }
        }
        let track = match self.track_handle_or_err(track_name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        match target {
            crate::message::TrackMidiLearnTarget::Volume => {
                track.lock().midi_learn_volume = binding.clone();
            }
            crate::message::TrackMidiLearnTarget::Balance => {
                track.lock().midi_learn_balance = binding.clone();
            }
            crate::message::TrackMidiLearnTarget::Mute => {
                track.lock().midi_learn_mute = binding.clone();
            }
            crate::message::TrackMidiLearnTarget::Solo => {
                track.lock().midi_learn_solo = binding.clone();
            }
            crate::message::TrackMidiLearnTarget::Arm => {
                track.lock().midi_learn_arm = binding.clone();
            }
            crate::message::TrackMidiLearnTarget::InputMonitor => {
                track.lock().midi_learn_input_monitor = binding.clone();
            }
            crate::message::TrackMidiLearnTarget::DiskMonitor => {
                track.lock().midi_learn_disk_monitor = binding.clone();
            }
        }

        false
    }

    pub(crate) async fn handle_set_session_midi_learn_binding(&mut self, action: Action) -> bool {
        let Action::SetSessionMidiLearnBinding {
            ref target,
            ref binding,
        } = action
        else {
            return false;
        };

        if let Some(binding) = binding.as_ref() {
            let conflicts = self
                .midi_learn_slot_conflicts(binding, Some(MidiLearnSlot::Session(target.clone())));
            if !conflicts.is_empty() {
                self.notify_clients(Err(format!(
                    "Session MIDI learn conflict for {:?}: {}",
                    target,
                    conflicts.join(", ")
                )))
                .await;
                return true;
            }
        }
        match target {
            crate::message::SessionMidiLearnTarget::Slot {
                track_name,
                scene_index,
            } => {
                if binding.is_some() {
                    self.session_midi_learn_slots
                        .insert((track_name.clone(), *scene_index), binding.clone().unwrap());
                } else {
                    self.session_midi_learn_slots
                        .remove(&(track_name.clone(), *scene_index));
                }
            }
            crate::message::SessionMidiLearnTarget::Scene(scene_index) => {
                if binding.is_some() {
                    self.session_midi_learn_scenes
                        .insert(*scene_index, binding.clone().unwrap());
                } else {
                    self.session_midi_learn_scenes.remove(scene_index);
                }
            }
            crate::message::SessionMidiLearnTarget::StopTrack(track_name) => {
                if binding.is_some() {
                    self.session_midi_learn_stop_track
                        .insert(track_name.clone(), binding.clone().unwrap());
                } else {
                    self.session_midi_learn_stop_track.remove(track_name);
                }
            }
            crate::message::SessionMidiLearnTarget::StopAll => {
                self.session_midi_learn_stop_all = binding.clone();
            }
        }

        false
    }

    pub(crate) async fn handle_set_global_midi_learn_binding(&mut self, a: Action) -> bool {
        let Action::SetGlobalMidiLearnBinding {
            target,
            ref binding,
        } = a
        else {
            return false;
        };

        if let Some(binding) = binding.as_ref() {
            let conflicts =
                self.midi_learn_slot_conflicts(binding, Some(MidiLearnSlot::Global(target)));
            if !conflicts.is_empty() {
                self.notify_clients(Err(format!(
                    "Global MIDI learn conflict for {:?}: {}",
                    target,
                    conflicts.join(", ")
                )))
                .await;
                return true;
            }
        }
        match target {
            crate::message::GlobalMidiLearnTarget::PlayPause => {
                self.global_midi_learn_play_pause = binding.clone();
            }
            crate::message::GlobalMidiLearnTarget::Stop => {
                self.global_midi_learn_stop = binding.clone();
            }
            crate::message::GlobalMidiLearnTarget::RecordToggle => {
                self.global_midi_learn_record_toggle = binding.clone();
            }
        }

        false
    }

    pub(crate) async fn handle_clear_all_midi_learn_bindings(&mut self, a: Action) -> bool {
        let Action::ClearAllMidiLearnBindings = a else {
            return false;
        };

        self.pending_midi_learn = None;
        self.pending_global_midi_learn = None;
        self.pending_session_midi_learn = None;
        self.global_midi_learn_play_pause = None;
        self.global_midi_learn_stop = None;
        self.global_midi_learn_record_toggle = None;
        self.session_midi_learn_slots.clear();
        self.session_midi_learn_scenes.clear();
        self.session_midi_learn_stop_track.clear();
        self.session_midi_learn_stop_all = None;
        self.midi_cc_gate.clear();
        for track in self.state_snapshot.load_full().tracks.values() {
            let mut t = track.lock();
            t.midi_learn_volume = None;
            t.midi_learn_balance = None;
            t.midi_learn_mute = None;
            t.midi_learn_solo = None;
            t.midi_learn_arm = None;
            t.midi_learn_input_monitor = None;
            t.midi_learn_disk_monitor = None;
        }

        false
    }

    pub(crate) async fn handle_panic(&mut self, a: Action) -> bool {
        let Action::Panic = a else {
            return false;
        };

        let panic_events = self.panic_events_for_all_hw_midi_outputs();
        if let Some(worker) = &self.hw_worker {
            if !panic_events.is_empty() {
                if let Err(e) = worker.tx.send(Message::ClearHWMidiOutEvents).await {
                    error!("Error clearing HW MIDI queue for panic {e}");
                }
                if let Err(e) = worker.tx.send(Message::HWMidiOutEvents(panic_events)).await {
                    error!("Error sending HW MIDI panic events {e}");
                }
            }
        } else if !panic_events.is_empty() {
            if let Some(midi_hub) = self.midi_hub.as_mut() {
                midi_hub.write_events_blocking(&panic_events, Duration::from_millis(250));
            } else {
                self.pending_hw_midi_out_events_by_device
                    .extend(panic_events);
            }
        }

        false
    }
}
