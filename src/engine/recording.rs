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
    /// Discard count for a new take: the not-yet-valid startup region of the
    /// capture stream. Uses the backend's exact first-valid capture frame
    /// when one is reported, otherwise the buffer-size + input-latency
    /// heuristic (JACK2 reports only static capture latency and gives no
    /// validity signal). This is additional to the round-trip latency
    /// segment compensation in `recording_segments_for_cycle`: the
    /// compensation aligns capture with the monitored signal, while the
    /// discard removes the capture frames before even that region is valid.
    pub(crate) fn record_discard_seed(
        capture_frame: Option<i64>,
        transport_position: usize,
        buffer_size: usize,
        input_latency: usize,
    ) -> usize {
        match capture_frame {
            Some(frame) => (frame - transport_position as i64).max(0) as usize,
            None => buffer_size.saturating_add(input_latency),
        }
    }

    /// Seed `discard_remaining_frames` for a take starting at the current
    /// transport position. Runs synchronously on the engine dispatcher.
    pub(crate) fn seed_record_start_discard(&mut self) {
        if self.hw_driver_info.is_some_and(|info| info.fresh_capture) {
            self.recording.discard_remaining_frames = 0;
            return;
        }
        let buffer_size = self.current_cycle_samples();
        let input_latency = self.transport.hw_input_latency_frames;
        let position = self.transport.transport_sample;
        let capture_frame = self.current_capture_frame();
        let seed = Self::record_discard_seed(capture_frame, position, buffer_size, input_latency);
        let cap = self
            .hw_driver_info
            .map(|info| info.capture_buffer_frames)
            .unwrap_or(0);
        self.recording.discard_remaining_frames = Self::clamp_record_discard_seed(seed, cap);
    }

    /// Bound a discard seed by the capture buffer capacity. Backend capture
    /// counters are monotonic since device open and do not follow transport
    /// rewinds, so after the first stop/rewind the raw seed can sit
    /// arbitrarily far ahead of the transport position and would swallow
    /// every later take entirely. Only up to one capture buffer of audio can
    /// still be pending at a take start, so the seed is bounded by it.
    /// `cap == 0` means the capacity is unknown and the seed passes through.
    pub(crate) fn clamp_record_discard_seed(seed: usize, cap: usize) -> usize {
        if cap > 0 { seed.min(cap) } else { seed }
    }

    /// Split one transport segment at the take-start discard: returns
    /// `(skip, start, offset, keep)` — how many captured frames to drop, the
    /// shifted take `start_sample`, the shifted capture buffer offset, and
    /// the kept length. Advancing both start and buffer offset preserves the
    /// alignment of the retained audio.
    pub(crate) fn segment_discard_split(
        discard: usize,
        segment_start: usize,
        frame_offset: usize,
        segment_len: usize,
    ) -> (usize, usize, usize, usize) {
        let skip = discard.min(segment_len);
        (
            skip,
            segment_start.saturating_add(skip),
            frame_offset.saturating_add(skip),
            segment_len - skip,
        )
    }

    pub(crate) fn recording_segments_for_cycle(&self, frames: usize) -> Vec<(usize, usize, usize)> {
        // Capture is input-latency behind the audible timeline, which is
        // itself output-latency behind the render clock. Apply both here so
        // live stripes, punch/loop boundaries and saved audio share a timeline.
        let delay = self
            .transport
            .hw_output_latency_frames
            .saturating_add(self.transport.hw_input_latency_frames);
        let (origin, elapsed) = self
            .transport
            .render_clock
            .unwrap_or((0, self.transport.transport_sample));
        let skip = delay.saturating_sub(elapsed).min(frames);
        let position = self.transport.normalize_transport_sample(
            origin.saturating_add(elapsed.saturating_add(skip).saturating_sub(delay)),
        );
        let segments: Vec<_> = self
            .cycle_segments_at(position, frames - skip)
            .into_iter()
            .map(|(start, end, offset)| (start, end, offset + skip))
            .collect();
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
        let mut discard_leftover: Option<usize> = None;
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
            // The discard counter is global to the take; every armed track
            // shares the same take timeline, so consume it once (from the
            // first track's pass) and reuse the same skips for the rest.
            let mut discard = discard_leftover.unwrap_or(self.recording.discard_remaining_frames);
            for (segment_start, segment_end, frame_offset) in segments {
                let segment_len = segment_end.saturating_sub(segment_start);
                if segment_len == 0 {
                    continue;
                }
                let (skip, start, offset, keep) =
                    Self::segment_discard_split(discard, segment_start, frame_offset, segment_len);
                discard -= skip;

                if audio_channels > 0 && audio_frames > 0 {
                    if !self.recording.audio_recordings.contains_key(name.as_str())
                        && self.recording.discard_remaining_frames == 0
                    {
                        // A new take is starting (punch-in or loop-wrap
                        // restart) and the previous discard is exhausted:
                        // seed a fresh one for this take.
                        self.seed_record_start_discard();
                        discard = self.recording.discard_remaining_frames;
                    }
                    let audio_entry = self
                        .recording
                        .audio_recordings
                        .entry(name.clone())
                        .or_insert_with(|| RecordingSession {
                            start_sample: start,
                            samples: Vec::with_capacity(keep * audio_channels * 2),
                            channels: audio_channels,
                            file_name: Self::next_recording_file_name(name),
                            stripe_peaks: vec![Vec::new(); audio_channels],
                            current_stripe_frames: 0,
                        });
                    if audio_entry.channels != audio_channels {
                        continue;
                    }
                    if let Some(entry) = self.recording.audio_recordings.get_mut(name.as_str())
                        && keep > 0
                    {
                        let from = offset.min(audio_frames);
                        let to = offset.saturating_add(keep).min(audio_frames);
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

                if keep > 0 {
                    if !self.recording.midi_recordings.contains_key(name.as_str())
                        && self.recording.discard_remaining_frames == 0
                    {
                        // MIDI-only fresh take (see the audio branch above).
                        self.seed_record_start_discard();
                        discard = self.recording.discard_remaining_frames;
                    }
                    let entry = self
                        .recording
                        .midi_recordings
                        .entry(name.clone())
                        .or_insert_with(|| MidiRecordingSession {
                            start_sample: start,
                            events: Vec::new(),
                            file_name: Self::next_midi_recording_file_name(name),
                        });
                    let from = offset;
                    let to = offset.saturating_add(keep);
                    for event in &track.rt.record_tap_midi_in {
                        let frame = event.frame as usize;
                        if frame < from || frame >= to {
                            continue;
                        }
                        let abs_sample = start as u64 + (frame - from) as u64;
                        entry.events.push((abs_sample, event.data.clone()));
                    }
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
            if discard_leftover.is_none() {
                discard_leftover = Some(discard);
            }
        }
        if let Some(leftover) = discard_leftover {
            self.recording.discard_remaining_frames = leftover;
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

        // Placement was compensated at capture time. Trimming output latency
        // here would compensate it twice and discard the start of the take.
        let samples = &rec.samples[..];
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
        let start_sample = rec.start_sample;
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
        if enabled && self.transport.playing {
            // Seed synchronously on the engine side: a message would race
            // the prepare generation and the first captured cycles.
            self.seed_record_start_discard();
        }
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

#[cfg(test)]
mod tests {
    use super::Engine;

    #[test]
    fn calibration_splits_rounded_total_equally_with_odd_frame_for_playback() {
        use crate::mtdm::{IoDelayReport, IoDelayStatus};
        let mut report = IoDelayReport {
            status: IoDelayStatus::Resolved,
            delay_frames: 1217.574,
            error: 0.004,
            inverted: false,
            final_report: false,
        };
        assert_eq!(Engine::calibrated_io_latencies(report), Some((609, 609)));
        report.delay_frames = 100.2;
        assert_eq!(Engine::calibrated_io_latencies(report), Some((50, 50)));
        report.delay_frames = 799.0;
        assert_eq!(Engine::calibrated_io_latencies(report), Some((399, 400)));
        report.delay_frames = 1.0;
        assert_eq!(Engine::calibrated_io_latencies(report), Some((0, 1)));
        report.delay_frames = 0.0;
        assert_eq!(Engine::calibrated_io_latencies(report), Some((0, 0)));
        for invalid in [f64::NAN, f64::INFINITY, -1.0, usize::MAX as f64] {
            report.delay_frames = invalid;
            assert_eq!(Engine::calibrated_io_latencies(report), None);
        }
        report.delay_frames = 1217.574;
        for status in [IoDelayStatus::Collecting, IoDelayStatus::BelowThreshold] {
            report.status = status;
            assert_eq!(Engine::calibrated_io_latencies(report), None);
        }
    }

    #[test]
    fn discard_seed_uses_first_valid_capture_frame_when_reported() {
        // 480 frames captured ahead of the transport position: the first 480
        // captured frames of the take are not yet valid and must be dropped.
        assert_eq!(Engine::record_discard_seed(Some(480), 0, 256, 128), 480);
        // The discard never extends before the transport position.
        assert_eq!(Engine::record_discard_seed(Some(100), 480, 256, 128), 0);
    }

    #[test]
    fn discard_seed_falls_back_to_buffer_plus_input_latency() {
        // No validity signal (e.g. JACK2): buffer size + input latency.
        assert_eq!(Engine::record_discard_seed(None, 10_000, 256, 128), 384);
        assert_eq!(Engine::record_discard_seed(None, 0, 0, 0), 0);
    }

    #[test]
    fn discard_seed_is_clamped_to_capture_buffer_capacity() {
        // OSS GETIPTR counts since device open and keeps advancing while the
        // transport is stopped: after a stop/rewind the raw seed is in the
        // millions and would swallow the whole take. At most one capture
        // buffer can still be pending, so the seed is bounded by it.
        let raw = Engine::record_discard_seed(Some(28_800_000), 0, 256, 128);
        assert_eq!(Engine::clamp_record_discard_seed(raw, 8192), 8192);
        // A small legitimate seed passes through untouched.
        assert_eq!(Engine::clamp_record_discard_seed(480, 8192), 480);
        // Unknown capacity (cap 0) disables the clamp.
        assert_eq!(Engine::clamp_record_discard_seed(raw, 0), raw);
    }

    #[test]
    fn segment_discard_split_shifts_start_and_offset_like_the_flush_trim() {
        // 100 frames to drop from a 256-frame segment starting at transport
        // sample 1000 with capture offset 0: the take starts 100 samples
        // later, matching the retained samples in the capture buffer.
        let (skip, start, offset, keep) = Engine::segment_discard_split(100, 1000, 0, 256);
        assert_eq!(skip, 100);
        assert_eq!(start, 1100);
        assert_eq!(offset, 100);
        assert_eq!(keep, 156);
    }

    #[test]
    fn segment_discard_split_caps_at_segment_len() {
        let (skip, start, offset, keep) = Engine::segment_discard_split(500, 1000, 0, 256);
        assert_eq!(skip, 256);
        assert_eq!(start, 1256);
        assert_eq!(offset, 256);
        assert_eq!(keep, 0);
    }

    #[test]
    fn discard_counter_survives_across_segments_within_a_take() {
        // A 384-frame discard across two 256-frame segments (e.g. a loop wrap
        // mid-cycle): the first segment is fully consumed, the second keeps
        // its tail after the counter runs dry.
        let mut discard = 384_usize;
        let (skip, _start, _offset, keep) = Engine::segment_discard_split(discard, 0, 0, 256);
        discard -= skip;
        assert_eq!((skip, keep), (256, 0));
        assert_eq!(discard, 128);
        let (skip, start, _offset, keep) = Engine::segment_discard_split(discard, 256, 256, 256);
        discard -= skip;
        assert_eq!((skip, start, keep), (128, 384, 128));
        assert_eq!(discard, 0);
    }
}
