use super::*;
use crate::history::UndoEntry;
use crate::message::Action;

pub struct History {
    undo_stack: VecDeque<UndoEntry>,
    redo_stack: VecDeque<UndoEntry>,
    max_history: usize,
    save_point: Option<usize>,
}

impl History {
    pub fn new(max_history: usize) -> Self {
        Self {
            undo_stack: VecDeque::new(),
            redo_stack: VecDeque::new(),
            max_history,
            save_point: None,
        }
    }

    pub fn mark_save_point(&mut self) {
        self.save_point = Some(self.undo_stack.len());
    }

    pub fn is_dirty(&self) -> bool {
        match self.save_point {
            Some(point) => self.undo_stack.len() != point,
            None => !self.undo_stack.is_empty(),
        }
    }

    pub fn record(&mut self, entry: UndoEntry) {
        self.undo_stack.push_back(entry);
        self.redo_stack.clear();

        if self.undo_stack.len() > self.max_history {
            self.undo_stack.pop_front();
        }
    }

    pub fn undo(&mut self) -> Option<Vec<Action>> {
        self.undo_stack.pop_back().map(|entry| {
            let inverse = entry.inverse_actions.clone();
            self.redo_stack.push_back(entry);
            inverse
        })
    }

    pub fn redo(&mut self) -> Option<Vec<Action>> {
        self.redo_stack.pop_back().map(|entry| {
            let forward = entry.forward_actions.clone();
            self.undo_stack.push_back(entry);
            forward
        })
    }

    pub fn clear(&mut self) {
        self.undo_stack.clear();
        self.redo_stack.clear();
    }
}

impl Default for History {
    fn default() -> Self {
        Self::new(100)
    }
}
/// Aggregate undo-record predicate across the feature modules (per-variant
/// decisions live next to the handlers they invert; Phase 4).
pub(crate) fn should_record_action(action: &Action) -> bool {
    transport::undo_should_record(action)
        || automation::undo_should_record(action)
        || session::undo_should_record(action)
        || midi::undo_should_record(action)
        || topology::undo_should_record(action)
}

/// Aggregate single inverse constructor across the feature modules.
pub(crate) fn create_inverse_action_for(action: &Action, state: &State) -> Option<Action> {
    automation::undo_inverse(action, state)
        .or_else(|| session::undo_inverse(action, state))
        .or_else(|| midi::undo_inverse(action, state))
        .or_else(|| topology::undo_inverse(action, state))
}

/// Aggregate multi-inverse constructor across the feature modules.
pub(crate) fn create_inverse_actions_for(action: &Action, state: &State) -> Option<Vec<Action>> {
    automation::undo_inverse_actions(action, state)
        .or_else(|| session::undo_inverse_actions(action, state))
        .or_else(|| midi::undo_inverse_actions(action, state))
        .or_else(|| topology::undo_inverse_actions(action, state))
}
/// Reorder a completed history group's inverse actions so track additions
/// undo first, then everything else, and connections last.
pub(crate) fn compress_history_group(group: &mut UndoEntry) {
    let mut add_tracks = Vec::new();
    let mut connections = Vec::new();
    let mut rest = Vec::new();
    for action in std::mem::take(&mut group.inverse_actions) {
        if matches!(action, Action::AddTrack { .. }) {
            add_tracks.push(action);
        } else if matches!(action, Action::Connect { .. }) {
            connections.push(action);
        } else {
            rest.push(action);
        }
    }
    group.inverse_actions = add_tracks;
    group.inverse_actions.extend(rest);
    group.inverse_actions.extend(connections);
}

impl Engine {
    /// History and session-restore request arms.
    pub(crate) async fn handle_history_request(&mut self, a: Action) -> bool {
        match a {
            Action::BeginHistoryGroup => {
                if self.history_group.is_none() {
                    self.history_group = Some(UndoEntry {
                        forward_actions: vec![],
                        inverse_actions: vec![],
                    });
                }
            }
            Action::EndHistoryGroup => {
                if let Some(mut group) = self.history_group.take()
                    && !group.forward_actions.is_empty()
                    && !group.inverse_actions.is_empty()
                {
                    compress_history_group(&mut group);
                    self.history.record(group);
                }
            }
            Action::MarkHistorySavePoint => {
                self.history.mark_save_point();
                self.notify_event(Event::HistoryState {
                    dirty: self.history.is_dirty(),
                })
                .await;
            }
            Action::ClearHistory => {
                self.history.clear();
                self.history.mark_save_point();
            }
            Action::BeginSessionRestore => {
                self.history_suspended = true;
                self.history.clear();
            }
            Action::EndSessionRestore => {
                self.history.clear();
                self.history_suspended = false;
                self.preload_track_clips_spawn();
            }
            Action::Undo | Action::Redo | Action::ApplyGroupedActions(_) => {}
            _ => {}
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::clip::AudioClip;
    use crate::audio::io::AudioIO;
    use crate::history::{create_inverse_action, create_inverse_actions, should_record};
    use crate::kind::Kind;
    use crate::message::{ClipMoveFrom, ClipMoveTo, MidiLearnBinding, TrackMidiLearnTarget};
    use crate::plugins::types::Vst3PluginState;
    use crate::state::State;
    use crate::track::Track;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    fn make_state_with_track(track: Track) -> State {
        let mut state = State::default();
        state.tracks.insert(track.name.clone(), Arc::new(track));
        state
    }

    fn binding(cc: u8) -> MidiLearnBinding {
        MidiLearnBinding {
            device: Some("midi".to_string()),
            channel: 1,
            cc,
        }
    }

    #[test]
    fn history_record_limits_size_and_clears_redo_on_new_entry() {
        let mut history = History::new(2);
        let a = UndoEntry {
            forward_actions: vec![Action::SetTempo(120.0)],
            inverse_actions: vec![Action::SetTempo(110.0)],
        };
        let b = UndoEntry {
            forward_actions: vec![Action::SetLoopEnabled(true)],
            inverse_actions: vec![Action::SetLoopEnabled(false)],
        };
        let c = UndoEntry {
            forward_actions: vec![Action::SetMetronomeEnabled(true)],
            inverse_actions: vec![Action::SetMetronomeEnabled(false)],
        };

        history.record(a);
        history.record(b.clone());
        history.record(c.clone());

        let undo = history.undo().unwrap();
        assert!(matches!(
            undo.as_slice(),
            [Action::SetMetronomeEnabled(false)]
        ));

        let redo = history.redo().unwrap();
        assert!(matches!(
            redo.as_slice(),
            [Action::SetMetronomeEnabled(true)]
        ));

        history.undo();
        history.record(UndoEntry {
            forward_actions: vec![Action::SetClipPlaybackEnabled(true)],
            inverse_actions: vec![Action::SetClipPlaybackEnabled(false)],
        });

        assert!(history.redo().is_none());
        let undo = history.undo().unwrap();
        assert!(matches!(
            undo.as_slice(),
            [Action::SetClipPlaybackEnabled(false)]
        ));
        let undo = history.undo().unwrap();
        assert!(matches!(undo.as_slice(), [Action::SetLoopEnabled(false)]));
        assert!(history.undo().is_none());
    }

    #[test]
    fn inverse_audio_clip_move_restores_cross_section_fades() {
        let track = Track::new("Synth".to_string(), 1, 1, 0, 0, 128, 48_000.0);
        let mut first = AudioClip::new("first.wav".to_string(), 0, 100);
        first.fade_in_samples = 11;
        first.fade_out_samples = 12;
        let mut second = AudioClip::new("second.wav".to_string(), 150, 250);
        second.fade_in_samples = 21;
        second.fade_out_samples = 22;
        track.audio.push_clip(first);
        track.audio.push_clip(second);
        let state = make_state_with_track(track);

        let actions = create_inverse_actions(
            &Action::ClipMove {
                kind: Kind::Audio,
                from: ClipMoveFrom {
                    track_name: "Synth".to_string(),
                    clip_index: 0,
                },
                to: ClipMoveTo {
                    track_name: "Synth".to_string(),
                    sample_offset: 80,
                    input_channel: 0,
                },
                copy: false,
            },
            &state,
        )
        .unwrap();

        assert!(matches!(actions[0], Action::ClipMove { .. }));
        assert!(matches!(
            actions[1],
            Action::SetClipFade {
                clip_index: 0,
                fade_in_samples: 21,
                fade_out_samples: 22,
                ..
            }
        ));
        assert!(matches!(
            actions[2],
            Action::SetClipFade {
                clip_index: 1,
                fade_in_samples: 11,
                fade_out_samples: 12,
                ..
            }
        ));
    }

    #[test]
    fn history_clear_removes_pending_undo_and_redo_entries() {
        let mut history = History::new(4);
        history.record(UndoEntry {
            forward_actions: vec![Action::SetTempo(120.0)],
            inverse_actions: vec![Action::SetTempo(100.0)],
        });
        history.record(UndoEntry {
            forward_actions: vec![Action::SetLoopEnabled(true)],
            inverse_actions: vec![Action::SetLoopEnabled(false)],
        });

        assert!(history.undo().is_some());
        assert!(history.redo().is_some());

        history.clear();

        assert!(history.undo().is_none());
        assert!(history.redo().is_none());
    }

    #[test]
    fn history_with_zero_capacity_discards_recorded_entries() {
        let mut history = History::new(0);
        history.record(UndoEntry {
            forward_actions: vec![Action::SetTempo(120.0)],
            inverse_actions: vec![Action::SetTempo(100.0)],
        });

        assert!(history.undo().is_none());
        assert!(history.redo().is_none());
    }

    #[test]
    fn should_record_covers_recent_transport_and_lv2_actions() {
        assert!(should_record(&Action::SetLoopEnabled(true)));
        assert!(should_record(&Action::SetLoopRange(Some((64, 128)))));
        assert!(should_record(&Action::SetPunchEnabled(true)));
        assert!(should_record(&Action::SetPunchRange(Some((32, 96)))));
        assert!(should_record(&Action::SetMetronomeEnabled(true)));
        assert!(!should_record(&Action::SetClipPlaybackEnabled(false)));
        assert!(!should_record(&Action::SetRecordEnabled(true)));
        assert!(should_record(&Action::SetClipBounds {
            track_name: "t".to_string(),
            clip_index: 0,
            kind: Kind::Audio,
            start: 64,
            length: 32,
            offset: 16,
        }));
        assert!(should_record(&Action::TrackLoadVst3Plugin {
            track_name: "t".to_string(),
            plugin_id: "/tmp/test.vst3".to_string(),
            instance_id: None,
        }));
        #[cfg(unix)]
        {
            assert!(should_record(&Action::TrackLoadLv2Plugin {
                track_name: "t".to_string(),
                plugin_uri: "urn:test".to_string(),
                instance_id: None,
            }));
            assert!(should_record(&Action::TrackSetLv2ControlValue {
                track_name: "t".to_string(),
                instance_id: 0,
                index: 1,
                value: 0.5,
            }));
            assert!(!should_record(&Action::TrackSetLv2PluginState {
                track_name: "t".to_string(),
                instance_id: 0,
                state: vec![],
            }));
        }
        assert!(!should_record(&Action::TrackVst3RestoreState {
            track_name: "t".to_string(),
            instance_id: 0,
            state: Vst3PluginState {
                plugin_id: "id".to_string(),
                component_state: vec![],
                controller_state: vec![],
            },
        }));
    }

    #[test]
    fn create_inverse_action_for_add_clip_targets_next_clip_index() {
        let track = Track::new("t".to_string(), 1, 1, 0, 0, 64, 48_000.0);
        track
            .audio
            .push_clip(AudioClip::new("existing".to_string(), 0, 16));
        let state = make_state_with_track(track);

        let inverse = create_inverse_action(
            &Action::AddClip {
                clip_id: String::new(),
                name: "new".to_string(),
                track_name: "t".to_string(),
                start: 32,
                length: 16,
                offset: 0,
                input_channel: 0,
                muted: false,
                reversed: false,
                gain_db: 0.0,
                peaks_file: None,
                kind: Kind::Audio,
                fade_enabled: false,
                fade_in_samples: 0,
                fade_out_samples: 0,
                source_name: None,
                source_offset: None,
                source_length: None,
                preview_name: None,
                pitch_correction_points: vec![],
                pitch_correction_frame_likeness: None,
                pitch_correction_inertia_ms: None,
                pitch_correction_formant_compensation: None,
                pitch_correction_detector: Default::default(),
                pitch_correction_mode: Default::default(),
                plugin_graph_json: None,
            },
            &state,
        )
        .unwrap();

        match inverse {
            Action::RemoveClip {
                track_name,
                kind,
                clip_indices,
            } => {
                assert_eq!(track_name, "t");
                assert_eq!(kind, Kind::Audio);
                assert_eq!(clip_indices, vec![1]);
            }
            other => panic!("unexpected inverse action: {other:?}"),
        }
    }

    #[test]
    fn create_inverse_action_for_set_clip_bounds_restores_previous_audio_bounds() {
        let track = Track::new("t".to_string(), 1, 1, 0, 0, 64, 48_000.0);
        let mut clip = AudioClip::new("clip".to_string(), 10, 30);
        clip.offset = 7;
        track.audio.push_clip(clip);
        let state = make_state_with_track(track);

        let inverse = create_inverse_action(
            &Action::SetClipBounds {
                track_name: "t".to_string(),
                clip_index: 0,
                kind: Kind::Audio,
                start: 14,
                length: 22,
                offset: 11,
            },
            &state,
        )
        .expect("inverse action");

        match inverse {
            Action::SetClipBounds {
                track_name,
                clip_index,
                kind,
                start,
                length,
                offset,
            } => {
                assert_eq!(track_name, "t");
                assert_eq!(clip_index, 0);
                assert_eq!(kind, Kind::Audio);
                assert_eq!(start, 10);
                assert_eq!(length, 20);
                assert_eq!(offset, 7);
            }
            other => panic!("unexpected inverse action: {other:?}"),
        }
    }

    #[test]
    fn create_inverse_action_for_set_clip_bounds_restores_previous_midi_bounds() {
        let track = Track::new("t".to_string(), 0, 0, 1, 1, 64, 48_000.0);
        track.midi.push_clip(crate::midi::clip::MIDIClip {
            name: "pattern.mid".to_string(),
            start: 24,
            end: 120,
            offset: 9,
            ..Default::default()
        });
        let state = make_state_with_track(track);

        let inverse = create_inverse_action(
            &Action::SetClipBounds {
                track_name: "t".to_string(),
                clip_index: 0,
                kind: Kind::MIDI,
                start: 32,
                length: 48,
                offset: 4,
            },
            &state,
        )
        .expect("inverse action");

        match inverse {
            Action::SetClipBounds {
                track_name,
                clip_index,
                kind,
                start,
                length,
                offset,
            } => {
                assert_eq!(track_name, "t");
                assert_eq!(clip_index, 0);
                assert_eq!(kind, Kind::MIDI);
                assert_eq!(start, 24);
                assert_eq!(length, 96);
                assert_eq!(offset, 9);
            }
            other => panic!("unexpected inverse action: {other:?}"),
        }
    }

    #[test]
    fn create_inverse_action_for_set_clip_muted_restores_audio_and_midi_flags() {
        let track = Track::new("t".to_string(), 1, 1, 1, 1, 64, 48_000.0);
        let mut audio_clip = AudioClip::new("audio.wav".to_string(), 0, 16);
        audio_clip.muted = true;
        track.audio.push_clip(audio_clip);
        let midi_clip = crate::midi::clip::MIDIClip {
            name: "pattern.mid".to_string(),
            muted: false,
            ..Default::default()
        };
        track.midi.push_clip(midi_clip);
        let state = make_state_with_track(track);

        let audio_inverse = create_inverse_action(
            &Action::SetClipMuted {
                track_name: "t".to_string(),
                clip_index: 0,
                kind: Kind::Audio,
                muted: false,
            },
            &state,
        )
        .expect("audio inverse");
        let midi_inverse = create_inverse_action(
            &Action::SetClipMuted {
                track_name: "t".to_string(),
                clip_index: 0,
                kind: Kind::MIDI,
                muted: true,
            },
            &state,
        )
        .expect("midi inverse");

        assert!(matches!(
            audio_inverse,
            Action::SetClipMuted {
                muted: true,
                kind: Kind::Audio,
                ..
            }
        ));
        assert!(matches!(
            midi_inverse,
            Action::SetClipMuted {
                muted: false,
                kind: Kind::MIDI,
                ..
            }
        ));
    }

    #[test]
    fn create_inverse_action_for_rename_clip_restores_previous_name() {
        let track = Track::new("t".to_string(), 1, 1, 0, 0, 64, 48_000.0);
        track
            .audio
            .push_clip(AudioClip::new("before.wav".to_string(), 0, 16));
        let state = make_state_with_track(track);

        let inverse = create_inverse_action(
            &Action::RenameClip {
                track_name: "t".to_string(),
                kind: Kind::Audio,
                clip_index: 0,
                new_name: "after.wav".to_string(),
            },
            &state,
        )
        .expect("inverse action");

        assert!(matches!(
            inverse,
            Action::RenameClip { new_name, kind: Kind::Audio, .. } if new_name == "before.wav"
        ));
    }

    #[test]
    fn create_inverse_action_for_remove_audio_clip_restores_peaks_file() {
        let track = Track::new("t".to_string(), 1, 1, 0, 0, 64, 48_000.0);
        let mut clip = AudioClip::new("audio/clip.wav".to_string(), 48, 144);
        clip.offset = 12;
        clip.input_channel = 0;
        clip.muted = true;
        clip.peaks_file = Some("peaks/clip.json".to_string());
        track.audio.push_clip(clip);
        let state = make_state_with_track(track);

        let inverse = create_inverse_action(
            &Action::RemoveClip {
                track_name: "t".to_string(),
                kind: Kind::Audio,
                clip_indices: vec![0],
            },
            &state,
        )
        .expect("inverse action");

        match inverse {
            Action::AddClip {
                name,
                track_name,
                start,
                length,
                offset,
                input_channel,
                muted,
                peaks_file,
                kind,
                ..
            } => {
                assert_eq!(name, "audio/clip.wav");
                assert_eq!(track_name, "t");
                assert_eq!(start, 48);
                assert_eq!(length, 96);
                assert_eq!(offset, 12);
                assert_eq!(input_channel, 0);
                assert!(muted);
                assert_eq!(peaks_file.as_deref(), Some("peaks/clip.json"));
                assert_eq!(kind, Kind::Audio);
            }
            other => panic!("unexpected inverse action: {other:?}"),
        }
    }

    #[test]
    fn create_inverse_action_for_move_clip_to_unused_restores_clip() {
        let track = Track::new("t".to_string(), 1, 1, 0, 0, 64, 48_000.0);
        let mut clip = AudioClip::new("audio/clip.wav".to_string(), 48, 144);
        clip.offset = 12;
        track.audio.push_clip(clip);
        let state = make_state_with_track(track);

        let inverse = create_inverse_action(
            &Action::MoveClipToUnused {
                track_name: "t".to_string(),
                kind: Kind::Audio,
                clip_indices: vec![0],
            },
            &state,
        )
        .expect("inverse action");

        match inverse {
            Action::AddClip {
                name,
                track_name,
                start,
                length,
                offset,
                kind,
                ..
            } => {
                assert_eq!(name, "audio/clip.wav");
                assert_eq!(track_name, "t");
                assert_eq!(start, 48);
                assert_eq!(length, 96);
                assert_eq!(offset, 12);
                assert_eq!(kind, Kind::Audio);
            }
            other => panic!("unexpected inverse action: {other:?}"),
        }
    }

    #[test]
    fn should_record_covers_unused_clip_pool_actions() {
        assert!(should_record(&Action::MoveClipToUnused {
            track_name: "t".to_string(),
            kind: Kind::Audio,
            clip_indices: vec![0],
        }));
        assert!(!should_record(&Action::DeleteUnusedClips {
            clip_ids: vec!["clip-1".to_string()],
        }));
        assert!(!should_record(&Action::SetUnusedClips {
            audio: vec![],
            midi: vec![],
        }));
    }

    #[test]
    fn create_inverse_action_for_remove_grouped_audio_clip_restores_group() {
        let track = Track::new("t".to_string(), 1, 1, 0, 0, 64, 48_000.0);
        let mut group = AudioClip::new("Group".to_string(), 48, 144);
        group
            .grouped_clips
            .push(AudioClip::new("child.wav".to_string(), 0, 32));
        track.audio.push_clip(group);
        let state = make_state_with_track(track);

        let inverse = create_inverse_action(
            &Action::RemoveClip {
                track_name: "t".to_string(),
                kind: Kind::Audio,
                clip_indices: vec![0],
            },
            &state,
        )
        .expect("inverse action");

        match inverse {
            Action::AddGroupedClip {
                track_name,
                kind,
                audio_clip,
                midi_clip,
            } => {
                assert_eq!(track_name, "t");
                assert_eq!(kind, Kind::Audio);
                assert!(midi_clip.is_none());
                let audio_clip = audio_clip.expect("audio clip payload");
                assert_eq!(audio_clip.name, "Group");
                assert_eq!(audio_clip.grouped_clips.len(), 1);
                assert_eq!(audio_clip.grouped_clips[0].name, "child.wav");
            }
            other => panic!("unexpected inverse action: {other:?}"),
        }
    }

    #[test]
    fn create_inverse_action_for_remove_midi_clip_restores_clip() {
        let track = Track::new("t".to_string(), 0, 0, 1, 1, 64, 48_000.0);
        track.midi.push_clip(crate::midi::clip::MIDIClip {
            name: "pattern.mid".to_string(),
            start: 48,
            end: 144,
            offset: 12,
            input_channel: 3,
            muted: true,
            ..Default::default()
        });
        let state = make_state_with_track(track);

        let inverse = create_inverse_action(
            &Action::RemoveClip {
                track_name: "t".to_string(),
                kind: Kind::MIDI,
                clip_indices: vec![0],
            },
            &state,
        )
        .expect("inverse action");

        match inverse {
            Action::AddClip {
                name,
                track_name,
                start,
                length,
                offset,
                input_channel,
                muted,
                kind,
                ..
            } => {
                assert_eq!(name, "pattern.mid");
                assert_eq!(track_name, "t");
                assert_eq!(start, 48);
                assert_eq!(length, 96);
                assert_eq!(offset, 12);
                assert_eq!(input_channel, 3);
                assert!(muted);
                assert_eq!(kind, Kind::MIDI);
            }
            other => panic!("unexpected inverse action: {other:?}"),
        }
    }

    #[test]
    fn create_inverse_action_for_remove_grouped_midi_clip_restores_group() {
        let track = Track::new("t".to_string(), 0, 0, 1, 1, 64, 48_000.0);
        let mut group = crate::midi::clip::MIDIClip::new("Group".to_string(), 32, 160);
        group.grouped_clips.push(crate::midi::clip::MIDIClip::new(
            "child.mid".to_string(),
            0,
            48,
        ));
        track.midi.push_clip(group);
        let state = make_state_with_track(track);

        let inverse = create_inverse_action(
            &Action::RemoveClip {
                track_name: "t".to_string(),
                kind: Kind::MIDI,
                clip_indices: vec![0],
            },
            &state,
        )
        .expect("inverse action");

        match inverse {
            Action::AddGroupedClip {
                track_name,
                kind,
                audio_clip,
                midi_clip,
            } => {
                assert_eq!(track_name, "t");
                assert_eq!(kind, Kind::MIDI);
                assert!(audio_clip.is_none());
                let midi_clip = midi_clip.expect("midi clip payload");
                assert_eq!(midi_clip.name, "Group");
                assert_eq!(midi_clip.grouped_clips.len(), 1);
                assert_eq!(midi_clip.grouped_clips[0].name, "child.mid");
            }
            other => panic!("unexpected inverse action: {other:?}"),
        }
    }

    #[test]
    fn create_inverse_action_for_remove_grouped_audio_clip_preserves_child_metadata() {
        let track = Track::new("t".to_string(), 1, 1, 0, 0, 64, 48_000.0);
        let mut child = AudioClip::new("child.wav".to_string(), 4, 40);
        child.peaks_file = Some("peaks/child.json".to_string());
        child.pitch_correction_source_name = Some("source.wav".to_string());
        child.pitch_correction_source_offset = Some(8);
        child.pitch_correction_source_length = Some(24);
        child.pitch_correction_preview_name = Some("preview.wav".to_string());
        child.pitch_correction_points = vec![crate::message::PitchCorrectionPointData {
            start_sample: 1,
            length_samples: 2,
            detected_midi_pitch: 60.0,
            target_midi_pitch: 62.0,
            clarity: 0.75,
        }];
        child.pitch_correction_frame_likeness = Some(0.25);
        child.pitch_correction_inertia_ms = Some(100);
        child.pitch_correction_formant_compensation = Some(true);
        child.plugin_graph_json = Some(serde_json::json!({"plugins":[],"connections":[]}));
        let mut group = AudioClip::new("Group".to_string(), 48, 144);
        group.grouped_clips.push(child);
        track.audio.push_clip(group);
        let state = make_state_with_track(track);

        let inverse = create_inverse_action(
            &Action::RemoveClip {
                track_name: "t".to_string(),
                kind: Kind::Audio,
                clip_indices: vec![0],
            },
            &state,
        )
        .expect("inverse action");

        match inverse {
            Action::AddGroupedClip {
                audio_clip: Some(audio_clip),
                ..
            } => {
                let child = &audio_clip.grouped_clips[0];
                assert_eq!(child.peaks_file.as_deref(), Some("peaks/child.json"));
                assert_eq!(child.source_name.as_deref(), Some("source.wav"));
                assert_eq!(child.source_offset, Some(8));
                assert_eq!(child.source_length, Some(24));
                assert_eq!(child.preview_name.as_deref(), Some("preview.wav"));
                assert_eq!(child.pitch_correction_points.len(), 1);
                assert_eq!(child.pitch_correction_frame_likeness, Some(0.25));
                assert_eq!(child.pitch_correction_inertia_ms, Some(100));
                assert_eq!(child.pitch_correction_formant_compensation, Some(true));
                assert!(child.plugin_graph_json.is_some());
            }
            other => panic!("unexpected inverse action: {other:?}"),
        }
    }

    #[test]
    fn create_inverse_action_for_remove_grouped_midi_clip_preserves_child_structure() {
        let track = Track::new("t".to_string(), 0, 0, 1, 1, 64, 48_000.0);
        let child = crate::midi::clip::MIDIClip::new("child.mid".to_string(), 0, 48);
        let mut group = crate::midi::clip::MIDIClip::new("Group".to_string(), 32, 160);
        group.grouped_clips.push(child);
        track.midi.push_clip(group);
        let state = make_state_with_track(track);

        let inverse = create_inverse_action(
            &Action::RemoveClip {
                track_name: "t".to_string(),
                kind: Kind::MIDI,
                clip_indices: vec![0],
            },
            &state,
        )
        .expect("inverse action");

        match inverse {
            Action::AddGroupedClip {
                midi_clip: Some(midi_clip),
                ..
            } => {
                let child = &midi_clip.grouped_clips[0];
                assert_eq!(child.name, "child.mid");
                assert_eq!(child.start, 0);
                assert_eq!(child.length, 48);
            }
            other => panic!("unexpected inverse action: {other:?}"),
        }
    }

    #[test]
    fn create_inverse_action_for_set_clip_pitch_correction_restores_previous_values() {
        let track = Track::new("t".to_string(), 1, 1, 0, 0, 64, 48_000.0);
        let mut clip = AudioClip::new("audio.wav".to_string(), 0, 128);
        clip.pitch_correction_preview_name = Some("audio_preview.wav".to_string());
        clip.pitch_correction_source_name = Some("audio_source.wav".to_string());
        clip.pitch_correction_source_offset = Some(12);
        clip.pitch_correction_source_length = Some(96);
        clip.pitch_correction_points = vec![crate::message::PitchCorrectionPointData {
            start_sample: 4,
            length_samples: 32,
            detected_midi_pitch: 60.2,
            target_midi_pitch: 61.0,
            clarity: 0.8,
        }];
        clip.pitch_correction_frame_likeness = Some(0.4);
        clip.pitch_correction_inertia_ms = Some(250);
        clip.pitch_correction_formant_compensation = Some(false);
        track.audio.push_clip(clip);
        let state = make_state_with_track(track);

        let inverse = create_inverse_action(
            &Action::SetClipPitchCorrection {
                track_name: "t".to_string(),
                clip_index: 0,
                preview_name: None,
                source_name: None,
                source_offset: None,
                source_length: None,
                pitch_correction_points: vec![],
                pitch_correction_frame_likeness: None,
                pitch_correction_inertia_ms: None,
                pitch_correction_formant_compensation: None,
            },
            &state,
        )
        .expect("inverse action");

        match inverse {
            Action::SetClipPitchCorrection {
                track_name,
                clip_index,
                preview_name,
                source_name,
                source_offset,
                source_length,
                pitch_correction_points,
                pitch_correction_frame_likeness,
                pitch_correction_inertia_ms,
                pitch_correction_formant_compensation,
            } => {
                assert_eq!(track_name, "t");
                assert_eq!(clip_index, 0);
                assert_eq!(preview_name.as_deref(), Some("audio_preview.wav"));
                assert_eq!(source_name.as_deref(), Some("audio_source.wav"));
                assert_eq!(source_offset, Some(12));
                assert_eq!(source_length, Some(96));
                assert_eq!(pitch_correction_points.len(), 1);
                assert_eq!(pitch_correction_points[0].target_midi_pitch, 61.0);
                assert_eq!(pitch_correction_frame_likeness, Some(0.4));
                assert_eq!(pitch_correction_inertia_ms, Some(250));
                assert_eq!(pitch_correction_formant_compensation, Some(false));
            }
            other => panic!("unexpected inverse action: {other:?}"),
        }
    }

    #[test]
    fn create_inverse_action_for_clip_copy_targets_new_destination_clip() {
        let source = Track::new("src".to_string(), 1, 1, 0, 0, 64, 48_000.0);
        source
            .audio
            .push_clip(AudioClip::new("source.wav".to_string(), 12, 48));
        let dest = Track::new("dst".to_string(), 1, 1, 0, 0, 64, 48_000.0);
        dest.audio
            .push_clip(AudioClip::new("existing.wav".to_string(), 0, 24));

        let mut state = State::default();
        state.tracks.insert(source.name.clone(), Arc::new(source));
        state.tracks.insert(dest.name.clone(), Arc::new(dest));

        let inverse = create_inverse_action(
            &Action::ClipMove {
                kind: Kind::Audio,
                from: ClipMoveFrom {
                    track_name: "src".to_string(),
                    clip_index: 0,
                },
                to: ClipMoveTo {
                    track_name: "dst".to_string(),
                    sample_offset: 96,
                    input_channel: 0,
                },
                copy: true,
            },
            &state,
        )
        .expect("inverse action");

        match inverse {
            Action::RemoveClip {
                track_name,
                kind,
                clip_indices,
            } => {
                assert_eq!(track_name, "dst");
                assert_eq!(kind, Kind::Audio);
                assert_eq!(clip_indices, vec![1]);
            }
            other => panic!("unexpected inverse action: {other:?}"),
        }
    }

    #[test]
    fn create_inverse_action_for_same_track_clip_move_reverses_last_destination_clip() {
        let track = Track::new("t".to_string(), 1, 1, 0, 0, 64, 48_000.0);
        let mut original = AudioClip::new("clip.wav".to_string(), 20, 40);
        original.input_channel = 2;
        let moved = AudioClip::new("moved.wav".to_string(), 80, 32);
        track.audio.push_clip(original);
        track.audio.push_clip(moved);
        let state = make_state_with_track(track);

        let inverse = create_inverse_action(
            &Action::ClipMove {
                kind: Kind::Audio,
                from: ClipMoveFrom {
                    track_name: "t".to_string(),
                    clip_index: 0,
                },
                to: ClipMoveTo {
                    track_name: "t".to_string(),
                    sample_offset: 80,
                    input_channel: 1,
                },
                copy: false,
            },
            &state,
        )
        .expect("inverse action");

        match inverse {
            Action::ClipMove {
                kind,
                from,
                to,
                copy,
            } => {
                assert_eq!(kind, Kind::Audio);
                assert_eq!(from.track_name, "t");
                assert_eq!(from.clip_index, 1);
                assert_eq!(to.track_name, "t");
                assert_eq!(to.sample_offset, 20);
                assert_eq!(to.input_channel, 2);
                assert!(!copy);
            }
            other => panic!("unexpected inverse action: {other:?}"),
        }
    }

    #[test]
    fn create_inverse_action_for_track_midi_binding_restores_previous_binding() {
        let mut track = Track::new("t".to_string(), 1, 1, 0, 0, 64, 48_000.0);
        track.midi_learn.volume = Some(binding(7));
        let state = make_state_with_track(track);

        let inverse = create_inverse_action(
            &Action::TrackSetMidiLearnBinding {
                track_name: "t".to_string(),
                target: TrackMidiLearnTarget::Volume,
                binding: Some(binding(9)),
            },
            &state,
        )
        .unwrap();

        match inverse {
            Action::TrackSetMidiLearnBinding {
                track_name,
                target,
                binding,
            } => {
                assert_eq!(track_name, "t");
                assert_eq!(target, TrackMidiLearnTarget::Volume);
                assert_eq!(binding.unwrap().cc, 7);
            }
            other => panic!("unexpected inverse action: {other:?}"),
        }
    }

    #[test]
    fn create_inverse_action_for_vst3_load_uses_next_runtime_instance_id() {
        let track = Track::new("t".to_string(), 1, 1, 0, 0, 64, 48_000.0);
        track.next_vst3_instance_id.store(42, Ordering::Relaxed);
        let state = make_state_with_track(track);

        let inverse = create_inverse_action(
            &Action::TrackLoadVst3Plugin {
                track_name: "t".to_string(),
                plugin_id: "/tmp/test.vst3".to_string(),
                instance_id: None,
            },
            &state,
        )
        .unwrap();

        match inverse {
            Action::TrackUnloadVst3PluginInstance {
                track_name,
                instance_id,
            } => {
                assert_eq!(track_name, "t");
                assert_eq!(instance_id, 42);
            }
            other => panic!("unexpected inverse action: {other:?}"),
        }
    }

    #[test]
    #[cfg(unix)]
    fn create_inverse_action_for_lv2_load_uses_next_runtime_instance_id() {
        let track = Track::new("t".to_string(), 1, 1, 0, 0, 64, 48_000.0);
        track.next_lv2_instance_id.store(5, Ordering::Relaxed);
        let state = make_state_with_track(track);

        let inverse = create_inverse_action(
            &Action::TrackLoadLv2Plugin {
                track_name: "t".to_string(),
                plugin_uri: "urn:test".to_string(),
                instance_id: None,
            },
            &state,
        )
        .unwrap();

        match inverse {
            Action::TrackUnloadLv2PluginInstance {
                track_name,
                instance_id,
            } => {
                assert_eq!(track_name, "t");
                assert_eq!(instance_id, 5);
            }
            other => panic!("unexpected inverse action: {other:?}"),
        }
    }

    #[test]
    fn create_inverse_actions_for_clear_all_midi_learn_bindings_restores_only_existing_bindings() {
        let mut track = Track::new("t".to_string(), 1, 1, 0, 0, 64, 48_000.0);
        track.midi_learn.volume = Some(binding(7));
        track.midi_learn.disk_monitor = Some(binding(64));
        let state = make_state_with_track(track);

        let inverses = create_inverse_actions(&Action::ClearAllMidiLearnBindings, &state).unwrap();

        assert_eq!(inverses.len(), 2);
        assert!(inverses.iter().any(|action| {
            matches!(
                action,
                Action::TrackSetMidiLearnBinding {
                    target: TrackMidiLearnTarget::Volume,
                    binding: Some(MidiLearnBinding { cc: 7, .. }),
                    ..
                }
            )
        }));
        assert!(inverses.iter().any(|action| {
            matches!(
                action,
                Action::TrackSetMidiLearnBinding {
                    target: TrackMidiLearnTarget::DiskMonitor,
                    binding: Some(MidiLearnBinding { cc: 64, .. }),
                    ..
                }
            )
        }));
    }

    #[test]
    fn create_inverse_actions_for_remove_track_restores_io_flags_and_bindings() {
        let mut track = Track::new("t".to_string(), 1, 1, 1, 1, 64, 48_000.0);
        track.set_level(-3.0);
        track.set_balance(0.25);
        track.armed.store(true, Ordering::Relaxed);
        track.set_muted(true);
        track.soloed.store(true, Ordering::Relaxed);
        track.set_input_monitor(vec![true]);
        track.set_disk_monitor(vec![false]);
        track.midi_learn.volume = Some(binding(10));
        track.audio.ins.push(Arc::new(AudioIO::new(64)));
        track.audio.outs.push(Arc::new(AudioIO::new(64)));
        let state = make_state_with_track(track);

        let inverses =
            create_inverse_actions(&Action::RemoveTrack("t".to_string()), &state).unwrap();

        assert!(matches!(
            inverses.first(),
            Some(Action::AddTrack {
                name,
                audio_ins: 1,
                audio_outs: 1,
                midi_ins: 1,
                midi_outs: 1,
                folder: false,
                mixosc_addr: None,
            }) if name == "t"
        ));
        assert!(
            inverses
                .iter()
                .any(|action| matches!(action, Action::TrackAddAudioInput(name) if name == "t"))
        );
        assert!(
            inverses
                .iter()
                .any(|action| matches!(action, Action::TrackAddAudioOutput(name) if name == "t"))
        );
        assert!(
            inverses.iter().any(
                |action| matches!(action, Action::TrackToggleInputMonitor { track_name, .. } if track_name == "t")
            )
        );
        assert!(
            inverses.iter().any(
                |action| matches!(action, Action::TrackToggleDiskMonitor { track_name, .. } if track_name == "t")
            )
        );
        assert!(inverses.iter().any(|action| {
            matches!(
                action,
                Action::TrackSetMidiLearnBinding {
                    target: TrackMidiLearnTarget::Volume,
                    binding: Some(MidiLearnBinding { cc: 10, .. }),
                    ..
                }
            )
        }));
    }

    #[test]
    fn create_inverse_actions_for_remove_track_omits_internal_passthrough() {
        let mut track = Track::new("t".to_string(), 1, 1, 0, 0, 64, 48_000.0);
        track.ensure_default_audio_passthrough();
        track.ensure_default_midi_passthrough();
        let state = make_state_with_track(track);

        let inverses =
            create_inverse_actions(&Action::RemoveTrack("t".to_string()), &state).unwrap();

        assert!(
            !inverses.iter().any(|action| matches!(
                action,
                Action::Connect {
                    from_track,
                    to_track,
                    ..
                } if from_track == to_track
            )),
            "internal passthrough should not be captured as a track-to-track Connect action"
        );
    }

    #[test]
    fn create_inverse_actions_for_remove_folder_track_restores_parent_and_omits_child_wiring() {
        let mut state = State::default();
        let folder = Track::new_folder("folder".to_string(), 1, 1, 0, 0, 64, 48_000.0);
        let child = Track::new("child".to_string(), 1, 1, 0, 0, 64, 48_000.0);
        state.tracks.insert("folder".to_string(), Arc::new(folder));
        state.tracks.insert("child".to_string(), Arc::new(child));
        {
            let folder_arc = state.tracks.get("folder").unwrap().clone();
            let child_arc = state.tracks.get("child").unwrap().clone();
            child_arc.lock().parent_track = Some("folder".to_string());
            folder_arc.lock().child_tracks.push(child_arc.clone());

            // Simulate the implicit wiring TrackSetParent creates.
            let (folder_in, folder_out) = {
                let folder = folder_arc.lock();
                (folder.audio.ins[0].clone(), folder.audio.outs[0].clone())
            };
            let (child_in, child_out) = {
                let child = child_arc.lock();
                (child.audio.ins[0].clone(), child.audio.outs[0].clone())
            };
            AudioIO::connect(&folder_in, &child_in);
            AudioIO::connect(&child_out, &folder_out);
        }

        let mut inverses =
            create_inverse_actions(&Action::RemoveTrack("folder".to_string()), &state).unwrap();
        inverses.extend(
            create_inverse_actions(&Action::RemoveTrack("child".to_string()), &state).unwrap(),
        );

        assert!(
            inverses.iter().any(|action| matches!(
                action,
                Action::TrackSetParent {
                    track_name,
                    parent_name: Some(parent_name),
                } if track_name == "child" && parent_name == "folder"
            )),
            "inverse should restore the child-to-folder parent relationship"
        );
        assert!(
            !inverses.iter().any(|action| matches!(
                action,
                Action::Connect { from_track, to_track, .. }
                if (from_track == "folder" && to_track == "child")
                    || (from_track == "child" && to_track == "folder")
            )),
            "implicit folder-to-child wiring should not be captured as a generic Connect action"
        );
    }

    #[test]
    fn create_inverse_action_for_track_set_session_slot_restores_previous_clip() {
        let mut track = Track::new("t".to_string(), 1, 1, 0, 0, 64, 48_000.0);
        track
            .rt
            .session_slots
            .insert(0, crate::track::SessionSlot::new("old-clip".to_string()));
        let state = make_state_with_track(track);

        let inverse = create_inverse_action(
            &Action::TrackSetSessionSlot {
                track_name: "t".to_string(),
                scene_index: 0,
                clip_id: Some("new-clip".to_string()),
            },
            &state,
        )
        .unwrap();

        match inverse {
            Action::TrackSetSessionSlot {
                track_name,
                scene_index,
                clip_id,
            } => {
                assert_eq!(track_name, "t");
                assert_eq!(scene_index, 0);
                assert_eq!(clip_id, Some("old-clip".to_string()));
            }
            other => panic!("unexpected inverse action: {other:?}"),
        }
    }

    #[test]
    fn create_inverse_action_for_track_set_session_slot_clear_restores_none() {
        let mut track = Track::new("t".to_string(), 1, 1, 0, 0, 64, 48_000.0);
        track
            .rt
            .session_slots
            .insert(0, crate::track::SessionSlot::new("old-clip".to_string()));
        let state = make_state_with_track(track);

        let inverse = create_inverse_action(
            &Action::TrackSetSessionSlot {
                track_name: "t".to_string(),
                scene_index: 0,
                clip_id: None,
            },
            &state,
        )
        .unwrap();

        match inverse {
            Action::TrackSetSessionSlot {
                track_name,
                scene_index,
                clip_id,
            } => {
                assert_eq!(track_name, "t");
                assert_eq!(scene_index, 0);
                assert_eq!(clip_id, Some("old-clip".to_string()));
            }
            other => panic!("unexpected inverse action: {other:?}"),
        }
    }

    #[test]
    fn create_inverse_action_for_track_set_session_slot_play_enabled_restores_previous_flag() {
        let mut track = Track::new("t".to_string(), 1, 1, 0, 0, 64, 48_000.0);
        track
            .rt
            .session_slots
            .insert(0, crate::track::SessionSlot::new("clip".to_string()));
        track.rt.session_slots.get_mut(&0).unwrap().play_enabled = false;
        let state = make_state_with_track(track);

        let inverse = create_inverse_action(
            &Action::TrackSetSessionSlotPlayEnabled {
                track_name: "t".to_string(),
                scene_index: 0,
                enabled: true,
            },
            &state,
        )
        .unwrap();

        match inverse {
            Action::TrackSetSessionSlotPlayEnabled {
                track_name,
                scene_index,
                enabled,
            } => {
                assert_eq!(track_name, "t");
                assert_eq!(scene_index, 0);
                assert!(!enabled);
            }
            other => panic!("unexpected inverse action: {other:?}"),
        }
    }
}

#[cfg(test)]
mod undo_consistency {
    //! Phase 4 consistency test: every `Action` variant must be classifiable
    //! (an undoable command XOR intentionally ignored for undo), and every
    //! command the history pipeline records must produce a non-`None`
    //! inverse. Adding a new `Action` variant without updating the matches
    //! below panics here instead of silently breaking undo.
    use crate::audio::clip::AudioClip;
    use crate::engine::Engine;
    use crate::history::{create_inverse_actions, should_record};
    use crate::kind::Kind;
    use crate::message::{
        Action, ClipMoveFrom, ClipMoveTo, GlobalMidiLearnTarget, MidiControllerData,
        MidiLearnBinding, MidiNoteData, MidiRawEventData, OfflineAutomationTarget,
        SessionMidiLearnTarget, TrackMidiLearnTarget,
    };
    use crate::midi::clip::MIDIClip;
    use crate::state::State;
    use crate::track::Track;

    fn binding(cc: u8) -> MidiLearnBinding {
        MidiLearnBinding {
            device: Some("midi".to_string()),
            channel: 1,
            cc,
        }
    }

    fn fixture_state() -> State {
        let track = Track::new("t1".to_string(), 1, 1, 1, 1, 64, 48_000.0);
        track
            .audio
            .push_clip(AudioClip::new("a.wav".to_string(), 0, 16));
        track
            .midi
            .push_clip(MIDIClip::new("m.mid".to_string(), 0, 16));
        // An automation lane with a point at sample 8 so point-delete
        // inverses have an old value to restore.
        let mut track = track;
        track.automation_lanes =
            serde_json::to_value(vec![crate::message::OfflineAutomationLane {
                target: crate::message::OfflineAutomationTarget::Volume,
                visible: true,
                points: vec![crate::message::OfflineAutomationPoint {
                    sample: 8,
                    value: 0.5,
                }],
            }])
            .unwrap();
        let mut state = State::default();
        state
            .tracks
            .insert("t1".to_string(), std::sync::Arc::new(track));
        state
    }

    /// One instance of every undoable command (recordable by the history
    /// pipeline). Patterns only classify; these instances feed the inverse
    /// assertions, so payloads reference the fixture track "t1" / clip 0.
    fn undoable_command_instances() -> Vec<Action> {
        let mut v = vec![
            Action::SetTempo(128.0),
            Action::SetLoopEnabled(true),
            Action::SetLoopRange(Some((0, 48000))),
            Action::SetPunchEnabled(true),
            Action::SetPunchRange(Some((0, 24000))),
            Action::SetMetronomeEnabled(true),
            Action::SetTimeSignature {
                numerator: 3,
                denominator: 4,
            },
            Action::SetTempoMap {
                tempo_points: vec![],
                time_signature_points: vec![],
            },
            Action::SetClipPlaybackEnabled(false),
            Action::SetRecordEnabled(true),
            Action::SetModulators(vec![]),
            Action::SetTrackAutomationLanes {
                track_name: "t1".to_string(),
                lanes: serde_json::Value::Array(vec![]),
                mode: crate::message::TrackAutomationMode::Read,
            },
            Action::TrackAutomationToggleLane {
                track_name: "t1".to_string(),
                target: OfflineAutomationTarget::Volume,
            },
            Action::TrackAutomationInsertPoint {
                track_name: "t1".to_string(),
                target: OfflineAutomationTarget::Volume,
                sample: 8,
                value: 0.5,
            },
            Action::TrackAutomationDeletePoint {
                track_name: "t1".to_string(),
                target: OfflineAutomationTarget::Volume,
                sample: 8,
            },
            Action::TrackAutomationSetMode {
                track_name: "t1".to_string(),
                mode: crate::message::TrackAutomationMode::Write,
            },
            Action::AddTrack {
                name: "t1".to_string(),
                audio_ins: 1,
                midi_ins: 1,
                audio_outs: 1,
                midi_outs: 1,
                folder: false,
                mixosc_addr: None,
            },
            Action::RemoveTrack("t1".to_string()),
            Action::RenameTrack {
                old_name: "t1".to_string(),
                new_name: "t1".to_string(),
            },
            Action::TrackLevel("t1".to_string(), -3.0),
            Action::TrackBalance("t1".to_string(), 0.25),
            Action::TrackToggleArm("t1".to_string()),
            Action::TrackToggleMute("t1".to_string()),
            Action::TrackTogglePhase("t1".to_string()),
            Action::TrackToggleSolo("t1".to_string()),
            Action::TrackToggleMaster("t1".to_string()),
            Action::TrackToggleInputMonitor {
                track_name: "t1".to_string(),
                lane: 0,
            },
            Action::TrackToggleDiskMonitor {
                track_name: "t1".to_string(),
                lane: 0,
            },
            Action::TrackToggleMidiInputMonitor {
                track_name: "t1".to_string(),
                lane: 0,
            },
            Action::TrackToggleMidiDiskMonitor {
                track_name: "t1".to_string(),
                lane: 0,
            },
            Action::TrackSetColor {
                track_name: "t1".to_string(),
                color: None,
            },
            Action::TrackSetMidiLearnBinding {
                track_name: "t1".to_string(),
                target: TrackMidiLearnTarget::Volume,
                binding: Some(binding(20)),
            },
            Action::SetGlobalMidiLearnBinding {
                target: GlobalMidiLearnTarget::PlayPause,
                binding: Some(binding(21)),
            },
            Action::SetSessionMidiLearnBinding {
                target: SessionMidiLearnTarget::Scene(0),
                binding: Some(binding(22)),
            },
            Action::ClearAllMidiLearnBindings,
            Action::TrackSetFrozen {
                track_name: "t1".to_string(),
                frozen: true,
            },
            Action::TrackSetSessionSlot {
                track_name: "t1".to_string(),
                scene_index: 0,
                clip_id: Some("clip".to_string()),
            },
            Action::TrackSetSessionSlotPlayEnabled {
                track_name: "t1".to_string(),
                scene_index: 0,
                enabled: true,
            },
            Action::TrackSetSessionSlotStopEnabled {
                track_name: "t1".to_string(),
                scene_index: 0,
                enabled: true,
            },
            Action::TrackSetFolder {
                track_name: "t1".to_string(),
                is_folder: false,
            },
            Action::TrackSetParent {
                track_name: "t1".to_string(),
                parent_name: None,
            },
            Action::TrackToggleFolder {
                track_name: "t1".to_string(),
            },
            Action::TrackAddAudioInput("t1".to_string()),
            Action::TrackAddAudioOutput("t1".to_string()),
            Action::TrackRemoveAudioInput("t1".to_string()),
            Action::TrackRemoveAudioOutput("t1".to_string()),
            Action::AddClip {
                clip_id: "c".to_string(),
                name: "c".to_string(),
                track_name: "t1".to_string(),
                start: 0,
                length: 16,
                offset: 0,
                input_channel: 0,
                muted: false,
                reversed: false,
                gain_db: 0.0,
                peaks_file: None,
                kind: Kind::Audio,
                fade_enabled: false,
                fade_in_samples: 0,
                fade_out_samples: 0,
                source_name: None,
                source_offset: None,
                source_length: None,
                preview_name: None,
                pitch_correction_points: vec![],
                pitch_correction_frame_likeness: None,
                pitch_correction_inertia_ms: None,
                pitch_correction_formant_compensation: None,
                pitch_correction_detector: Default::default(),
                pitch_correction_mode: Default::default(),
                plugin_graph_json: None,
            },
            Action::AddGroupedClip {
                track_name: "t1".to_string(),
                kind: Kind::Audio,
                audio_clip: None,
                midi_clip: None,
            },
            Action::RemoveClip {
                track_name: "t1".to_string(),
                kind: Kind::Audio,
                clip_indices: vec![0],
            },
            Action::MoveClipToUnused {
                track_name: "t1".to_string(),
                kind: Kind::Audio,
                clip_indices: vec![0],
            },
            Action::RenameClip {
                track_name: "t1".to_string(),
                kind: Kind::Audio,
                clip_index: 0,
                new_name: "n".to_string(),
            },
            Action::SetClipIdentity {
                track_name: "t1".to_string(),
                kind: Kind::Audio,
                clip_index: 0,
                new_id: "id".to_string(),
                new_name: "n".to_string(),
            },
            Action::ClipMove {
                from: ClipMoveFrom {
                    track_name: "t1".to_string(),
                    clip_index: 0,
                },
                to: ClipMoveTo {
                    track_name: "t1".to_string(),
                    sample_offset: 32,
                    input_channel: 0,
                },
                kind: Kind::Audio,
                copy: false,
            },
            Action::SetClipFade {
                track_name: "t1".to_string(),
                clip_index: 0,
                kind: Kind::Audio,
                fade_enabled: true,
                fade_in_samples: 4,
                fade_out_samples: 4,
            },
            Action::SetClipBounds {
                track_name: "t1".to_string(),
                clip_index: 0,
                kind: Kind::Audio,
                start: 4,
                length: 8,
                offset: 2,
            },
            Action::SetClipMuted {
                track_name: "t1".to_string(),
                kind: Kind::Audio,
                clip_index: 0,
                muted: true,
            },
            Action::SetClipReversed {
                track_name: "t1".to_string(),
                kind: Kind::Audio,
                clip_index: 0,
                reversed: true,
            },
            Action::SetClipSourceName {
                track_name: "t1".to_string(),
                kind: Kind::Audio,
                clip_index: 0,
                name: "s".to_string(),
            },
            Action::SetClipPluginGraphJson {
                track_name: "t1".to_string(),
                clip_index: 0,
                plugin_graph_json: None,
            },
            Action::SetClipPitchCorrection {
                track_name: "t1".to_string(),
                clip_index: 0,
                preview_name: None,
                source_name: None,
                source_offset: None,
                source_length: None,
                pitch_correction_points: vec![],
                pitch_correction_frame_likeness: None,
                pitch_correction_inertia_ms: None,
                pitch_correction_formant_compensation: None,
            },
            Action::Connect {
                from_track: "t1".to_string(),
                from_port: 0,
                to_track: "t1".to_string(),
                to_port: 0,
                kind: Kind::Audio,
            },
            Action::Disconnect {
                from_track: "t1".to_string(),
                from_port: 0,
                to_track: "t1".to_string(),
                to_port: 0,
                kind: Kind::Audio,
            },
            Action::TrackConnectVst3Audio {
                track_name: "t1".to_string(),
                from_node: crate::message::Vst3GraphNode::TrackInput,
                from_port: 0,
                to_node: crate::message::Vst3GraphNode::TrackOutput,
                to_port: 0,
            },
            Action::TrackDisconnectVst3Audio {
                track_name: "t1".to_string(),
                from_node: crate::message::Vst3GraphNode::TrackInput,
                from_port: 0,
                to_node: crate::message::Vst3GraphNode::TrackOutput,
                to_port: 0,
            },
            Action::TrackConnectPluginAudio {
                track_name: "t1".to_string(),
                from_node: crate::message::PluginGraphNode::TrackInput,
                from_port: 0,
                to_node: crate::message::PluginGraphNode::TrackOutput,
                to_port: 0,
            },
            Action::TrackDisconnectPluginAudio {
                track_name: "t1".to_string(),
                from_node: crate::message::PluginGraphNode::TrackInput,
                from_port: 0,
                to_node: crate::message::PluginGraphNode::TrackOutput,
                to_port: 0,
            },
            Action::TrackConnectPluginMidi {
                track_name: "t1".to_string(),
                from_node: crate::message::PluginGraphNode::TrackInput,
                from_port: 0,
                to_node: crate::message::PluginGraphNode::TrackOutput,
                to_port: 0,
            },
            Action::TrackDisconnectPluginMidi {
                track_name: "t1".to_string(),
                from_node: crate::message::PluginGraphNode::TrackInput,
                from_port: 0,
                to_node: crate::message::PluginGraphNode::TrackOutput,
                to_port: 0,
            },
            Action::TrackConnectAudio {
                track_name: "t1".to_string(),
                from: crate::message::ConnectableRef::TrackInput,
                from_port: 0,
                to: crate::message::ConnectableRef::TrackOutput,
                to_port: 0,
            },
            Action::TrackDisconnectAudio {
                track_name: "t1".to_string(),
                from: crate::message::ConnectableRef::TrackInput,
                from_port: 0,
                to: crate::message::ConnectableRef::TrackOutput,
                to_port: 0,
            },
            Action::TrackConnectMidi {
                track_name: "t1".to_string(),
                from: crate::message::ConnectableRef::TrackInput,
                from_port: 0,
                to: crate::message::ConnectableRef::TrackOutput,
                to_port: 0,
            },
            Action::TrackDisconnectMidi {
                track_name: "t1".to_string(),
                from: crate::message::ConnectableRef::TrackInput,
                from_port: 0,
                to: crate::message::ConnectableRef::TrackOutput,
                to_port: 0,
            },
            Action::TrackLoadClapPlugin {
                track_name: "t1".to_string(),
                plugin_id: "p".to_string(),
                instance_id: None,
            },
            Action::TrackUnloadClapPlugin {
                track_name: "t1".to_string(),
                plugin_id: "p".to_string(),
            },
            Action::TrackUnloadClapPluginInstance {
                track_name: "t1".to_string(),
                instance_id: 0,
            },
            Action::TrackLoadVst3Plugin {
                track_name: "t1".to_string(),
                plugin_id: "p".to_string(),
                instance_id: None,
            },
            Action::TrackUnloadVst3PluginInstance {
                track_name: "t1".to_string(),
                instance_id: 0,
            },
            Action::TrackSetClapParameter {
                track_name: "t1".to_string(),
                instance_id: 0,
                param_id: 0,
                value: 0.5,
            },
            Action::ClipSetClapParameter {
                track_name: "t1".to_string(),
                clip_idx: 0,
                instance_id: 0,
                param_id: 0,
                value: 0.5,
            },
            Action::TrackSetVst3Parameter {
                track_name: "t1".to_string(),
                instance_id: 0,
                param_id: 0,
                value: 0.5,
            },
            Action::TrackSetPluginBypassed {
                track_name: "t1".to_string(),
                instance_id: 0,
                format: "clap".to_string(),
                bypassed: true,
            },
            Action::ModifyMidiNotes {
                track_name: "t1".to_string(),
                clip_index: 0,
                note_indices: vec![0],
                new_notes: vec![note()],
                old_notes: vec![note()],
            },
            Action::ModifyMidiControllers {
                track_name: "t1".to_string(),
                clip_index: 0,
                controller_indices: vec![0],
                new_controllers: vec![controller()],
                old_controllers: vec![controller()],
            },
            Action::DeleteMidiControllers {
                track_name: "t1".to_string(),
                clip_index: 0,
                controller_indices: vec![0],
                deleted_controllers: vec![(0, controller())],
            },
            Action::InsertMidiControllers {
                track_name: "t1".to_string(),
                clip_index: 0,
                controllers: vec![(0, controller())],
            },
            Action::DeleteMidiNotes {
                track_name: "t1".to_string(),
                clip_index: 0,
                note_indices: vec![0],
                deleted_notes: vec![(0, note())],
            },
            Action::InsertMidiNotes {
                track_name: "t1".to_string(),
                clip_index: 0,
                notes: vec![(0, note())],
            },
            Action::SetMidiSysExEvents {
                track_name: "t1".to_string(),
                clip_index: 0,
                new_sysex_events: vec![sysex()],
                old_sysex_events: vec![sysex()],
            },
        ];
        #[cfg(unix)]
        v.extend([
            Action::TrackLoadLv2Plugin {
                track_name: "t1".to_string(),
                plugin_uri: "u".to_string(),
                instance_id: None,
            },
            Action::TrackUnloadLv2PluginInstance {
                track_name: "t1".to_string(),
                instance_id: 0,
            },
            Action::TrackSetLv2ControlValue {
                track_name: "t1".to_string(),
                instance_id: 0,
                index: 0,
                value: 0.5,
            },
        ]);
        v
    }

    fn note() -> MidiNoteData {
        MidiNoteData {
            start_sample: 0,
            length_samples: 4,
            pitch: 60,
            velocity: 100,
            channel: 0,
            mpe: Default::default(),
        }
    }

    fn controller() -> MidiControllerData {
        MidiControllerData {
            sample: 0,
            controller: 1,
            value: 64,
            channel: 0,
        }
    }

    fn sysex() -> MidiRawEventData {
        MidiRawEventData {
            sample: 0,
            data: vec![0xF0, 0xF7],
        }
    }

    /// Recordable commands whose inverse legitimately requires an
    /// out-of-process plugin instance (or loaded plugin state) that an
    /// in-process fixture cannot hold. For these the pipeline correctly
    /// declines to record when the target instance is absent, so only the
    /// record/ignore classification is asserted.
    fn fixture_limited(action: &Action) -> bool {
        matches!(
            action,
            Action::TrackUnloadClapPlugin { .. }
                | Action::TrackUnloadClapPluginInstance { .. }
                | Action::TrackUnloadVst3PluginInstance { .. }
                | Action::TrackSetClapParameter { .. }
                | Action::ClipSetClapParameter { .. }
                | Action::TrackSetVst3Parameter { .. }
                | Action::TrackSetPluginBypassed { .. }
                | Action::TrackLoadLv2Plugin { .. }
                | Action::TrackUnloadLv2PluginInstance { .. }
                | Action::TrackSetLv2ControlValue { .. }
        )
    }

    #[test]
    fn undo_consistency_commands_are_recordable_with_inverses() {
        let state = fixture_state();
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let engine = Engine::new(rx, tx);
        for action in undoable_command_instances() {
            assert!(
                is_undoable_command(&action),
                "{action:?} must be classified as an undoable command"
            );
            if fixture_limited(&action) {
                // Correctly declines to record when the out-of-process
                // plugin instance the inverse would restore is absent.
                continue;
            }
            let engine_state_inverse = engine
                .undo_engine_state_inverse_transport(&action)
                .or_else(|| engine.undo_engine_state_inverse_recording(&action))
                .or_else(|| engine.undo_engine_state_inverse_midi(&action))
                .or_else(|| engine.undo_engine_state_inverse_automation(&action));
            let recorded = (should_record(&action)
                && create_inverse_actions(&action, &state).is_some())
                || engine_state_inverse.is_some();
            assert!(
                recorded,
                "{action:?} is an undoable command but the history pipeline produced no inverse"
            );
            let inverse = create_inverse_actions(&action, &state).or(engine_state_inverse);
            assert!(
                inverse.is_some(),
                "{action:?} should_record = {} but its inverse is None",
                should_record(&action),
            );
        }
    }

    /// Classification registry: every `Action` variant must appear exactly
    /// once, as either an undoable command (`true`) or an action that is
    /// intentionally never recorded (`false`). The catch-all panics when a
    /// variant is added without a decision, so future additions fail tests
    /// instead of silently breaking undo.
    fn is_undoable_command(action: &Action) -> bool {
        match action {
            Action::SetTempo(_)
            | Action::SetLoopEnabled(_)
            | Action::SetLoopRange(_)
            | Action::SetPunchEnabled(_)
            | Action::SetPunchRange(_)
            | Action::SetMetronomeEnabled(_)
            | Action::SetTimeSignature { .. }
            | Action::SetTempoMap { .. }
            | Action::SetModulators(_)
            | Action::SetTrackAutomationLanes { .. }
            | Action::TrackAutomationToggleLane { .. }
            | Action::TrackAutomationInsertPoint { .. }
            | Action::TrackAutomationDeletePoint { .. }
            | Action::TrackAutomationSetMode { .. }
            | Action::AddTrack { .. }
            | Action::RemoveTrack(_)
            | Action::RenameTrack { .. }
            | Action::TrackLevel(_, _)
            | Action::TrackBalance(_, _)
            | Action::TrackToggleArm(_)
            | Action::TrackToggleMute(_)
            | Action::TrackTogglePhase(_)
            | Action::TrackToggleSolo(_)
            | Action::TrackToggleInputMonitor { .. }
            | Action::TrackToggleDiskMonitor { .. }
            | Action::TrackToggleMidiInputMonitor { .. }
            | Action::TrackToggleMidiDiskMonitor { .. }
            | Action::TrackSetColor { .. }
            | Action::TrackSetMidiLearnBinding { .. }
            | Action::SetGlobalMidiLearnBinding { .. }
            | Action::SetSessionMidiLearnBinding { .. }
            | Action::TrackSetFrozen { .. }
            | Action::TrackSetSessionSlot { .. }
            | Action::TrackSetSessionSlotPlayEnabled { .. }
            | Action::TrackSetSessionSlotStopEnabled { .. }
            | Action::TrackSetFolder { .. }
            | Action::TrackSetParent { .. }
            | Action::TrackToggleFolder { .. }
            | Action::TrackToggleMaster(_)
            | Action::TrackAddAudioInput(_)
            | Action::TrackAddAudioOutput(_)
            | Action::TrackRemoveAudioInput(_)
            | Action::TrackRemoveAudioOutput(_)
            | Action::AddClip { .. }
            | Action::AddGroupedClip { .. }
            | Action::RemoveClip { .. }
            | Action::MoveClipToUnused { .. }
            | Action::RenameClip { .. }
            | Action::SetClipIdentity { .. }
            | Action::ClipMove { .. }
            | Action::SetClipFade { .. }
            | Action::SetClipBounds { .. }
            | Action::SetClipMuted { .. }
            | Action::SetClipReversed { .. }
            | Action::SetClipSourceName { .. }
            | Action::SetClipPluginGraphJson { .. }
            | Action::SetClipPitchCorrection { .. }
            | Action::ClearAllMidiLearnBindings
            | Action::Connect { .. }
            | Action::Disconnect { .. }
            | Action::TrackConnectVst3Audio { .. }
            | Action::TrackDisconnectVst3Audio { .. }
            | Action::TrackLoadClapPlugin { .. }
            | Action::TrackUnloadClapPlugin { .. }
            | Action::TrackUnloadClapPluginInstance { .. }
            | Action::TrackLoadVst3Plugin { .. }
            | Action::TrackUnloadVst3PluginInstance { .. }
            | Action::TrackSetClapParameter { .. }
            | Action::ClipSetClapParameter { .. }
            | Action::TrackSetVst3Parameter { .. }
            | Action::TrackSetPluginBypassed { .. }
            | Action::TrackUnloadVst3Plugin { .. }
            | Action::ClipSetVst3Parameter { .. }
            | Action::ModifyMidiNotes { .. }
            | Action::ModifyMidiControllers { .. }
            | Action::DeleteMidiControllers { .. }
            | Action::InsertMidiControllers { .. }
            | Action::DeleteMidiNotes { .. }
            | Action::InsertMidiNotes { .. }
            | Action::SetMidiSysExEvents { .. }
            | Action::TrackConnectPluginAudio { .. }
            | Action::TrackDisconnectPluginAudio { .. }
            | Action::TrackConnectPluginMidi { .. }
            | Action::TrackDisconnectPluginMidi { .. }
            | Action::TrackConnectAudio { .. }
            | Action::TrackDisconnectAudio { .. }
            | Action::TrackConnectMidi { .. }
            | Action::TrackDisconnectMidi { .. }
            | Action::SetClipPlaybackEnabled(_)
            | Action::SetRecordEnabled(_) => true,
            #[cfg(unix)]
            Action::TrackLoadLv2Plugin { .. }
            | Action::TrackUnloadLv2PluginInstance { .. }
            | Action::TrackSetLv2ControlValue { .. } => true,

            // Intentionally never recorded: transport commands, queries and
            // their echo responses, realtime echoes, history control, session
            // runtime actions, device/JACK control, and GUI/plugin responses.
            Action::Quit
            | Action::Play
            | Action::Pause
            | Action::Stop
            | Action::SessionPlay
            | Action::TransportPosition(_)
            | Action::JumpToEnd
            | Action::SetOscEnabled(_)
            | Action::SetSessionClipPlaybackEnabled(_)
            | Action::SetStepRecording(_)
            | Action::Panic
            | Action::Session(_)
            | Action::SetSessionPath(_)
            | Action::BeginHistoryGroup
            | Action::EndHistoryGroup
            | Action::ApplyGroupedActions(_)
            | Action::ClearHistory
            | Action::BeginSessionRestore
            | Action::EndSessionRestore
            | Action::MarkHistorySavePoint
            | Action::Undo
            | Action::Redo
            | Action::TrackAutomationLevel(_, _)
            | Action::TrackAutomationBalance(_, _)
            | Action::TrackMidiCc { .. }
            | Action::TrackMeters { .. }
            | Action::RequestMeterSnapshot
            | Action::RequestTrackList
            | Action::RequestTransportState
            | Action::TrackArmMidiLearn { .. }
            | Action::GlobalArmMidiLearn { .. }
            | Action::SessionArmMidiLearn { .. }
            | Action::TrackSetMidiLaneChannel { .. }
            | Action::TrackSetMpeZone { .. }
            | Action::TrackSetMpePitchBendSensitivity { .. }
            | Action::DeleteUnusedClips { .. }
            | Action::SetUnusedClips { .. }
            | Action::SetClipGainDb { .. }
            | Action::SyncClipBounds { .. }
            | Action::TrackOfflineBounce { .. }
            | Action::TrackOfflineBounceCancel { .. }
            | Action::TrackOfflineBounceCancelAll
            | Action::TrackOfflineBounceCanceled { .. }
            | Action::TrackOfflineBounceProgress { .. }
            | Action::PianoKey { .. }
            | Action::TrackClearDefaultPassthrough { .. }
            | Action::TrackClearPlugins { .. }
            | Action::ListLv2Plugins
            | Action::ListVst3Plugins
            | Action::ListClapPlugins
            | Action::ListClapPluginsWithCapabilities
            | Action::TrackGetPluginGraph { .. }
            | Action::TrackGetClapNoteNames { .. }
            | Action::TrackGetLv2Midnam { .. }
            | Action::TrackShowClapGui { .. }
            | Action::ClipShowClapGui { .. }
            | Action::TrackShowVst3Gui { .. }
            | Action::ClipShowVst3Gui { .. }
            | Action::TrackShowLv2Gui { .. }
            | Action::ClipShowLv2Gui { .. }
            | Action::TrackUnloadLv2Plugin { .. }
            | Action::TrackSetLv2PluginState { .. }
            | Action::ClipSetLv2PluginState { .. }
            | Action::ClipSetLv2ControlValue { .. }
            | Action::TrackLv2SnapshotState { .. }
            | Action::ClipLv2SnapshotState { .. }
            | Action::TrackGetLv2PluginControls { .. }
            | Action::ClipGetLv2PluginControls { .. }
            | Action::TrackSetPluginResourceDir { .. }
            | Action::TrackClapCollectResources { .. }
            | Action::ClipSetPluginResourceDir { .. }
            | Action::ClipClapCollectResources { .. }
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
            | Action::TrackGetVst3Parameters { .. }
            | Action::ClipGetVst3Parameters { .. }
            | Action::TrackVst3SnapshotState { .. }
            | Action::ClipVst3SnapshotState { .. }
            | Action::TrackVst3RestoreState { .. }
            | Action::ClipVst3RestoreState { .. }
            | Action::OpenAudioDevice { .. }
            | Action::JackAddAudioInputPort
            | Action::JackRemoveAudioInputPort(_)
            | Action::JackAddAudioOutputPort
            | Action::JackRemoveAudioOutputPort(_)
            | Action::JackGetGraph
            | Action::JackConnect { .. }
            | Action::JackDisconnect { .. }
            | Action::OpenMidiInputDevice(_)
            | Action::OpenMidiOutputDevice(_)
            | Action::RequestSessionDiagnostics
            | Action::RequestMidiLearnMappingsReport
            | Action::TransportPositionAt { .. }
            | Action::StepRecordMidiNote { .. }
            | Action::Log { .. } => false,
        }
    }

    #[test]
    fn undo_consistency_classification_registry_is_exhaustive() {
        for action in undoable_command_instances() {
            assert!(
                is_undoable_command(&action),
                "{action:?} must be classified as an undoable command"
            );
        }
    }
}
