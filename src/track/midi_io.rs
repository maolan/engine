use super::{HwMidiOutEvent, TrackData};
use crate::midi::io::MidiEvent;

impl TrackData {
    pub fn push_hw_midi_events(&mut self, events: &[MidiEvent]) {
        let Some(input) = self.midi.ins.first() else {
            return;
        };
        if events.is_empty() {
            return;
        }
        // Safety: engine thread writes before the cycle starts; no node is
        // running.
        unsafe { input.buffer_mut() }.extend_from_slice(events);
    }

    pub fn push_hw_midi_events_to_port(&mut self, port: usize, events: &[MidiEvent]) {
        let Some(input) = self.midi.ins.get(port) else {
            return;
        };
        if events.is_empty() {
            return;
        }
        // Safety: engine thread writes before the cycle starts; no node is
        // running.
        unsafe { input.buffer_mut() }.extend_from_slice(events);
    }

    pub(crate) fn collect_track_input_midi_events(&mut self) -> Vec<Vec<MidiEvent>> {
        self.rt.record_tap_midi_in.clear();
        let midi_disk_monitor = self.midi_disk_monitor();
        let midi_input_monitor = self.midi_input_monitor();
        let midi_lane_channels = self.midi_lane_channels();
        let midi_disk_active = midi_disk_monitor.iter().any(|&m| m);
        let clip_playback_active = midi_disk_active && self.rt.clip_playback_enabled;
        // Collect raw events from MIDI inputs first; allocation needs mutable
        // access to `self` (for the MPE voice allocator), so it is done after
        // the input borrow is released.
        let mut lane_buffers: Vec<Vec<MidiEvent>> = Vec::with_capacity(self.midi.ins.len());
        for (lane, input) in self.midi.ins.iter().enumerate() {
            // Safety: plan single-writer invariant — this task is the sole
            // writer of its own ports this cycle; sources it reads were
            // produced by earlier plan nodes (LOCKLESS.md Phase 3).
            let mut buffer = unsafe { input.buffer_mut() };
            for event in buffer.iter() {
                self.mpe_state.feed(&event.data);
            }
            self.rt.record_tap_midi_in.extend(buffer.iter().cloned());
            let monitor = midi_input_monitor.get(lane).copied().unwrap_or(false);
            let channel_filter = midi_lane_channels.get(lane).copied().flatten();
            if clip_playback_active && !monitor {
                buffer.clear();
            } else if (monitor || self.rt.record_tap_enabled)
                && let Some(channel) = channel_filter
            {
                buffer.retain(|event| Self::event_matches_midi_channel(event, channel));
            }
            buffer.sort_by_key(|event| event.frame);
            input.mark_finished();
            lane_buffers.push(std::mem::take(&mut *buffer));
        }
        for buffer in &mut lane_buffers {
            *buffer = self.allocate_mpe_events(std::mem::take(buffer));
            buffer.sort_by_key(|event| event.frame);
        }
        self.rt.record_tap_midi_in.sort_by_key(|e| e.frame);
        lane_buffers
    }

    pub(crate) fn event_matches_midi_channel(event: &MidiEvent, channel: u8) -> bool {
        let Some(status) = event.data.first().copied() else {
            return true;
        };
        if !(0x80..=0xEF).contains(&status) {
            return true;
        }
        (status & 0x0F) == channel.min(15)
    }

    pub(crate) fn route_track_inputs_to_track_outputs(&mut self, _input_events: &[Vec<MidiEvent>]) {
        for out in &self.midi.outs {
            // Safety: plan single-writer invariant — the folder-output task is
            // the sole writer of its own ports this cycle (LOCKLESS.md
            // Phase 3).
            unsafe { out.buffer_mut() }.clear();
        }
        if !self.output_enabled() || self.is_folder {
            return;
        }
        for out in &self.midi.outs {
            // Safety: as above — sole writer; sources were produced by earlier
            // plan nodes.
            unsafe { out.process() };
        }
    }

    pub(crate) fn route_modulator_midi_to_track_outputs(&mut self) {
        if self.rt.pending_modulator_midi_events.is_empty() {
            return;
        }
        let events = std::mem::take(&mut self.rt.pending_modulator_midi_events);
        if !self.output_enabled() {
            return;
        }
        for out in &self.midi.outs {
            // Safety: plan single-writer invariant — the folder-output task is
            // the sole writer of its own ports this cycle (LOCKLESS.md
            // Phase 3).
            unsafe { out.buffer_mut() }.extend_from_slice(&events);
        }
    }

    pub(crate) fn route_automation_midi_to_track_outputs(&mut self) {
        if self.rt.pending_automation_midi_events.is_empty() {
            return;
        }
        let events = std::mem::take(&mut self.rt.pending_automation_midi_events);
        if !self.output_enabled() {
            return;
        }
        for out in &self.midi.outs {
            // Safety: plan single-writer invariant — the folder-output task is
            // the sole writer of its own ports this cycle (LOCKLESS.md
            // Phase 3).
            unsafe { out.buffer_mut() }.extend_from_slice(&events);
        }
    }

    pub(crate) fn clear_local_midi_inputs(&self) {
        for input in &self.midi.ins {
            // Safety: plan edges serialize every consumer of the track inputs
            // before this folder-output node runs; no reader is active now.
            unsafe { input.buffer_mut() }.clear();
        }
    }

    pub(crate) fn collect_hw_midi_output_events(&mut self) {
        self.rt.pending_hw_midi_out_events.clear();
        for (port, out) in self.midi.outs.iter().enumerate() {
            // Safety: plan single-writer invariant — this folder-output task
            // is the sole writer of these ports and just filled them this
            // cycle (LOCKLESS.md Phase 3).
            let buffer = unsafe { out.buffer() };
            self.rt.pending_hw_midi_out_events.extend(
                buffer
                    .iter()
                    .cloned()
                    .map(|event| HwMidiOutEvent { port, event }),
            );
        }
    }

    pub fn take_hw_midi_out_events(&mut self) -> Vec<HwMidiOutEvent> {
        std::mem::take(&mut self.rt.pending_hw_midi_out_events)
    }
}
