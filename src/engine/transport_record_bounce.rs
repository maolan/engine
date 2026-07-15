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
    audio::clip::AudioClip,
    kind::Kind,
    message::{
        Action, LaunchQuantization, Message, OfflineAutomationLane, OfflineAutomationPoint,
        SessionAction,
    },
    midi::clip::MIDIClip,
    track::{SessionSlot, Track},
};
use midly::{
    Arena, Format, Header, MetaMessage, Smf, Timing, TrackEvent, TrackEventKind,
    live::LiveEvent,
    num::{u15, u24, u28},
};
use std::{
    fs::File,
    path::Path,
    sync::{Arc, atomic::AtomicBool},
    time::{SystemTime, UNIX_EPOCH},
};
use tracing::error;

impl Engine {
    pub(crate) async fn ensure_metronome_track(&mut self) {
        if self.state.lock().tracks.contains_key(Self::METRONOME_TRACK) {
            return;
        }
        let (cycle_samples, sample_rate_hz, output_channels): (usize, f64, usize) =
            if let Some(info) = self.hw_driver_info {
                (
                    info.cycle_samples,
                    info.sample_rate as f64,
                    info.output_channels,
                )
            } else {
                #[cfg(unix)]
                {
                    if let Some(jack) = &self.jack_runtime {
                        (
                            jack.buffer_size,
                            jack.sample_rate as f64,
                            jack.audio_outs().len(),
                        )
                    } else {
                        return;
                    }
                }
                #[cfg(not(unix))]
                {
                    return;
                }
            };
        if output_channels == 0 {
            return;
        }
        self.state.lock().tracks.insert(
            Self::METRONOME_TRACK.to_string(),
            Arc::new(Track::new(
                Self::METRONOME_TRACK.to_string(),
                0,
                1,
                0,
                0,
                cycle_samples.max(1),
                sample_rate_hz.max(1.0),
            )),
        );
        if let Some(track) = self.state.lock().tracks.get(Self::METRONOME_TRACK).cloned() {
            track.lock().set_level(Self::METRONOME_DEFAULT_LEVEL_DB);
            track.lock().set_metronome_enabled(self.metronome_enabled);
        }
        self.notify_clients(Ok(Action::AddTrack {
            name: Self::METRONOME_TRACK.to_string(),
            audio_ins: 0,
            midi_ins: 0,
            audio_outs: 1,
            midi_outs: 0,
            folder: false,
        }))
        .await;
        self.notify_clients(Ok(Action::TrackLevel(
            Self::METRONOME_TRACK.to_string(),
            Self::METRONOME_DEFAULT_LEVEL_DB,
        )))
        .await;
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
        if !self.playing || !self.record_enabled {
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
                    let audio_entry =
                        self.audio_recordings
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
                    if let Some(entry) = self.audio_recordings.get_mut(name.as_str()) {
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

                let entry = self.midi_recordings.entry(name.clone()).or_insert_with(|| {
                    MidiRecordingSession {
                        start_sample: segment_start,
                        events: Vec::new(),
                        file_name: Self::next_midi_recording_file_name(name),
                    }
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

                if self.punch_enabled
                    && let Some((_, punch_end)) = self.punch_range_samples
                    && segment_end == punch_end
                {
                    if let Some(done) = self.audio_recordings.remove(name.as_str()) {
                        self.completed_audio_recordings.push((name.clone(), done));
                    }
                    if let Some(done) = self.midi_recordings.remove(name.as_str()) {
                        self.completed_midi_recordings.push((name.clone(), done));
                    }
                } else if self.loop_enabled
                    && let Some((_, loop_end)) = self.loop_range_samples
                    && segment_end == loop_end
                {
                    if let Some(done) = self.audio_recordings.remove(name.as_str()) {
                        self.completed_audio_recordings.push((name.clone(), done));
                    }
                    if let Some(done) = self.midi_recordings.remove(name.as_str()) {
                        self.completed_midi_recordings.push((name.clone(), done));
                    }
                }
            }
        }
    }

    pub(crate) async fn flush_completed_recordings(&mut self) {
        if self.completed_audio_recordings.is_empty() && self.completed_midi_recordings.is_empty() {
            return;
        }
        let Some(audio_dir) = self.session_audio_dir() else {
            self.completed_audio_recordings.clear();
            self.completed_midi_recordings.clear();
            return;
        };
        let Some(midi_dir) = self.session_midi_dir() else {
            self.completed_audio_recordings.clear();
            self.completed_midi_recordings.clear();
            return;
        };
        if std::fs::create_dir_all(&audio_dir).is_err()
            || std::fs::create_dir_all(&midi_dir).is_err()
        {
            self.completed_audio_recordings.clear();
            self.completed_midi_recordings.clear();
            return;
        }
        let rate = self
            .hw_driver_info
            .map(|info| info.sample_rate)
            .unwrap_or(48_000);
        let completed_audio = std::mem::take(&mut self.completed_audio_recordings);
        for (track_name, rec) in completed_audio {
            self.flush_recording_entry(&audio_dir, rate, track_name, rec)
                .await;
        }
        let completed_midi = std::mem::take(&mut self.completed_midi_recordings);
        for (track_name, rec) in completed_midi {
            self.flush_midi_recording_entry(&midi_dir, rate as u32, track_name, rec)
                .await;
        }
    }

    pub(crate) async fn flush_recordings(&mut self) {
        let Some(audio_dir) = self.session_audio_dir() else {
            if !self.audio_recordings.is_empty()
                || !self.midi_recordings.is_empty()
                || !self.completed_audio_recordings.is_empty()
                || !self.completed_midi_recordings.is_empty()
            {
                self.notify_clients(Err("Recording stopped: session path is not set".to_string()))
                    .await;
            }
            self.audio_recordings.clear();
            self.midi_recordings.clear();
            self.completed_audio_recordings.clear();
            self.completed_midi_recordings.clear();
            return;
        };
        if std::fs::create_dir_all(&audio_dir).is_err() {
            self.notify_clients(Err(format!(
                "Recording stopped: failed to create audio directory {}",
                audio_dir.display()
            )))
            .await;
            self.audio_recordings.clear();
            self.midi_recordings.clear();
            self.completed_audio_recordings.clear();
            self.completed_midi_recordings.clear();
            return;
        }
        let Some(midi_dir) = self.session_midi_dir() else {
            self.audio_recordings.clear();
            self.midi_recordings.clear();
            self.completed_audio_recordings.clear();
            self.completed_midi_recordings.clear();
            return;
        };
        if std::fs::create_dir_all(&midi_dir).is_err() {
            self.audio_recordings.clear();
            self.midi_recordings.clear();
            self.completed_audio_recordings.clear();
            self.completed_midi_recordings.clear();
            return;
        }
        let rate = self
            .hw_driver_info
            .map(|info| info.sample_rate)
            .unwrap_or(48_000);
        let completed_audio = std::mem::take(&mut self.completed_audio_recordings);
        for (track_name, rec) in completed_audio {
            self.flush_recording_entry(&audio_dir, rate, track_name, rec)
                .await;
        }
        let completed_midi = std::mem::take(&mut self.completed_midi_recordings);
        for (track_name, rec) in completed_midi {
            self.flush_midi_recording_entry(&midi_dir, rate as u32, track_name, rec)
                .await;
        }
        let recordings = std::mem::take(&mut self.audio_recordings);
        for (track_name, rec) in recordings {
            self.flush_recording_entry(&audio_dir, rate, track_name, rec)
                .await;
        }
        let midi_recordings = std::mem::take(&mut self.midi_recordings);
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

        let trim_frames = self.hw_output_latency_frames;
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
            self.audio_recordings.remove(track_name);
            self.midi_recordings.remove(track_name);
            self.completed_audio_recordings
                .retain(|(name, _)| name != track_name);
            self.completed_midi_recordings
                .retain(|(name, _)| name != track_name);
            return;
        };
        let Some(midi_dir) = self.session_midi_dir() else {
            self.audio_recordings.remove(track_name);
            self.midi_recordings.remove(track_name);
            self.completed_audio_recordings
                .retain(|(name, _)| name != track_name);
            self.completed_midi_recordings
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
        while i < self.completed_audio_recordings.len() {
            if self.completed_audio_recordings[i].0 == track_name {
                let (name, rec) = self.completed_audio_recordings.remove(i);
                self.flush_recording_entry(&audio_dir, rate, name, rec)
                    .await;
            } else {
                i += 1;
            }
        }
        let mut j = 0;
        while j < self.completed_midi_recordings.len() {
            if self.completed_midi_recordings[j].0 == track_name {
                let (name, rec) = self.completed_midi_recordings.remove(j);
                self.flush_midi_recording_entry(&midi_dir, rate as u32, name, rec)
                    .await;
            } else {
                j += 1;
            }
        }

        let Some(rec) = self.audio_recordings.remove(track_name) else {
            if let Some(mrec) = self.midi_recordings.remove(track_name) {
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
        if let Some(mrec) = self.midi_recordings.remove(track_name) {
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

    pub(crate) fn parse_automation_lanes(value: &serde_json::Value) -> Vec<OfflineAutomationLane> {
        parse_automation_lanes(value)
    }

    /// Drop a queued scene's pending launches and revert the stops it
    /// scheduled. The queue marker is taken from `queue`, which ends up
    /// `None` whether or not a queue was stored.
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

    pub(crate) async fn handle_session_action(&mut self, action: SessionAction) {
        let sample_rate = self.sample_rate();
        let bpm = self.tempo_bpm;
        let tsig_num = self.tsig_num;
        let tsig_denom = self.tsig_denom;
        let session_active = self.session_clip_playback_enabled && self.playing;
        let quantize_reference_sample = if session_active {
            self.session_transport_sample
        } else {
            self.transport_sample
        };
        let quantize = |sample: usize, quantization: LaunchQuantization| -> usize {
            if !self.transport_running && !session_active {
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
                tracing::info!(
                    "Session launch track={} scene={} clip_id={} kind={:?} launch_at={} transport_running={}",
                    track_name,
                    scene_index,
                    clip_id,
                    kind,
                    launch_at_sample,
                    self.transport_running
                );
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
                self.session_current_scene = Some(scene_index);
                let tracks: Vec<_> = self
                    .state_snapshot
                    .load_full()
                    .tracks
                    .values()
                    .cloned()
                    .collect();
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
            SessionAction::QueueScene { scene_index } => {
                let tracks: Vec<_> = self
                    .state_snapshot
                    .load_full()
                    .tracks
                    .values()
                    .cloned()
                    .collect();

                // A previously queued scene is replaced by the new one.
                Self::cancel_session_scene_queue(&mut self.session_scene_queue, &tracks);

                // Fire when the longest currently playing clip finishes its
                // current pass; immediately when nothing is playing.
                let mut max_remaining = 0usize;
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
                let base_sample = if session_active {
                    self.session_transport_sample
                } else {
                    self.transport_sample
                };
                let launch_at_sample = base_sample.saturating_add(max_remaining);

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
                        .or(self.session_current_scene);
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
                    self.session_scene_queue = Some((scene_index, launch_at_sample));
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
            #[cfg(all(unix, not(target_os = "macos")))]
            let mut lv2_instance_count = 0usize;
            #[cfg(not(all(unix, not(target_os = "macos"))))]
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
                #[cfg(all(unix, not(target_os = "macos")))]
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
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        let _lv2_instance_count = lv2_instance_count;
        let pending_hw_midi_events = self.pending_hw_midi_events.len()
            + self
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
        self.notify_clients(Ok(Action::SessionDiagnosticsReport {
            track_count,
            frozen_track_count,
            audio_clip_count,
            midi_clip_count,
            #[cfg(all(unix, not(target_os = "macos")))]
            lv2_instance_count,
            vst3_instance_count,
            clap_instance_count,
            pending_requests: self.pending_requests.len(),
            workers_total: self.workers.len(),
            workers_ready: self.ready_workers.len(),
            pending_hw_midi_events,
            playing: self.playing,
            transport_running: self.transport_running,
            transport_sample: self.transport_sample,
            tempo_bpm: self.tempo_bpm,
            sample_rate_hz,
            cycle_samples,
        }))
        .await;
    }

    pub(crate) async fn handle_track_offline_bounce(&mut self, action: Action) {
        let Action::TrackOfflineBounce {
            track_name,
            output_path,
            start_sample,
            length_samples,
            automation_lanes,
            apply_fader,
        } = action
        else {
            return;
        };
        if self.offline_bounce_jobs.contains_key(&track_name) {
            self.notify_clients(Err(format!(
                "Offline bounce for track '{}' is already in progress",
                track_name
            )))
            .await;
            return;
        }
        if let Err(e) = self.track_handle_or_err(&track_name) {
            self.notify_clients(Err(e)).await;
            return;
        }
        if length_samples == 0 {
            self.notify_clients(Err(format!(
                "Track '{}' has no renderable content for offline bounce",
                track_name
            )))
            .await;
            return;
        }
        let Some(worker_index) = self.take_ready_worker_index() else {
            self.pending_requests
                .push_front(Action::TrackOfflineBounce {
                    track_name,
                    output_path,
                    start_sample,
                    length_samples,
                    automation_lanes,
                    apply_fader,
                });
            return;
        };
        let cancel = Arc::new(AtomicBool::new(false));
        self.offline_bounce_jobs.insert(
            track_name.clone(),
            OfflineBounceJob {
                cancel: cancel.clone(),
            },
        );
        let job = crate::message::OfflineBounceWork {
            state: self.state_snapshot.load_full(),
            track_name,
            output_path,
            start_sample,
            length_samples,
            tempo_bpm: self.tempo_bpm,
            tsig_num: self.tsig_num,
            tsig_denom: self.tsig_denom,
            automation_lanes,
            cancel,
            apply_fader,
        };
        if self.executor.cycle_complete() {
            self.send_bounce_job(worker_index, job).await;
        } else {
            // A plan cycle is in flight; starting the bounce now would race
            // its workers on the live track bodies. The job is registered,
            // so new cycles are suspended from now on, and the work is
            // handed over when the cycle completes (on_all_tracks_finished).
            self.pending_bounce_starts.push((worker_index, job));
        }
    }

    pub(crate) async fn handle_play(&mut self, action: Action) -> bool {
        let Action::Play = action else {
            return false;
        };

        tracing::debug!(
            "Action::Play pressed, transport_sample={}",
            self.transport_sample
        );
        self.playing = true;
        self.transport_running = true;
        self.transport_restart_pending = true;
        self.notified_loop_wrap_sample = None;
        self.publish_transport_snapshot();
        self.set_hw_playing(true).await;
        #[cfg(unix)]
        if let Some(jack) = &self.jack_runtime
            && let Err(e) = jack.transport_start()
        {
            self.notify_clients(Err(e)).await;
        }
        self.notify_clients(Ok(Action::TransportPosition(self.transport_sample)))
            .await;
        self.preload_track_clips().await;
        {
            let echoes = self.apply_modulators(self.active_transport_sample());
            for action in echoes {
                self.notify_clients(Ok(action)).await;
            }
        }
        if !self.awaiting_hwfinished && !self.handling_hwfinished {
            let completed = self.start_plan_cycle().await;
            if completed {
                self.transport_restart_pending = false;
            }
        }

        false
    }

    pub(crate) async fn handle_pause(&mut self, action: Action) -> bool {
        let Action::Pause = action else {
            return false;
        };

        self.clip_playback_enabled = false;
        self.session_clip_playback_enabled = false;
        for track in self.state_snapshot.load_full().tracks.values() {
            let mut t = track.lock();
            t.set_clip_playback_enabled(false);
            t.set_session_clip_playback_enabled(false);
        }
        self.transport_running = false;
        self.publish_transport_snapshot();
        if !self.playing {
            self.playing = true;
            self.transport_restart_pending = true;
            self.notified_loop_wrap_sample = None;
            self.publish_transport_snapshot();
            self.set_hw_playing(true).await;
            #[cfg(unix)]
            if let Some(jack) = &self.jack_runtime
                && let Err(e) = jack.transport_start()
            {
                self.notify_clients(Err(e)).await;
            }
            self.preload_track_clips().await;
            if !self.awaiting_hwfinished && !self.handling_hwfinished {
                let completed = self.start_plan_cycle().await;
                if completed {
                    self.transport_restart_pending = false;
                }
            }
        }
        self.notify_clients(Ok(Action::Pause)).await;
        self.notify_clients(Ok(Action::TransportPosition(self.transport_sample)))
            .await;

        false
    }

    pub(crate) async fn handle_stop(&mut self, action: Action) -> bool {
        let Action::Stop = action else {
            return false;
        };

        self.playing = false;
        self.transport_running = false;
        self.transport_panic_flush_pending = false;
        self.transport_restart_pending = false;
        self.notified_loop_wrap_sample = None;
        self.clip_playback_enabled = true;
        self.session_clip_playback_enabled = false;
        self.session_transport_sample = 0;
        self.session_scene_queue = None;
        self.session_current_scene = None;
        for track in self.state_snapshot.load_full().tracks.values() {
            let mut t = track.lock();
            t.set_clip_playback_enabled(true);
            t.set_session_clip_playback_enabled(false);
            t.stop_all_session_clips_immediate();
        }
        self.publish_transport_snapshot();
        self.set_hw_playing(false).await;
        #[cfg(unix)]
        if let Some(jack) = &self.jack_runtime
            && let Err(e) = jack.transport_stop()
        {
            self.notify_clients(Err(e)).await;
        }
        let panic_events = self.note_off_events_for_all_active_tracks();
        if let Some(worker) = &self.hw_worker {
            if !panic_events.is_empty()
                && let Err(e) = worker.tx.send(Message::HWMidiOutEvents(panic_events)).await
            {
                error!("Error sending stop MIDI panic events {e}");
            }
        } else {
            self.pending_hw_midi_out_events_by_device
                .extend(panic_events);
        }
        self.reset_meters_after_stop();
        self.flush_recordings().await;
        self.notify_clients(Ok(Action::TransportPosition(self.transport_sample)))
            .await;

        false
    }

    pub(crate) async fn handle_session_play(&mut self, action: Action) -> bool {
        let Action::SessionPlay = action else {
            return false;
        };

        tracing::info!(
            "Action::SessionPlay pressed, transport_sample={} playing={} transport_running={} clip_enabled={} session_enabled={}",
            self.transport_sample,
            self.playing,
            self.transport_running,
            self.clip_playback_enabled,
            self.session_clip_playback_enabled
        );
        self.playing = true;
        self.transport_running = false;
        self.transport_restart_pending = true;
        self.notified_loop_wrap_sample = None;
        self.clip_playback_enabled = false;
        self.session_clip_playback_enabled = true;
        self.session_transport_sample = 0;
        self.publish_transport_snapshot();
        self.set_hw_playing(true).await;
        #[cfg(unix)]
        if let Some(jack) = &self.jack_runtime
            && let Err(e) = jack.transport_start()
        {
            self.notify_clients(Err(e)).await;
        }
        self.notify_clients(Ok(Action::TransportPosition(self.transport_sample)))
            .await;
        self.preload_track_clips().await;
        {
            let echoes = self.apply_modulators(self.active_transport_sample());
            for action in echoes {
                self.notify_clients(Ok(action)).await;
            }
        }
        if !self.awaiting_hwfinished && !self.handling_hwfinished {
            let completed = self.start_plan_cycle().await;
            if completed {
                self.transport_restart_pending = false;
            }
        }

        false
    }

    pub(crate) async fn handle_transport_position(&mut self, a: Action) -> bool {
        let Action::TransportPosition(sample) = a else {
            return false;
        };

        self.transport_sample = self.normalize_transport_sample(sample);
        self.notified_loop_wrap_sample = None;
        self.publish_transport_snapshot();
        {
            let echoes = self.apply_modulators(self.active_transport_sample());
            for action in echoes {
                self.notify_clients(Ok(action)).await;
            }
        }
        #[cfg(unix)]
        if let Some(jack) = &self.jack_runtime
            && let Err(e) = jack.transport_locate(self.transport_sample)
        {
            self.notify_clients(Err(e)).await;
        }
        if self.playing {
            self.transport_restart_pending = true;
            self.transport_panic_flush_pending = self.hw_worker.is_some();
            self.clear_hw_midi_output_state(true).await;
            // The running cycle (if any) finishes naturally — at most one
            // block at the old position; the next dispatch reads the new
            // transport sample.
            if !self.awaiting_hwfinished && !self.handling_hwfinished {
                let completed = self.start_plan_cycle().await;
                if completed {
                    self.transport_restart_pending = false;
                }
            }
        }

        false
    }

    pub(crate) async fn handle_track_automation_insert_point(&mut self, a: Action) -> bool {
        let Action::TrackAutomationInsertPoint {
            ref track_name,
            ref target,
            sample,
            value,
        } = a
        else {
            return false;
        };

        if let Some(track) = self
            .state_snapshot
            .load_full()
            .tracks
            .get(track_name)
            .cloned()
        {
            let mut track = track.lock();
            let mut lanes = Self::parse_automation_lanes(&track.automation_lanes);
            let lane = match lanes.iter_mut().find(|lane| lane.target == *target) {
                Some(lane) => lane,
                None => {
                    lanes.push(OfflineAutomationLane {
                        target: target.clone(),
                        visible: true,
                        points: vec![],
                    });
                    lanes.last_mut().expect("just pushed")
                }
            };
            if let Some(point) = lane.points.iter_mut().find(|p| p.sample == sample) {
                point.value = value;
            } else {
                lane.points.push(OfflineAutomationPoint { sample, value });
                lane.points.sort_unstable_by_key(|p| p.sample);
            }
            track.automation_lanes = serde_json::to_value(&lanes).unwrap_or_default();
        }

        false
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

    pub(crate) async fn handle_set_loop_range(&mut self, a: Action) -> bool {
        let Action::SetLoopRange(range) = a else {
            return false;
        };

        self.loop_range_samples = range.and_then(|(start, end)| {
            if end > start {
                Some((start, end))
            } else {
                None
            }
        });
        self.loop_enabled = self.loop_range_samples.is_some();
        self.notified_loop_wrap_sample = None;
        if self.loop_enabled
            && let Some((loop_start, loop_end)) = self.loop_range_samples
            && self.transport_sample >= loop_end
        {
            self.transport_sample = loop_start;
            self.notify_clients(Ok(Action::TransportPosition(self.transport_sample)))
                .await;
        }

        false
    }

    pub(crate) async fn handle_track_automation_toggle_lane(&mut self, a: Action) -> bool {
        let Action::TrackAutomationToggleLane {
            ref track_name,
            ref target,
        } = a
        else {
            return false;
        };

        if let Some(track) = self
            .state_snapshot
            .load_full()
            .tracks
            .get(track_name)
            .cloned()
        {
            let mut track = track.lock();
            let mut lanes = Self::parse_automation_lanes(&track.automation_lanes);
            if let Some(lane) = lanes.iter_mut().find(|lane| lane.target == *target) {
                lane.visible = !lane.visible;
            } else {
                lanes.push(OfflineAutomationLane {
                    target: target.clone(),
                    visible: true,
                    points: vec![],
                });
            }
            track.automation_lanes = serde_json::to_value(&lanes).unwrap_or_default();
        }

        false
    }

    pub(crate) async fn handle_set_record_enabled(&mut self, a: Action) -> bool {
        let Action::SetRecordEnabled(enabled) = a else {
            return false;
        };

        self.record_enabled = enabled;
        if !enabled {
            if self.awaiting_hwfinished {
                self.append_recorded_cycle();
            }
            self.flush_recordings().await;
        } else if self.session_dir.is_none() {
            self.notify_clients(Err(
                "Recording enabled but session path is not set".to_string()
            ))
            .await;
        }

        false
    }

    pub(crate) async fn handle_track_automation_delete_point(&mut self, a: Action) -> bool {
        let Action::TrackAutomationDeletePoint {
            ref track_name,
            ref target,
            sample,
        } = a
        else {
            return false;
        };

        if let Some(track) = self
            .state_snapshot
            .load_full()
            .tracks
            .get(track_name)
            .cloned()
        {
            let mut track = track.lock();
            let mut lanes = Self::parse_automation_lanes(&track.automation_lanes);
            if let Some(lane) = lanes.iter_mut().find(|lane| lane.target == *target) {
                lane.points.retain(|point| point.sample != sample);
            }
            track.automation_lanes = serde_json::to_value(&lanes).unwrap_or_default();
        }

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
