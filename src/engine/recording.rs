use super::*;
use crate::{audio::clip::AudioClip, kind::Kind, message::Action, midi::clip::MIDIClip};
use midly::{
    Arena, Format, Header, MetaMessage, Smf, Timing, TrackEvent, TrackEventKind,
    live::LiveEvent,
    num::{u15, u24, u28},
};
use std::{
    fs::File,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

impl Engine {
    pub(crate) fn recording_segments_for_cycle(&self, frames: usize) -> Vec<(usize, usize, usize)> {
        let segments = self.cycle_segments(frames);
        let comp = self.transport.hw_input_latency_frames;
        let segments: Vec<_> = if comp > 0 {
            segments
                .into_iter()
                .map(|(start, end, offset)| {
                    (start.saturating_sub(comp), end.saturating_sub(comp), offset)
                })
                .collect()
        } else {
            segments
        };
        if !self.transport.punch_enabled {
            return segments;
        }
        let Some((punch_start, punch_end)) = self.transport.punch_range_samples else {
            return vec![];
        };
        if punch_end <= punch_start {
            return vec![];
        }
        let mut clipped = Vec::new();
        for (segment_start, segment_end, frame_offset) in segments {
            let start = segment_start.max(punch_start);
            let end = segment_end.min(punch_end);
            if end <= start {
                continue;
            }
            let clipped_offset = frame_offset.saturating_add(start.saturating_sub(segment_start));
            clipped.push((start, end, clipped_offset));
        }
        clipped
    }

    pub(crate) fn sanitize_file_stem(name: &str) -> String {
        let mut out = String::with_capacity(name.len());
        for c in name.chars() {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                out.push(c);
            } else {
                out.push('_');
            }
        }
        if out.is_empty() {
            "track".to_string()
        } else {
            out
        }
    }

    pub(crate) fn next_recording_file_name(track_name: &str) -> String {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        format!("{}_{}.wav", Self::sanitize_file_stem(track_name), ts)
    }

    pub(crate) fn next_midi_recording_file_name(track_name: &str) -> String {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        format!("{}_{}.mid", Self::sanitize_file_stem(track_name), ts)
    }

    pub(crate) fn append_recorded_cycle(&mut self) {
        if !self.transport.playing || !self.recording.record_enabled {
            return;
        }
        let state = self.state_snapshot.load_full();
        for (name, track_handle) in &state.tracks {
            let track = track_handle.lock();
            if !track.armed() {
                continue;
            }
            let audio_channels = track.rt.record_tap_outs.len();
            let audio_frames = track
                .rt
                .record_tap_outs
                .first()
                .map(|ch| ch.len())
                .unwrap_or(0);
            let frames = audio_frames.max(self.current_cycle_samples());
            if frames == 0 {
                continue;
            }
            let segments = self.recording_segments_for_cycle(frames);
            for (segment_start, segment_end, frame_offset) in segments {
                let segment_len = segment_end.saturating_sub(segment_start);
                if segment_len == 0 {
                    continue;
                }

                if audio_channels > 0 && audio_frames > 0 {
                    let audio_entry = self
                        .recording
                        .audio_recordings
                        .entry(name.clone())
                        .or_insert_with(|| RecordingSession {
                            start_sample: segment_start,
                            samples: Vec::with_capacity(segment_len * audio_channels * 2),
                            channels: audio_channels,
                            file_name: Self::next_recording_file_name(name),
                            stripe_peaks: vec![Vec::new(); audio_channels],
                            current_stripe_frames: 0,
                        });
                    if audio_entry.channels != audio_channels {
                        continue;
                    }
                    if let Some(entry) = self.recording.audio_recordings.get_mut(name.as_str()) {
                        let from = frame_offset.min(audio_frames);
                        let to = frame_offset.saturating_add(segment_len).min(audio_frames);
                        for frame in from..to {
                            let is_new_stripe =
                                entry.current_stripe_frames % RECORDING_STRIPE_FRAMES == 0;
                            for ch in 0..audio_channels {
                                let sample = track.rt.record_tap_outs[ch][frame].clamp(-1.0, 1.0);
                                if is_new_stripe {
                                    entry.stripe_peaks[ch].push([sample, sample]);
                                } else {
                                    let idx = entry.stripe_peaks[ch].len() - 1;
                                    entry.stripe_peaks[ch][idx][0] =
                                        entry.stripe_peaks[ch][idx][0].min(sample);
                                    entry.stripe_peaks[ch][idx][1] =
                                        entry.stripe_peaks[ch][idx][1].max(sample);
                                }
                                entry.samples.push(track.rt.record_tap_outs[ch][frame]);
                            }
                            entry.current_stripe_frames += 1;
                        }
                    }
                }

                let entry = self
                    .recording
                    .midi_recordings
                    .entry(name.clone())
                    .or_insert_with(|| MidiRecordingSession {
                        start_sample: segment_start,
                        events: Vec::new(),
                        file_name: Self::next_midi_recording_file_name(name),
                    });
                let from = frame_offset;
                let to = frame_offset.saturating_add(segment_len);
                for event in &track.rt.record_tap_midi_in {
                    let frame = event.frame as usize;
                    if frame < from || frame >= to {
                        continue;
                    }
                    let abs_sample = segment_start as u64 + (frame - from) as u64;
                    entry.events.push((abs_sample, event.data.clone()));
                }

                if self.transport.punch_enabled
                    && let Some((_, punch_end)) = self.transport.punch_range_samples
                    && segment_end == punch_end
                {
                    if let Some(done) = self.recording.audio_recordings.remove(name.as_str()) {
                        self.recording
                            .completed_audio_recordings
                            .push((name.clone(), done));
                    }
                    if let Some(done) = self.recording.midi_recordings.remove(name.as_str()) {
                        self.recording
                            .completed_midi_recordings
                            .push((name.clone(), done));
                    }
                } else if self.transport.loop_enabled
                    && let Some((_, loop_end)) = self.transport.loop_range_samples
                    && segment_end == loop_end
                {
                    if let Some(done) = self.recording.audio_recordings.remove(name.as_str()) {
                        self.recording
                            .completed_audio_recordings
                            .push((name.clone(), done));
                    }
                    if let Some(done) = self.recording.midi_recordings.remove(name.as_str()) {
                        self.recording
                            .completed_midi_recordings
                            .push((name.clone(), done));
                    }
                }
            }
        }
    }

    pub(crate) async fn flush_completed_recordings(&mut self) {
        if self.recording.completed_audio_recordings.is_empty()
            && self.recording.completed_midi_recordings.is_empty()
        {
            return;
        }
        let Some(audio_dir) = self.session_audio_dir() else {
            self.recording.completed_audio_recordings.clear();
            self.recording.completed_midi_recordings.clear();
            return;
        };
        let Some(midi_dir) = self.session_midi_dir() else {
            self.recording.completed_audio_recordings.clear();
            self.recording.completed_midi_recordings.clear();
            return;
        };
        if std::fs::create_dir_all(&audio_dir).is_err()
            || std::fs::create_dir_all(&midi_dir).is_err()
        {
            self.recording.completed_audio_recordings.clear();
            self.recording.completed_midi_recordings.clear();
            return;
        }
        let rate = self
            .hw_driver_info
            .map(|info| info.sample_rate)
            .unwrap_or(48_000);
        let completed_audio = std::mem::take(&mut self.recording.completed_audio_recordings);
        for (track_name, rec) in completed_audio {
            self.flush_recording_entry(&audio_dir, rate, track_name, rec)
                .await;
        }
        let completed_midi = std::mem::take(&mut self.recording.completed_midi_recordings);
        for (track_name, rec) in completed_midi {
            self.flush_midi_recording_entry(&midi_dir, rate as u32, track_name, rec)
                .await;
        }
    }

    pub(crate) async fn flush_recordings(&mut self) {
        let Some(audio_dir) = self.session_audio_dir() else {
            if !self.recording.audio_recordings.is_empty()
                || !self.recording.midi_recordings.is_empty()
                || !self.recording.completed_audio_recordings.is_empty()
                || !self.recording.completed_midi_recordings.is_empty()
            {
                self.notify_clients(Err("Recording stopped: session path is not set".to_string()))
                    .await;
            }
            self.recording.audio_recordings.clear();
            self.recording.midi_recordings.clear();
            self.recording.completed_audio_recordings.clear();
            self.recording.completed_midi_recordings.clear();
            return;
        };
        if std::fs::create_dir_all(&audio_dir).is_err() {
            self.notify_clients(Err(format!(
                "Recording stopped: failed to create audio directory {}",
                audio_dir.display()
            )))
            .await;
            self.recording.audio_recordings.clear();
            self.recording.midi_recordings.clear();
            self.recording.completed_audio_recordings.clear();
            self.recording.completed_midi_recordings.clear();
            return;
        }
        let Some(midi_dir) = self.session_midi_dir() else {
            self.recording.audio_recordings.clear();
            self.recording.midi_recordings.clear();
            self.recording.completed_audio_recordings.clear();
            self.recording.completed_midi_recordings.clear();
            return;
        };
        if std::fs::create_dir_all(&midi_dir).is_err() {
            self.recording.audio_recordings.clear();
            self.recording.midi_recordings.clear();
            self.recording.completed_audio_recordings.clear();
            self.recording.completed_midi_recordings.clear();
            return;
        }
        let rate = self
            .hw_driver_info
            .map(|info| info.sample_rate)
            .unwrap_or(48_000);
        let completed_audio = std::mem::take(&mut self.recording.completed_audio_recordings);
        for (track_name, rec) in completed_audio {
            self.flush_recording_entry(&audio_dir, rate, track_name, rec)
                .await;
        }
        let completed_midi = std::mem::take(&mut self.recording.completed_midi_recordings);
        for (track_name, rec) in completed_midi {
            self.flush_midi_recording_entry(&midi_dir, rate as u32, track_name, rec)
                .await;
        }
        let recordings = std::mem::take(&mut self.recording.audio_recordings);
        for (track_name, rec) in recordings {
            self.flush_recording_entry(&audio_dir, rate, track_name, rec)
                .await;
        }
        let midi_recordings = std::mem::take(&mut self.recording.midi_recordings);
        for (track_name, rec) in midi_recordings {
            self.flush_midi_recording_entry(&midi_dir, rate as u32, track_name, rec)
                .await;
        }
    }

    pub(crate) fn compute_peaks_from_stripes(
        stripe_peaks: &[Vec<[f32; 2]>],
        total_frames: usize,
        channels: usize,
    ) -> serde_json::Value {
        const MAX_PEAK_BINS: usize = 32_768;
        if total_frames == 0 || stripe_peaks.is_empty() {
            return serde_json::json!({"peaks": []});
        }
        let target_bins = total_frames.clamp(1024, MAX_PEAK_BINS);
        let mut peaks = vec![vec![[0.0_f32, 0.0_f32]; target_bins]; channels];
        for (ch, channel_peaks) in peaks.iter_mut().enumerate() {
            let mut touched = vec![false; target_bins];
            let empty = Vec::new();
            let channel_stripes = stripe_peaks.get(ch).unwrap_or(&empty);
            for (stripe_idx, stripe) in channel_stripes.iter().enumerate() {
                let stripe_start = stripe_idx * RECORDING_STRIPE_FRAMES;
                let stripe_end = ((stripe_idx + 1) * RECORDING_STRIPE_FRAMES).min(total_frames);
                let start_bin = (stripe_start * target_bins) / total_frames.max(1);
                let end_bin = ((stripe_end.saturating_sub(1)) * target_bins / total_frames.max(1))
                    .min(target_bins - 1);
                for bin in start_bin..=end_bin {
                    if !touched[bin] {
                        channel_peaks[bin] = *stripe;
                        touched[bin] = true;
                    } else {
                        channel_peaks[bin][0] = channel_peaks[bin][0].min(stripe[0]);
                        channel_peaks[bin][1] = channel_peaks[bin][1].max(stripe[1]);
                    }
                }
            }
        }
        serde_json::json!({
            "peaks": peaks.iter().map(|ch| {
                ch.iter().map(|pair| serde_json::json!([pair[0], pair[1]])).collect::<Vec<_>>()
            }).collect::<Vec<_>>()
        })
    }

    pub(crate) async fn flush_recording_entry(
        &mut self,
        audio_dir: &Path,
        rate: i32,
        track_name: String,
        rec: RecordingSession,
    ) {
        if rec.samples.is_empty() || rec.channels == 0 {
            return;
        }

        let trim_frames = self.transport.hw_output_latency_frames;
        let trim_samples = trim_frames * rec.channels;
        let samples = if trim_samples > 0 && rec.samples.len() > trim_samples {
            &rec.samples[trim_samples..]
        } else {
            &rec.samples[..]
        };
        if samples.is_empty() {
            return;
        }
        let file_path = audio_dir.join(&rec.file_name);
        let write_result =
            crate::audio_codec::write_wav_f32(&file_path, samples, rec.channels, rate as u32);
        if let Err(e) = write_result {
            tracing::error!("flush_recording_entry: WAV write failed: {}", e);
            self.notify_clients(Err(format!(
                "Failed to write recording {}: {}",
                file_path.display(),
                e
            )))
            .await;
            return;
        }

        let total_frames = rec.current_stripe_frames;
        let peaks_json =
            Self::compute_peaks_from_stripes(&rec.stripe_peaks, total_frames, rec.channels);
        let peaks_file_name = format!("{}.json", rec.file_name);
        let peaks_rel = format!("peaks/{}", peaks_file_name);
        let peaks_path = self.session_peaks_dir().map(|d| d.join(&peaks_file_name));
        if let Some(peaks_dir) = self.session_peaks_dir() {
            let _ = std::fs::create_dir_all(&peaks_dir);
        }
        if let Some(ref path) = peaks_path
            && let Err(e) = std::fs::write(
                path,
                serde_json::to_string_pretty(&peaks_json).unwrap_or_default(),
            )
        {
            tracing::warn!("Failed to write peaks file {}: {}", path.display(), e);
        }
        let length = samples.len() / rec.channels;
        let start_sample = rec.start_sample.saturating_add(trim_frames);
        let clip_rel_name = format!("audio/{}", rec.file_name);
        let mut clip = AudioClip::new(
            clip_rel_name.clone(),
            start_sample,
            start_sample.saturating_add(length.max(1)),
        );
        let (audio_ins, audio_outs) =
            if let Some(track) = self.state_snapshot.load_full().tracks.get(&track_name) {
                let track = track.lock();
                let audio_ins = track.audio.ins.len();
                let audio_outs = track.audio.outs.len();
                track.audio.push_clip(clip.clone());
                (audio_ins, audio_outs)
            } else {
                tracing::warn!(
                    "flush_recording_entry: track '{}' not found in engine state",
                    track_name
                );
                (0, 0)
            };
        let clip_id = crate::message::generate_clip_id();
        clip.id.clone_from(&clip_id);
        self.notify_clients(Ok(Action::AddClip {
            clip_id,
            name: clip_rel_name,
            track_name: track_name.clone(),
            start: start_sample,
            length,
            offset: 0,
            input_channel: 0,
            muted: false,
            reversed: false,
            gain_db: 0.0,
            peaks_file: peaks_path.is_some().then_some(peaks_rel),
            kind: Kind::Audio,
            fade_enabled: clip.fade_enabled,
            fade_in_samples: clip.fade_in_samples,
            fade_out_samples: clip.fade_out_samples,
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
            plugin_graph_json: Some(Self::default_clip_plugin_graph_json(audio_ins, audio_outs)),
        }))
        .await;
        if let Some(track) = self
            .state_snapshot
            .load_full()
            .tracks
            .get(&track_name)
            .cloned()
        {
            tokio::task::spawn_blocking(move || {
                track.lock().preload_clips();
                tracing::debug!("Preloaded clips for track '{}' after recording", track_name);
            });
        }
    }

    pub(crate) async fn flush_track_recording(&mut self, track_name: &str) {
        let Some(audio_dir) = self.session_audio_dir() else {
            self.recording.audio_recordings.remove(track_name);
            self.recording.midi_recordings.remove(track_name);
            self.recording
                .completed_audio_recordings
                .retain(|(name, _)| name != track_name);
            self.recording
                .completed_midi_recordings
                .retain(|(name, _)| name != track_name);
            return;
        };
        let Some(midi_dir) = self.session_midi_dir() else {
            self.recording.audio_recordings.remove(track_name);
            self.recording.midi_recordings.remove(track_name);
            self.recording
                .completed_audio_recordings
                .retain(|(name, _)| name != track_name);
            self.recording
                .completed_midi_recordings
                .retain(|(name, _)| name != track_name);
            return;
        };
        if std::fs::create_dir_all(&audio_dir).is_err()
            || std::fs::create_dir_all(&midi_dir).is_err()
        {
            return;
        }
        let rate = self
            .hw_driver_info
            .map(|info| info.sample_rate)
            .unwrap_or(48_000);
        let mut i = 0;
        while i < self.recording.completed_audio_recordings.len() {
            if self.recording.completed_audio_recordings[i].0 == track_name {
                let (name, rec) = self.recording.completed_audio_recordings.remove(i);
                self.flush_recording_entry(&audio_dir, rate, name, rec)
                    .await;
            } else {
                i += 1;
            }
        }
        let mut j = 0;
        while j < self.recording.completed_midi_recordings.len() {
            if self.recording.completed_midi_recordings[j].0 == track_name {
                let (name, rec) = self.recording.completed_midi_recordings.remove(j);
                self.flush_midi_recording_entry(&midi_dir, rate as u32, name, rec)
                    .await;
            } else {
                j += 1;
            }
        }

        let Some(rec) = self.recording.audio_recordings.remove(track_name) else {
            if let Some(mrec) = self.recording.midi_recordings.remove(track_name) {
                self.flush_midi_recording_entry(
                    &midi_dir,
                    rate as u32,
                    track_name.to_string(),
                    mrec,
                )
                .await;
            }
            return;
        };
        self.flush_recording_entry(&audio_dir, rate, track_name.to_string(), rec)
            .await;
        if let Some(mrec) = self.recording.midi_recordings.remove(track_name) {
            self.flush_midi_recording_entry(&midi_dir, rate as u32, track_name.to_string(), mrec)
                .await;
        }
    }

    pub(crate) async fn flush_midi_recording_entry(
        &mut self,
        midi_dir: &Path,
        sample_rate: u32,
        track_name: String,
        mut rec: MidiRecordingSession,
    ) {
        if rec.events.is_empty() {
            return;
        }
        rec.events.sort_by_key(|(sample, _)| *sample);
        let clip_rel_name = format!("midi/{}", rec.file_name);
        let clip_len_samples = rec
            .events
            .last()
            .map(|(s, _)| s.saturating_sub(rec.start_sample as u64) as usize + 1)
            .unwrap_or(1);

        for (sample, _) in &mut rec.events {
            *sample = sample.saturating_sub(rec.start_sample as u64);
        }
        let path = midi_dir.join(&rec.file_name);
        if let Err(e) = Self::write_midi_file(&path, sample_rate, &rec.events) {
            self.notify_clients(Err(format!(
                "Failed to write MIDI recording {}: {}",
                path.display(),
                e
            )))
            .await;
            return;
        }
        let mut clip = MIDIClip::new(
            clip_rel_name.clone(),
            rec.start_sample,
            rec.start_sample.saturating_add(clip_len_samples.max(1)),
        );
        clip.offset = 0;
        let clip_id = crate::message::generate_clip_id();
        clip.id.clone_from(&clip_id);
        if let Some(track) = self.state_snapshot.load_full().tracks.get(&track_name) {
            track.lock().midi.push_clip(clip);
        }
        self.notify_clients(Ok(Action::AddClip {
            clip_id,
            name: clip_rel_name,
            track_name: track_name.clone(),
            start: rec.start_sample,
            length: clip_len_samples,
            offset: 0,
            input_channel: 0,
            muted: false,
            reversed: false,
            gain_db: 0.0,
            peaks_file: None,
            kind: Kind::MIDI,
            fade_enabled: true,
            fade_in_samples: 240,
            fade_out_samples: 240,
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
        }))
        .await;
        if let Some(track) = self
            .state_snapshot
            .load_full()
            .tracks
            .get(&track_name)
            .cloned()
        {
            tokio::task::spawn_blocking(move || {
                track.lock().preload_clips();
                tracing::debug!(
                    "Preloaded clips for track '{}' after MIDI recording",
                    track_name
                );
            });
        }
    }

    pub(crate) fn write_midi_file(
        path: &Path,
        sample_rate: u32,
        events: &[(u64, Vec<u8>)],
    ) -> Result<(), String> {
        let ppq: u16 = 480;
        let ticks_per_second: u64 = 960;
        let arena = Arena::new();
        let mut track_events: Vec<TrackEvent<'_>> = vec![TrackEvent {
            delta: u28::new(0),
            kind: TrackEventKind::Meta(MetaMessage::Tempo(u24::new(500_000))),
        }];
        let mut prev_ticks = 0_u64;
        for (sample, data) in events {
            let ticks = sample.saturating_mul(ticks_per_second) / sample_rate.max(1) as u64;
            let delta = ticks.saturating_sub(prev_ticks).min(u32::MAX as u64) as u32;
            prev_ticks = ticks;
            let Ok(live) = LiveEvent::parse(data) else {
                continue;
            };
            let kind = live.as_track_event(&arena);
            track_events.push(TrackEvent {
                delta: u28::new(delta),
                kind,
            });
        }
        track_events.push(TrackEvent {
            delta: u28::new(0),
            kind: TrackEventKind::Meta(MetaMessage::EndOfTrack),
        });

        let smf = Smf {
            header: Header::new(Format::SingleTrack, Timing::Metrical(u15::new(ppq))),
            tracks: vec![track_events],
        };
        let mut file = File::create(path).map_err(|e| e.to_string())?;
        smf.write_std(&mut file).map_err(|e| e.to_string())
    }

    pub(crate) async fn handle_set_record_enabled(&mut self, a: Action) -> bool {
        let Action::SetRecordEnabled(enabled) = a else {
            return false;
        };

        self.recording.record_enabled = enabled;
        self.bump_prepare_generation();
        if !enabled {
            if self.transport.awaiting_hwfinished {
                self.append_recorded_cycle();
            }
            self.flush_recordings().await;
        } else if self.session.session_dir.is_none() {
            self.notify_clients(Err(
                "Recording enabled but session path is not set".to_string()
            ))
            .await;
        }

        false
    }
}

impl Engine {
    /// Recording-related request arms: record enable, step recording, arm
    /// toggling.
    pub(crate) async fn handle_recording_request(&mut self, a: Action) -> bool {
        match a {
            Action::SetRecordEnabled(..) => {
                if Self::box_bool(self.handle_set_record_enabled(a.clone())).await {
                    return true;
                }
            }
            Action::SetStepRecording(enabled) => {
                self.recording.step_recording_enabled = enabled;
            }
            Action::TrackToggleArm(..)
                if Self::box_bool(self.handle_track_toggle_arm(a.clone())).await =>
            {
                return true;
            }
            _ => {}
        }
        false
    }
}

impl Engine {
    /// Engine-state inverse for the record-enable toggle (colocated from
    /// `prepare_inverse_actions` in Phase 4).
    pub(crate) fn undo_engine_state_inverse_recording(
        &self,
        action: &Action,
    ) -> Option<Vec<Action>> {
        match action {
            Action::SetRecordEnabled(_) => Some(vec![Action::SetRecordEnabled(
                self.recording.record_enabled,
            )]),
            _ => None,
        }
    }
}
