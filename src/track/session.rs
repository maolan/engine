#[cfg(target_os = "macos")]
use crate::clap::ClapMidiOutputEvent;

use super::*;
use crate::kind::Kind;
use crate::midi::io::MidiEvent;
use std::collections::HashSet;

impl Track {
    pub fn quantize_sample_to_boundary(
        sample: usize,
        quantization: crate::message::LaunchQuantization,
        bpm: f64,
        tsig_num: u16,
        tsig_denom: u16,
        sample_rate: f64,
    ) -> usize {
        use crate::message::LaunchQuantization;
        match quantization {
            LaunchQuantization::None => sample,
            _ => {
                let denom = tsig_denom.max(1) as f64;
                let beats_per_bar = tsig_num.max(1) as f64;
                let samples_per_beat = ((sample_rate * 60.0) / bpm.max(1.0)) * (4.0 / denom);
                if !samples_per_beat.is_finite() || samples_per_beat <= 0.0 {
                    return sample;
                }
                let interval = match quantization {
                    LaunchQuantization::Beat => samples_per_beat,
                    LaunchQuantization::Bar => samples_per_beat * beats_per_bar,
                    LaunchQuantization::TwoBars => samples_per_beat * beats_per_bar * 2.0,
                    LaunchQuantization::FourBars => samples_per_beat * beats_per_bar * 4.0,
                    LaunchQuantization::EightBars => samples_per_beat * beats_per_bar * 8.0,
                    LaunchQuantization::Eighth => samples_per_beat / 2.0,
                    LaunchQuantization::Sixteenth => samples_per_beat / 4.0,
                    LaunchQuantization::ThirtySecond => samples_per_beat / 8.0,
                    LaunchQuantization::SixtyFourth => samples_per_beat / 16.0,
                    LaunchQuantization::None => return sample,
                };
                ((sample as f64 / interval).ceil() * interval).max(0.0) as usize
            }
        }
    }

    pub fn schedule_session_launch(&mut self, launch: PendingSessionLaunch) {
        self.pending_session_launches.push(launch);
    }

    pub fn schedule_session_stop(&mut self, scene_index: usize, stop_at_sample: usize) {
        if let Some(clip) = self
            .playing_session_clips
            .iter_mut()
            .find(|c| c.scene_index == scene_index && c.stop_at_sample.is_none())
        {
            clip.stop_at_sample = Some(stop_at_sample);
        }
    }

    pub fn process_session_clips(&mut self, cycle_start: usize, cycle_end: usize, _frames: usize) {
        let _ = cycle_end;
        let mut activated = Vec::new();
        self.pending_session_launches.retain(|launch| {
            if launch.launch_at_sample <= cycle_start {
                activated.push(launch.clone());
                false
            } else {
                true
            }
        });
        if !activated.is_empty() {
            tracing::info!(
                "process_session_clips track={} cycle_start={} activated={}",
                self.name,
                cycle_start,
                activated.len()
            );
        }
        for launch in activated {
            let exists = match launch.kind {
                Kind::Audio => self.audio.clips.iter().any(|c| c.id == launch.clip_id),
                Kind::MIDI => self.midi.clips.iter().any(|c| c.id == launch.clip_id),
            };
            if !exists {
                tracing::warn!(
                    "Session launch references missing clip {} on track {}",
                    launch.clip_id,
                    self.name
                );
                continue;
            }
            self.playing_session_clips
                .retain(|c| c.scene_index != launch.scene_index);
            self.playing_session_clips.push(PlayingSessionClip {
                scene_index: launch.scene_index,
                clip_id: launch.clip_id,
                kind: launch.kind,
                play_position_samples: 0,
                elapsed_samples: 0,
                loop_enabled: launch.loop_enabled,
                loop_start_samples: launch.loop_start_samples,
                loop_end_samples: launch.loop_end_samples,
                stop_at_sample: None,
                active_midi_notes: HashSet::new(),
            });
        }

        let mut note_offs = Vec::new();
        let mut remove_indices = Vec::new();
        for (i, clip) in self.playing_session_clips.iter().enumerate() {
            let should_stop = if let Some(stop_at) = clip.stop_at_sample {
                stop_at <= cycle_start
            } else {
                false
            };
            if should_stop {
                if clip.kind == Kind::MIDI {
                    for (channel, note) in &clip.active_midi_notes {
                        note_offs.push(MidiEvent::new(
                            0,
                            vec![0x80 | (*channel).min(15), (*note).min(127), 64],
                        ));
                    }
                }
                remove_indices.push(i);
            }
        }
        for i in remove_indices.into_iter().rev() {
            self.playing_session_clips.remove(i);
        }
        self.pending_session_midi_note_offs.extend(note_offs);
    }

    pub(crate) fn mix_session_audio_into_inputs(&mut self) {
        let frames = self
            .audio
            .ins
            .first()
            .map(|audio_in| audio_in.buffer.lock().len())
            .unwrap_or(0);
        if frames == 0 || self.audio.ins.is_empty() {
            return;
        }
        let mut active_clip_plugin_keys = HashSet::new();
        let channel_count = self.audio.ins.len();
        let clip_count = self.playing_session_clips.len();
        let mut position_updates = Vec::new();
        let mut remove_indices = Vec::new();

        for i in 0..clip_count {
            if self.playing_session_clips[i].kind != Kind::Audio {
                continue;
            }
            let clip_id = self.playing_session_clips[i].clip_id.clone();
            let arrangement_clip = match self.audio.clips.iter().find(|c| c.id == clip_id).cloned()
            {
                Some(c) => c,
                None => {
                    remove_indices.push(i);
                    continue;
                }
            };
            let clip_length = arrangement_clip.end.saturating_sub(arrangement_clip.start);
            if clip_length == 0 {
                remove_indices.push(i);
                continue;
            }
            let loop_enabled = self.playing_session_clips[i].loop_enabled;
            let loop_start = self.playing_session_clips[i].loop_start_samples;
            let loop_end = if self.playing_session_clips[i].loop_end_samples == 0 && loop_enabled {
                clip_length
            } else {
                self.playing_session_clips[i]
                    .loop_end_samples
                    .max(loop_start.saturating_add(1))
                    .min(clip_length)
            };
            let mut play_position = self.playing_session_clips[i].play_position_samples;
            let mut elapsed_samples = self.playing_session_clips[i].elapsed_samples;
            let mut out_offset = 0usize;
            let mut rendered_any = false;

            while out_offset < frames {
                if play_position >= clip_length {
                    if loop_enabled && loop_end > loop_start {
                        play_position =
                            loop_start + ((play_position - loop_start) % (loop_end - loop_start));
                    } else {
                        break;
                    }
                }
                let segment_end = if loop_enabled { loop_end } else { clip_length };
                let remaining_segment = segment_end.saturating_sub(play_position);
                if remaining_segment == 0 {
                    break;
                }
                let segment_len = (frames - out_offset).min(remaining_segment);
                let mut session_clip = arrangement_clip.clone();
                session_clip.start = 0;
                session_clip.end = clip_length;

                let processed = match self.render_audio_clip_segment(
                    &session_clip,
                    0,
                    play_position,
                    segment_len,
                    &mut active_clip_plugin_keys,
                ) {
                    Some(p) => p,
                    None => break,
                };
                for ch in 0..channel_count {
                    let in_samples = self.audio.ins[ch].buffer.lock();
                    let src = processed.get(ch).or_else(|| processed.first());
                    if let Some(src) = src {
                        let len = src
                            .len()
                            .min(segment_len)
                            .min(in_samples.len().saturating_sub(out_offset));
                        crate::simd::add_inplace(
                            &mut in_samples[out_offset..out_offset + len],
                            &src[..len],
                        );
                    }
                }
                rendered_any = true;
                play_position += segment_len;
                elapsed_samples += segment_len;
                out_offset += segment_len;
            }

            if rendered_any {
                position_updates.push((i, play_position, elapsed_samples));
            } else if play_position >= clip_length && !loop_enabled {
                remove_indices.push(i);
            } else {
                position_updates.push((i, play_position, elapsed_samples));
            }
        }

        for (idx, pos, elapsed) in position_updates {
            self.playing_session_clips[idx].play_position_samples = pos;
            self.playing_session_clips[idx].elapsed_samples = elapsed;
        }
        for idx in remove_indices.into_iter().rev() {
            self.playing_session_clips.remove(idx);
        }
        self.clip_plugin_tracks
            .retain(|key, _| active_clip_plugin_keys.contains(key));
        let peak = self
            .audio
            .ins
            .iter()
            .map(|in_| {
                in_.buffer
                    .lock()
                    .iter()
                    .map(|&s| s.abs())
                    .fold(0.0_f32, f32::max)
            })
            .fold(0.0_f32, f32::max);
        tracing::debug!(
            "mix_session_audio_into_inputs track={} playing_clips={} input_peak={}",
            self.name,
            self.playing_session_clips.len(),
            peak
        );
    }

    pub(crate) fn mix_session_midi_into_inputs(
        &mut self,
        input_events: &mut [Vec<MidiEvent>],
        frames: usize,
    ) {
        if frames == 0 || input_events.is_empty() {
            return;
        }
        for event in self.pending_session_midi_note_offs.drain(..) {
            for lane in input_events.iter_mut() {
                lane.push(event.clone());
            }
        }

        let clip_count = self.playing_session_clips.len();
        let mut position_updates = Vec::new();
        let mut remove_indices = Vec::new();

        for i in 0..clip_count {
            if self.playing_session_clips[i].kind != Kind::MIDI {
                continue;
            }
            let clip_id = self.playing_session_clips[i].clip_id.clone();
            let arrangement_clip = match self.midi.clips.iter().find(|c| c.id == clip_id).cloned() {
                Some(c) => c,
                None => {
                    remove_indices.push(i);
                    continue;
                }
            };
            let clip_length = arrangement_clip.end.saturating_sub(arrangement_clip.start);
            if clip_length == 0 {
                remove_indices.push(i);
                continue;
            }
            let loop_enabled = self.playing_session_clips[i].loop_enabled;
            let loop_start = self.playing_session_clips[i].loop_start_samples;
            let loop_end = if self.playing_session_clips[i].loop_end_samples == 0 && loop_enabled {
                clip_length
            } else {
                self.playing_session_clips[i]
                    .loop_end_samples
                    .max(loop_start.saturating_add(1))
                    .min(clip_length)
            };
            let mut play_position = self.playing_session_clips[i].play_position_samples;
            let mut elapsed_samples = self.playing_session_clips[i].elapsed_samples;
            let input_lane = arrangement_clip
                .input_channel
                .min(input_events.len().saturating_sub(1));
            let events = match self.midi_clip_cache.get(&arrangement_clip.name).cloned() {
                Some(e) => e,
                None => {
                    remove_indices.push(i);
                    continue;
                }
            };

            let mut out_offset = 0usize;
            let mut emitted_any = false;

            while out_offset < frames {
                if play_position >= clip_length {
                    if loop_enabled && loop_end > loop_start {
                        for (channel, note) in &self.playing_session_clips[i].active_midi_notes {
                            input_events[input_lane].push(MidiEvent::new(
                                out_offset as u32,
                                vec![0x80 | (*channel).min(15), (*note).min(127), 64],
                            ));
                        }
                        self.playing_session_clips[i].active_midi_notes.clear();
                        play_position =
                            loop_start + ((play_position - loop_start) % (loop_end - loop_start));
                    } else {
                        break;
                    }
                }
                let segment_end = if loop_enabled { loop_end } else { clip_length };
                let remaining_segment = segment_end.saturating_sub(play_position);
                if remaining_segment == 0 {
                    break;
                }
                let segment_len = (frames - out_offset).min(remaining_segment);
                let content_start = arrangement_clip.offset.saturating_add(play_position);
                let content_end = content_start.saturating_add(segment_len);

                for (source_sample, data) in events.iter() {
                    if *source_sample < content_start {
                        continue;
                    }
                    if *source_sample >= content_end {
                        break;
                    }
                    let frame = out_offset + (source_sample - content_start);
                    if frame < frames {
                        input_events[input_lane].push(MidiEvent::new(frame as u32, data.clone()));
                        if let Some(&status) = data.first() {
                            let channel = status & 0x0F;
                            if let Some(&note) = data.get(1) {
                                if status & 0xF0 == 0x90 && data.get(2).copied().unwrap_or(0) > 0 {
                                    self.playing_session_clips[i]
                                        .active_midi_notes
                                        .insert((channel, note));
                                } else if status & 0xF0 == 0x80
                                    || (status & 0xF0 == 0x90
                                        && data.get(2).copied().unwrap_or(0) == 0)
                                {
                                    self.playing_session_clips[i]
                                        .active_midi_notes
                                        .remove(&(channel, note));
                                }
                            }
                        }
                    }
                }
                emitted_any = true;
                play_position += segment_len;
                elapsed_samples += segment_len;
                out_offset += segment_len;
            }

            if emitted_any {
                position_updates.push((i, play_position, elapsed_samples));
            } else if play_position >= clip_length && !loop_enabled {
                for (channel, note) in &self.playing_session_clips[i].active_midi_notes {
                    input_events[input_lane].push(MidiEvent::new(
                        frames.saturating_sub(1) as u32,
                        vec![0x80 | (*channel).min(15), (*note).min(127), 64],
                    ));
                }
                remove_indices.push(i);
            } else {
                position_updates.push((i, play_position, elapsed_samples));
            }
        }

        for (idx, pos, elapsed) in position_updates {
            self.playing_session_clips[idx].play_position_samples = pos;
            self.playing_session_clips[idx].elapsed_samples = elapsed;
        }
        for idx in remove_indices.into_iter().rev() {
            self.playing_session_clips.remove(idx);
        }
        for events in input_events.iter_mut() {
            events.sort_by_key(|event| event.frame);
        }
    }
}
