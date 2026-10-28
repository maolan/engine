use crate::{
    executor::NodeJob,
    message::{
        Action, Message, OfflineAutomationLane, OfflineAutomationTarget, OfflineBounceStem,
        OfflineBounceWork, OfflineStemsBounceWork, ProcessTask,
    },
    midi::io::MidiEvent,
    render_plan::Op,
    state::TrackHandle,
};
#[cfg(unix)]
use nix::libc;
use rubato::Resampler as _;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{Receiver, Sender};

pub(crate) struct NodeJobResult {
    pub(crate) worker_id: usize,
    pub(crate) epoch: u64,
    pub(crate) node: u32,
    pub(crate) output_linear: Vec<f32>,
    pub(crate) parameter_updates: Vec<Action>,
    pub(crate) latency_changed: bool,
}

/// Reusable per-node scratch buffers. A node rebuilds each buffer with
/// `clear` + `extend` before use, so the steady state performs no
/// allocations on the RT path. The reference-carrying buffers hold slices
/// derived from the render-plan arena via unsafe pointer dereferences; they
/// are only valid for the duration of the node call that filled them and
/// must never be read afterwards.
#[derive(Default)]
struct NodeScratch {
    input_ptrs: Vec<*mut Vec<f32>>,
    output_ptrs: Vec<*mut Vec<f32>>,
    inputs_mut: Vec<&'static mut [f32]>,
    inputs_shared: Vec<&'static [f32]>,
    outputs_mut: Vec<&'static mut [f32]>,
    source_buffers: Vec<(usize, &'static [f32], usize)>,
}

#[derive(Debug)]
pub struct Worker {
    id: usize,
    rx: Receiver<Message>,
    tx: Sender<Message>,
    realtime_priority: i32,
}

/// Write a freeze/offline-bounce WAV file, or fail when codec support is
/// compiled out.
#[cfg(feature = "audio")]
fn write_freeze_wav(
    path: &std::path::Path,
    samples: &[f32],
    channels: usize,
    sample_rate: u32,
) -> Result<(), String> {
    crate::audio_codec::write_wav_f32(path, samples, channels, sample_rate)
        .map_err(|e| e.to_string())
}

#[cfg(not(feature = "audio"))]
fn write_freeze_wav(
    path: &std::path::Path,
    samples: &[f32],
    channels: usize,
    sample_rate: u32,
) -> Result<(), String> {
    let _ = (path, samples, channels, sample_rate);
    Err("audio codec support is disabled (enable the `audio` cargo feature)".to_string())
}

/// Per-stem output encoder. The full `StreamingEncoder` (WAV + FLAC +
/// Ogg-FLAC + MP3) with the `audio` feature; WAV-only without it, matching
/// `write_freeze_wav`'s gating.
#[cfg(feature = "audio")]
type StemEncoder = crate::audio_codec::StreamingEncoder;

#[cfg(not(feature = "audio"))]
type StemEncoder = DisabledStemEncoder;

#[cfg(not(feature = "audio"))]
struct DisabledStemEncoder(crate::wav_stream::WavStreamWriter);

#[cfg(not(feature = "audio"))]
impl DisabledStemEncoder {
    fn create(
        path: &std::path::Path,
        format: crate::wav_stream::AudioEncodeFormat,
        channels: usize,
        sample_rate: u32,
        dither: crate::wav_stream::AudioDither,
    ) -> Result<Self, String> {
        match format {
            crate::wav_stream::AudioEncodeFormat::Wav(depth) => {
                crate::wav_stream::WavStreamWriter::create(
                    path,
                    channels,
                    sample_rate,
                    depth,
                    dither,
                )
                .map(Self)
                .map_err(|e| e.to_string())
            }
            _ => Err(
                "audio codec support is disabled (enable the `audio` cargo feature)".to_string(),
            ),
        }
    }

    fn encode_block(&mut self, samples: &[f32]) -> std::io::Result<()> {
        self.0.write_interleaved_f32(samples)
    }

    fn finish(mut self) -> std::io::Result<()> {
        self.0.finalize()
    }
}

/// Per-stem sample-rate converter (session rate → export rate), the
/// `rubato` polyphase idiom shared with `track::streaming`.
struct StemResampler {
    inner: rubato::Async<f32>,
    input: Vec<f32>,
    output: Vec<f32>,
    channels: usize,
    block_size: usize,
}

impl StemResampler {
    fn new(engine_rate: u32, export_rate: u32, channels: usize, block_size: usize) -> Option<Self> {
        if engine_rate == 0 || engine_rate == export_rate {
            return None;
        }
        let ratio = export_rate as f64 / engine_rate as f64;
        let inner = rubato::Async::<f32>::new_poly(
            ratio,
            1.5,
            rubato::PolynomialDegree::Cubic,
            block_size,
            channels,
            rubato::FixedAsync::Input,
        )
        .ok()?;
        let output_frames = inner.output_frames_max();
        Some(Self {
            inner,
            input: vec![0.0_f32; block_size.saturating_mul(channels)],
            output: vec![0.0_f32; output_frames.saturating_mul(channels)],
            channels,
            block_size,
        })
    }

    /// Feed `frames` interleaved samples and return the resampled piece.
    fn process(&mut self, block: &[f32], frames: usize) -> std::io::Result<&[f32]> {
        self.input[..block.len()].copy_from_slice(block);
        let input =
            audioadapter_buffers::direct::InterleavedSlice::new(&self.input, self.channels, frames)
                .map_err(|e| std::io::Error::other(format!("resampler input adapter: {e}")))?;
        let capacity = self.output.len() / self.channels.max(1);
        let mut output = audioadapter_buffers::direct::InterleavedSlice::new_mut(
            &mut self.output,
            self.channels,
            capacity,
        )
        .map_err(|e| std::io::Error::other(format!("resampler output adapter: {e}")))?;
        let full_input = frames >= self.block_size;
        let indexing = if full_input {
            None
        } else {
            Some(rubato::Indexing::new().partial_len(frames))
        };
        let (_, written) = self
            .inner
            .process_into_buffer(&input, &mut output, indexing.as_ref())
            .map_err(|e| std::io::Error::other(format!("stem resample failed: {e}")))?;
        Ok(&self.output[..written.saturating_mul(self.channels)])
    }
}

impl Worker {
    fn apply_freeze_automation_at_sample(
        track: &mut crate::track::TrackData,
        sample: usize,
        lanes: &[OfflineAutomationLane],
    ) {
        for lane in lanes {
            if matches!(
                lane.target,
                OfflineAutomationTarget::Volume | OfflineAutomationTarget::Balance
            ) {
                continue;
            }
            let Some(value) = lane.value_at(sample) else {
                continue;
            };
            match lane.target {
                OfflineAutomationTarget::Volume | OfflineAutomationTarget::Balance => {}
                OfflineAutomationTarget::MidiCc { channel, cc } => {
                    let cc_value = (value * 127.0).round() as u8;
                    track.rt.pending_automation_midi_events.push(MidiEvent::new(
                        0,
                        vec![0xB0 | channel.min(15), cc.min(127), cc_value],
                    ));
                }
                #[cfg(all(unix, feature = "plugins"))]
                OfflineAutomationTarget::Lv2Parameter {
                    instance_id,
                    index,
                    min,
                    max,
                } => {
                    let lo = min.min(max);
                    let hi = max.max(min);
                    let param_value = (lo + value * (hi - lo)).clamp(lo, hi);
                    if let Err(e) =
                        track.set_lv2_control_value(instance_id, index as usize, param_value as f64)
                    {
                        tracing::warn!("Offline LV2 automation failed: {e}");
                    }
                }
                #[cfg(feature = "plugins")]
                OfflineAutomationTarget::Vst3Parameter {
                    instance_id,
                    param_id,
                } => {
                    if let Err(e) =
                        track.set_vst3_parameter(instance_id, param_id, value.clamp(0.0, 1.0))
                    {
                        tracing::warn!("Offline VST3 automation failed: {e}");
                    }
                }
                #[cfg(all(target_os = "macos", feature = "plugins"))]
                OfflineAutomationTarget::AuParameter {
                    instance_id,
                    param_index,
                } => {
                    if let Err(e) =
                        track.set_au_parameter(instance_id, param_index, value.clamp(0.0, 1.0))
                    {
                        tracing::warn!("Offline Audio Unit automation failed: {e}");
                    }
                }
                #[cfg(feature = "plugins")]
                OfflineAutomationTarget::ClapParameter {
                    instance_id,
                    param_id,
                    min,
                    max,
                } => {
                    let lo = min.min(max);
                    let hi = max.max(min);
                    let param_value = (lo + value as f64 * (hi - lo)).clamp(lo, hi);
                    if let Err(e) =
                        track.set_clap_parameter_at(instance_id, param_id, param_value, 0)
                    {
                        tracing::warn!("Offline CLAP automation failed: {e}");
                    }
                }
                OfflineAutomationTarget::MixOsc { .. } => {
                    // MixOSC values are sent to external hardware in real time,
                    // not rendered into frozen audio.
                }
            }
        }
    }

    fn prepare_track_for_freeze_render(track: &mut crate::track::TrackData) -> (f32, f32) {
        let original_level = track.level();
        let original_balance = track.balance();
        track.set_level(0.0);
        track.set_balance(0.0);
        (original_level, original_balance)
    }

    fn restore_track_after_freeze_render(
        track: &mut crate::track::TrackData,
        original_level: f32,
        original_balance: f32,
    ) {
        track.set_level(original_level);
        track.set_balance(original_balance);
    }

    async fn process_offline_bounce(&self, job: OfflineBounceWork) {
        let track_handle = job.state.tracks.get(&job.track_name).cloned();
        let Some(target_track) = track_handle else {
            crate::report_error(
                self.tx
                    .send(Message::OfflineBounceFinished {
                        result: Err(format!("Track not found: {}", job.track_name)),
                    })
                    .await,
                "Failed to report missing offline bounce track",
            );
            crate::report_error(
                self.tx.send(Message::Ready(self.id)).await,
                "Failed to return offline bounce worker",
            );
            return;
        };
        let (channels, block_size, sample_rate) = {
            let t = target_track.lock();
            let block_size = t
                .audio
                .outs
                .first()
                .map(|io| io.buffer_size())
                .or_else(|| t.audio.ins.first().map(|io| io.buffer_size()))
                .unwrap_or(0)
                .max(1);
            (
                t.audio.outs.len().max(1),
                block_size,
                t.sample_rate.round().max(1.0) as i32,
            )
        };
        let freeze_state = if job.apply_fader {
            None
        } else {
            let mut t = target_track.lock();
            Some(Self::prepare_track_for_freeze_render(&mut t))
        };

        let plan_collector = basedrop::Collector::new();
        let render_plan = crate::render_plan::RenderPlan::compile(&job.state, &[], &[], block_size);
        let render_nodes = render_plan.nodes_for_track_output(&target_track);
        let mut seen_tracks = std::collections::HashSet::new();
        let render_tracks: Vec<_> = render_nodes
            .iter()
            .filter_map(|&node| {
                let Op::Task { task, .. } = &render_plan.nodes[node as usize] else {
                    return None;
                };
                let track = match task {
                    ProcessTask::Track(track)
                    | ProcessTask::FolderInput(track)
                    | ProcessTask::FolderOutput(track) => track,
                    #[cfg(feature = "plugins")]
                    ProcessTask::Plugin { track, .. } => track,
                };
                seen_tracks
                    .insert(Arc::as_ptr(track) as usize)
                    .then(|| track.clone())
            })
            .collect();
        tracing::debug!(
            track = %job.track_name,
            selected_nodes = render_nodes.len(),
            total_nodes = render_plan.nodes.len(),
            selected_tracks = render_tracks.len(),
            "Selected offline render dependencies"
        );
        let offline_render = crate::offline_render::OfflineRender::new(&render_plan, &render_nodes);
        let render_plan = Arc::new(basedrop::Owned::new(&plan_collector.handle(), render_plan));

        let mut output_samples =
            Vec::<f32>::with_capacity(job.length_samples.saturating_mul(channels.max(1)));

        let mut cursor = 0usize;
        let mut last_reported_progress = 0.0_f32;
        let mut total_process_time = Duration::ZERO;
        let mut total_write_time = Duration::ZERO;
        while cursor < job.length_samples {
            if job.cancel.load(std::sync::atomic::Ordering::Relaxed) {
                crate::report_error(
                    std::fs::remove_file(&job.output_path),
                    "Failed to remove canceled offline bounce",
                );
                if let Some((original_level, original_balance)) = freeze_state {
                    let mut t = target_track.lock();
                    Self::restore_track_after_freeze_render(
                        &mut t,
                        original_level,
                        original_balance,
                    );
                }
                crate::report_error(
                    self.tx
                        .send(Message::OfflineBounceFinished {
                            result: Ok(Action::TrackOfflineBounceCanceled {
                                track_name: job.track_name.clone(),
                            }),
                        })
                        .await,
                    "Failed to report canceled offline bounce",
                );
                crate::report_error(
                    self.tx.send(Message::Ready(self.id)).await,
                    "Failed to return offline bounce worker",
                );
                return;
            }

            let step = (job.length_samples - cursor).min(block_size);
            for handle in &render_tracks {
                let mut t = handle.lock();
                t.audio.set_finished(false);
                t.audio.set_processing(false);
                t.set_transport_sample(job.start_sample.saturating_add(cursor));
                t.set_loop_config(false, None);
                t.set_transport_timing(job.tempo_bpm, job.tsig_num, job.tsig_denom);
                t.set_clip_playback_enabled(true);
                t.set_record_tap_enabled(false);
            }
            {
                let mut t = target_track.lock();
                Self::apply_freeze_automation_at_sample(
                    &mut t,
                    job.start_sample.saturating_add(cursor),
                    &job.automation_lanes,
                );
            }

            let p_start = Instant::now();
            offline_render.render(self.id, &render_plan);
            total_process_time += p_start.elapsed();

            let write_start = Instant::now();
            {
                let t = target_track.lock();
                let outs = t.last_audio_outputs();
                for i in 0..step {
                    for ch in 0..channels {
                        let sample = outs
                            .get(ch)
                            .and_then(|out| out.get(i))
                            .copied()
                            .unwrap_or(0.0);
                        output_samples.push(sample);
                    }
                }
            }
            total_write_time += write_start.elapsed();

            cursor = cursor.saturating_add(step);
            let progress = (cursor as f32 / job.length_samples as f32).clamp(0.0, 1.0);

            if progress - last_reported_progress >= 0.01 || cursor >= job.length_samples {
                last_reported_progress = progress;
                crate::report_error(
                    self.tx
                        .send(Message::OfflineBounceFinished {
                            result: Ok(Action::TrackOfflineBounceProgress {
                                track_name: job.track_name.clone(),
                                progress,
                                operation: Some("Rendering freeze".to_string()),
                            }),
                        })
                        .await,
                    "Failed to report offline bounce progress",
                );
            }
        }

        if let Err(e) = write_freeze_wav(
            std::path::Path::new(&job.output_path),
            &output_samples,
            channels,
            sample_rate as u32,
        ) {
            crate::report_error(
                std::fs::remove_file(&job.output_path),
                "Failed to remove failed offline bounce",
            );
            if let Some((original_level, original_balance)) = freeze_state {
                let mut t = target_track.lock();
                Self::restore_track_after_freeze_render(&mut t, original_level, original_balance);
            }
            crate::report_error(
                self.tx
                    .send(Message::OfflineBounceFinished {
                        result: Err(format!(
                            "Failed to write offline bounce '{}': {e}",
                            job.output_path
                        )),
                    })
                    .await,
                "Failed to report offline bounce write error",
            );
            crate::report_error(
                self.tx.send(Message::Ready(self.id)).await,
                "Failed to return offline bounce worker",
            );
            return;
        }

        if let Some((original_level, original_balance)) = freeze_state {
            let mut t = target_track.lock();
            Self::restore_track_after_freeze_render(&mut t, original_level, original_balance);
        }

        crate::report_error(
            self.tx
                .send(Message::OfflineBounceFinished {
                    result: Ok(Action::TrackOfflineBounce {
                        track_name: job.track_name,
                        output_path: job.output_path,
                        start_sample: job.start_sample,
                        length_samples: job.length_samples,
                        automation_lanes: vec![],
                        apply_fader: job.apply_fader,
                    }),
                })
                .await,
            "Failed to report completed offline bounce",
        );
        crate::report_error(
            self.tx.send(Message::Ready(self.id)).await,
            "Failed to return offline bounce worker",
        );
    }

    /// Render several tracks' outputs in a single offline pass, capturing
    /// every stem per block the way realtime playback renders all tracks at
    /// once. Each stem streams 32-bit float WAV to its own file, so memory
    /// stays bounded by one block per stem regardless of session length.
    async fn process_offline_stems_bounce(&self, job: OfflineStemsBounceWork) {
        // Resolve every target before creating files so a bad name fails
        // the whole job without leaving partial output behind.
        let mut targets: Vec<(&OfflineBounceStem, TrackHandle, usize, u32)> =
            Vec::with_capacity(job.stems.len());
        for stem in &job.stems {
            let Some(handle) = job.state.tracks.get(&stem.track_name).cloned() else {
                crate::report_error(
                    self.tx
                        .send(Message::OfflineBounceFinished {
                            result: Err(format!("Track not found: {}", stem.track_name)),
                        })
                        .await,
                    "Failed to report missing offline stems bounce track",
                );
                crate::report_error(
                    self.tx.send(Message::Ready(self.id)).await,
                    "Failed to return offline bounce worker",
                );
                return;
            };
            let (channels, engine_rate) = {
                let t = handle.lock();
                (
                    t.audio.outs.len().max(1),
                    t.sample_rate.round().max(1.0) as u32,
                )
            };
            targets.push((stem, handle, channels, engine_rate));
        }
        if targets.is_empty() {
            crate::report_error(
                self.tx
                    .send(Message::OfflineBounceFinished {
                        result: Ok(Action::StemsOfflineBounced { stems: vec![] }),
                    })
                    .await,
                "Failed to report empty offline stems bounce",
            );
            crate::report_error(
                self.tx.send(Message::Ready(self.id)).await,
                "Failed to return offline bounce worker",
            );
            return;
        }
        let block_size = targets
            .iter()
            .map(|(_, handle, _, _)| {
                handle
                    .lock()
                    .audio
                    .outs
                    .first()
                    .map(|io| io.buffer_size())
                    .unwrap_or(0)
            })
            .max()
            .unwrap_or(0)
            .max(1);
        // Neutralize the fader of pre-fader stems for the whole pass, like
        // the single-track freeze bounce does. Note a nested pre-fader
        // folder therefore renders without its fader in every stem captured
        // this pass, including ancestor folders; post-fader stems are exact.
        let mut restore_faders: Vec<(TrackHandle, f32, f32)> = Vec::new();
        for (stem, handle, _, _) in &targets {
            if !stem.apply_fader {
                let mut t = handle.lock();
                let originals = Self::prepare_track_for_freeze_render(&mut t);
                restore_faders.push((handle.clone(), originals.0, originals.1));
            }
        }
        let restore = |restore_faders: &[(TrackHandle, f32, f32)]| {
            for (handle, original_level, original_balance) in restore_faders {
                let mut t = handle.lock();
                Self::restore_track_after_freeze_render(&mut t, *original_level, *original_balance);
            }
        };
        let cleanup_files = |stems: &[&OfflineBounceStem]| {
            for stem in stems {
                for output in &stem.outputs {
                    if let Err(e) = std::fs::remove_file(&output.path)
                        && e.kind() != std::io::ErrorKind::NotFound
                    {
                        crate::report_error(
                            Err::<(), _>(e),
                            "Failed to remove offline stems bounce output",
                        );
                    }
                }
            }
        };

        // One encoder per (stem, output). FLAC/Ogg/MP3 only materialize their
        // file at `finish`, so cancel cleanup must tolerate missing paths.
        let mut encoders: Vec<(usize, StemEncoder)> = Vec::new();
        for (stem_idx, (stem, _, channels, _)) in targets.iter().enumerate() {
            for output in &stem.outputs {
                match StemEncoder::create(
                    std::path::Path::new(&output.path),
                    output.format,
                    *channels,
                    stem.sample_rate,
                    output.dither,
                ) {
                    Ok(encoder) => encoders.push((stem_idx, encoder)),
                    Err(e) => {
                        restore(&restore_faders);
                        cleanup_files(&job.stems.iter().collect::<Vec<_>>());
                        crate::report_error(
                            self.tx
                                .send(Message::OfflineBounceFinished {
                                    result: Err(format!(
                                        "Failed to create offline stems bounce '{}': {e}",
                                        output.path
                                    )),
                                })
                                .await,
                            "Failed to report offline stems bounce create error",
                        );
                        crate::report_error(
                            self.tx.send(Message::Ready(self.id)).await,
                            "Failed to return offline bounce worker",
                        );
                        return;
                    }
                }
            }
        }

        let plan_collector = basedrop::Collector::new();
        let render_plan = crate::render_plan::RenderPlan::compile(&job.state, &[], &[], block_size);
        // Union of every target's dependency nodes, in plan order, so one
        // pass renders exactly the graph the stems need.
        let mut required = vec![false; render_plan.nodes.len()];
        for (_, handle, _, _) in &targets {
            for node in render_plan.nodes_for_track_output(handle) {
                required[node as usize] = true;
            }
        }
        let render_nodes: Vec<u32> = required
            .iter()
            .enumerate()
            .filter_map(|(id, &needed)| needed.then_some(id as u32))
            .collect();
        let mut seen_tracks = std::collections::HashSet::new();
        let render_tracks: Vec<_> = render_nodes
            .iter()
            .filter_map(|&node| {
                let Op::Task { task, .. } = &render_plan.nodes[node as usize] else {
                    return None;
                };
                let track = match task {
                    ProcessTask::Track(track)
                    | ProcessTask::FolderInput(track)
                    | ProcessTask::FolderOutput(track) => track,
                    #[cfg(feature = "plugins")]
                    ProcessTask::Plugin { track, .. } => track,
                };
                seen_tracks
                    .insert(Arc::as_ptr(track) as usize)
                    .then(|| track.clone())
            })
            .collect();
        tracing::debug!(
            stems = targets.len(),
            selected_nodes = render_nodes.len(),
            total_nodes = render_plan.nodes.len(),
            selected_tracks = render_tracks.len(),
            "Selected offline stems render dependencies"
        );
        let offline_render = crate::offline_render::OfflineRender::new(&render_plan, &render_nodes);
        let render_plan = Arc::new(basedrop::Owned::new(&plan_collector.handle(), render_plan));

        let pass_length = job
            .stems
            .iter()
            .map(|stem| stem.length_samples)
            .max()
            .unwrap_or(0)
            .max(1);
        let mut frames_written = vec![0_usize; targets.len()];
        let mut scratch: Vec<Vec<f32>> = vec![Vec::new(); targets.len()];
        // Sample-rate conversion state and exact output bookkeeping per stem.
        let mut resamplers: Vec<Option<StemResampler>> = targets
            .iter()
            .map(|(stem, _, channels, engine_rate)| {
                StemResampler::new(*engine_rate, stem.sample_rate, *channels, block_size)
            })
            .collect();
        let expected_frames: Vec<usize> = targets
            .iter()
            .map(|(stem, _, _, engine_rate)| {
                if *engine_rate == 0 || *engine_rate == stem.sample_rate {
                    stem.length_samples
                } else {
                    (stem.length_samples as f64 * stem.sample_rate as f64 / *engine_rate as f64)
                        .round() as usize
                }
            })
            .collect();
        let mut emitted_frames = vec![0_usize; targets.len()];
        // Feed `piece` to every encoder of `stem_idx`, trimming to the exact
        // expected export-rate length.
        let feed_encoders = |encoders: &mut Vec<(usize, StemEncoder)>,
                             stem_idx: usize,
                             piece: &[f32],
                             channels: usize,
                             expected: &[usize],
                             emitted: &mut [usize]| {
            let take = piece
                .len()
                .saturating_div(channels)
                .min(expected[stem_idx].saturating_sub(emitted[stem_idx]));
            if take == 0 {
                return Ok(());
            }
            let slice = &piece[..take.saturating_mul(channels)];
            for (idx, encoder) in encoders.iter_mut() {
                if *idx == stem_idx {
                    encoder.encode_block(slice)?;
                }
            }
            emitted[stem_idx] += take;
            Ok::<(), std::io::Error>(())
        };

        let mut cursor = 0_usize;
        let mut last_progress_report = Instant::now();
        let mut write_error: Option<(String, String)> = None;
        while cursor < pass_length {
            if job.cancel.load(std::sync::atomic::Ordering::Relaxed) {
                drop(encoders);
                cleanup_files(&job.stems.iter().collect::<Vec<_>>());
                restore(&restore_faders);
                crate::report_error(
                    self.tx
                        .send(Message::OfflineBounceFinished {
                            result: Ok(Action::StemsOfflineBounceCanceled),
                        })
                        .await,
                    "Failed to report canceled offline stems bounce",
                );
                crate::report_error(
                    self.tx.send(Message::Ready(self.id)).await,
                    "Failed to return offline bounce worker",
                );
                return;
            }

            let step = (pass_length - cursor).min(block_size);
            let transport_sample = job
                .stems
                .first()
                .map(|stem| stem.start_sample.saturating_add(cursor))
                .unwrap_or(cursor);
            for handle in &render_tracks {
                let mut t = handle.lock();
                t.audio.set_finished(false);
                t.audio.set_processing(false);
                t.set_transport_sample(transport_sample);
                t.set_loop_config(false, None);
                t.set_transport_timing(job.tempo_bpm, job.tsig_num, job.tsig_denom);
                t.set_clip_playback_enabled(true);
                t.set_record_tap_enabled(false);
            }
            for (stem, handle, _, _) in &targets {
                if stem.automation_lanes.is_empty() {
                    continue;
                }
                let mut t = handle.lock();
                Self::apply_freeze_automation_at_sample(
                    &mut t,
                    transport_sample,
                    &stem.automation_lanes,
                );
            }

            offline_render.render(self.id, &render_plan);

            for (idx, (stem, handle, channels, _)) in targets.iter().enumerate() {
                let remaining = stem.length_samples.saturating_sub(frames_written[idx]);
                if remaining == 0 {
                    continue;
                }
                let frames = step.min(remaining);
                let block = &mut scratch[idx];
                block.clear();
                block.reserve(frames * channels);
                {
                    let t = handle.lock();
                    let outs = t.last_audio_outputs();
                    for i in 0..frames {
                        for ch in 0..*channels {
                            block.push(
                                outs.get(ch)
                                    .and_then(|out| out.get(i))
                                    .copied()
                                    .unwrap_or(0.0),
                            );
                        }
                    }
                }
                // Master limiter (stateless clamp) before any conversion.
                if let Some(ceiling_dbtp) = stem.limiter_ceiling {
                    let amp = 10.0_f32.powf(ceiling_dbtp / 20.0).clamp(0.0, 1.0);
                    for sample in block.iter_mut() {
                        *sample = sample.clamp(-amp, amp);
                    }
                }
                let result = if let Some(resampler) = resamplers[idx].as_mut() {
                    resampler.process(block, frames).and_then(|piece| {
                        feed_encoders(
                            &mut encoders,
                            idx,
                            piece,
                            *channels,
                            &expected_frames,
                            &mut emitted_frames,
                        )
                    })
                } else {
                    feed_encoders(
                        &mut encoders,
                        idx,
                        block,
                        *channels,
                        &expected_frames,
                        &mut emitted_frames,
                    )
                };
                if let Err(e) = result {
                    write_error = Some((stem.track_name.clone(), e.to_string()));
                    break;
                }
                frames_written[idx] += frames;
            }
            if let Some((track_name, error)) = write_error.take() {
                drop(encoders);
                cleanup_files(&job.stems.iter().collect::<Vec<_>>());
                restore(&restore_faders);
                crate::report_error(
                    self.tx
                        .send(Message::OfflineBounceFinished {
                            result: Err(format!(
                                "Failed to write offline stems bounce '{track_name}': {error}"
                            )),
                        })
                        .await,
                    "Failed to report offline stems bounce write error",
                );
                crate::report_error(
                    self.tx.send(Message::Ready(self.id)).await,
                    "Failed to return offline bounce worker",
                );
                return;
            }

            cursor = cursor.saturating_add(step);
            let progress = (cursor as f32 / pass_length as f32).clamp(0.0, 1.0);
            if last_progress_report.elapsed() >= Duration::from_millis(50) || cursor >= pass_length
            {
                last_progress_report = Instant::now();
                crate::report_error(
                    self.tx
                        .send(Message::OfflineBounceFinished {
                            result: Ok(Action::StemsOfflineBounceProgress {
                                progress,
                                operation: Some("Rendering stems".to_string()),
                            }),
                        })
                        .await,
                    "Failed to report offline stems bounce progress",
                );
                // Sending to a channel with spare capacity does not suspend
                // this CPU-bound task. Let the dispatcher and GUI subscription
                // run now instead of leaving progress queued until rendering ends.
                tokio::task::yield_now().await;
            }
        }

        // Flush resampler delay tails with silence so every stem reaches its
        // exact export-rate length, then finalize all encoder files.
        for (idx, (stem, _, channels, _)) in targets.iter().enumerate() {
            if resamplers[idx].is_some() {
                let silence = vec![0.0_f32; block_size.saturating_mul(*channels)];
                while emitted_frames[idx] < expected_frames[idx] {
                    let before = emitted_frames[idx];
                    let result = resamplers[idx]
                        .as_mut()
                        .expect("resampler")
                        .process(&silence, block_size);
                    let result = result.and_then(|piece| {
                        feed_encoders(
                            &mut encoders,
                            idx,
                            piece,
                            *channels,
                            &expected_frames,
                            &mut emitted_frames,
                        )
                    });
                    if let Err(e) = result {
                        write_error = Some((stem.track_name.clone(), e.to_string()));
                        break;
                    }
                    if emitted_frames[idx] == before {
                        break;
                    }
                }
            }
            // Guard: pad with silence if the resampler still came up short.
            while emitted_frames[idx] < expected_frames[idx] {
                let take = (expected_frames[idx] - emitted_frames[idx]).min(block_size);
                let silence = vec![0.0_f32; take.saturating_mul(*channels)];
                if let Err(e) = feed_encoders(
                    &mut encoders,
                    idx,
                    &silence,
                    *channels,
                    &expected_frames,
                    &mut emitted_frames,
                ) {
                    write_error = Some((stem.track_name.clone(), e.to_string()));
                    break;
                }
            }
            if write_error.is_some() {
                break;
            }
        }
        let mut finalize_error: Option<(String, String)> = None;
        for (stem_idx, encoder) in encoders {
            if let Err(e) = encoder.finish() {
                finalize_error = Some((job.stems[stem_idx].track_name.clone(), e.to_string()));
                break;
            }
        }
        if let Some((output_path, error)) = write_error.take().or(finalize_error) {
            cleanup_files(&job.stems.iter().collect::<Vec<_>>());
            restore(&restore_faders);
            crate::report_error(
                self.tx
                    .send(Message::OfflineBounceFinished {
                        result: Err(format!(
                            "Failed to finalize offline stems bounce '{output_path}': {error}"
                        )),
                    })
                    .await,
                "Failed to report offline stems bounce finalize error",
            );
            crate::report_error(
                self.tx.send(Message::Ready(self.id)).await,
                "Failed to return offline bounce worker",
            );
            return;
        }
        restore(&restore_faders);

        crate::report_error(
            self.tx
                .send(Message::OfflineBounceFinished {
                    result: Ok(Action::StemsOfflineBounced { stems: job.stems }),
                })
                .await,
            "Failed to report completed offline stems bounce",
        );
        crate::report_error(
            self.tx.send(Message::Ready(self.id)).await,
            "Failed to return offline bounce worker",
        );
    }

    #[cfg(unix)]
    pub(crate) fn try_enable_realtime(priority: i32) -> Result<(), String> {
        let thread = unsafe { libc::pthread_self() };
        let policy = libc::SCHED_FIFO;
        let param = unsafe {
            let mut p = std::mem::zeroed::<libc::sched_param>();
            p.sched_priority = priority;
            p
        };
        let rc = unsafe { libc::pthread_setschedparam(thread, policy, &param) };
        if rc == 0 {
            Ok(())
        } else {
            Err(format!("pthread_setschedparam failed with errno {}", rc))
        }
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn try_enable_realtime(_priority: i32) -> Result<(), String> {
        use std::{cell::Cell, ffi::OsStr, os::windows::ffi::OsStrExt};

        #[link(name = "avrt")]
        unsafe extern "system" {
            fn AvSetMmThreadCharacteristicsW(task_name: *const u16, task_index: *mut u32) -> isize;
        }

        thread_local! {
            static MMCSS_TASK_HANDLE: Cell<isize> = const { Cell::new(0) };
        }

        MMCSS_TASK_HANDLE.with(|handle| {
            if handle.get() != 0 {
                return Ok(());
            }

            let task_name: Vec<u16> = OsStr::new("Pro Audio")
                .encode_wide()
                .chain(Some(0))
                .collect();
            let mut task_index = 0_u32;
            let mmcss_handle =
                unsafe { AvSetMmThreadCharacteristicsW(task_name.as_ptr(), &mut task_index) };
            if mmcss_handle == 0 {
                Err(format!(
                    "AvSetMmThreadCharacteristicsW(Pro Audio) failed: {}",
                    std::io::Error::last_os_error()
                ))
            } else {
                handle.set(mmcss_handle);
                Ok(())
            }
        })
    }

    #[cfg(all(not(unix), not(target_os = "windows")))]
    pub(crate) fn try_enable_realtime(_priority: i32) -> Result<(), String> {
        Err("Realtime thread priority is not supported on this platform".to_string())
    }

    pub async fn new(
        id: usize,
        rx: Receiver<Message>,
        tx: Sender<Message>,
        realtime_priority: i32,
    ) -> Worker {
        let worker = Worker {
            id,
            rx,
            tx,
            realtime_priority,
        };
        worker.send(Message::Ready(id)).await;
        worker
    }

    pub async fn send(&self, message: Message) {
        self.tx
            .send(message)
            .await
            .expect("Failed to send message from worker");
    }

    /// Unbound a plan-arena slice: the arena outlives the node execution,
    /// and the caller must not access the returned slice after the node
    /// call that created it ends.
    unsafe fn unbound_slice(slice: &[f32]) -> &'static [f32] {
        unsafe { std::slice::from_raw_parts(slice.as_ptr(), slice.len()) }
    }

    /// Run `f` with the executing thread's node scratch. Each worker thread
    /// runs nodes sequentially, so a thread-local gives per-worker reuse
    /// without changing `process_node_job_result`'s signature at its four
    /// call sites. All buffers are cleared up front; arms fill them with
    /// `extend` (reusing capacity).
    fn with_node_scratch<R>(f: impl FnOnce(&mut NodeScratch) -> R) -> R {
        thread_local! {
            static NODE_SCRATCH: std::cell::RefCell<NodeScratch> =
                std::cell::RefCell::new(NodeScratch::default());
        }
        NODE_SCRATCH.with(|scratch| {
            let mut scratch = scratch.borrow_mut();
            scratch.input_ptrs.clear();
            scratch.output_ptrs.clear();
            scratch.inputs_mut.clear();
            scratch.inputs_shared.clear();
            scratch.outputs_mut.clear();
            scratch.source_buffers.clear();
            f(&mut scratch)
        })
    }

    #[cfg(feature = "plugins")]
    fn collect_arena_input_slices(
        plan: &crate::render_plan::RenderPlan,
        ins: &[crate::render_plan::BufferId],
        out: &mut Vec<&'static [f32]>,
    ) {
        out.extend(ins.iter().map(|&buf| {
            // Safety: the plan dispatched this task only after every
            // producer of the input buffer completed. The unbound slice is
            // only read while this node executes.
            unsafe { Self::unbound_slice(plan.buffer(buf)) }
        }));
    }

    fn collect_arena_source_slices(
        plan: &crate::render_plan::RenderPlan,
        sources: &[Arc<crate::audio::io::AudioIO>],
        writable: &[crate::render_plan::BufferId],
        out: &mut Vec<(usize, &'static [f32], usize)>,
    ) {
        out.extend(sources.iter().filter_map(|source| {
            let key = Arc::as_ptr(source) as usize;
            let &buf = plan.port_map.get(&key)?;
            if writable.contains(&buf) {
                return None;
            }
            // Safety: only the folder's inputs, child outputs and plugin
            // outputs are borrowed. Their producers precede folder output
            // in the DAG. Borrowing every arena buffer here would alias
            // writes from unrelated nodes running on other workers.
            let slice = unsafe { Self::unbound_slice(plan.buffer(buf)) };
            Some((key, slice, plan.buffer_latency(buf)))
        }));
    }

    fn metronome_output_buffer(
        plan: &crate::render_plan::RenderPlan,
        t: &crate::track::TrackData,
        outs: &[crate::render_plan::BufferId],
    ) -> Option<crate::render_plan::BufferId> {
        let source = t.metronome_source()?;
        let key = Arc::as_ptr(&source) as usize;
        let &buf = plan.port_map.get(&key)?;
        outs.contains(&buf).then_some(buf)
    }

    /// Execute one node of a render plan (Phase 2, see `LOCKLESS.md`).
    ///
    /// `Sum`/`Zero` are pure arena ops. Task nodes run the legacy track body
    /// (it still re-sums its inputs from the port graph — identical to the
    /// plan's `Sum` result) and then copy their output ports into the arena
    /// so downstream `Sum` nodes and the hardware drain see the result.
    pub(crate) fn process_node_job_result(worker_id: usize, job: NodeJob) -> NodeJobResult {
        let NodeJob { epoch, plan, node } = job;
        let (output_linear, parameter_updates, latency_changed) =
            Self::with_node_scratch(|scratch| {
                match &plan.nodes[node as usize] {
                    Op::Zero { output } => {
                        // Safety: this worker executes this node; the plan's
                        // single-producer-chain invariant guarantees exclusive
                        // access to the output buffer.
                        unsafe { &mut *plan.buffer_ptr(*output) }.fill(0.0);
                        plan.set_buffer_latency(*output, 0);
                        (Vec::new(), Vec::new(), false)
                    }
                    Op::Sum {
                        inputs,
                        delays,
                        output,
                    } => {
                        // Safety: see `Op::Zero`; additionally, every input buffer's
                        // producer completed before this node was dispatched.
                        let out = unsafe { &mut *plan.buffer_ptr(*output) };
                        out.fill(0.0);
                        let max_latency = inputs
                            .iter()
                            .map(|&input| plan.buffer_latency(input))
                            .max()
                            .unwrap_or(0);
                        for (idx, &input) in inputs.iter().enumerate() {
                            let src = unsafe { plan.buffer(input) };
                            let delay = max_latency.saturating_sub(plan.buffer_latency(input));
                            // Safety: this Sum node is the only writer of its delay
                            // lines during this cycle, and the executor never
                            // dispatches the same node concurrently.
                            let line = unsafe { &mut *delays[idx].get() };
                            line.process(src, delay, out, idx != 0);
                        }
                        plan.set_buffer_latency(*output, max_latency);
                        (Vec::new(), Vec::new(), false)
                    }
                    Op::HwInput { output, .. } => {
                        // The hardware driver wrote this buffer before the cycle
                        // started; nothing to do.
                        plan.set_buffer_latency(*output, 0);
                        (Vec::new(), Vec::new(), false)
                    }
                    Op::IoDelayGenerator { node, output } => {
                        // Safety: this worker owns the generator output buffer.
                        let out_buf = unsafe { &mut *plan.buffer_ptr(*output) };
                        node.process_generator(out_buf);
                        plan.set_buffer_latency(*output, 0);
                        (Vec::new(), Vec::new(), false)
                    }
                    Op::IoDelayMeasurement { node, input } => {
                        // Safety: this worker reads the input after its Sum node completed.
                        let in_buf = unsafe { plan.buffer(*input) };
                        node.process(in_buf);
                        (Vec::new(), Vec::new(), false)
                    }
                    Op::Task { task, ins, outs } => {
                        let track = match task {
                            ProcessTask::Track(t)
                            | ProcessTask::FolderInput(t)
                            | ProcessTask::FolderOutput(t) => t,
                            #[cfg(feature = "plugins")]
                            ProcessTask::Plugin { track, .. } => track,
                        };
                        let mut t = track.lock();
                        // The dispatcher may skip `prepare_task_track` entirely
                        // (generation fast path), so the worker marks the track as
                        // processing for the whole task; task bodies clear the flag
                        // at the end. Plugin-mutation handlers rely on this flag to
                        // reject changes while a task holds the track.
                        t.audio.set_processing(true);
                        match task {
                            ProcessTask::Track(_) => {
                                let audio_out_count = t.audio.outs.len();
                                let metronome_output =
                                    Self::metronome_output_buffer(&plan, &t, outs);
                                // Safety: track tasks are registered as in-place
                                // writers for their input buffers.
                                scratch
                                    .input_ptrs
                                    .extend(ins.iter().map(|&buf| unsafe { plan.buffer_ptr(buf) }));
                                // Safety: each pointer came from a task input buffer
                                // this node owns in-place.
                                scratch.inputs_mut.extend(
                                    scratch
                                        .input_ptrs
                                        .iter()
                                        .map(|&ptr| unsafe { (&mut *ptr).as_mut_slice() }),
                                );
                                // The track body appends its own inputs and
                                // inline plugin outputs after processing them.
                                // Do not borrow arena inputs while inputs_mut
                                // owns them, or unrelated workers' buffers.
                                // Keep only input latency metadata here; the
                                // track replaces the empty slices after its
                                // in-place input processing has finished.
                                scratch
                                    .source_buffers
                                    .extend(t.audio.ins.iter().zip(ins).map(|(port, &buffer)| {
                                        (
                                            Arc::as_ptr(port) as usize,
                                            &[][..],
                                            plan.buffer_latency(buffer),
                                        )
                                    }));
                                // Safety: this worker executes the unique producer
                                // node for each output buffer.
                                scratch.output_ptrs.extend(
                                    outs.iter()
                                        .take(audio_out_count)
                                        .map(|&buf| unsafe { plan.buffer_ptr(buf) }),
                                );
                                // Safety: each pointer came from a distinct task
                                // output buffer owned by this node.
                                scratch.outputs_mut.extend(
                                    scratch
                                        .output_ptrs
                                        .iter()
                                        .map(|&ptr| unsafe { (&mut *ptr).as_mut_slice() }),
                                );
                                let metronome_output_ptr = metronome_output.map(|buf| {
                                    // Safety: the track task is the registered
                                    // producer of the metronome side-output buffer.
                                    unsafe { plan.buffer_ptr(buf) }
                                });
                                let metronome_output = metronome_output_ptr.map(|ptr| {
                                    // Safety: the side-output buffer is excluded from
                                    // the normal audio output slice above.
                                    unsafe { (&mut *ptr).as_mut_slice() }
                                });
                                t.process_render_block_with_audio_buffers_and_metronome(
                                    &mut scratch.inputs_mut,
                                    &mut scratch.outputs_mut,
                                    &mut scratch.source_buffers,
                                    metronome_output,
                                );
                                #[cfg(feature = "plugins")]
                                for &out in outs.iter().take(audio_out_count) {
                                    plan.set_buffer_latency(out, t.plugin_graph_latency_samples());
                                }
                            }
                            ProcessTask::FolderInput(_) => {
                                let metronome_output =
                                    Self::metronome_output_buffer(&plan, &t, outs);
                                // Safety: folder-input tasks are registered as
                                // in-place writers for their input buffers.
                                scratch
                                    .input_ptrs
                                    .extend(ins.iter().map(|&buf| unsafe { plan.buffer_ptr(buf) }));
                                // Safety: each pointer came from a task input buffer
                                // this node owns in-place.
                                scratch.inputs_mut.extend(
                                    scratch
                                        .input_ptrs
                                        .iter()
                                        .map(|&ptr| unsafe { (&mut *ptr).as_mut_slice() }),
                                );
                                let metronome_output_ptr = metronome_output.map(|buf| {
                                    // Safety: the folder-input task is the registered
                                    // producer of the metronome side-output buffer.
                                    unsafe { plan.buffer_ptr(buf) }
                                });
                                let metronome_output = metronome_output_ptr.map(|ptr| {
                                    // Safety: this buffer is a side output, distinct
                                    // from the folder input buffers.
                                    unsafe { (&mut *ptr).as_mut_slice() }
                                });
                                t.process_folder_input_with_audio_buffers_and_metronome(
                                    &mut scratch.inputs_mut,
                                    metronome_output,
                                );
                                for &input in ins {
                                    plan.set_buffer_latency(input, 0);
                                }
                            }
                            ProcessTask::FolderOutput(_) => {
                                Self::collect_arena_source_slices(
                                    &plan,
                                    &t.internal_audio_sources(),
                                    outs,
                                    &mut scratch.source_buffers,
                                );
                                // Safety: this worker executes the unique producer
                                // node for each output buffer.
                                scratch.output_ptrs.extend(
                                    outs.iter().map(|&buf| unsafe { plan.buffer_ptr(buf) }),
                                );
                                // Safety: each pointer came from a distinct task
                                // output buffer owned by this node.
                                scratch.outputs_mut.extend(
                                    scratch
                                        .output_ptrs
                                        .iter()
                                        .map(|&ptr| unsafe { (&mut *ptr).as_mut_slice() }),
                                );
                                t.process_folder_output_with_audio_buffers(
                                    &mut scratch.outputs_mut,
                                    &scratch.source_buffers,
                                );
                                #[cfg(feature = "plugins")]
                                for &out in outs {
                                    plan.set_buffer_latency(out, t.plugin_graph_latency_samples());
                                }
                            }
                            #[cfg(feature = "plugins")]
                            ProcessTask::Plugin { kind, index, .. } => {
                                let input_latency = ins
                                    .iter()
                                    .map(|&input| plan.buffer_latency(input))
                                    .max()
                                    .unwrap_or(0);
                                Self::collect_arena_input_slices(
                                    &plan,
                                    ins,
                                    &mut scratch.inputs_shared,
                                );
                                // Safety: this worker executes the unique producer
                                // node for each output buffer.
                                scratch.output_ptrs.extend(
                                    outs.iter().map(|&buf| unsafe { plan.buffer_ptr(buf) }),
                                );
                                // Safety: each pointer came from a distinct task
                                // output buffer owned by this node.
                                scratch.outputs_mut.extend(
                                    scratch
                                        .output_ptrs
                                        .iter()
                                        .map(|&ptr| unsafe { (&mut *ptr).as_mut_slice() }),
                                );
                                t.process_plugin_with_audio_buffers(
                                    *kind,
                                    *index,
                                    &scratch.inputs_shared,
                                    &mut scratch.outputs_mut,
                                );
                                let latency = input_latency
                                    .saturating_add(t.plugin_latency_samples(*kind, *index));
                                for &out in outs {
                                    plan.set_buffer_latency(out, latency);
                                }
                            }
                        }
                        t.audio.set_processing(false);
                        #[cfg(feature = "plugins")]
                        let latency_changed = t.take_plugin_latency_changed();
                        #[cfg(not(feature = "plugins"))]
                        let latency_changed = false;
                        let updates = std::mem::take(&mut t.rt.echoed_parameter_updates);
                        let meter = t.output_meter_linear();
                        (meter, updates, latency_changed)
                    }
                }
            });
        NodeJobResult {
            worker_id,
            epoch,
            node,
            output_linear,
            parameter_updates,
            latency_changed,
        }
    }

    async fn process_node_job(&self, job: NodeJob) {
        let result = Self::process_node_job_result(self.id, job);
        if let Err(e) = self.tx.send(result.into()).await {
            tracing::warn!(
                "Worker {} failed to report completed node job: {e}",
                self.id
            );
        }
    }

    pub async fn work(&mut self) {
        crate::enable_flush_denormals_to_zero();
        if let Err(e) = Self::try_enable_realtime(self.realtime_priority) {
            tracing::warn!(
                "Worker {} realtime priority {} not enabled: {}",
                self.id,
                self.realtime_priority,
                e
            );
        }
        while let Some(message) = self.rx.recv().await {
            match message {
                Message::Request(Action::Quit) => {
                    return;
                }
                Message::ProcessOfflineBounce(job) => {
                    self.process_offline_bounce(job).await;
                }
                Message::ProcessOfflineStemsBounce(job) => {
                    self.process_offline_stems_bounce(job).await;
                }
                Message::NodeJob(job) => {
                    self.process_node_job(job).await;
                }
                _ => {}
            }
        }
    }
}

impl From<NodeJobResult> for Message {
    fn from(result: NodeJobResult) -> Self {
        Message::NodeDone {
            worker_id: result.worker_id,
            epoch: result.epoch,
            node: result.node,
            output_linear: result.output_linear,
            parameter_updates: result.parameter_updates,
            latency_changed: result.latency_changed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Worker;
    use crate::message::{
        Action, Message, OfflineAutomationLane, OfflineAutomationPoint, OfflineAutomationTarget,
        OfflineBounceWork,
    };
    use crate::state::State;
    use crate::track::Track;
    use std::path::PathBuf;
    use std::sync::{Arc, atomic::AtomicBool};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::sync::mpsc::channel;

    fn make_state_with_track(track: Track) -> State {
        let mut state = State::default();
        state.tracks.insert(track.name.clone(), Arc::new(track));
        state
    }

    fn unique_temp_wav(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!("maolan_{name}_{nanos}.wav"))
    }

    #[test]
    fn prepare_track_for_freeze_render_neutralizes_level_and_balance() {
        let mut track = Track::new("track".to_string(), 1, 2, 0, 0, 64, 48_000.0);
        track.set_level(-6.0);
        track.set_balance(0.35);

        let (level, balance) = Worker::prepare_track_for_freeze_render(&mut track);

        assert_eq!(level, -6.0);
        assert_eq!(balance, 0.35);
        assert_eq!(track.level(), 0.0);
        assert_eq!(track.balance(), 0.0);

        Worker::restore_track_after_freeze_render(&mut track, level, balance);
        assert_eq!(track.level(), -6.0);
        assert_eq!(track.balance(), 0.35);
    }

    #[test]
    fn freeze_automation_ignores_volume_and_balance_lanes() {
        let mut track = Track::new("track".to_string(), 1, 2, 0, 1, 64, 48_000.0);
        let lanes = vec![
            OfflineAutomationLane {
                target: OfflineAutomationTarget::Volume,
                visible: true,
                points: vec![OfflineAutomationPoint {
                    sample: 0,
                    value: 0.0,
                }],
            },
            OfflineAutomationLane {
                target: OfflineAutomationTarget::Balance,
                visible: true,
                points: vec![OfflineAutomationPoint {
                    sample: 0,
                    value: 1.0,
                }],
            },
            OfflineAutomationLane {
                target: OfflineAutomationTarget::MidiCc { channel: 0, cc: 7 },
                visible: true,
                points: vec![OfflineAutomationPoint {
                    sample: 0,
                    value: 1.0,
                }],
            },
        ];

        Worker::apply_freeze_automation_at_sample(&mut track, 0, &lanes);

        assert_eq!(track.level(), 0.0);
        assert_eq!(track.balance(), 0.0);
        assert_eq!(track.rt.pending_automation_midi_events.len(), 1);
        assert_eq!(
            track.rt.pending_automation_midi_events[0].data,
            vec![0xB0, 7, 127]
        );
    }

    #[test]
    fn automation_lane_value_at_interpolates_between_points() {
        let lane = OfflineAutomationLane {
            target: OfflineAutomationTarget::Volume,
            visible: true,
            points: vec![
                OfflineAutomationPoint {
                    sample: 10,
                    value: 0.25,
                },
                OfflineAutomationPoint {
                    sample: 20,
                    value: 0.75,
                },
            ],
        };
        let value = lane.value_at(15).expect("value");

        assert!((value - 0.5).abs() < 1.0e-6);
    }

    #[test]
    fn freeze_automation_applies_interpolated_midi_cc_lane() {
        let mut track = Track::new("track".to_string(), 1, 1, 0, 1, 64, 48_000.0);
        let lanes = vec![OfflineAutomationLane {
            target: OfflineAutomationTarget::MidiCc { channel: 0, cc: 7 },
            visible: true,
            points: vec![
                OfflineAutomationPoint {
                    sample: 0,
                    value: 0.0,
                },
                OfflineAutomationPoint {
                    sample: 10,
                    value: 1.0,
                },
            ],
        }];

        Worker::apply_freeze_automation_at_sample(&mut track, 5, &lanes);
        assert_eq!(track.rt.pending_automation_midi_events.len(), 1);
        assert_eq!(track.rt.pending_automation_midi_events[0].data[2], 64);

        track.rt.pending_automation_midi_events.clear();
        Worker::apply_freeze_automation_at_sample(&mut track, 2, &lanes);
        assert_eq!(track.rt.pending_automation_midi_events.len(), 1);
        assert_eq!(track.rt.pending_automation_midi_events[0].data[2], 25);
    }

    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "Tokio runtime uses kqueue, which Miri does not support on FreeBSD"
    )]
    #[tokio::test]
    async fn process_node_job_sums_arena_buffers() {
        use crate::render_plan::{Op, RenderPlan};
        use std::cell::UnsafeCell;
        use std::collections::HashMap;

        let (_rx_unused_tx, rx_unused) = channel(1);
        let (tx, mut out_rx) = channel(8);
        let worker = Worker {
            id: 4,
            rx: rx_unused,
            tx,
            realtime_priority: 0,
        };
        let collector = basedrop::Collector::new();
        let plan = RenderPlan {
            buffer_size: 4,
            buffers: (0..3).map(|_| UnsafeCell::new(vec![0.0; 4])).collect(),
            buffer_latencies: (0..3)
                .map(|_| std::sync::atomic::AtomicUsize::new(0))
                .collect(),
            nodes: vec![Op::Sum {
                inputs: vec![0, 1],
                delays: vec![
                    UnsafeCell::new(crate::render_plan::DelayLine::new()),
                    UnsafeCell::new(crate::render_plan::DelayLine::new()),
                ],
                output: 2,
            }],
            indegree: vec![0],
            dependents: vec![vec![]],
            sources: vec![0],
            hw_in_map: vec![],
            hw_out_map: vec![],
            port_map: HashMap::new(),
            midi_edges: vec![],
            forced: vec![],
        };
        // Safety: test thread, no node is executing yet.
        unsafe {
            (&mut *plan.buffer_ptr(0)).copy_from_slice(&[0.25, 0.5, 0.75, 1.0]);
            (&mut *plan.buffer_ptr(1)).copy_from_slice(&[0.75, 0.5, 0.25, f32::NAN]);
        }
        let shared = std::sync::Arc::new(basedrop::Owned::new(&collector.handle(), plan));

        worker
            .process_node_job(crate::executor::NodeJob {
                epoch: 1,
                plan: shared.clone(),
                node: 0,
            })
            .await;

        // Safety: the job completed, so the Sum node's writes are done.
        // The NaN in the second source is sanitized to 0 before adding.
        unsafe {
            assert_eq!(
                &*shared.buffer_ptr(2),
                &vec![1.0, 1.0, 1.0, 1.0],
                "sanitized sum in the arena"
            );
        }
        match out_rx.recv().await.expect("message") {
            Message::NodeDone {
                worker_id,
                epoch,
                node,
                output_linear,
                ..
            } => {
                assert_eq!(worker_id, 4);
                assert_eq!(epoch, 1);
                assert_eq!(node, 0);
                assert!(output_linear.is_empty());
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "Tokio runtime uses kqueue, which Miri does not support on FreeBSD"
    )]
    #[tokio::test]
    async fn process_offline_bounce_errors_when_track_is_missing() {
        let (_rx_unused_tx, rx_unused) = channel(1);
        let (tx, mut out_rx) = channel(8);
        let worker = Worker {
            id: 7,
            rx: rx_unused,
            tx,
            realtime_priority: 0,
        };
        let job = OfflineBounceWork {
            state: Arc::new(State::default().snapshot()),
            track_name: "missing".to_string(),
            output_path: unique_temp_wav("missing").to_string_lossy().to_string(),
            start_sample: 0,
            length_samples: 8,
            tempo_bpm: 120.0,
            tsig_num: 4,
            tsig_denom: 4,
            automation_lanes: vec![],
            cancel: Arc::new(AtomicBool::new(false)),
            apply_fader: false,
        };

        worker.process_offline_bounce(job).await;

        match out_rx.recv().await.expect("message") {
            Message::OfflineBounceFinished { result: Err(err) } => {
                assert!(err.contains("Track not found: missing"));
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "Tokio runtime uses kqueue, which Miri does not support on FreeBSD"
    )]
    #[tokio::test]
    async fn process_offline_bounce_cancels_and_restores_track_state() {
        let (_rx_unused_tx, rx_unused) = channel(1);
        let (tx, mut out_rx) = channel(8);
        let worker = Worker {
            id: 5,
            rx: rx_unused,
            tx,
            realtime_priority: 0,
        };
        let track = Track::new("track".to_string(), 1, 2, 0, 0, 4, 48_000.0);
        track.set_level(-9.0);
        track.set_balance(-0.3);
        let state = make_state_with_track(track);
        let job = OfflineBounceWork {
            state: Arc::new(state.lock().snapshot()),
            track_name: "track".to_string(),
            output_path: unique_temp_wav("cancel").to_string_lossy().to_string(),
            start_sample: 0,
            length_samples: 8,
            tempo_bpm: 120.0,
            tsig_num: 4,
            tsig_denom: 4,
            automation_lanes: vec![],
            cancel: Arc::new(AtomicBool::new(true)),
            apply_fader: false,
        };

        worker.process_offline_bounce(job).await;

        match out_rx.recv().await.expect("message") {
            Message::OfflineBounceFinished {
                result: Ok(Action::TrackOfflineBounceCanceled { track_name }),
            } => assert_eq!(track_name, "track"),
            other => panic!("unexpected message: {other:?}"),
        }
        assert!(matches!(out_rx.recv().await, Some(Message::Ready(5))));
        let state_guard = state.lock();
        let track = state_guard.tracks.get("track").expect("track").lock();
        assert_eq!(track.level(), -9.0);
        assert_eq!(track.balance(), -0.3);
    }

    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "Tokio runtime uses kqueue, which Miri does not support on FreeBSD"
    )]
    #[tokio::test]
    async fn process_offline_bounce_restores_track_state_on_write_failure() {
        let (_rx_unused_tx, rx_unused) = channel(1);
        let (tx, mut out_rx) = channel(8);
        let worker = Worker {
            id: 3,
            rx: rx_unused,
            tx,
            realtime_priority: 0,
        };
        let track = Track::new("track".to_string(), 1, 2, 0, 0, 4, 48_000.0);
        track.set_level(-4.0);
        track.set_balance(0.25);
        let state = make_state_with_track(track);
        let output_path = std::env::temp_dir().to_string_lossy().to_string();
        let job = OfflineBounceWork {
            state: Arc::new(state.lock().snapshot()),
            track_name: "track".to_string(),
            output_path,
            start_sample: 0,
            length_samples: 4,
            tempo_bpm: 120.0,
            tsig_num: 4,
            tsig_denom: 4,
            automation_lanes: vec![],
            cancel: Arc::new(AtomicBool::new(false)),
            apply_fader: false,
        };

        worker.process_offline_bounce(job).await;

        let mut saw_error = false;
        while let Some(message) = out_rx.recv().await {
            match message {
                Message::OfflineBounceFinished {
                    result: Ok(Action::TrackOfflineBounceProgress { .. }),
                } => {}
                Message::OfflineBounceFinished { result: Err(err) } => {
                    assert!(
                        err.contains("Failed to create offline bounce")
                            || err.contains("Failed to write offline bounce")
                            || err.contains("Failed to finalize offline bounce")
                    );
                    saw_error = true;
                }
                Message::Ready(3) => break,
                other => panic!("unexpected message: {other:?}"),
            }
        }
        assert!(saw_error);
        let state_guard = state.lock();
        let track = state_guard.tracks.get("track").expect("track").lock();
        assert_eq!(track.level(), -4.0);
        assert_eq!(track.balance(), 0.25);
    }

    #[cfg(feature = "audio")]
    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "Tokio runtime uses kqueue, which Miri does not support on FreeBSD"
    )]
    #[tokio::test]
    async fn process_offline_bounce_emits_progress_and_completion() {
        let (_rx_unused_tx, rx_unused) = channel(1);
        let (tx, mut out_rx) = channel(16);
        let worker = Worker {
            id: 2,
            rx: rx_unused,
            tx,
            realtime_priority: 0,
        };
        let track = Track::new("track".to_string(), 1, 1, 0, 0, 4, 48_000.0);
        track.set_level(-3.0);
        track.set_balance(0.4);
        let state = make_state_with_track(track);
        let output = unique_temp_wav("success");
        let job = OfflineBounceWork {
            state: Arc::new(state.lock().snapshot()),
            track_name: "track".to_string(),
            output_path: output.to_string_lossy().to_string(),
            start_sample: 0,
            length_samples: 8,
            tempo_bpm: 120.0,
            tsig_num: 4,
            tsig_denom: 4,
            automation_lanes: vec![],
            cancel: Arc::new(AtomicBool::new(false)),
            apply_fader: false,
        };

        worker.process_offline_bounce(job).await;

        let mut saw_progress = false;
        let mut saw_complete = false;
        let mut saw_ready = false;
        while let Some(message) = out_rx.recv().await {
            match message {
                Message::OfflineBounceFinished {
                    result:
                        Ok(Action::TrackOfflineBounceProgress {
                            track_name,
                            progress,
                            ..
                        }),
                } => {
                    assert_eq!(track_name, "track");
                    assert!(progress > 0.0);
                    saw_progress = true;
                }
                Message::OfflineBounceFinished {
                    result:
                        Ok(Action::TrackOfflineBounce {
                            track_name,
                            output_path,
                            ..
                        }),
                } => {
                    assert_eq!(track_name, "track");
                    assert_eq!(output_path, output.to_string_lossy());
                    saw_complete = true;
                }
                Message::Ready(2) => {
                    saw_ready = true;
                    break;
                }
                other => panic!("unexpected message: {other:?}"),
            }
        }

        assert!(saw_progress);
        assert!(saw_complete);
        assert!(saw_ready);
        assert!(output.exists());
        std::fs::remove_file(&output).expect("remove temp wav");
        let state_guard = state.lock();
        let track = state_guard.tracks.get("track").expect("track").lock();
        assert_eq!(track.level(), -3.0);
        assert_eq!(track.balance(), 0.4);
        assert!(!track.muted());
    }

    /// Read the interleaved f32 payload of a float WAV written by
    /// `WavStreamWriter`, scanning chunks for the data section so the
    /// test needs no codec.
    fn read_wav_f32_payload(path: &PathBuf) -> Vec<f32> {
        let bytes = std::fs::read(path).expect("read wav stem");
        let mut pos = 12_usize;
        while pos + 8 <= bytes.len() {
            let id = &bytes[pos..pos + 4];
            let size = u32::from_le_bytes([
                bytes[pos + 4],
                bytes[pos + 5],
                bytes[pos + 6],
                bytes[pos + 7],
            ]) as usize;
            let start = pos + 8;
            if id == b"data" {
                let payload = &bytes[start..(start + size).min(bytes.len())];
                return payload
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
                    .collect();
            }
            pos = start + size + (size & 1);
        }
        panic!("no data chunk in {}", path.display());
    }

    /// Build a track whose clip audio comes from a pre-populated buffer
    /// cache, so the test needs no codec or files on disk.
    fn stems_test_track(name: &str, clip_value: f32, level_db: f32) -> Track {
        use crate::audio::clip::AudioClip;
        use crate::track::AudioClipBuffer;
        let frames = 128_usize;
        let mut track = Track::new(name.to_string(), 2, 2, 0, 0, 64, 48_000.0);
        let mut clip = AudioClip::new(format!("{name}_clip"), 0, frames);
        clip.fade_enabled = false;
        track.audio.push_clip(clip);
        track.insert_audio_clip_buffer_for_test(
            &format!("{name}_clip"),
            Arc::new(AudioClipBuffer::Buffered {
                channels: 2,
                samples: vec![clip_value; frames * 2],
            }),
        );
        track.set_level(level_db);
        track
    }

    fn stem_output(
        path: &std::path::Path,
        depth: crate::wav_stream::WavBitDepth,
    ) -> crate::message::OfflineStemOutput {
        crate::message::OfflineStemOutput {
            path: path.to_string_lossy().to_string(),
            format: crate::wav_stream::AudioEncodeFormat::Wav(depth),
            dither: crate::wav_stream::AudioDither::None,
        }
    }

    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "Tokio runtime uses kqueue, which Miri does not support on FreeBSD"
    )]
    #[tokio::test]
    async fn process_offline_stems_bounce_captures_every_track_in_one_pass() {
        let (_rx_unused_tx, rx_unused) = channel(1);
        let (tx, mut out_rx) = channel(32);
        let worker = Worker {
            id: 6,
            rx: rx_unused,
            tx,
            realtime_priority: 0,
        };
        let track_a = stems_test_track("stem_a", 0.5, 0.0);
        let track_b = stems_test_track("stem_b", 0.5, -6.0);
        let mut state = State::default();
        state.tracks.insert(track_a.name.clone(), Arc::new(track_a));
        state.tracks.insert(track_b.name.clone(), Arc::new(track_b));
        let output_a = unique_temp_wav("stems_a");
        let output_b = unique_temp_wav("stems_b");
        let job = crate::message::OfflineStemsBounceWork {
            state: Arc::new(state.lock().snapshot()),
            stems: vec![
                crate::message::OfflineBounceStem {
                    track_name: "stem_a".to_string(),
                    start_sample: 0,
                    length_samples: 128,
                    sample_rate: 48_000,
                    limiter_ceiling: None,
                    automation_lanes: vec![],
                    apply_fader: true,
                    outputs: vec![stem_output(
                        &output_a,
                        crate::wav_stream::WavBitDepth::Float32,
                    )],
                },
                crate::message::OfflineBounceStem {
                    track_name: "stem_b".to_string(),
                    start_sample: 0,
                    length_samples: 96,
                    sample_rate: 48_000,
                    limiter_ceiling: None,
                    automation_lanes: vec![],
                    apply_fader: true,
                    outputs: vec![stem_output(
                        &output_b,
                        crate::wav_stream::WavBitDepth::Float32,
                    )],
                },
            ],
            tempo_bpm: 120.0,
            tsig_num: 4,
            tsig_denom: 4,
            cancel: Arc::new(AtomicBool::new(false)),
        };

        let finished = AtomicBool::new(false);
        tokio::join!(
            biased;
            async {
                worker.process_offline_stems_bounce(job).await;
                finished.store(true, std::sync::atomic::Ordering::Relaxed);
            },
            async {
                let mut saw_progress = false;
                let mut saw_complete = false;
                while let Some(message) = out_rx.recv().await {
                    match message {
                        Message::OfflineBounceFinished {
                            result: Ok(Action::StemsOfflineBounceProgress { .. }),
                        } => {
                            assert!(
                                !finished.load(std::sync::atomic::Ordering::Relaxed),
                                "progress must reach consumers before the bounce finishes"
                            );
                            saw_progress = true;
                        }
                        Message::OfflineBounceFinished {
                            result: Ok(Action::StemsOfflineBounced { stems }),
                        } => {
                            assert_eq!(stems.len(), 2);
                            saw_complete = true;
                        }
                        Message::Ready(6) => break,
                        other => panic!("unexpected message: {other:?}"),
                    }
                }
                assert!(saw_progress);
                assert!(saw_complete);
            }
        );

        // The pass runs to the longest stem (128 frames); the shorter stem
        // stops at its own length.
        let samples_a = read_wav_f32_payload(&output_a);
        assert_eq!(samples_a.len(), 128 * 2);
        let expected_a = 0.5_f32;
        for sample in &samples_a {
            assert!(
                (sample - expected_a).abs() < 1.0e-4,
                "stem_a sample {sample} != {expected_a}"
            );
        }
        let samples_b = read_wav_f32_payload(&output_b);
        assert_eq!(samples_b.len(), 96 * 2);
        let expected_b = 0.5 * 10.0_f32.powf(-6.0 / 20.0);
        for sample in &samples_b {
            assert!(
                (sample - expected_b).abs() < 1.0e-4,
                "stem_b sample {sample} != {expected_b}"
            );
        }
        std::fs::remove_file(&output_a).expect("remove stem a");
        std::fs::remove_file(&output_b).expect("remove stem b");
    }

    #[cfg(feature = "audio")]
    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "Tokio runtime uses kqueue, which Miri does not support on FreeBSD"
    )]
    #[tokio::test]
    async fn process_offline_stems_bounce_resamples_and_converts_bit_depth() {
        let (_rx_unused_tx, rx_unused) = channel(1);
        let (tx, mut out_rx) = channel(32);
        let worker = Worker {
            id: 9,
            rx: rx_unused,
            tx,
            realtime_priority: 0,
        };
        let track = stems_test_track("stem_resampled", 0.5, 0.0);
        let mut state = State::default();
        state.tracks.insert(track.name.clone(), Arc::new(track));
        let output = unique_temp_wav("stems_resampled");
        let job = crate::message::OfflineStemsBounceWork {
            state: Arc::new(state.lock().snapshot()),
            stems: vec![crate::message::OfflineBounceStem {
                track_name: "stem_resampled".to_string(),
                start_sample: 0,
                length_samples: 128,
                sample_rate: 24_000,
                limiter_ceiling: None,
                automation_lanes: vec![],
                apply_fader: true,
                outputs: vec![stem_output(&output, crate::wav_stream::WavBitDepth::Int16)],
            }],
            tempo_bpm: 120.0,
            tsig_num: 4,
            tsig_denom: 4,
            cancel: Arc::new(AtomicBool::new(false)),
        };

        worker.process_offline_stems_bounce(job).await;

        while let Some(message) = out_rx.recv().await {
            match message {
                Message::OfflineBounceFinished {
                    result: Ok(Action::StemsOfflineBounceProgress { .. }),
                } => {}
                Message::OfflineBounceFinished {
                    result: Ok(Action::StemsOfflineBounced { .. }),
                } => break,
                other => panic!("unexpected message: {other:?}"),
            }
        }

        let (decoded, channels, rate) =
            crate::audio_codec::decode_audio_to_f32_interleaved_sync(&output)
                .expect("decode resampled stem");
        std::fs::remove_file(&output).expect("remove resampled stem");
        assert_eq!(channels, 2);
        assert_eq!(rate, 24_000);
        // 128 session-rate frames at 48 kHz -> 64 export-rate frames.
        assert_eq!(decoded.len(), 64 * 2);
        let frames: Vec<_> = decoded.as_chunks::<2>().0.iter().collect();
        for frame in &frames[4..frames.len() - 4] {
            for &sample in *frame {
                assert!(
                    (sample - 0.5).abs() < 2.0e-2,
                    "resampled sample {sample} != 0.5"
                );
            }
        }
    }

    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "Tokio runtime uses kqueue, which Miri does not support on FreeBSD"
    )]
    #[tokio::test]
    async fn process_offline_stems_bounce_errors_when_track_is_missing() {
        let (_rx_unused_tx, rx_unused) = channel(1);
        let (tx, mut out_rx) = channel(8);
        let worker = Worker {
            id: 7,
            rx: rx_unused,
            tx,
            realtime_priority: 0,
        };
        let output = unique_temp_wav("stems_missing");
        let job = crate::message::OfflineStemsBounceWork {
            state: Arc::new(State::default().snapshot()),
            stems: vec![crate::message::OfflineBounceStem {
                track_name: "missing".to_string(),
                start_sample: 0,
                length_samples: 8,
                sample_rate: 48_000,
                limiter_ceiling: None,
                automation_lanes: vec![],
                apply_fader: true,
                outputs: vec![stem_output(
                    &output,
                    crate::wav_stream::WavBitDepth::Float32,
                )],
            }],
            tempo_bpm: 120.0,
            tsig_num: 4,
            tsig_denom: 4,
            cancel: Arc::new(AtomicBool::new(false)),
        };

        worker.process_offline_stems_bounce(job).await;

        match out_rx.recv().await.expect("message") {
            Message::OfflineBounceFinished { result: Err(err) } => {
                assert!(err.contains("Track not found: missing"));
            }
            other => panic!("unexpected message: {other:?}"),
        }
        assert!(matches!(out_rx.recv().await, Some(Message::Ready(7))));
        assert!(!output.exists());
    }

    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "Tokio runtime uses kqueue, which Miri does not support on FreeBSD"
    )]
    #[tokio::test]
    async fn process_offline_stems_bounce_cancels_and_restores_track_state() {
        let (_rx_unused_tx, rx_unused) = channel(1);
        let (tx, mut out_rx) = channel(8);
        let worker = Worker {
            id: 8,
            rx: rx_unused,
            tx,
            realtime_priority: 0,
        };
        let track = Track::new("stem_track".to_string(), 1, 2, 0, 0, 4, 48_000.0);
        track.set_level(-9.0);
        track.set_balance(-0.3);
        let state = make_state_with_track(track);
        let output = unique_temp_wav("stems_cancel");
        let job = crate::message::OfflineStemsBounceWork {
            state: Arc::new(state.lock().snapshot()),
            stems: vec![crate::message::OfflineBounceStem {
                track_name: "stem_track".to_string(),
                start_sample: 0,
                length_samples: 8,
                sample_rate: 48_000,
                limiter_ceiling: None,
                automation_lanes: vec![],
                apply_fader: false,
                outputs: vec![stem_output(
                    &output,
                    crate::wav_stream::WavBitDepth::Float32,
                )],
            }],
            tempo_bpm: 120.0,
            tsig_num: 4,
            tsig_denom: 4,
            cancel: Arc::new(AtomicBool::new(true)),
        };

        worker.process_offline_stems_bounce(job).await;

        match out_rx.recv().await.expect("message") {
            Message::OfflineBounceFinished {
                result: Ok(Action::StemsOfflineBounceCanceled),
            } => {}
            other => panic!("unexpected message: {other:?}"),
        }
        assert!(matches!(out_rx.recv().await, Some(Message::Ready(8))));
        assert!(!output.exists());
        let state_guard = state.lock();
        let track = state_guard.tracks.get("stem_track").expect("track").lock();
        assert_eq!(track.level(), -9.0);
        assert_eq!(track.balance(), -0.3);
    }
}
