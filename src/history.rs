use crate::message::Action;
use crate::state::State;

pub(crate) fn audio_clip_to_data(
    clip: &crate::audio::clip::AudioClip,
) -> crate::message::AudioClipData {
    crate::message::AudioClipData {
        id: clip.id.clone(),
        name: clip.name.clone(),
        start: clip.start,
        length: clip.end.saturating_sub(clip.start).max(1),
        offset: clip.offset,
        input_channel: clip.input_channel,
        muted: clip.muted,
        reversed: clip.reversed,
        gain_db: clip.gain_db,
        peaks_file: clip.peaks_file.clone(),
        fade_enabled: clip.fade_enabled,
        fade_in_samples: clip.fade_in_samples,
        fade_out_samples: clip.fade_out_samples,
        preview_name: clip.pitch_correction_preview_name.clone(),
        source_name: clip.pitch_correction_source_name.clone(),
        source_offset: clip.pitch_correction_source_offset,
        source_length: clip.pitch_correction_source_length,
        pitch_correction_points: clip.pitch_correction_points.clone(),
        pitch_correction_frame_likeness: clip.pitch_correction_frame_likeness,
        pitch_correction_inertia_ms: clip.pitch_correction_inertia_ms,
        pitch_correction_formant_compensation: clip.pitch_correction_formant_compensation,
        pitch_correction_detector: Default::default(),
        pitch_correction_mode: Default::default(),
        plugin_graph_json: clip.plugin_graph_json.clone(),
        grouped_clips: clip.grouped_clips.iter().map(audio_clip_to_data).collect(),
    }
}

pub(crate) fn midi_clip_to_data(
    clip: &crate::midi::clip::MIDIClip,
) -> crate::message::MidiClipData {
    crate::message::MidiClipData {
        id: clip.id.clone(),
        name: clip.name.clone(),
        start: clip.start,
        length: clip.end.saturating_sub(clip.start).max(1),
        offset: clip.offset,
        input_channel: clip.input_channel,
        muted: clip.muted,
        reversed: clip.reversed,
        grouped_clips: clip.grouped_clips.iter().map(midi_clip_to_data).collect(),
    }
}

#[derive(Clone, Debug)]
pub struct UndoEntry {
    pub forward_actions: Vec<Action>,
    pub inverse_actions: Vec<Action>,
}

/// Whether `action` is undoable. Thin dispatcher: per-variant decisions live
/// in the engine feature modules (Phase 4).
pub fn should_record(action: &Action) -> bool {
    crate::engine::history::should_record_action(action)
}

/// Single inverse constructor. Thin dispatcher over the feature modules.
pub fn create_inverse_action(action: &Action, state: &State) -> Option<Action> {
    crate::engine::history::create_inverse_action_for(action, state)
}

/// Multi-inverse constructor. Thin dispatcher over the feature modules.
pub fn create_inverse_actions(action: &Action, state: &State) -> Option<Vec<Action>> {
    crate::engine::history::create_inverse_actions_for(action, state)
}
