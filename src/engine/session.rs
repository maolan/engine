use super::*;
use crate::engine::fields::SessionFields;
use crate::{
    kind::Kind,
    message::SessionSlotState,
    message::{Action, LaunchQuantization, SessionAction},
    track::{SessionSlot, Track, TrackData},
};
use std::path::Path;

#[derive(Clone, Copy)]
struct SessionSceneLengthTiming {
    launch_quantization: LaunchQuantization,
    bpm: f64,
    tsig_num: u16,
    tsig_denom: u16,
    sample_rate: f64,
}

impl Engine {
    #[cfg(unix)]
    pub(crate) fn session_plugins_dir(&self) -> Option<PathBuf> {
        self.session.session_dir.as_ref().map(|d| d.join("plugins"))
    }

    pub(crate) fn session_audio_dir(&self) -> Option<PathBuf> {
        self.session.session_dir.as_ref().map(|d| d.join("audio"))
    }

    pub(crate) fn session_midi_dir(&self) -> Option<PathBuf> {
        self.session.session_dir.as_ref().map(|d| d.join("midi"))
    }

    pub(crate) fn session_peaks_dir(&self) -> Option<PathBuf> {
        self.session.session_dir.as_ref().map(|d| d.join("peaks"))
    }

    pub(crate) fn ensure_session_subdirs(&self) {
        if let Some(root) = &self.session.session_dir {
            let _ = std::fs::create_dir_all(root.join("plugins"));
            let _ = std::fs::create_dir_all(root.join("audio"));
            let _ = std::fs::create_dir_all(root.join("midi"));
            let _ = std::fs::create_dir_all(root.join("peaks"));
        }
    }

    pub(crate) fn session_end_sample(&self) -> usize {
        self.state
            .lock()
            .tracks
            .values()
            .map(|track| {
                let track = track.lock();
                let audio_end = track
                    .audio
                    .clips()
                    .iter()
                    .map(|clip| clip.end)
                    .max()
                    .unwrap_or(0);
                let midi_end = track
                    .midi
                    .clips()
                    .iter()
                    .map(|clip| clip.end)
                    .max()
                    .unwrap_or(0);
                audio_end.max(midi_end)
            })
            .max()
            .unwrap_or(0)
    }

    fn record_session_completed_clip_pass(
        &mut self,
        track_name: String,
        scene_index: usize,
        clip_id: String,
        pass_index: usize,
        start_sample: usize,
        length_samples: usize,
    ) {
        let key = (
            track_name.clone(),
            scene_index,
            clip_id.clone(),
            pass_index,
            start_sample,
        );
        if self.session.session_reported_clip_passes.insert(key) {
            self.session.session_completed_clip_passes.push(
                crate::meter::SessionCompletedClipPass {
                    track_name,
                    scene_index,
                    clip_id,
                    pass_index,
                    start_sample,
                    length_samples,
                },
            );
        }
    }

    fn record_completed_session_scene_span(
        &mut self,
        tracks: &HashMap<String, crate::state::TrackHandle>,
        scene_index: usize,
        previous_scene: Option<usize>,
        scene_start_sample: usize,
        elapsed_samples: usize,
    ) {
        for (track_name, track) in tracks {
            let track = track.lock();
            let slot = track.rt.session_slots.get(&scene_index);
            let prev_slot = previous_scene.and_then(|scene| track.rt.session_slots.get(&scene));
            let playing_clip_id = track
                .rt
                .playing_session_clips
                .last()
                .map(|clip| clip.clip_id.as_str());
            let clip_id = match (slot, prev_slot) {
                (Some(slot), _) if slot.play_enabled => Some(slot.clip_id.as_str()),
                (Some(slot), _) if slot.stop_enabled => None,
                (_, Some(prev_slot)) if prev_slot.play_enabled => Some(prev_slot.clip_id.as_str()),
                (_, Some(prev_slot)) if prev_slot.stop_enabled => None,
                _ => playing_clip_id,
            }
            .filter(|clip_id| !clip_id.is_empty());
            let Some(clip_id) = clip_id else { continue };
            let clip_length = track
                .session_clip_length(clip_id, crate::kind::Kind::Audio)
                .or_else(|| track.session_clip_length(clip_id, crate::kind::Kind::MIDI))
                .unwrap_or(0);
            if clip_length == 0 {
                continue;
            }
            let completed_passes = elapsed_samples / clip_length;
            for pass_index in 0..completed_passes {
                self.record_session_completed_clip_pass(
                    track_name.clone(),
                    scene_index,
                    clip_id.to_string(),
                    pass_index,
                    scene_start_sample.saturating_add(pass_index * clip_length),
                    clip_length,
                );
            }
        }
    }

    pub(crate) async fn publish_session_runtime_reports(&mut self) {
        let mut current = HashMap::<(String, usize), (SessionSlotState, usize, usize)>::new();
        {
            let state = self.state_snapshot.load_full();
            if let Some((queued_scene, queued_launch_at)) = self.session.session_scene_queue {
                // Drop the queue marker once it has fired: no matching
                // pending launch or scheduled stop remains and the session
                // transport has passed the launch time. The transport check
                // matters when the queued scene scheduled nothing (all
                // tracks continue): the marker must still hold until the
                // launch time arrives. The fired scene becomes the current
                // one.
                let still_pending = state.tracks.values().any(|track| {
                    let track = track.lock();
                    track.rt.pending_session_launches.iter().any(|launch| {
                        launch.scene_index == queued_scene
                            && launch.launch_at_sample == queued_launch_at
                    }) || track
                        .rt
                        .playing_session_clips
                        .iter()
                        .any(|clip| clip.stop_at_sample == Some(queued_launch_at))
                });
                if !still_pending && self.transport.session_transport_sample >= queued_launch_at {
                    if let Some(current_scene) = self.session.session_current_scene {
                        let scene_start = self.session.session_current_scene_start_sample;
                        let elapsed = queued_launch_at.saturating_sub(scene_start);
                        self.record_completed_session_scene_span(
                            &state.tracks,
                            current_scene,
                            self.session.session_current_scene_previous_scene,
                            scene_start,
                            elapsed,
                        );
                    }
                    self.session.session_scene_queue = None;
                    self.session.session_current_scene_previous_scene =
                        self.session.session_current_scene;
                    self.session.session_current_scene = Some(queued_scene);
                    self.session.session_current_scene_start_sample = queued_launch_at;
                    self.session.session_current_scene_length_samples =
                        self.session.session_scene_queue_length_samples;
                    self.session.session_scene_queue_length_samples = 0;
                }
            }
            if let Some(current_scene) = self.session.session_current_scene
                && self.session.session_current_scene_length_samples > 0
            {
                self.record_completed_session_scene_span(
                    &state.tracks,
                    current_scene,
                    self.session.session_current_scene_previous_scene,
                    self.session.session_current_scene_start_sample,
                    self.transport
                        .session_transport_sample
                        .saturating_sub(self.session.session_current_scene_start_sample),
                );
            }
            for (track_name, track) in &state.tracks {
                let track = track.lock();
                for launch in &track.rt.pending_session_launches {
                    current.insert(
                        (track_name.clone(), launch.scene_index),
                        (SessionSlotState::Queued, 0, 0),
                    );
                }
                for clip in &track.rt.playing_session_clips {
                    if self.session.session_current_scene_length_samples == 0
                        && let Some(clip_length) =
                            track.session_clip_length(&clip.clip_id, clip.kind)
                    {
                        let scene_report = self
                            .session
                            .session_current_scene
                            .map(|_| self.session.session_current_scene_length_samples)
                            .filter(|length| *length > 0)
                            .map(|length| {
                                (
                                    self.session
                                        .session_current_scene
                                        .unwrap_or(clip.scene_index),
                                    self.session.session_current_scene_start_sample,
                                    self.transport.session_transport_sample.saturating_sub(
                                        self.session.session_current_scene_start_sample,
                                    ),
                                    length,
                                )
                            });
                        let launch_sample = self
                            .transport
                            .session_transport_sample
                            .saturating_sub(clip.elapsed_samples);
                        let (
                            report_scene_index,
                            report_start_sample,
                            report_elapsed,
                            report_length,
                        ) = scene_report.unwrap_or((
                            clip.scene_index,
                            launch_sample,
                            clip.elapsed_samples,
                            clip_length,
                        ));
                        if report_length == 0 {
                            continue;
                        }
                        let completed_passes = report_elapsed / report_length;
                        for pass_index in 0..completed_passes {
                            self.record_session_completed_clip_pass(
                                track_name.clone(),
                                report_scene_index,
                                clip.clip_id.clone(),
                                pass_index,
                                report_start_sample.saturating_add(pass_index * report_length),
                                report_length,
                            );
                        }
                    }
                    current.insert(
                        (track_name.clone(), clip.scene_index),
                        (
                            SessionSlotState::Playing,
                            clip.play_position_samples,
                            clip.elapsed_samples,
                        ),
                    );
                }
            }
        }

        if self
            .session
            .last_session_report_publish
            .is_some_and(|t| t.elapsed() < SessionFields::SESSION_RUNTIME_REPORT_INTERVAL)
        {
            return;
        }

        let snapshot = self
            .session
            .session_runtime_snapshot_producer
            .write_buffer();
        snapshot.session_sample = self.transport.session_transport_sample;
        snapshot.slots.clear();
        snapshot.slots.extend(current.iter().map(
            |((track_name, scene_index), (state, play_position_samples, elapsed_samples))| {
                crate::meter::SessionRuntimeSlotSnapshot {
                    track_name: track_name.clone(),
                    scene_index: *scene_index,
                    state: *state,
                    play_position_samples: *play_position_samples,
                    elapsed_samples: *elapsed_samples,
                }
            },
        ));
        snapshot.completed_clip_passes = self.session.session_completed_clip_passes.clone();
        snapshot.current_scene = self.session.session_current_scene;
        self.session.session_runtime_snapshot_producer.publish();
        self.session.last_session_report_publish = Some(Instant::now());
    }

    fn cancel_session_scene_queue(
        queue: &mut Option<(usize, usize)>,
        tracks: &[crate::state::TrackHandle],
    ) {
        let Some((prev_scene, prev_launch_at)) = queue.take() else {
            return;
        };
        for track in tracks {
            let mut track_lock = track.lock();
            track_lock.rt.pending_session_launches.retain(|launch| {
                !(launch.scene_index == prev_scene && launch.launch_at_sample == prev_launch_at)
            });
            for clip in &mut track_lock.rt.playing_session_clips {
                if clip.stop_at_sample == Some(prev_launch_at) {
                    clip.stop_at_sample = None;
                }
            }
        }
    }

    fn session_scene_length_samples(
        tracks: &[crate::state::TrackHandle],
        scene_index: usize,
        previous_scene: Option<usize>,
        timing: SessionSceneLengthTiming,
    ) -> usize {
        let snap_interval = TrackData::launch_quantization_interval_samples(
            timing.launch_quantization,
            timing.bpm,
            timing.tsig_num,
            timing.tsig_denom,
            timing.sample_rate,
        );
        let mut longest_scene_clip = 0usize;
        for track in tracks {
            let track = track.lock();
            let slot = track.rt.session_slots.get(&scene_index);
            let prev_scene = track
                .rt
                .playing_session_clips
                .last()
                .map(|clip| clip.scene_index)
                .or(previous_scene);
            let prev_slot = prev_scene.and_then(|scene| track.rt.session_slots.get(&scene));
            let playing_clip_id = track
                .rt
                .playing_session_clips
                .last()
                .map(|clip| clip.clip_id.as_str());

            let clip_id = match (slot, prev_slot) {
                (Some(slot), _) if slot.play_enabled => Some(slot.clip_id.as_str()),
                (Some(slot), _) if slot.stop_enabled => None,
                (_, Some(prev_slot)) if prev_slot.play_enabled => Some(prev_slot.clip_id.as_str()),
                (_, Some(prev_slot)) if prev_slot.stop_enabled => None,
                _ => playing_clip_id,
            }
            .filter(|clip_id| !clip_id.is_empty());
            let Some(clip_id) = clip_id else { continue };
            let clip_length = track
                .session_clip_length(clip_id, Kind::Audio)
                .or_else(|| track.session_clip_length(clip_id, Kind::MIDI))
                .unwrap_or(0);
            longest_scene_clip = longest_scene_clip.max(clip_length);
        }
        snap_interval.max(longest_scene_clip).max(1)
    }

    pub(crate) async fn handle_session_action(&mut self, action: SessionAction) {
        let sample_rate = self.sample_rate();
        let bpm = self.transport.tempo_bpm;
        let tsig_num = self.transport.tsig_num;
        let tsig_denom = self.transport.tsig_denom;
        let scene_length_timing = |launch_quantization| SessionSceneLengthTiming {
            launch_quantization,
            bpm,
            tsig_num,
            tsig_denom,
            sample_rate,
        };
        let session_active = self.transport.session_clip_playback_enabled && self.transport.playing;
        let quantize_reference_sample = if session_active {
            self.transport.session_transport_sample
        } else {
            self.transport.transport_sample
        };
        let quantize = |sample: usize, quantization: LaunchQuantization| -> usize {
            if !self.transport.transport_running && !session_active {
                return sample;
            }
            Track::quantize_sample_to_boundary(
                sample,
                quantization,
                bpm,
                tsig_num,
                tsig_denom,
                sample_rate,
            )
        };

        match action {
            SessionAction::LaunchClip {
                track_name,
                scene_index,
                clip_id,
                launch_quantization,
                loop_enabled,
                loop_start_samples,
                loop_end_samples,
            } => {
                let Some(track) = self.track_handle_by_name(&track_name) else {
                    tracing::warn!("Session launch for unknown track '{}'", track_name);
                    return;
                };
                let mut track = track.lock();
                let clip_id = if clip_id.is_empty() {
                    track
                        .rt
                        .session_slots
                        .get(&scene_index)
                        .map(|slot| slot.clip_id.clone())
                        .unwrap_or_default()
                } else {
                    clip_id
                };
                let kind = if track.audio.clips().iter().any(|c| c.id == clip_id) {
                    Kind::Audio
                } else if track.midi.clips().iter().any(|c| c.id == clip_id) {
                    Kind::MIDI
                } else if track
                    .rt
                    .session_clip_pool_audio
                    .iter()
                    .any(|c| c.id == clip_id)
                {
                    Kind::Audio
                } else if track
                    .rt
                    .session_clip_pool_midi
                    .iter()
                    .any(|c| c.id == clip_id)
                {
                    Kind::MIDI
                } else {
                    tracing::warn!(
                        "Session launch for unknown clip '{}' on track '{}'",
                        clip_id,
                        track_name
                    );
                    return;
                };
                let launch_at_sample = quantize(quantize_reference_sample, launch_quantization);
                track.schedule_session_launch(crate::track::PendingSessionLaunch {
                    scene_index,
                    clip_id,
                    kind,
                    launch_at_sample,
                    loop_enabled,
                    loop_start_samples,
                    loop_end_samples,
                });
            }
            SessionAction::StopClip {
                track_name,
                scene_index,
                launch_quantization,
            } => {
                let Some(track) = self.track_handle_by_name(&track_name) else {
                    return;
                };
                let stop_at_sample = quantize(quantize_reference_sample, launch_quantization);
                track
                    .lock()
                    .schedule_session_stop(scene_index, stop_at_sample);
            }
            SessionAction::LaunchScene {
                scene_index,
                launch_quantization,
            } => {
                let launch_at_sample = quantize(quantize_reference_sample, launch_quantization);
                self.session.session_current_scene_previous_scene =
                    self.session.session_current_scene;
                self.session.session_current_scene = Some(scene_index);
                self.session.session_current_scene_start_sample = launch_at_sample;
                let tracks: Vec<_> = self
                    .state_snapshot
                    .load_full()
                    .tracks
                    .values()
                    .cloned()
                    .collect();
                self.session.session_current_scene_length_samples =
                    Self::session_scene_length_samples(
                        &tracks,
                        scene_index,
                        None,
                        scene_length_timing(launch_quantization),
                    );
                for track in tracks {
                    let mut track_lock = track.lock();
                    let Some(slot) = track_lock.rt.session_slots.get(&scene_index) else {
                        continue;
                    };
                    if !slot.play_enabled {
                        continue;
                    }
                    let clip_id = slot.clip_id.clone();
                    let kind = if track_lock.audio.clips().iter().any(|c| c.id == clip_id) {
                        Kind::Audio
                    } else if track_lock.midi.clips().iter().any(|c| c.id == clip_id) {
                        Kind::MIDI
                    } else if track_lock
                        .rt
                        .session_clip_pool_audio
                        .iter()
                        .any(|c| c.id == clip_id)
                    {
                        Kind::Audio
                    } else if track_lock
                        .rt
                        .session_clip_pool_midi
                        .iter()
                        .any(|c| c.id == clip_id)
                    {
                        Kind::MIDI
                    } else {
                        continue;
                    };
                    track_lock.schedule_session_launch(crate::track::PendingSessionLaunch {
                        scene_index,
                        clip_id,
                        kind,
                        launch_at_sample,
                        loop_enabled: true,
                        loop_start_samples: 0,
                        loop_end_samples: 0,
                    });
                }
            }
            SessionAction::StopScene {
                scene_index,
                launch_quantization,
            } => {
                let stop_at_sample = quantize(quantize_reference_sample, launch_quantization);
                let tracks: Vec<_> = self
                    .state_snapshot
                    .load_full()
                    .tracks
                    .values()
                    .cloned()
                    .collect();
                for track in tracks {
                    track
                        .lock()
                        .schedule_session_stop(scene_index, stop_at_sample);
                }
            }
            SessionAction::QueueScene {
                scene_index,
                launch_quantization,
            } => {
                let tracks: Vec<_> = self
                    .state_snapshot
                    .load_full()
                    .tracks
                    .values()
                    .cloned()
                    .collect();

                // A previously queued scene is replaced by the new one.
                Self::cancel_session_scene_queue(&mut self.session.session_scene_queue, &tracks);
                self.session.session_scene_queue_length_samples = 0;

                // Fire at the next current-scene boundary. Scene length is
                // independent per scene: at least the selected snap interval
                // and at least the longest clip populated in that scene.
                // Fall back to currently playing clip pass length when no
                // current scene marker exists.
                let mut max_remaining = 0usize;
                let base_sample = if session_active {
                    self.transport.session_transport_sample
                } else {
                    self.transport.transport_sample
                };

                if let Some(current_scene) = self.session.session_current_scene {
                    let scene_length = if self.session.session_current_scene_length_samples > 0 {
                        self.session.session_current_scene_length_samples
                    } else {
                        Self::session_scene_length_samples(
                            &tracks,
                            current_scene,
                            Some(current_scene),
                            scene_length_timing(launch_quantization),
                        )
                    };
                    let elapsed =
                        base_sample.saturating_sub(self.session.session_current_scene_start_sample);
                    let boundary_offset = if elapsed == 0 {
                        scene_length
                    } else {
                        let remainder = elapsed % scene_length;
                        if remainder == 0 {
                            elapsed
                        } else {
                            elapsed.saturating_add(scene_length - remainder)
                        }
                    };
                    let launch_at_sample = self
                        .session
                        .session_current_scene_start_sample
                        .saturating_add(boundary_offset);
                    max_remaining = launch_at_sample.saturating_sub(base_sample);
                } else {
                    for track in &tracks {
                        let track_lock = track.lock();
                        for clip in &track_lock.rt.playing_session_clips {
                            let Some(clip_length) =
                                track_lock.session_clip_length(&clip.clip_id, clip.kind)
                            else {
                                continue;
                            };
                            if clip_length == 0 {
                                continue;
                            }
                            let loop_end = if clip.loop_enabled && clip.loop_end_samples > 0 {
                                clip.loop_end_samples.min(clip_length)
                            } else {
                                clip_length
                            };
                            let position = clip.play_position_samples.min(loop_end);
                            max_remaining = max_remaining.max(loop_end.saturating_sub(position));
                        }
                    }
                }
                let launch_at_sample = base_sample.saturating_add(max_remaining);
                let queued_scene_length = Self::session_scene_length_samples(
                    &tracks,
                    scene_index,
                    self.session.session_current_scene,
                    scene_length_timing(launch_quantization),
                );

                /// How a track behaves when the queued scene launches.
                enum SceneSwitch {
                    Play,
                    Stop,
                    InheritPlay,
                    Continue,
                }

                // Resolve a clip's kind from the timeline clips or the
                // session clip pool; `None` when the clip is unknown.
                let clip_kind = |track: &crate::track::TrackData, clip_id: &str| {
                    if track.audio.clips().iter().any(|c| c.id == clip_id) {
                        Some(Kind::Audio)
                    } else if track.midi.clips().iter().any(|c| c.id == clip_id) {
                        Some(Kind::MIDI)
                    } else if track
                        .rt
                        .session_clip_pool_audio
                        .iter()
                        .any(|c| c.id == clip_id)
                    {
                        Some(Kind::Audio)
                    } else if track
                        .rt
                        .session_clip_pool_midi
                        .iter()
                        .any(|c| c.id == clip_id)
                    {
                        Some(Kind::MIDI)
                    } else {
                        None
                    }
                };

                let mut queue_changed_anything = false;
                for track in &tracks {
                    let mut track_lock = track.lock();
                    // A slot with neither play nor stop marked inherits from
                    // the same track's slot in the previously playing scene
                    // (the most recently launched playing clip, falling back
                    // to the last fired scene). Inheriting "play" plays the
                    // previous scene's clip: it keeps going when already
                    // playing and starts when the track is silent. If the
                    // previous slot is also unmarked, the track keeps doing
                    // whatever it was doing.
                    let prev_scene = track_lock
                        .rt
                        .playing_session_clips
                        .last()
                        .map(|clip| clip.scene_index)
                        .or(self.session.session_current_scene);
                    let (play_marked, stop_marked, slot_clip_id) = track_lock
                        .rt
                        .session_slots
                        .get(&scene_index)
                        .map(|slot| {
                            (
                                slot.play_enabled,
                                slot.stop_enabled,
                                Some(slot.clip_id.clone()),
                            )
                        })
                        .unwrap_or((false, false, None));
                    let (prev_play_marked, prev_stop_marked, prev_clip_id) = prev_scene
                        .and_then(|scene| track_lock.rt.session_slots.get(&scene))
                        .map(|slot| (slot.play_enabled, slot.stop_enabled, slot.clip_id.clone()))
                        .unwrap_or((false, false, String::new()));
                    let switch = if play_marked {
                        SceneSwitch::Play
                    } else if stop_marked {
                        SceneSwitch::Stop
                    } else if prev_play_marked {
                        SceneSwitch::InheritPlay
                    } else if prev_stop_marked {
                        SceneSwitch::Stop
                    } else {
                        SceneSwitch::Continue
                    };
                    match switch {
                        SceneSwitch::Continue => continue,
                        SceneSwitch::InheritPlay => {
                            // The previous scene's clip plays: nothing to do
                            // while it is playing; start it when the track
                            // is silent.
                            if !track_lock.rt.playing_session_clips.is_empty() {
                                continue;
                            }
                            let Some(prev_scene) = prev_scene else {
                                continue;
                            };
                            if prev_clip_id.is_empty() {
                                continue;
                            }
                            let Some(kind) = clip_kind(&track_lock, &prev_clip_id) else {
                                continue;
                            };
                            track_lock.schedule_session_launch(
                                crate::track::PendingSessionLaunch {
                                    scene_index: prev_scene,
                                    clip_id: prev_clip_id,
                                    kind,
                                    launch_at_sample,
                                    loop_enabled: true,
                                    loop_start_samples: 0,
                                    loop_end_samples: 0,
                                },
                            );
                            queue_changed_anything = true;
                            continue;
                        }
                        SceneSwitch::Play | SceneSwitch::Stop => {}
                    }
                    for clip in &mut track_lock.rt.playing_session_clips {
                        clip.stop_at_sample = Some(launch_at_sample);
                    }
                    queue_changed_anything = true;
                    if matches!(switch, SceneSwitch::Stop) {
                        continue;
                    }
                    let Some(clip_id) = slot_clip_id else {
                        // Marked to play but the slot has no clip: the track
                        // goes silent when the scene launches.
                        continue;
                    };
                    let Some(kind) = clip_kind(&track_lock, &clip_id) else {
                        continue;
                    };
                    track_lock.schedule_session_launch(crate::track::PendingSessionLaunch {
                        scene_index,
                        clip_id,
                        kind,
                        launch_at_sample,
                        loop_enabled: true,
                        loop_start_samples: 0,
                        loop_end_samples: 0,
                    });
                }
                if queue_changed_anything || max_remaining > 0 {
                    self.session.session_scene_queue = Some((scene_index, launch_at_sample));
                    self.session.session_scene_queue_length_samples = queued_scene_length;
                }
            }
            SessionAction::StopAllClips => {
                let stop_at_sample = quantize(quantize_reference_sample, LaunchQuantization::Bar);
                let tracks: Vec<_> = self
                    .state_snapshot
                    .load_full()
                    .tracks
                    .values()
                    .cloned()
                    .collect();
                for track in tracks {
                    let mut track = track.lock();
                    for clip in &mut track.rt.playing_session_clips {
                        if clip.stop_at_sample.is_none() {
                            clip.stop_at_sample = Some(stop_at_sample);
                        }
                    }
                }
            }
        }
    }

    pub(crate) async fn handle_request_session_diagnostics(&mut self) {
        let (
            track_count,
            frozen_track_count,
            audio_clip_count,
            midi_clip_count,
            lv2_instance_count,
            vst3_instance_count,
            clap_instance_count,
        ) = {
            let state = self.state_snapshot.load_full();
            let tracks = &state.tracks;
            let mut track_count = 0usize;
            let mut frozen_track_count = 0usize;
            let mut audio_clip_count = 0usize;
            let mut midi_clip_count = 0usize;
            #[cfg(unix)]
            let mut lv2_instance_count = 0usize;
            #[cfg(not(unix))]
            let lv2_instance_count = 0usize;
            let mut vst3_instance_count = 0usize;
            let mut clap_instance_count = 0usize;
            for track in tracks.values() {
                let t = track.lock();
                track_count += 1;
                if t.frozen() {
                    frozen_track_count += 1;
                }
                audio_clip_count += t.audio.clips().len();
                midi_clip_count += t.midi.clips().len();
                #[cfg(unix)]
                {
                    lv2_instance_count += t.lv2_plugins.len();
                }
                vst3_instance_count += t.vst3_plugins.len();
                clap_instance_count += t.clap_plugins.len();
            }
            (
                track_count,
                frozen_track_count,
                audio_clip_count,
                midi_clip_count,
                lv2_instance_count,
                vst3_instance_count,
                clap_instance_count,
            )
        };
        #[cfg(not(unix))]
        let _lv2_instance_count = lv2_instance_count;
        let pending_hw_midi_events = self.hw_midi.pending_hw_midi_events.len()
            + self
                .hw_midi
                .pending_hw_midi_events_by_device
                .values()
                .map(std::vec::Vec::len)
                .sum::<usize>();
        let sample_rate_hz = if let Some(info) = self.hw_driver_info {
            info.sample_rate as usize
        } else {
            #[cfg(unix)]
            {
                self.jack_runtime
                    .as_ref()
                    .map(|j| j.sample_rate)
                    .unwrap_or(0)
            }
            #[cfg(not(unix))]
            0
        };
        let cycle_samples = self.current_cycle_samples();
        self.notify_query_reply(QueryReply::SessionDiagnosticsReport {
            track_count,
            frozen_track_count,
            audio_clip_count,
            midi_clip_count,
            #[cfg(unix)]
            lv2_instance_count,
            vst3_instance_count,
            clap_instance_count,
            pending_requests: self.dispatch.pending_requests.len(),
            workers_total: self.workers.len(),
            workers_ready: self.dispatch.ready_workers.len(),
            pending_hw_midi_events,
            playing: self.transport.playing,
            transport_running: self.transport.transport_running,
            transport_sample: self.transport.transport_sample,
            tempo_bpm: self.transport.tempo_bpm,
            sample_rate_hz,
            cycle_samples,
        })
        .await;
    }

    pub(crate) async fn handle_track_set_session_slot(&mut self, a: Action) -> bool {
        let Action::TrackSetSessionSlot {
            ref track_name,
            scene_index,
            ref clip_id,
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
        let mut track = track.lock();
        match clip_id {
            Some(id) => {
                let on_track = track.audio.clips().iter().any(|clip| clip.id == *id)
                    || track.midi.clips().iter().any(|clip| clip.id == *id);
                let in_pool = track
                    .rt
                    .session_clip_pool_audio
                    .iter()
                    .any(|clip| clip.id == *id)
                    || track
                        .rt
                        .session_clip_pool_midi
                        .iter()
                        .any(|clip| clip.id == *id);
                if !on_track && !in_pool {
                    let state = self.state.lock();
                    if let Some(data) = state.unused_audio_clips.iter().find(|clip| clip.id == *id)
                    {
                        track
                            .rt
                            .session_clip_pool_audio
                            .push(Arc::new(Self::audio_clip_from_data(data)));
                    } else if let Some(data) =
                        state.unused_midi_clips.iter().find(|clip| clip.id == *id)
                    {
                        track
                            .rt
                            .session_clip_pool_midi
                            .push(Arc::new(Self::midi_clip_from_data(data)));
                    }
                }
                let (play_enabled, stop_enabled) = track
                    .rt
                    .session_slots
                    .get(&scene_index)
                    .map(|slot| (slot.play_enabled, slot.stop_enabled))
                    .unwrap_or((true, false));
                track.rt.session_slots.insert(
                    scene_index,
                    SessionSlot {
                        clip_id: id.clone(),
                        play_enabled,
                        stop_enabled,
                    },
                );
            }
            None => {
                track.rt.session_slots.remove(&scene_index);
            }
        }
        track.rt.prune_session_clip_pool();

        false
    }

    pub(crate) async fn handle_track_set_session_slot_play_enabled(&mut self, a: Action) -> bool {
        let Action::TrackSetSessionSlotPlayEnabled {
            ref track_name,
            scene_index,
            enabled,
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
        let mut track = track.lock();
        track
            .rt
            .session_slots
            .entry(scene_index)
            .or_insert_with(|| SessionSlot {
                clip_id: String::new(),
                play_enabled: false,
                stop_enabled: false,
            })
            .play_enabled = enabled;

        false
    }

    pub(crate) async fn handle_track_set_session_slot_stop_enabled(&mut self, a: Action) -> bool {
        let Action::TrackSetSessionSlotStopEnabled {
            ref track_name,
            scene_index,
            enabled,
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
        let mut track = track.lock();
        track
            .rt
            .session_slots
            .entry(scene_index)
            .or_insert_with(|| SessionSlot {
                clip_id: String::new(),
                play_enabled: false,
                stop_enabled: false,
            })
            .stop_enabled = enabled;

        false
    }
}

impl Engine {
    /// Session view request arms: clip/scene runtime, session path, session
    /// MIDI learn, session slot setup.
    pub(crate) async fn handle_session_request(&mut self, a: Action) -> bool {
        match a {
            Action::Session(ref session_action) => {
                self.handle_session_action(session_action.clone()).await;
            }
            Action::SetClipPlaybackEnabled(enabled) => {
                self.transport.clip_playback_enabled = enabled;
                self.bump_prepare_generation();
                for track in self.state_snapshot.load_full().tracks.values() {
                    track.lock().set_clip_playback_enabled(enabled);
                }
            }
            Action::SetSessionClipPlaybackEnabled(enabled) => {
                self.transport.session_clip_playback_enabled = enabled;
                self.bump_prepare_generation();
                for track in self.state_snapshot.load_full().tracks.values() {
                    track.lock().set_session_clip_playback_enabled(enabled);
                }
            }
            Action::SetSessionPath(ref path) => {
                self.session.session_dir = Some(Path::new(path).to_path_buf());
                self.ensure_session_subdirs();
                #[cfg(unix)]
                let _lv2_dir = self.session_plugins_dir();
                for track in self.state_snapshot.load_full().tracks.values() {
                    track
                        .lock()
                        .set_session_base_dir(self.session.session_dir.clone());
                }
            }
            Action::SessionArmMidiLearn { ref target } => {
                self.midi_learn.pending_session_midi_learn = Some(target.clone());
            }
            Action::TrackSetSessionSlot { .. } => {
                if Self::box_bool(self.handle_track_set_session_slot(a.clone())).await {
                    return true;
                }
            }
            Action::TrackSetSessionSlotPlayEnabled { .. } => {
                if self
                    .handle_track_set_session_slot_play_enabled(a.clone())
                    .await
                {
                    return true;
                }
            }
            Action::TrackSetSessionSlotStopEnabled { .. }
                if self
                    .handle_track_set_session_slot_stop_enabled(a.clone())
                    .await =>
            {
                return true;
            }
            _ => {}
        }
        false
    }
}

// ---------- Undo/history support (colocated in Phase 4; formerly the crate::history matches) ----------
/// Whether `action` is an undoable command owned by this feature
/// (colocated from `crate::history::should_record` in Phase 4).
pub(crate) fn undo_should_record(action: &Action) -> bool {
    matches!(
        action,
        Action::TrackSetSessionSlot { .. }
            | Action::TrackSetSessionSlotPlayEnabled { .. }
            | Action::TrackSetSessionSlotStopEnabled { .. }
    )
}

/// State-based inverse constructor for this feature's commands
/// (colocated from `crate::history::create_inverse_action` in Phase 4).
pub(crate) fn undo_inverse(action: &Action, state: &State) -> Option<Action> {
    match action {
        Action::TrackSetSessionSlot {
            track_name,
            scene_index,
            clip_id: _,
        } => {
            let track = state.tracks.get(track_name)?;
            let track_lock = track.lock();
            Some(Action::TrackSetSessionSlot {
                track_name: track_name.clone(),
                scene_index: *scene_index,
                clip_id: track_lock
                    .rt
                    .session_slots
                    .get(scene_index)
                    .map(|slot| slot.clip_id.clone()),
            })
        }

        Action::TrackSetSessionSlotPlayEnabled {
            track_name,
            scene_index,
            enabled: _,
        } => {
            let track = state.tracks.get(track_name)?;
            let track_lock = track.lock();
            let enabled = track_lock
                .rt
                .session_slots
                .get(scene_index)
                .map(|slot| slot.play_enabled)
                .unwrap_or(true);
            Some(Action::TrackSetSessionSlotPlayEnabled {
                track_name: track_name.clone(),
                scene_index: *scene_index,
                enabled,
            })
        }

        Action::TrackSetSessionSlotStopEnabled {
            track_name,
            scene_index,
            enabled: _,
        } => {
            let track = state.tracks.get(track_name)?;
            let track_lock = track.lock();
            let enabled = track_lock
                .rt
                .session_slots
                .get(scene_index)
                .map(|slot| slot.stop_enabled)
                .unwrap_or(false);
            Some(Action::TrackSetSessionSlotStopEnabled {
                track_name: track_name.clone(),
                scene_index: *scene_index,
                enabled,
            })
        }
        _ => None,
    }
}

/// Multi-action inverse constructor for this feature's commands
/// (colocated from `crate::history::create_inverse_actions` in Phase 4).
pub(crate) fn undo_inverse_actions(action: &Action, state: &State) -> Option<Vec<Action>> {
    undo_inverse(action, state).map(|a| vec![a])
}
