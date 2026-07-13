#[cfg(target_os = "macos")]
use crate::clap::ClapMidiOutputEvent;
use crate::message::{PluginGraphNode, PluginKind};
#[cfg(unix)]
use crate::rubberband::LivePitchShifter;

use super::*;
use crate::{audio::io::AudioIO, midi::io::MidiEvent};
use midly::{MetaMessage, Smf, Timing, TrackEventKind, live::LiveEvent};
use serde_json::Value;
use std::{collections::HashSet, path::Path, sync::Arc};

impl TrackData {
    pub(crate) fn invalidate_midi_clip_cache(&mut self, clip_name: &str) {
        self.rt.midi_clip_cache.remove(clip_name);
    }

    pub(crate) fn load_audio_clip_buffer(path: &Path) -> Option<AudioClipBuffer> {
        let open_started = std::time::Instant::now();
        let (samples, channels, _sample_rate) =
            crate::audio_codec::decode_audio_to_f32_interleaved_preferring_wav(path).ok()?;
        let open_elapsed = open_started.elapsed().as_secs_f64() * 1000.0;
        let read_started = std::time::Instant::now();
        let read_elapsed = read_started.elapsed().as_secs_f64() * 1000.0;
        if open_elapsed > 20.0 || read_elapsed > 20.0 {
            tracing::warn!(
                "Slow audio load '{}' open={:.1}ms read={:.1}ms samples={} channels={}",
                path.display(),
                open_elapsed,
                read_elapsed,
                samples.len(),
                channels
            );
        }
        if samples.is_empty() {
            return None;
        }
        Some(AudioClipBuffer { channels, samples })
    }

    pub(crate) fn clip_buffer(&mut self, clip_name: &str) -> Option<Arc<AudioClipBuffer>> {
        if let Some(cached) = self.rt.audio_clip_cache.get(clip_name) {
            return Some(cached.clone());
        }
        let path = self.resolve_clip_path(clip_name);
        let load_started = std::time::Instant::now();
        let loaded = Self::load_audio_clip_buffer(&path)?;
        let elapsed = load_started.elapsed().as_secs_f64() * 1000.0;
        if elapsed > 20.0 {
            tracing::warn!(
                "Slow load_audio_clip_buffer for '{}' ({}) took {:.1}ms",
                clip_name,
                path.display(),
                elapsed
            );
        }
        let loaded = Arc::new(loaded);
        self.rt
            .audio_clip_cache
            .insert(clip_name.to_string(), loaded.clone());
        Some(loaded)
    }

    pub(crate) fn clip_playback_name(clip: &crate::audio::clip::AudioClip) -> &str {
        if let Some(preview_name) = clip.pitch_correction_preview_name.as_deref() {
            preview_name
        } else if !clip.pitch_correction_points.is_empty() {
            clip.pitch_correction_source_name
                .as_deref()
                .unwrap_or(&clip.name)
        } else {
            clip.name.as_str()
        }
    }

    #[cfg(unix)]
    pub(crate) fn clip_pitch_key(clip: &crate::audio::clip::AudioClip) -> String {
        format!(
            "{}:{}:{}:{}:{}",
            clip.name, clip.start, clip.end, clip.offset, clip.input_channel
        )
    }

    pub(crate) fn clip_plugin_runtime_key(
        clip: &crate::audio::clip::AudioClip,
        input_count: usize,
        output_count: usize,
    ) -> String {
        let graph = clip
            .plugin_graph_json
            .as_ref()
            .and_then(|value| serde_json::to_string(value).ok())
            .unwrap_or_default();
        format!(
            "{}:{}:{}:{}:{}:{}:{}:{}",
            clip.name,
            clip.start,
            clip.end,
            clip.offset,
            clip.input_channel,
            input_count,
            output_count,
            graph
        )
    }

    pub(crate) fn clip_plugin_runtime_node_from_json(
        value: &Value,
        runtime_nodes: &[PluginGraphNode],
    ) -> Option<PluginGraphNode> {
        let kind = value.get("type")?.as_str()?;
        match kind {
            "track_input" => Some(PluginGraphNode::TrackInput),
            "track_output" => Some(PluginGraphNode::TrackOutput),
            #[cfg(all(unix, not(target_os = "macos")))]
            "plugin" => runtime_nodes
                .get(value.get("plugin_index")?.as_u64()? as usize)
                .and_then(|node| {
                    matches!(node, PluginGraphNode::Lv2PluginInstance(_)).then(|| node.clone())
                }),
            #[cfg(not(all(unix, not(target_os = "macos"))))]
            "plugin" => None,
            "vst3_plugin" => runtime_nodes
                .get(value.get("plugin_index")?.as_u64()? as usize)
                .and_then(|node| {
                    matches!(node, PluginGraphNode::Vst3PluginInstance(_)).then(|| node.clone())
                }),
            "clap_plugin" => runtime_nodes
                .get(value.get("plugin_index")?.as_u64()? as usize)
                .and_then(|node| {
                    matches!(node, PluginGraphNode::ClapPluginInstance(_)).then(|| node.clone())
                }),
            _ => None,
        }
    }

    pub(crate) fn clip_graph_uses_plugin_runtime(graph: &Value) -> bool {
        graph
            .get("plugins")
            .and_then(Value::as_array)
            .is_some_and(|plugins| !plugins.is_empty())
    }

    pub(crate) fn clip_graph_track_io_node(value: &Value) -> Option<bool> {
        let kind = if let Some(kind) = value.get("type").and_then(Value::as_str) {
            kind
        } else {
            value.as_str()?
        };
        match kind {
            "track_input" | "TrackInput" => Some(true),
            "track_output" | "TrackOutput" => Some(false),
            _ => None,
        }
    }

    pub(crate) fn process_direct_clip_graph(
        graph: &Value,
        input_blocks: &[Vec<f32>],
        request_len: usize,
    ) -> Vec<Vec<f32>> {
        let output_count = input_blocks.len().max(1);
        let mut outputs = vec![vec![0.0; request_len]; output_count];
        let Some(connections) = graph.get("connections").and_then(Value::as_array) else {
            return outputs;
        };
        for connection in connections {
            let Some(from_is_input) =
                Self::clip_graph_track_io_node(connection.get("from_node").unwrap_or(&Value::Null))
            else {
                continue;
            };
            let Some(to_is_input) =
                Self::clip_graph_track_io_node(connection.get("to_node").unwrap_or(&Value::Null))
            else {
                continue;
            };
            let from_port = connection
                .get("from_port")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
            let to_port = connection
                .get("to_port")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
            if !from_is_input || to_is_input {
                continue;
            }
            match connection.get("kind").and_then(Value::as_str) {
                Some("audio") | Some("Audio") => {}
                _ => continue,
            }
            let Some(source) = input_blocks.get(from_port) else {
                continue;
            };
            let Some(target) = outputs.get_mut(to_port) else {
                continue;
            };
            let len = request_len.min(source.len()).min(target.len());
            crate::simd::add_inplace(&mut target[..len], &source[..len]);
        }
        outputs
    }

    pub(crate) fn build_clip_plugin_runtime(
        &self,
        clip: &crate::audio::clip::AudioClip,
        channels: usize,
        buffer_size: usize,
    ) -> Result<ClipPluginRuntime, String> {
        let input_sources = (0..channels.max(1))
            .map(|_| Arc::new(AudioIO::new(buffer_size.max(1))))
            .collect::<Vec<_>>();
        let outputs = (0..channels.max(1))
            .map(|_| Arc::new(AudioIO::new(buffer_size.max(1))))
            .collect::<Vec<_>>();
        let mut runtime = ClipPluginRuntime {
            input_sources,
            outputs,
            clap_plugins: Vec::new(),
            vst3_plugins: Vec::new(),
            #[cfg(all(unix, not(target_os = "macos")))]
            lv2_plugins: Vec::new(),
            plugin_midi_connections: Vec::new(),
        };

        let Some(graph) = clip.plugin_graph_json.as_ref() else {
            return Ok(runtime);
        };

        let mut runtime_nodes = Vec::new();
        let mut next_plugin_instance_id = 0usize;

        if let Some(plugins) = graph.get("plugins").and_then(Value::as_array) {
            for plugin in plugins {
                let Some(uri) = plugin.get("uri").and_then(Value::as_str) else {
                    continue;
                };
                let format = plugin
                    .get("format")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let id = next_plugin_instance_id;
                next_plugin_instance_id += 1;
                match format {
                    "CLAP" | "clap" => {
                        let host_binary = match crate::plugins::ipc::find_plugin_host_binary() {
                            Some(b) => b,
                            None => continue,
                        };
                        let plugin_spec = match crate::plugins::resolve_plugin_identifier(
                            PluginKind::Clap,
                            uri,
                        ) {
                            Ok(p) => p,
                            Err(_) => continue,
                        };
                        let processor = match crate::clap_proc::ClapProcessor::new(
                            self.sample_rate,
                            buffer_size,
                            &plugin_spec,
                            channels.max(1),
                            channels.max(1),
                            host_binary,
                        ) {
                            Ok(p) => p,
                            Err(_) => continue,
                        };
                        runtime
                            .clap_plugins
                            .push(ClapInstance::new(id, Arc::new(processor)));
                        runtime_nodes.push(PluginGraphNode::ClapPluginInstance(id));
                    }
                    "VST3" | "vst3" => {
                        let host_binary = match crate::plugins::ipc::find_plugin_host_binary() {
                            Some(b) => b,
                            None => continue,
                        };
                        let plugin_path = match crate::plugins::resolve_plugin_identifier(
                            PluginKind::Vst3,
                            uri,
                        ) {
                            Ok(p) => p,
                            Err(_) => continue,
                        };
                        let processor = match crate::vst3_proc::Vst3Processor::new(
                            self.sample_rate,
                            buffer_size,
                            &plugin_path,
                            uri,
                            channels.max(1),
                            channels.max(1),
                            host_binary,
                        ) {
                            Ok(p) => p,
                            Err(_) => continue,
                        };
                        runtime
                            .vst3_plugins
                            .push(Vst3Instance::new(id, Arc::new(processor)));
                        runtime_nodes.push(PluginGraphNode::Vst3PluginInstance(id));
                    }
                    #[cfg(all(unix, not(target_os = "macos")))]
                    "LV2" | "lv2" => {
                        let host_binary = match crate::plugins::ipc::find_plugin_host_binary() {
                            Some(b) => b,
                            None => continue,
                        };
                        let processor = match crate::lv2_proc::Lv2Processor::new(
                            self.sample_rate,
                            buffer_size,
                            uri,
                            channels.max(1),
                            channels.max(1),
                            host_binary,
                        ) {
                            Ok(p) => p,
                            Err(_) => continue,
                        };
                        runtime
                            .lv2_plugins
                            .push(Lv2Instance::new(id, Arc::new(processor)));
                        #[cfg(all(unix, not(target_os = "macos")))]
                        runtime_nodes.push(PluginGraphNode::Lv2PluginInstance(id));
                    }
                    _ => {}
                }
            }
        }

        if let Some(connections) = graph.get("connections").and_then(Value::as_array) {
            for connection in connections {
                let Some(from_node) = Self::clip_plugin_runtime_node_from_json(
                    connection.get("from_node").unwrap_or(&Value::Null),
                    &runtime_nodes,
                ) else {
                    continue;
                };
                let Some(to_node) = Self::clip_plugin_runtime_node_from_json(
                    connection.get("to_node").unwrap_or(&Value::Null),
                    &runtime_nodes,
                ) else {
                    continue;
                };
                let from_port = connection
                    .get("from_port")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                let to_port = connection
                    .get("to_port")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                match connection.get("kind").and_then(Value::as_str) {
                    Some("audio") => {
                        runtime.connect_audio(from_node, from_port, to_node, to_port)?;
                    }
                    Some("midi") => {
                        runtime.connect_midi(from_node, from_port, to_node, to_port);
                    }
                    _ => {}
                }
            }
        }

        Ok(runtime)
    }

    pub(crate) fn process_clip_plugin_runtime_segment(
        &mut self,
        clip: &crate::audio::clip::AudioClip,
        input_blocks: &[Vec<f32>],
        _absolute_start_sample: usize,
        request_len: usize,
    ) -> Result<Vec<Vec<f32>>, String> {
        if let Some(graph) = clip.plugin_graph_json.as_ref()
            && !Self::clip_graph_uses_plugin_runtime(graph)
        {
            return Ok(Self::process_direct_clip_graph(
                graph,
                input_blocks,
                request_len,
            ));
        }
        let input_count = input_blocks.len().max(1);
        let runtime_key = Self::clip_plugin_runtime_key(clip, input_count, input_count);
        if !self.rt.clip_plugin_tracks.contains_key(&runtime_key) {
            let runtime =
                self.build_clip_plugin_runtime(clip, input_count, self.process_block_size())?;
            self.rt
                .clip_plugin_tracks
                .insert(runtime_key.clone(), runtime);
        }
        let runtime = self
            .rt
            .clip_plugin_tracks
            .get_mut(&runtime_key)
            .ok_or_else(|| "Missing clip plugin runtime".to_string())?;

        Ok(runtime.process(input_blocks, request_len, ClipRuntimeProcessContext {}))
    }

    pub(crate) fn apply_audio_clip_fades(
        clip: &crate::audio::clip::AudioClip,
        absolute_clip_start: usize,
        clip_len: usize,
        absolute_from: usize,
        samples: &mut [Vec<f32>],
    ) {
        if !clip.fade_enabled {
            return;
        }
        for channel in samples.iter_mut() {
            let channel_len = channel.len();
            let fade_in_start = absolute_clip_start.saturating_sub(absolute_from);
            let fade_in_end = (absolute_clip_start + clip.fade_in_samples)
                .saturating_sub(absolute_from)
                .min(channel_len);
            if fade_in_start < fade_in_end {
                let dt = 1.0 / clip.fade_in_samples.max(1) as f32;
                let start_t =
                    (absolute_from + fade_in_start).saturating_sub(absolute_clip_start) as f32 * dt;
                crate::simd::apply_fade_in_inplace(
                    &mut channel[fade_in_start..fade_in_end],
                    start_t,
                    dt,
                );
            }
            let fade_out_start = (absolute_clip_start
                + clip_len.saturating_sub(clip.fade_out_samples))
            .saturating_sub(absolute_from);
            let fade_out_end = (absolute_clip_start + clip_len)
                .saturating_sub(absolute_from)
                .min(channel_len);
            if fade_out_start < fade_out_end {
                let dt = 1.0 / clip.fade_out_samples.max(1) as f32;
                let start_t = (absolute_from + fade_out_start).saturating_sub(
                    absolute_clip_start + clip_len.saturating_sub(clip.fade_out_samples),
                ) as f32
                    * dt;
                crate::simd::apply_fade_out_inplace(
                    &mut channel[fade_out_start..fade_out_end],
                    start_t,
                    dt,
                );
            }
        }
    }

    pub(crate) fn render_audio_clip_segment(
        &mut self,
        clip: &crate::audio::clip::AudioClip,
        parent_start: usize,
        absolute_from: usize,
        request_len: usize,
        active_clip_plugin_keys: &mut HashSet<String>,
    ) -> Option<Vec<Vec<f32>>> {
        let clip_start = parent_start.saturating_add(clip.start);
        let clip_len = clip.end;
        if clip_len == 0 || request_len == 0 {
            return None;
        }
        let clip_end = clip_start.saturating_add(clip_len);
        let absolute_to = absolute_from.saturating_add(request_len);
        if absolute_to <= clip_start || absolute_from >= clip_end {
            return None;
        }

        if !clip.grouped_clips.is_empty() {
            let channel_count = self.audio.ins.len().max(1);
            let mut input_blocks = vec![vec![0.0; request_len]; channel_count];
            for child in clip.grouped_clips.clone() {
                let child_start = clip_start.saturating_add(child.start);
                let child_end = child_start.saturating_add(child.end);
                if absolute_to <= child_start || absolute_from >= child_end {
                    continue;
                }
                let child_from = absolute_from.max(child_start);
                let child_to = absolute_to.min(child_end);
                let child_len = child_to.saturating_sub(child_from);
                if child_len == 0 {
                    continue;
                }
                if let Some(child_blocks) = self.render_audio_clip_segment(
                    &child,
                    clip_start,
                    child_from,
                    child_len,
                    active_clip_plugin_keys,
                ) {
                    let out_offset = child_from.saturating_sub(absolute_from);
                    for (channel_idx, channel) in input_blocks.iter_mut().enumerate() {
                        let source = child_blocks
                            .get(channel_idx)
                            .or_else(|| child_blocks.first());
                        if let Some(source) = source {
                            let dest_len = channel.len().saturating_sub(out_offset);
                            let src_len = source.len();
                            let len = dest_len.min(src_len);
                            crate::simd::add_inplace(
                                &mut channel[out_offset..out_offset + len],
                                &source[..len],
                            );
                        }
                    }
                }
            }
            Self::apply_audio_clip_fades(
                clip,
                clip_start,
                clip_len,
                absolute_from,
                &mut input_blocks,
            );
            if clip.plugin_graph_json.is_some() {
                active_clip_plugin_keys.insert(Self::clip_plugin_runtime_key(
                    clip,
                    channel_count,
                    channel_count,
                ));
                return Some(
                    self.process_clip_plugin_runtime_segment(
                        clip,
                        &input_blocks,
                        absolute_from,
                        request_len,
                    )
                    .unwrap_or(input_blocks),
                );
            }
            return Some(input_blocks);
        }

        let playback_name = Self::clip_playback_name(clip);
        let buffer = match self.rt.audio_clip_cache.get(playback_name).cloned() {
            Some(buffer) => buffer,
            None => {
                tracing::warn!(
                    "Audio clip cache miss for '{}' on track '{}'; loading from disk",
                    playback_name,
                    self.name
                );
                self.clip_buffer(playback_name)?
            }
        };
        let has_clip_plugins = clip.plugin_graph_json.is_some();
        if has_clip_plugins {
            active_clip_plugin_keys.insert(Self::clip_plugin_runtime_key(
                clip,
                self.audio.ins.len(),
                self.audio.ins.len(),
            ));
        }

        #[cfg(unix)]
        if !clip.pitch_correction_points.is_empty() {
            let input_count = self.audio.ins.len().max(1);
            let effective_channels = if buffer.channels == 1 {
                1
            } else {
                input_count.min(buffer.channels).max(1)
            };
            let total_frames = buffer.samples.len() / buffer.channels.max(1);
            if total_frames == 0 {
                return None;
            }
            let source_offset = clip.pitch_correction_source_offset.unwrap_or(clip.offset);
            let inertia_samples = ((self.sample_rate as u64
                * clip.pitch_correction_inertia_ms.unwrap_or(100) as u64)
                / 1000) as usize;
            let formant = clip.pitch_correction_formant_compensation.unwrap_or(true);
            let key = Self::clip_pitch_key(clip);
            let corrected = {
                let shifter =
                    self.rt
                        .clip_pitch_shifters
                        .entry(key)
                        .or_insert_with(|| ClipPitchShifter {
                            shifter: LivePitchShifter::new(
                                self.sample_rate.round().max(1.0) as usize,
                                effective_channels,
                                formant,
                            )
                            .expect("rubberband live shifter"),
                        });
                shifter.shifter.set_formant_preserved(formant);
                let block_size = shifter.shifter.block_size();
                let source_from =
                    source_offset.saturating_add(absolute_from.saturating_sub(clip_start));
                shifter
                    .shifter
                    .render(source_from, request_len, |block_start, input| {
                        let local_start = block_start.saturating_sub(source_offset);
                        let local_mid = local_start.saturating_add(block_size / 2);
                        let local_end = local_start.saturating_add(block_size.saturating_sub(1));
                        let semitones =
                            (Self::pitch_shift_for_sample(clip, local_start, inertia_samples)
                                + Self::pitch_shift_for_sample(clip, local_mid, inertia_samples)
                                + Self::pitch_shift_for_sample(clip, local_end, inertia_samples))
                                / 3.0;
                        let scale = 2.0_f64.powf((semitones as f64) / 12.0);
                        for (ch, channel_input) in
                            input.iter_mut().enumerate().take(effective_channels)
                        {
                            let source_channel = if buffer.channels == 1 { 0 } else { ch };
                            if buffer.channels == 1 {
                                let src_start = block_start.min(total_frames);
                                let src_end = (block_start + block_size).min(total_frames);
                                let len = src_end.saturating_sub(src_start);
                                channel_input[..len]
                                    .copy_from_slice(&buffer.samples[src_start..src_end]);
                            } else {
                                for (i, sample) in
                                    channel_input.iter_mut().enumerate().take(block_size)
                                {
                                    let source_frame = block_start.saturating_add(i);
                                    *sample = if source_frame < total_frames {
                                        buffer.samples
                                            [source_frame * buffer.channels + source_channel]
                                    } else {
                                        0.0
                                    };
                                }
                            }
                        }
                        scale
                    })
            };
            let mut input_blocks = vec![vec![0.0; request_len]; input_count];
            for (in_channel, block) in input_blocks.iter_mut().enumerate().take(input_count) {
                let source_channel = if effective_channels == 1 {
                    0
                } else if in_channel < effective_channels {
                    in_channel
                } else {
                    continue;
                };
                block[..request_len].copy_from_slice(&corrected[source_channel][..request_len]);
            }
            Self::apply_audio_clip_fades(
                clip,
                clip_start,
                clip_len,
                absolute_from,
                &mut input_blocks,
            );
            return Some(if has_clip_plugins {
                self.process_clip_plugin_runtime_segment(
                    clip,
                    &input_blocks,
                    absolute_from,
                    request_len,
                )
                .unwrap_or(input_blocks)
            } else {
                input_blocks
            });
        }

        let channels = buffer.channels.max(1);
        let total_frames = buffer.samples.len() / channels;
        tracing::debug!(
            "render_audio_clip_segment buffer '{}' channels={} total_frames={} first_sample={} max_abs={}",
            playback_name,
            channels,
            total_frames,
            buffer.samples.first().copied().unwrap_or(0.0),
            buffer
                .samples
                .iter()
                .map(|s| s.abs())
                .fold(0.0_f32, |a, b| a.max(b))
        );
        if total_frames == 0 {
            return None;
        }
        let mut input_blocks = vec![vec![0.0; request_len]; self.audio.ins.len().max(1)];
        for (in_channel, block) in input_blocks
            .iter_mut()
            .enumerate()
            .take(self.audio.ins.len().max(1))
        {
            let source_channel = if channels == 1 {
                0
            } else if in_channel < channels {
                in_channel
            } else {
                continue;
            };
            let start_clip_idx = absolute_from
                .saturating_sub(clip_start)
                .saturating_add(clip.offset);
            if start_clip_idx >= total_frames {
                continue;
            }
            let max_copy = total_frames.saturating_sub(start_clip_idx);
            if channels == 1 {
                let len = request_len.min(max_copy).min(block.len());
                let src_start = start_clip_idx;
                block[..len].copy_from_slice(&buffer.samples[src_start..src_start + len]);
            } else {
                for i in 0..request_len.min(max_copy).min(block.len()) {
                    let clip_idx = start_clip_idx + i;
                    block[i] = buffer.samples[clip_idx * channels + source_channel];
                }
            }
        }
        Self::apply_audio_clip_fades(clip, clip_start, clip_len, absolute_from, &mut input_blocks);
        Some(if has_clip_plugins {
            self.process_clip_plugin_runtime_segment(
                clip,
                &input_blocks,
                absolute_from,
                request_len,
            )
            .unwrap_or(input_blocks)
        } else {
            input_blocks
        })
    }

    pub(crate) fn collect_midi_clip_events_recursive(
        &self,
        clip: &crate::midi::clip::MIDIClip,
        parent_start: usize,
        input_events: &mut [Vec<MidiEvent>],
        frames: usize,
        segments: &[(usize, usize, usize)],
    ) {
        let clip_start = parent_start.saturating_add(clip.start);
        let clip_len = clip.end;
        if clip_len == 0 || clip.muted {
            return;
        }
        if !clip.grouped_clips.is_empty() {
            for child in &clip.grouped_clips {
                self.collect_midi_clip_events_recursive(
                    child,
                    clip_start,
                    input_events,
                    frames,
                    segments,
                );
            }
            return;
        }
        if input_events.is_empty() {
            return;
        }
        let input_lane = clip.input_channel.min(input_events.len().saturating_sub(1));
        if !self
            .midi_disk_monitor()
            .get(input_lane)
            .copied()
            .unwrap_or(true)
        {
            return;
        }
        let clip_end = clip_start.saturating_add(clip_len);
        let Some(events) = self.rt.midi_clip_cache.get(&clip.name) else {
            return;
        };
        for (segment_start, segment_end, out_offset) in segments {
            if clip_end <= *segment_start || clip_start >= *segment_end {
                continue;
            }
            let from = (*segment_start).max(clip_start);
            let to = (*segment_end).min(clip_end);
            let source_from = from.saturating_sub(clip_start).saturating_add(clip.offset);
            let source_to = to.saturating_sub(clip_start).saturating_add(clip.offset);
            for (source_sample, data) in events.iter() {
                if *source_sample < source_from {
                    continue;
                }
                let at_clip_end = *source_sample == clip.offset.saturating_add(clip_len);
                let boundary_note_off =
                    at_clip_end && *source_sample == source_to && Self::is_midi_note_off(data);
                if *source_sample >= source_to && !boundary_note_off {
                    break;
                }
                let absolute_sample =
                    clip_start.saturating_add(source_sample.saturating_sub(clip.offset));
                let mut frame_idx = out_offset.saturating_add(absolute_sample - *segment_start);
                if boundary_note_off {
                    frame_idx = frame_idx.min(frames.saturating_sub(1));
                }
                if frame_idx < frames {
                    input_events[input_lane].push(MidiEvent::new(frame_idx as u32, data.clone()));
                }
            }
            if to == clip_end {
                let frame_idx = out_offset
                    .saturating_add(clip_end.saturating_sub(*segment_start))
                    .min(frames.saturating_sub(1));
                for data in Self::synthetic_note_offs_at_clip_end(events, clip.offset, clip_len) {
                    input_events[input_lane].push(MidiEvent::new(frame_idx as u32, data));
                }
            }
        }
    }

    pub(crate) fn is_midi_note_off(data: &[u8]) -> bool {
        let Some(status) = data.first().copied() else {
            return false;
        };
        match status & 0xF0 {
            0x80 => true,
            0x90 => data.get(2).copied().unwrap_or(0) == 0,
            _ => false,
        }
    }

    pub(crate) fn synthetic_note_offs_at_clip_end(
        events: &[(usize, Vec<u8>)],
        clip_offset: usize,
        clip_len: usize,
    ) -> Vec<Vec<u8>> {
        let clip_end = clip_offset.saturating_add(clip_len);
        let mut active = std::collections::BTreeSet::<(u8, u8)>::new();

        for (sample, data) in events {
            if *sample < clip_offset {
                continue;
            }
            if *sample > clip_end {
                break;
            }
            let Some(status) = data.first().copied() else {
                continue;
            };
            let channel = status & 0x0F;
            let Some(note) = data.get(1).copied() else {
                continue;
            };
            match status & 0xF0 {
                0x80 => {
                    active.remove(&(channel, note));
                }
                0x90 => {
                    if data.get(2).copied().unwrap_or(0) == 0 {
                        active.remove(&(channel, note));
                    } else {
                        active.insert((channel, note));
                    }
                }
                _ => {}
            }
        }

        active
            .into_iter()
            .map(|(channel, note)| vec![0x80 | channel.min(15), note.min(127), 64])
            .collect()
    }

    pub(crate) fn ensure_clip_plugin_runtime(
        &mut self,
        clip_idx: usize,
        channels: usize,
    ) -> Result<&mut ClipPluginRuntime, String> {
        let clip = self.audio.clips().get(clip_idx).cloned().ok_or_else(|| {
            format!(
                "Track '{}' has no audio clip at index {}",
                self.name, clip_idx
            )
        })?;
        if clip.plugin_graph_json.is_none() {
            return Err(format!(
                "Track '{}' clip {} has no plugin graph",
                self.name, clip_idx
            ));
        }
        let runtime_key = Self::clip_plugin_runtime_key(&clip, channels, channels);
        if !self.rt.clip_plugin_tracks.contains_key(&runtime_key) {
            let runtime =
                self.build_clip_plugin_runtime(&clip, channels, self.process_block_size())?;
            self.rt
                .clip_plugin_tracks
                .insert(runtime_key.clone(), runtime);
        }
        let runtime = self
            .rt
            .clip_plugin_tracks
            .get_mut(&runtime_key)
            .ok_or_else(|| "Missing clip plugin runtime".to_string())?;
        Ok(runtime)
    }

    #[cfg(unix)]
    pub(crate) fn pitch_shift_for_sample(
        clip: &crate::audio::clip::AudioClip,
        sample: usize,
        inertia_samples: usize,
    ) -> f32 {
        if clip.pitch_correction_points.is_empty() {
            return 0.0;
        }
        let mut points = clip.pitch_correction_points.iter().collect::<Vec<_>>();
        points.sort_by_key(|point| point.start_sample);
        let mut previous_shift = points[0].target_midi_pitch - points[0].detected_midi_pitch;
        if sample < points[0].start_sample {
            return previous_shift;
        }
        for point in points {
            let target_shift = point.target_midi_pitch - point.detected_midi_pitch;
            if sample < point.start_sample {
                break;
            }
            if inertia_samples > 0
                && sample < point.start_sample.saturating_add(inertia_samples)
                && (target_shift - previous_shift).abs() > f32::EPSILON
            {
                let t = (sample - point.start_sample) as f32 / inertia_samples as f32;
                return previous_shift + (target_shift - previous_shift) * t.clamp(0.0, 1.0);
            }
            previous_shift = target_shift;
        }
        previous_shift
    }

    pub(crate) fn preload_audio_clip_cache(&mut self) {
        let missing: Vec<String> = self
            .audio
            .clips()
            .iter()
            .filter_map(|clip| {
                let clip_name = Self::clip_playback_name(clip);
                if self.rt.audio_clip_cache.contains_key(clip_name) {
                    None
                } else {
                    Some(clip_name.to_string())
                }
            })
            .collect();
        if !missing.is_empty() {
            let started = std::time::Instant::now();
            for clip_name in missing {
                let clip_started = std::time::Instant::now();
                let result = self.clip_buffer(&clip_name);
                let elapsed = clip_started.elapsed().as_secs_f64() * 1000.0;
                tracing::debug!(
                    "Preloaded clip '{}' in {:.1}ms (found={})",
                    clip_name,
                    elapsed,
                    result.is_some()
                );
            }
            let total = started.elapsed().as_secs_f64() * 1000.0;
            if total > 20.0 {
                tracing::warn!(
                    "Slow preload_audio_clip_cache for track '{}' took {:.1}ms for {} clips",
                    self.name,
                    total,
                    self.audio.clips().len()
                );
            }
        }
    }

    pub(crate) fn load_midi_clip_events(
        path: &Path,
        sample_rate: f64,
    ) -> Option<Vec<(usize, Vec<u8>)>> {
        let bytes = std::fs::read(path).ok()?;
        let smf = Smf::parse(&bytes).ok()?;
        let Timing::Metrical(ppq) = smf.header.timing else {
            return None;
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
                    (seg_ticks as u128).saturating_mul(current_tempo_us as u128) / (ppq as u128),
                );
                prev_tick = *change_tick;
                current_tempo_us = *tempo_us;
            }
            let tail_ticks = tick.saturating_sub(prev_tick);
            total_us = total_us.saturating_add(
                (tail_ticks as u128).saturating_mul(current_tempo_us as u128) / (ppq as u128),
            );
            ((total_us as f64) * (sample_rate / 1_000_000.0)).round() as usize
        };

        let mut out = Vec::<(usize, Vec<u8>)>::new();
        for track in &smf.tracks {
            let mut tick = 0_u64;
            for event in track {
                tick = tick.saturating_add(event.delta.as_int() as u64);
                let data = match event.kind {
                    TrackEventKind::Midi { channel, message } => {
                        let mut data = Vec::with_capacity(3);
                        if (LiveEvent::Midi { channel, message })
                            .write(&mut data)
                            .is_ok()
                        {
                            Some(data)
                        } else {
                            None
                        }
                    }
                    TrackEventKind::SysEx(payload) => {
                        let mut data = Vec::with_capacity(payload.len() + 2);
                        data.push(0xF0);
                        data.extend_from_slice(payload);
                        if data.last().copied() != Some(0xF7) {
                            data.push(0xF7);
                        }
                        Some(data)
                    }

                    TrackEventKind::Escape(payload) => {
                        let mut data = Vec::with_capacity(payload.len() + 1);
                        data.push(0xF7);
                        data.extend_from_slice(payload);
                        Some(data)
                    }
                    _ => None,
                };
                if let Some(data) = data {
                    out.push((ticks_to_samples(tick), data));
                }
            }
        }
        out.sort_by_key(|(sample, _)| *sample);
        Some(out)
    }

    pub(crate) fn midi_clip_events(&mut self, clip_name: &str) -> Option<MidiClipEvents> {
        if let Some(cached) = self.rt.midi_clip_cache.get(clip_name) {
            return Some(cached.clone());
        }
        let path = self.resolve_clip_path(clip_name);
        let loaded = Self::load_midi_clip_events(&path, self.sample_rate)?;
        let loaded = Arc::new(loaded);
        self.rt
            .midi_clip_cache
            .insert(clip_name.to_string(), loaded.clone());
        Some(loaded)
    }

    pub(crate) fn preload_midi_clip_cache(&mut self) {
        let missing: Vec<String> = self
            .midi
            .clips()
            .iter()
            .filter_map(|clip| {
                if self.rt.midi_clip_cache.contains_key(&clip.name) {
                    None
                } else {
                    Some(clip.name.clone())
                }
            })
            .collect();
        if !missing.is_empty() {
            let started = std::time::Instant::now();
            for clip_name in missing {
                let clip_started = std::time::Instant::now();
                let result = self.midi_clip_events(&clip_name);
                let elapsed = clip_started.elapsed().as_secs_f64() * 1000.0;
                tracing::debug!(
                    "Preloaded MIDI clip '{}' in {:.1}ms (found={})",
                    clip_name,
                    elapsed,
                    result.is_some()
                );
            }
            let total = started.elapsed().as_secs_f64() * 1000.0;
            if total > 20.0 {
                tracing::warn!(
                    "Slow preload_midi_clip_cache for track '{}' took {:.1}ms for {} clips",
                    self.name,
                    total,
                    self.midi.clips().len()
                );
            }
        }
    }

    pub fn preload_clips(&mut self) {
        self.preload_audio_clip_cache();
        self.preload_midi_clip_cache();
    }

    pub(crate) fn cycle_segments(&self, frames: usize) -> Vec<(usize, usize, usize)> {
        if frames == 0 {
            return vec![];
        }
        if !self.rt.loop_enabled {
            return vec![(
                self.rt.transport_sample,
                self.rt.transport_sample.saturating_add(frames),
                0,
            )];
        }
        let Some((loop_start, loop_end)) = self.rt.loop_range_samples else {
            return vec![(
                self.rt.transport_sample,
                self.rt.transport_sample.saturating_add(frames),
                0,
            )];
        };
        if loop_end <= loop_start {
            return vec![(
                self.rt.transport_sample,
                self.rt.transport_sample.saturating_add(frames),
                0,
            )];
        }
        let mut segments = Vec::new();
        let mut remaining = frames;
        let mut out_offset = 0usize;
        let mut current = self.rt.transport_sample;
        while remaining > 0 {
            let segment_end_limit = loop_end;
            let take = segment_end_limit.saturating_sub(current).min(remaining);
            if take == 0 {
                current = loop_start;
                continue;
            }
            segments.push((current, current.saturating_add(take), out_offset));
            out_offset = out_offset.saturating_add(take);
            remaining -= take;
            current = if remaining > 0 {
                loop_start
            } else {
                current.saturating_add(take)
            };
        }
        segments
    }

    pub(crate) fn mix_clip_audio_into_input_buffers(&mut self, inputs: &mut [&mut [f32]]) {
        let frames = inputs.first().map(|input| input.len()).unwrap_or(0);
        tracing::debug!(
            "mix_clip_audio_into_inputs for '{}' frames={} clips={}",
            self.name,
            frames,
            self.audio.clips().len()
        );
        if frames == 0 || inputs.is_empty() {
            return;
        }

        let mut active_clip_plugin_keys = HashSet::new();
        let segments = self.cycle_segments(frames);
        for clip in self.audio.clips().iter() {
            if clip.muted {
                tracing::debug!("mix_clip_audio_into_inputs clip '{}' muted", clip.name);
                continue;
            }
            for (segment_start, segment_end, out_offset) in &segments {
                let clip_start = clip.start;
                let clip_end = clip_start.saturating_add(clip.end);
                tracing::debug!(
                    "mix_clip_audio_into_inputs clip '{}' range={}-{} segment={}-{} transport={}",
                    clip.name,
                    clip_start,
                    clip_end,
                    segment_start,
                    segment_end,
                    self.rt.transport_sample
                );
                if clip_end <= *segment_start || clip_start >= *segment_end {
                    tracing::debug!(
                        "mix_clip_audio_into_inputs clip '{}' skipped (out of range)",
                        clip.name
                    );
                    continue;
                }
                let from = (*segment_start).max(clip_start);
                let to = (*segment_end).min(clip_end);
                let track_idx = out_offset + (from - *segment_start);
                let copy_len = to
                    .saturating_sub(from)
                    .min(inputs[0].len().saturating_sub(track_idx));
                if copy_len == 0 {
                    tracing::debug!("mix_clip_audio_into_inputs clip '{}' copy_len=0", clip.name);
                    continue;
                }
                let render_start = std::time::Instant::now();
                let Some(processed_blocks) = self.render_audio_clip_segment(
                    clip,
                    0,
                    from,
                    copy_len,
                    &mut active_clip_plugin_keys,
                ) else {
                    tracing::debug!(
                        "mix_clip_audio_into_inputs clip '{}' render returned None",
                        clip.name
                    );
                    continue;
                };
                let render_elapsed = render_start.elapsed().as_secs_f64() * 1000.0;
                if render_elapsed > 1.0 {
                    tracing::warn!(
                        "render_audio_clip_segment for '{}' clip '{}' len={} took {:.2}ms",
                        self.name,
                        clip.name,
                        copy_len,
                        render_elapsed
                    );
                }
                tracing::debug!(
                    "mix_clip_audio_into_inputs clip '{}' rendered {} channels, first sample={:?}",
                    clip.name,
                    processed_blocks.len(),
                    processed_blocks.first().and_then(|b| b.first())
                );
                for (in_channel, in_samples) in inputs.iter_mut().enumerate() {
                    if !self
                        .disk_monitor()
                        .get(in_channel)
                        .copied()
                        .unwrap_or(false)
                    {
                        continue;
                    }
                    let processed = processed_blocks
                        .get(in_channel)
                        .or_else(|| processed_blocks.first());
                    if let Some(processed) = processed {
                        let dest_len = in_samples.len().saturating_sub(track_idx);
                        let src_len = processed.len();
                        let len = dest_len.min(src_len);
                        crate::simd::add_inplace(
                            &mut in_samples[track_idx..track_idx + len],
                            &processed[..len],
                        );
                    }
                }
            }
        }
        self.rt
            .clip_plugin_tracks
            .retain(|key, _| active_clip_plugin_keys.contains(key));
    }

    pub(crate) fn mix_clip_midi_into_inputs(
        &mut self,
        input_events: &mut [Vec<MidiEvent>],
        frames: usize,
    ) {
        if frames == 0 || input_events.is_empty() {
            return;
        }
        let segments = self.cycle_segments(frames);
        for clip in self.midi.clips().iter() {
            self.collect_midi_clip_events_recursive(clip, 0, input_events, frames, &segments);
        }
        for events in input_events.iter_mut() {
            events.sort_by_key(|event| event.frame);
        }
    }
}
