use super::*;
#[cfg(target_os = "linux")]
use crate::hw::alsa::{HwDriver, HwOptions, MidiHub};
#[cfg(target_os = "macos")]
use crate::hw::coreaudio::{HwDriver, HwOptions, MidiHub};
#[cfg(target_os = "windows")]
use crate::hw::options::HwOptions;
#[cfg(target_os = "freebsd")]
use crate::hw::oss::MidiHub;
#[cfg(target_os = "openbsd")]
use crate::hw::sndio::{HwDriver, HwOptions, MidiHub};
#[cfg(target_os = "windows")]
use crate::hw::wasapi::{self, HwDriver, MidiHub};
#[cfg(target_os = "linux")]
use crate::workers::alsa_worker::HwWorker;
#[cfg(target_os = "macos")]
use crate::workers::coreaudio_worker::HwWorker;
#[cfg(target_os = "openbsd")]
use crate::workers::sndio_worker::HwWorker;
#[cfg(target_os = "windows")]
use crate::workers::wasapi_worker::HwWorker;
use crate::{
    history::{History, UndoEntry},
    hw::traits::HwWorkerDriver,
    message::{Action, HwMidiEvent, Message, PluginKind, ProcessTask, SessionSlotState},
    midi::io::MidiEvent,
    mutex::UnsafeMutex,
    osc::{OscArg, OscServer, build_error_packet, build_osc_packet},
    state::State,
    track::Track,
    workers::worker::Worker,
};
use std::{
    collections::{HashMap, VecDeque},
    net::{SocketAddr, UdpSocket},
    path::{Path, PathBuf},
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};
use tokio::sync::mpsc::{Receiver, Sender, channel};
use tracing::error;

impl Engine {
    pub fn state(&self) -> Arc<UnsafeMutex<State>> {
        self.state.clone()
    }

    pub(crate) fn timing_at_sample(&self, sample: usize) -> (f64, u16, u16) {
        let bpm = self
            .tempo_points
            .iter()
            .filter(|p| p.sample <= sample)
            .max_by_key(|p| p.sample)
            .map(|p| p.bpm)
            .unwrap_or(self.tempo_bpm)
            .max(1.0);
        let (num, den) = self
            .time_signature_points
            .iter()
            .filter(|p| p.sample <= sample)
            .max_by_key(|p| p.sample)
            .map(|p| (p.numerator.max(1), p.denominator.max(1)))
            .unwrap_or((self.tsig_num.max(1), self.tsig_denom.max(1)));
        (bpm, num, den)
    }

    pub(crate) fn update_global_tempo_from_map(&mut self) {
        let (bpm, num, den) = self.timing_at_sample(0);
        self.tempo_bpm = bpm;
        self.tsig_num = num;
        self.tsig_denom = den;
    }

    pub(crate) fn meter_linear_to_db(peak: f32) -> f32 {
        if peak <= 1.0e-6 {
            -90.0
        } else {
            (20.0 * peak.log10()).clamp(-90.0, 20.0)
        }
    }

    pub(crate) const METER_PUBLISH_INTERVAL: Duration = Duration::from_millis(50);
    pub(crate) const SESSION_RUNTIME_REPORT_INTERVAL: Duration = Duration::from_millis(50);
    pub(crate) const TRACK_PROCESS_TIMEOUT: Duration = Duration::from_millis(250);
    #[cfg(any(target_os = "freebsd", target_os = "linux", target_os = "openbsd"))]
    pub(crate) const HW_OUT_METER_LINEAR_EPSILON: f32 = 0.0025;

    #[cfg(all(unix, not(target_os = "macos")))]
    pub(crate) fn session_plugins_dir(&self) -> Option<PathBuf> {
        self.session_dir.as_ref().map(|d| d.join("plugins"))
    }

    pub(crate) fn session_audio_dir(&self) -> Option<PathBuf> {
        self.session_dir.as_ref().map(|d| d.join("audio"))
    }

    pub(crate) fn session_midi_dir(&self) -> Option<PathBuf> {
        self.session_dir.as_ref().map(|d| d.join("midi"))
    }

    pub(crate) fn session_peaks_dir(&self) -> Option<PathBuf> {
        self.session_dir.as_ref().map(|d| d.join("peaks"))
    }

    pub(crate) fn ensure_session_subdirs(&self) {
        if let Some(root) = &self.session_dir {
            let _ = std::fs::create_dir_all(root.join("plugins"));
            let _ = std::fs::create_dir_all(root.join("audio"));
            let _ = std::fs::create_dir_all(root.join("midi"));
            let _ = std::fs::create_dir_all(root.join("peaks"));
        }
    }

    pub fn new(rx: Receiver<Message>, tx: Sender<Message>) -> Self {
        Self {
            rx,
            tx,
            clients: vec![],
            state: Arc::new(UnsafeMutex::new(State::default())),
            workers: vec![],
            hw_driver: None,
            #[cfg(unix)]
            jack_runtime: None,
            midi_hub: Arc::new(UnsafeMutex::new(MidiHub::default())),
            hw_worker: None,
            osc_server: None,
            osc_reply_socket: None,
            osc_reply_target: None,
            pending_hw_midi_events: vec![],
            pending_hw_midi_events_by_device: HashMap::new(),
            pending_hw_midi_out_events: vec![],
            pending_hw_midi_out_events_by_device: vec![],
            active_hw_notes_by_track: HashMap::new(),
            active_hw_notes_cycle_start: HashMap::new(),
            midi_hw_in_routes: vec![],
            midi_hw_out_routes: vec![],
            midi_hw_thru_routes: vec![],
            ready_workers: vec![],
            pending_requests: VecDeque::new(),
            awaiting_hwfinished: false,
            handling_hwfinished: false,
            track_process_epoch: 0,
            transport_panic_flush_pending: false,
            transport_restart_pending: false,
            notified_loop_wrap_sample: None,
            transport_sample: 0,
            hw_input_latency_frames: 0,
            hw_output_latency_frames: 0,
            loop_enabled: false,
            loop_range_samples: None,
            metronome_enabled: false,
            tempo_bpm: 120.0,
            tsig_num: 4,
            tsig_denom: 4,
            tempo_points: vec![crate::message::TempoPoint {
                sample: 0,
                bpm: 120.0,
            }],
            time_signature_points: vec![crate::message::TimeSignaturePoint {
                sample: 0,
                numerator: 4,
                denominator: 4,
            }],
            punch_enabled: false,
            punch_range_samples: None,
            audio_recordings: std::collections::HashMap::new(),
            midi_recordings: std::collections::HashMap::new(),
            completed_audio_recordings: Vec::new(),
            completed_midi_recordings: Vec::new(),
            playing: false,
            transport_running: false,
            clip_playback_enabled: true,
            session_clip_playback_enabled: false,
            session_transport_sample: 0,
            record_enabled: false,
            step_recording_enabled: false,
            session_dir: None,
            hw_out_level_db: 0.0,
            hw_out_balance: 0.0,
            hw_out_muted: false,
            last_hw_out_meter_publish: None,
            #[cfg(any(target_os = "freebsd", target_os = "linux", target_os = "openbsd"))]
            last_hw_out_meter_linear: vec![],
            hw_out_peak_hold_linear: vec![],
            #[cfg(any(target_os = "freebsd", target_os = "linux", target_os = "openbsd"))]
            hw_out_meter_publish_phase: false,
            last_track_meter_publish: None,
            last_session_report_publish: None,
            session_report_state: HashMap::new(),
            track_meter_linear_by_track: HashMap::new(),
            task_processing_started_at: HashMap::new(),
            cycle_tasks: Vec::new(),
            cycle_task_deps: HashMap::new(),
            cycle_tasks_running: Vec::new(),
            cycle_tasks_finished: Vec::new(),
            latest_hw_out_meter_db: Arc::new(Vec::new()),
            latest_track_meter_snapshot: Arc::new(Vec::new()),
            history: History::default(),
            history_group: None,
            history_suspended: false,
            offline_bounce_jobs: HashMap::new(),
            pending_midi_learn: None,
            pending_global_midi_learn: None,
            pending_session_midi_learn: None,
            global_midi_learn_play_pause: None,
            global_midi_learn_stop: None,
            global_midi_learn_record_toggle: None,
            session_midi_learn_slots: HashMap::new(),
            session_midi_learn_scenes: HashMap::new(),
            session_midi_learn_stop_track: HashMap::new(),
            session_midi_learn_stop_all: None,
            midi_cc_gate: HashMap::new(),
            modulators: Vec::new(),
            modulator_values: None,
        }
    }

    pub(crate) fn hw_driver_cycle_samples(&self) -> Option<usize> {
        self.hw_driver.as_ref().map(|o| o.lock().cycle_samples())
    }

    #[cfg(unix)]
    pub(crate) fn jack_cycle_samples(&self) -> Option<usize> {
        self.jack_runtime.as_ref().map(|j| j.lock().buffer_size)
    }

    #[cfg(not(unix))]
    pub(crate) fn jack_cycle_samples(&self) -> Option<usize> {
        None
    }

    pub(crate) fn current_cycle_samples(&self) -> usize {
        self.hw_driver_cycle_samples()
            .or_else(|| self.jack_cycle_samples())
            .unwrap_or(0)
    }

    pub(crate) fn sample_rate(&self) -> f64 {
        if let Some(hw) = &self.hw_driver {
            hw.lock().sample_rate() as f64
        } else {
            #[cfg(unix)]
            {
                self.jack_runtime
                    .as_ref()
                    .map(|j| j.lock().sample_rate as f64)
                    .unwrap_or(48_000.0)
            }
            #[cfg(not(unix))]
            {
                48_000.0
            }
        }
    }

    pub(crate) fn active_transport_sample(&self) -> usize {
        if self.session_clip_playback_enabled && self.playing {
            self.session_transport_sample
        } else {
            self.transport_sample
        }
    }

    pub(crate) fn compute_modulator_values(
        &self,
        sample: usize,
    ) -> Arc<std::collections::HashMap<usize, f32>> {
        let sample_rate = self.sample_rate();
        let (bpm, tsig_num, tsig_denom) = self.timing_at_sample(sample);
        let values: std::collections::HashMap<usize, f32> = self
            .modulators
            .iter()
            .filter(|m| m.enabled)
            .map(|m| {
                (
                    m.id,
                    m.value_at(sample, sample_rate, bpm, tsig_num, tsig_denom),
                )
            })
            .collect();
        Arc::new(values)
    }

    pub(crate) fn apply_modulators(&mut self, sample: usize) -> Vec<Action> {
        use crate::modulator::ModulatorTarget;
        let values = self.compute_modulator_values(sample);
        self.modulator_values = Some(values.clone());
        let mut echoes = Vec::new();
        let mut per_track: HashMap<String, (Option<f32>, Option<f32>)> = HashMap::new();
        let mut clap_params: HashMap<(String, usize, u32), f64> = HashMap::new();
        let mut vst3_params: HashMap<(String, usize, u32), f32> = HashMap::new();
        #[cfg(all(unix, not(target_os = "macos")))]
        let mut lv2_params: HashMap<(String, usize, u32), f32> = HashMap::new();
        let mut midi_cc_events: HashMap<String, Vec<MidiEvent>> = HashMap::new();

        let map_f32 = |value: f32, min: f32, max: f32| -> f32 {
            crate::modulator::map_value(value, min, max)
        };
        let map_f64 = |value: f32, min: f64, max: f64| -> f64 {
            crate::modulator::map_value_f64(value, min, max)
        };

        for m in &self.modulators {
            if !m.enabled {
                continue;
            }
            let Some(&value) = values.get(&m.id) else {
                continue;
            };
            for target in &m.targets {
                match target {
                    ModulatorTarget::TrackVolume {
                        track_name,
                        min,
                        max,
                    } => {
                        let clamped = map_f32(value, *min, *max);
                        per_track.entry(track_name.clone()).or_default().0 = Some(clamped);
                    }
                    ModulatorTarget::TrackBalance {
                        track_name,
                        min,
                        max,
                    } => {
                        let clamped = map_f32(value, *min, *max);
                        per_track.entry(track_name.clone()).or_default().1 = Some(clamped);
                    }
                    ModulatorTarget::HwOutVolume { min, max } => {
                        let clamped = map_f32(value, *min, *max);
                        if (self.hw_out_level_db - clamped).abs() > f32::EPSILON {
                            self.hw_out_level_db = clamped;
                            echoes
                                .push(Action::TrackAutomationLevel("hw:out".to_string(), clamped));
                        }
                    }
                    ModulatorTarget::HwOutBalance { min, max } => {
                        let next = map_f32(value, *min, *max).clamp(-1.0, 1.0);
                        if (self.hw_out_balance - next).abs() > f32::EPSILON {
                            self.hw_out_balance = next;
                            echoes.push(Action::TrackAutomationBalance("hw:out".to_string(), next));
                        }
                    }
                    ModulatorTarget::ClapParameter {
                        track_name,
                        instance_id,
                        param_id,
                        min,
                        max,
                    } => {
                        let param_value = map_f64(value, *min, *max);
                        clap_params
                            .insert((track_name.clone(), *instance_id, *param_id), param_value);
                    }
                    ModulatorTarget::Vst3Parameter {
                        track_name,
                        instance_id,
                        param_id,
                        min,
                        max,
                    } => {
                        let param_value = map_f32(value, *min, *max);
                        vst3_params
                            .insert((track_name.clone(), *instance_id, *param_id), param_value);
                    }
                    #[cfg(all(unix, not(target_os = "macos")))]
                    ModulatorTarget::Lv2Parameter {
                        track_name,
                        instance_id,
                        index,
                        min,
                        max,
                    } => {
                        let param_value = map_f32(value, *min, *max);
                        lv2_params.insert((track_name.clone(), *instance_id, *index), param_value);
                    }
                    ModulatorTarget::MidiCc {
                        track_name,
                        channel,
                        cc,
                    } => {
                        let cc_value = (value * 127.0).round() as u8;
                        midi_cc_events
                            .entry(track_name.clone())
                            .or_default()
                            .push(MidiEvent::new(
                                0,
                                vec![0xB0 | (*channel).min(15), (*cc).min(127), cc_value],
                            ));
                    }
                }
            }
        }
        for (track_name, (level, balance)) in per_track {
            if let Some(level) = level
                && let Some(track) = self.state.lock().tracks.get(&track_name).cloned()
            {
                let t = track.lock();
                if (t.level() - level).abs() > f32::EPSILON {
                    t.set_level(level);
                    echoes.push(Action::TrackAutomationLevel(track_name.clone(), level));
                }
            }
            if let Some(balance) = balance
                && let Some(track) = self.state.lock().tracks.get(&track_name).cloned()
            {
                let t = track.lock();
                let next = balance.clamp(-1.0, 1.0);
                if (t.balance - next).abs() > f32::EPSILON {
                    t.set_balance(next);
                    echoes.push(Action::TrackAutomationBalance(track_name.clone(), next));
                }
            }
        }

        for (track_name, events) in midi_cc_events {
            if let Some(track) = self.state.lock().tracks.get(&track_name).cloned() {
                track.lock().pending_modulator_midi_events.extend(events);
            }
        }

        let state = self.state.lock();
        for ((track_name, instance_id, param_id), value) in clap_params {
            if let Some(track) = state.tracks.get(&track_name).cloned()
                && track
                    .lock()
                    .set_clap_parameter(instance_id, param_id, value)
                    .is_ok()
            {
                echoes.push(Action::TrackSetClapParameter {
                    track_name,
                    instance_id,
                    param_id,
                    value,
                });
            }
        }
        for ((track_name, instance_id, param_id), value) in vst3_params {
            if let Some(track) = state.tracks.get(&track_name).cloned()
                && track
                    .lock()
                    .set_vst3_parameter(instance_id, param_id, value)
                    .is_ok()
            {
                echoes.push(Action::TrackSetVst3Parameter {
                    track_name,
                    instance_id,
                    param_id,
                    value,
                });
            }
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        for ((track_name, instance_id, index), value) in lv2_params {
            if let Some(track) = state.tracks.get(&track_name).cloned()
                && track
                    .lock()
                    .set_lv2_control_value(instance_id, index as usize, f64::from(value))
                    .is_ok()
            {
                echoes.push(Action::TrackSetLv2ControlValue {
                    track_name,
                    instance_id,
                    index,
                    value,
                });
            }
        }

        echoes
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
                    .clips
                    .iter()
                    .map(|clip| clip.end)
                    .max()
                    .unwrap_or(0);
                let midi_end = track
                    .midi
                    .clips
                    .iter()
                    .map(|clip| clip.end)
                    .max()
                    .unwrap_or(0);
                audio_end.max(midi_end)
            })
            .max()
            .unwrap_or(0)
    }

    pub(crate) fn normalize_transport_sample(&self, sample: usize) -> usize {
        if self.loop_enabled
            && let Some((loop_start, loop_end)) = self.loop_range_samples
            && loop_end > loop_start
            && sample >= loop_end
        {
            let loop_len = loop_end - loop_start;
            return loop_start + (sample - loop_start) % loop_len;
        }
        sample
    }

    pub(crate) fn scheduled_loop_wrap_for_next_cycle(&self) -> Option<(usize, usize, usize)> {
        if !self.playing || !self.loop_enabled {
            return None;
        }
        let (loop_start, loop_end) = self.loop_range_samples?;
        if loop_end <= loop_start || self.transport_sample >= loop_end {
            return None;
        }
        let cycle_samples = self.current_cycle_samples();
        if cycle_samples == 0 {
            return None;
        }
        let next = self.transport_sample.saturating_add(cycle_samples);
        if next < loop_end {
            return None;
        }
        let after_frames = loop_end.saturating_sub(self.transport_sample);
        Some((
            after_frames,
            loop_start,
            self.normalize_transport_sample(next),
        ))
    }

    pub(crate) fn cycle_segments(&self, frames: usize) -> Vec<(usize, usize, usize)> {
        if frames == 0 {
            return vec![];
        }
        if !self.loop_enabled {
            return vec![(
                self.transport_sample,
                self.transport_sample.saturating_add(frames),
                0,
            )];
        }
        let Some((loop_start, loop_end)) = self.loop_range_samples else {
            return vec![(
                self.transport_sample,
                self.transport_sample.saturating_add(frames),
                0,
            )];
        };
        if loop_end <= loop_start {
            return vec![(
                self.transport_sample,
                self.transport_sample.saturating_add(frames),
                0,
            )];
        }
        let mut segments = Vec::new();
        let mut remaining = frames;
        let mut out_offset = 0usize;
        let mut current = self.transport_sample;
        while remaining > 0 {
            let take = loop_end.saturating_sub(current).min(remaining);
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

    pub(crate) fn recording_segments_for_cycle(&self, frames: usize) -> Vec<(usize, usize, usize)> {
        let segments = self.cycle_segments(frames);
        let comp = self.hw_input_latency_frames;
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
        if !self.punch_enabled {
            return segments;
        }
        let Some((punch_start, punch_end)) = self.punch_range_samples else {
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

    pub async fn init(&mut self) {
        let max_threads = num_cpus::get();
        for id in 0..max_threads {
            let (tx, rx) = channel::<Message>(32);
            let tx_thread = self.tx.clone();
            let handler = tokio::spawn(async move {
                let wrk = Worker::new(id, rx, tx_thread, 8);
                wrk.await.work().await;
            });
            self.workers.push(WorkerData::new(tx.clone(), handler));
        }
    }

    pub(crate) async fn notify_clients(&mut self, action: Result<Action, String>) {
        self.clients.retain(|client| !client.is_closed());
        for client in self.clients.iter() {
            if client
                .send(Message::Response(action.clone()))
                .await
                .is_err()
            {}
        }
        if let Some(reply_to) = self.osc_reply_target {
            match &action {
                Err(reason) => {
                    self.send_osc_reply(reply_to, &build_error_packet(reason));
                }
                Ok(Action::TrackList(names)) => {
                    let args: Vec<OscArg> = names
                        .iter()
                        .map(|name| OscArg::String(name.clone()))
                        .collect();
                    self.send_osc_reply(
                        reply_to,
                        &build_osc_packet("/response/tracks", &"s".repeat(names.len()), &args),
                    );
                }
                Ok(Action::TransportState {
                    sample,
                    tempo_bpm,
                    playing,
                    paused: _,
                    tsig_num,
                    tsig_denom,
                }) => {
                    self.send_osc_reply(
                        reply_to,
                        &build_osc_packet(
                            "/response/transport",
                            "idffii",
                            &[
                                OscArg::Int(*sample as i32),
                                OscArg::Int(if *playing { 1 } else { 0 }),
                                OscArg::Float(*tempo_bpm as f32),
                                OscArg::Float(0.0), // placeholder for future beat position
                                OscArg::Int(*tsig_num as i32),
                                OscArg::Int(*tsig_denom as i32),
                            ],
                        ),
                    );
                }
                Ok(Action::MeterSnapshot {
                    hw_out_db,
                    track_meters,
                }) => {
                    let mut args: Vec<OscArg> = Vec::new();
                    args.push(OscArg::Int(hw_out_db.len() as i32));
                    for db in hw_out_db.iter() {
                        args.push(OscArg::Float(*db));
                    }
                    args.push(OscArg::Int(track_meters.len() as i32));
                    for (name, channels) in track_meters.iter() {
                        args.push(OscArg::String(name.clone()));
                        args.push(OscArg::Int(channels.len() as i32));
                        for db in channels.iter() {
                            args.push(OscArg::Float(*db));
                        }
                    }
                    let types = args
                        .iter()
                        .map(|a| match a {
                            OscArg::String(_) => 's',
                            OscArg::Int(_) => 'i',
                            OscArg::Float(_) => 'f',
                        })
                        .collect::<String>();
                    self.send_osc_reply(
                        reply_to,
                        &build_osc_packet("/response/meters", &types, &args),
                    );
                }
                Ok(Action::TrackPluginGraph {
                    track_name,
                    plugins,
                    connections: _,
                    connectable_connections: _,
                }) => {
                    let mut args: Vec<OscArg> = vec![OscArg::String(track_name.clone())];
                    args.push(OscArg::Int(plugins.len() as i32));
                    for plugin in plugins.iter() {
                        args.push(OscArg::Int(plugin.instance_id as i32));
                        args.push(OscArg::String(plugin.format.clone()));
                        args.push(OscArg::String(plugin.uri.clone()));
                        args.push(OscArg::String(plugin.name.clone()));
                        args.push(OscArg::Int(plugin.bypassed as i32));
                    }
                    let types = args
                        .iter()
                        .map(|a| match a {
                            OscArg::String(_) => 's',
                            OscArg::Int(_) => 'i',
                            OscArg::Float(_) => 'f',
                        })
                        .collect::<String>();
                    self.send_osc_reply(
                        reply_to,
                        &build_osc_packet("/response/plugins", &types, &args),
                    );
                }
                Ok(Action::ClapPlugins(plugins)) => {
                    let args: Vec<OscArg> = plugins
                        .iter()
                        .map(|p| OscArg::String(format!("{}|{}", p.path, p.name)))
                        .collect();
                    let types = "s".repeat(args.len());
                    self.send_osc_reply(
                        reply_to,
                        &build_osc_packet("/response/clap_plugins", &types, &args),
                    );
                }
                Ok(Action::Vst3Plugins(plugins)) => {
                    let args: Vec<OscArg> = plugins
                        .iter()
                        .map(|p| OscArg::String(format!("{}|{}", p.id, p.name)))
                        .collect();
                    let types = "s".repeat(args.len());
                    self.send_osc_reply(
                        reply_to,
                        &build_osc_packet("/response/vst3_plugins", &types, &args),
                    );
                }
                #[cfg(all(unix, not(target_os = "macos")))]
                Ok(Action::Lv2Plugins(plugins)) => {
                    let args: Vec<OscArg> = plugins
                        .iter()
                        .map(|p| OscArg::String(format!("{}|{}", p.uri, p.name)))
                        .collect();
                    let types = "s".repeat(args.len());
                    self.send_osc_reply(
                        reply_to,
                        &build_osc_packet("/response/lv2_plugins", &types, &args),
                    );
                }
                Ok(Action::ClapPluginsUnavailable { error })
                | Ok(Action::Vst3PluginsUnavailable { error }) => {
                    self.send_osc_reply(reply_to, &build_error_packet(error));
                }
                #[cfg(all(unix, not(target_os = "macos")))]
                Ok(Action::Lv2PluginsUnavailable { error }) => {
                    self.send_osc_reply(reply_to, &build_error_packet(error));
                }
                Ok(Action::TrackClapParameters {
                    track_name,
                    instance_id,
                    parameters,
                }) => {
                    let json = serde_json::json!(
                        parameters
                            .iter()
                            .map(|p| serde_json::json!({
                                "id": p.id,
                                "name": p.name,
                                "module": p.module,
                                "min_value": p.min_value,
                                "max_value": p.max_value,
                                "default_value": p.default_value,
                            }))
                            .collect::<Vec<_>>()
                    )
                    .to_string();
                    self.send_osc_reply(
                        reply_to,
                        &build_osc_packet(
                            "/response/plugin_parameters",
                            "siss",
                            &[
                                OscArg::String(track_name.clone()),
                                OscArg::Int(*instance_id as i32),
                                OscArg::String("clap".to_string()),
                                OscArg::String(json),
                            ],
                        ),
                    );
                }
                Ok(Action::TrackVst3Parameters {
                    track_name,
                    instance_id,
                    parameters,
                }) => {
                    let json = serde_json::to_string(parameters).unwrap_or_default();
                    self.send_osc_reply(
                        reply_to,
                        &build_osc_packet(
                            "/response/plugin_parameters",
                            "siss",
                            &[
                                OscArg::String(track_name.clone()),
                                OscArg::Int(*instance_id as i32),
                                OscArg::String("vst3".to_string()),
                                OscArg::String(json),
                            ],
                        ),
                    );
                }
                #[cfg(all(unix, not(target_os = "macos")))]
                Ok(Action::TrackLv2PluginControls {
                    track_name,
                    instance_id,
                    controls,
                    instance_access_handle: _,
                }) => {
                    let json = serde_json::json!(
                        controls
                            .iter()
                            .map(|c| serde_json::json!({
                                "index": c.index,
                                "name": c.name,
                                "min": c.min,
                                "max": c.max,
                                "value": c.value,
                            }))
                            .collect::<Vec<_>>()
                    )
                    .to_string();
                    self.send_osc_reply(
                        reply_to,
                        &build_osc_packet(
                            "/response/plugin_parameters",
                            "siss",
                            &[
                                OscArg::String(track_name.clone()),
                                OscArg::Int(*instance_id as i32),
                                OscArg::String("lv2".to_string()),
                                OscArg::String(json),
                            ],
                        ),
                    );
                }
                Ok(Action::TrackClapNoteNames {
                    track_name,
                    note_names,
                }) => {
                    let json = serde_json::to_string(note_names).unwrap_or_default();
                    self.send_osc_reply(
                        reply_to,
                        &build_osc_packet(
                            "/response/clap_note_names",
                            "ss",
                            &[OscArg::String(track_name.clone()), OscArg::String(json)],
                        ),
                    );
                }
                #[cfg(all(unix, not(target_os = "macos")))]
                Ok(Action::TrackLv2Midnam {
                    track_name,
                    note_names,
                }) => {
                    let json = serde_json::to_string(note_names).unwrap_or_default();
                    self.send_osc_reply(
                        reply_to,
                        &build_osc_packet(
                            "/response/lv2_midnam",
                            "ss",
                            &[OscArg::String(track_name.clone()), OscArg::String(json)],
                        ),
                    );
                }
                _ => {}
            }
        }
    }

    pub(crate) fn send_osc_reply(&mut self, reply_to: SocketAddr, packet: &[u8]) {
        if self.osc_reply_socket.is_none() {
            self.osc_reply_socket = UdpSocket::bind("0.0.0.0:0").ok();
        }
        if let Some(socket) = self.osc_reply_socket.as_ref() {
            let _ = socket.send_to(packet, reply_to);
        }
    }

    pub(crate) async fn dispatch_request(&mut self, a: Action) {
        match a {
            Action::TrackOfflineBounceCancel { track_name } => {
                if let Some(job) = self.offline_bounce_jobs.get(&track_name) {
                    job.cancel.store(true, Ordering::Relaxed);
                }
            }
            Action::TrackOfflineBounceCancelAll => {
                for job in self.offline_bounce_jobs.values() {
                    job.cancel.store(true, Ordering::Relaxed);
                }
            }
            _ if !self.offline_bounce_jobs.is_empty() => {
                self.pending_requests.push_back(a);
            }
            Action::OpenAudioDevice { .. }
            | Action::OpenMidiInputDevice(_)
            | Action::OpenMidiOutputDevice(_)
            | Action::RequestMeterSnapshot
            | Action::RequestTrackList
            | Action::RequestTransportState
            | Action::Quit
            | Action::Log { .. }
            | Action::Play
            | Action::Pause
            | Action::Stop
            | Action::TransportPosition(_)
            | Action::JumpToEnd
            | Action::SetLoopEnabled(_)
            | Action::SetLoopRange(_)
            | Action::SetPunchEnabled(_)
            | Action::SetPunchRange(_)
            | Action::SetMetronomeEnabled(_)
            | Action::SetTempo(_)
            | Action::SetTimeSignature { .. }
            | Action::SetTempoMap { .. }
            | Action::SetOscEnabled(_)
            | Action::SetClipPlaybackEnabled(_)
            | Action::SetRecordEnabled(_)
            | Action::SetStepRecording(_)
            | Action::StepRecordMidiNote { .. }
            | Action::SetSessionPath(_)
            | Action::ClearHistory
            | Action::BeginSessionRestore
            | Action::PianoKey { .. }
            | Action::ModifyMidiNotes { .. }
            | Action::ModifyMidiControllers { .. }
            | Action::DeleteMidiControllers { .. }
            | Action::InsertMidiControllers { .. }
            | Action::DeleteMidiNotes { .. }
            | Action::InsertMidiNotes { .. }
            | Action::SetMidiSysExEvents { .. }
            | Action::Session(_) => {
                self.handle_request(a).await;
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            Action::ListLv2Plugins => {
                self.handle_request(a).await;
            }
            Action::ListVst3Plugins => {
                self.handle_request(a).await;
            }
            Action::ListClapPlugins => {
                self.handle_request(a).await;
            }
            Action::ListClapPluginsWithCapabilities => {
                self.handle_request(a).await;
            }
            _ => {
                self.pending_requests.push_back(a);
                if self.can_schedule_hw_cycle() {
                    self.request_hw_cycle().await;
                } else {
                    while let Some(next) = self.pending_requests.pop_front() {
                        self.handle_request(next).await;
                    }
                }
            }
        };
        self.publish_clap_state_dirty().await;
    }

    pub(crate) fn spawn_plugin_host_stderr_reader(
        &self,
        stderr: std::process::ChildStderr,
        source: String,
    ) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                if let Ok(line) = line
                    && !line.is_empty()
                {
                    let _ = tx.blocking_send(Message::Request(Action::Log {
                        source: source.clone(),
                        message: line,
                    }));
                }
            }
        });
    }

    pub(crate) fn set_osc_enabled_with<F>(
        &mut self,
        enabled: bool,
        start_server: F,
    ) -> Result<(), String>
    where
        F: FnOnce(Sender<Message>) -> Result<OscServer, String>,
    {
        if enabled {
            if self.osc_server.is_none() {
                self.osc_server = Some(start_server(self.tx.clone())?);
            }
        } else if let Some(mut server) = self.osc_server.take() {
            server.stop();
        }
        Ok(())
    }

    pub(crate) async fn request_hw_cycle(&mut self) {
        if self.awaiting_hwfinished {
            tracing::debug!("request_hw_cycle skipped (already awaiting)");
            return;
        }
        tracing::debug!("request_hw_cycle sending TracksFinished");
        self.apply_hw_out_gain_and_meter().await;
        if let Some((after_frames, loop_start, cycle_end_sample)) =
            self.scheduled_loop_wrap_for_next_cycle()
        {
            self.notified_loop_wrap_sample = Some(cycle_end_sample);
            self.notify_clients(Ok(Action::TransportPositionAt {
                sample: loop_start,
                after_frames,
            }))
            .await;
        } else {
            self.notified_loop_wrap_sample = None;
        }
        if let Some(worker) = &self.hw_worker {
            if !self.pending_hw_midi_out_events_by_device.is_empty() {
                let out_events = std::mem::take(&mut self.pending_hw_midi_out_events_by_device);
                if let Err(e) = worker.tx.send(Message::HWMidiOutEvents(out_events)).await {
                    error!("Error sending HWMidiOutEvents {e}");
                }
            }
            match worker.tx.send(Message::TracksFinished).await {
                Ok(_) => {
                    self.awaiting_hwfinished = true;
                }
                Err(e) => {
                    error!("Error sending TracksFinished {e}");
                }
            }
        }
    }

    pub(crate) fn invalidate_track_cycle_state(&mut self) {
        self.track_process_epoch = self.track_process_epoch.saturating_add(1);
        self.task_processing_started_at.clear();
        self.cycle_tasks.clear();
        self.cycle_task_deps.clear();
        self.cycle_tasks_running.clear();
        self.cycle_tasks_finished.clear();
        let state = self.state.lock();
        for track in state.tracks.values() {
            let t = track.lock();
            t.audio.finished = false;
            t.audio.processing = false;
        }
    }

    pub(crate) fn force_stalled_task_completions(&mut self) {
        let now = Instant::now();
        let running: Vec<ProcessTask> = self.cycle_tasks_running.clone();
        for task in running {
            let key = Self::task_key(&task);
            let Some(started) = self.task_processing_started_at.get(&key).copied() else {
                continue;
            };
            if now.duration_since(started) < Self::TRACK_PROCESS_TIMEOUT {
                continue;
            }
            if Self::task_running_finished_contains(&self.cycle_tasks_finished, &task) {
                self.task_processing_started_at.remove(&key);
                continue;
            }
            let track = match &task {
                ProcessTask::Track(t)
                | ProcessTask::FolderInput(t)
                | ProcessTask::FolderOutput(t) => t.clone(),
                ProcessTask::Plugin { track, .. } => track.clone(),
            };
            {
                let t = track.lock();
                if t.audio.finished || !t.audio.processing {
                    self.task_processing_started_at.remove(&key);
                    continue;
                }
                for out in &t.audio.outs {
                    out.buffer.lock().fill(0.0);
                    *out.finished.lock() = true;
                }
                t.audio.processing = false;
                t.audio.finished = true;
            }
            self.cycle_tasks_running
                .retain(|t| Self::task_key(t) != key);
            self.cycle_tasks_finished.push(task.clone());
            self.task_processing_started_at.remove(&key);
            tracing::warn!(
                "Task '{}' exceeded process timeout ({} ms); forcing silent completion for cycle",
                Self::task_track_name(&task),
                Self::TRACK_PROCESS_TIMEOUT.as_millis()
            );
        }
    }

    pub(crate) fn should_publish_hw_out_meters(&mut self) -> bool {
        let now = Instant::now();
        match self.last_hw_out_meter_publish {
            Some(last) if now.duration_since(last) < Self::METER_PUBLISH_INTERVAL => false,
            _ => {
                self.last_hw_out_meter_publish = Some(now);
                true
            }
        }
    }

    pub(crate) fn should_publish_track_meters(&mut self) -> bool {
        let now = Instant::now();
        match self.last_track_meter_publish {
            Some(last) if now.duration_since(last) < Self::METER_PUBLISH_INTERVAL => false,
            _ => {
                self.last_track_meter_publish = Some(now);
                true
            }
        }
    }

    pub(crate) fn should_publish_hw_out_linear(&mut self, peaks_linear: &[f32]) -> bool {
        #[cfg(any(target_os = "freebsd", target_os = "linux", target_os = "openbsd"))]
        {
            self.hw_out_meter_publish_phase = !self.hw_out_meter_publish_phase;
            if !self.hw_out_meter_publish_phase {
                return false;
            }
            let changed = if self.last_hw_out_meter_linear.len() != peaks_linear.len() {
                true
            } else {
                self.last_hw_out_meter_linear
                    .iter()
                    .zip(peaks_linear.iter())
                    .any(|(prev, next)| (prev - next).abs() >= Self::HW_OUT_METER_LINEAR_EPSILON)
            };
            if !changed {
                return false;
            }
            self.last_hw_out_meter_linear.clear();
            self.last_hw_out_meter_linear
                .extend_from_slice(peaks_linear);
            true
        }
        #[cfg(not(any(target_os = "freebsd", target_os = "linux", target_os = "openbsd")))]
        {
            let _ = peaks_linear;
            false
        }
    }

    pub(crate) async fn maybe_notify_hw_out_meter(&mut self, _meter_db: Vec<f32>) {
        {}
    }

    pub(crate) fn collect_changed_track_meters(
        &mut self,
        _tracks: &[(String, Arc<UnsafeMutex<Box<Track>>>)],
    ) -> Vec<(String, Vec<f32>)> {
        Vec::new()
    }

    pub(crate) async fn apply_hw_out_gain_and_meter(&mut self) {
        let gain = if self.hw_out_muted {
            0.0
        } else {
            10.0_f32.powf(self.hw_out_level_db / 20.0)
        };
        let should_notify_interval = self.should_publish_hw_out_meters();
        if let Some(oss) = self.hw_driver.clone() {
            let hw = oss.lock();
            hw.set_output_gain_balance(gain, self.hw_out_balance);
            if !should_notify_interval {
                return;
            }
        } else {
            #[cfg(unix)]
            {
                if let Some(jack) = self.jack_runtime.clone() {
                    jack.lock().set_output_gain_linear(gain);
                    jack.lock().set_output_balance(self.hw_out_balance);
                    if !should_notify_interval {
                        return;
                    }
                } else {
                    return;
                }
            }
            #[cfg(not(unix))]
            {
                return;
            }
        }
        let peaks_linear = if let Some(oss) = self.hw_driver.clone() {
            oss.lock().output_meter_linear(gain, self.hw_out_balance)
        } else {
            #[cfg(unix)]
            {
                if let Some(jack) = self.jack_runtime.clone() {
                    let outs = jack.lock().audio_outs();
                    let out_count = outs.len();
                    let b = if out_count == 2 {
                        self.hw_out_balance.clamp(-1.0, 1.0)
                    } else {
                        0.0
                    };
                    let mut meters_linear = Vec::with_capacity(out_count);
                    for (channel_idx, channel) in outs.iter().enumerate() {
                        let balance_gain = if out_count == 2 {
                            if channel_idx == 0 {
                                (1.0 - b).clamp(0.0, 1.0)
                            } else {
                                (1.0 + b).clamp(0.0, 1.0)
                            }
                        } else {
                            1.0
                        };
                        let buf = channel.buffer.lock();
                        let peak = crate::simd::peak_abs(buf) * gain * balance_gain;
                        meters_linear.push(peak);
                    }
                    meters_linear
                } else {
                    return;
                }
            }
            #[cfg(not(unix))]
            {
                return;
            }
        };
        if self.hw_out_peak_hold_linear.len() != peaks_linear.len() {
            self.hw_out_peak_hold_linear.resize(peaks_linear.len(), 0.0);
        }
        let mut held_peaks = Vec::with_capacity(peaks_linear.len());
        for (idx, peak_now) in peaks_linear.iter().copied().enumerate() {
            let held = self.hw_out_peak_hold_linear[idx] * 0.92;
            let next = peak_now.max(held);
            self.hw_out_peak_hold_linear[idx] = next;
            held_peaks.push(next);
        }
        let should_notify =
            should_notify_interval && self.should_publish_hw_out_linear(&held_peaks);
        let meter_db: Vec<f32> = held_peaks
            .into_iter()
            .map(Self::meter_linear_to_db)
            .collect();
        self.latest_hw_out_meter_db = Arc::new(meter_db.clone());
        if should_notify {
            self.maybe_notify_hw_out_meter(meter_db).await;
        }
    }

    pub(crate) fn preload_track_clips_spawn(&self) {
        let tracks: Vec<_> = self.state.lock().tracks.values().cloned().collect();
        for track in tracks {
            tokio::task::spawn_blocking(move || {
                track.lock().preload_clips();
            });
        }
    }

    pub(crate) async fn preload_track_clips(&self) {
        let tracks: Vec<_> = self.state.lock().tracks.values().cloned().collect();
        if tracks.is_empty() {
            return;
        }
        let mut handles = Vec::with_capacity(tracks.len());
        for track in tracks {
            handles.push(tokio::task::spawn_blocking(move || {
                track.lock().preload_clips();
            }));
        }
        for handle in handles {
            if let Err(e) = handle.await {
                tracing::warn!("Clip preload task panicked: {e}");
            }
        }
    }

    pub(crate) fn build_task_graph(
        &self,
    ) -> (
        Vec<ProcessTask>,
        std::collections::HashMap<String, Vec<String>>,
    ) {
        let state = self.state.lock();
        let ordered: Vec<(String, Arc<UnsafeMutex<Box<Track>>>)> = state
            .tracks
            .iter()
            .map(|(name, track)| (name.clone(), track.clone()))
            .collect();
        let mut tasks = Vec::new();
        let mut deps = std::collections::HashMap::new();

        for (_name, track) in &ordered {
            let t = track.lock();
            if t.parent_track.is_some() {
                continue;
            }
            self.append_track_tasks(track.clone(), None, &mut tasks, &mut deps);
        }

        (tasks, deps)
    }

    pub(crate) fn append_track_tasks(
        &self,
        track: Arc<UnsafeMutex<Box<Track>>>,
        predecessor: Option<String>,
        tasks: &mut Vec<ProcessTask>,
        deps: &mut std::collections::HashMap<String, Vec<String>>,
    ) -> (String, String) {
        use crate::message::ConnectableRef;
        let t = track.lock();
        if t.is_folder {
            let folder_input = ProcessTask::FolderInput(track.clone());
            let folder_input_key = Self::task_key(&folder_input);
            tasks.push(folder_input.clone());
            let folder_input_deps: Vec<_> = predecessor.into_iter().collect();
            deps.insert(folder_input_key.clone(), folder_input_deps);

            let mut source_keys: std::collections::HashMap<ConnectableRef, String> =
                std::collections::HashMap::new();
            let mut target_keys: std::collections::HashMap<ConnectableRef, String> =
                std::collections::HashMap::new();
            source_keys.insert(ConnectableRef::TrackInput, folder_input_key.clone());
            target_keys.insert(ConnectableRef::TrackInput, folder_input_key.clone());

            let mut plugin_keys: Vec<String> = Vec::new();
            for idx in 0..t.clap_plugins.len() {
                let plugin_task = ProcessTask::Plugin {
                    track: track.clone(),
                    kind: PluginKind::Clap,
                    index: idx,
                };
                let plugin_key = Self::task_key(&plugin_task);
                let id = t.clap_plugins[idx].id;
                source_keys.insert(ConnectableRef::ClapPlugin(id), plugin_key.clone());
                target_keys.insert(ConnectableRef::ClapPlugin(id), plugin_key.clone());
                tasks.push(plugin_task);
                deps.insert(plugin_key.clone(), vec![folder_input_key.clone()]);
                plugin_keys.push(plugin_key);
            }
            for idx in 0..t.vst3_plugins.len() {
                let plugin_task = ProcessTask::Plugin {
                    track: track.clone(),
                    kind: PluginKind::Vst3,
                    index: idx,
                };
                let plugin_key = Self::task_key(&plugin_task);
                let id = t.vst3_plugins[idx].id;
                source_keys.insert(ConnectableRef::Vst3Plugin(id), plugin_key.clone());
                target_keys.insert(ConnectableRef::Vst3Plugin(id), plugin_key.clone());
                tasks.push(plugin_task);
                deps.insert(plugin_key.clone(), vec![folder_input_key.clone()]);
                plugin_keys.push(plugin_key);
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            for idx in 0..t.lv2_plugins.len() {
                let plugin_task = ProcessTask::Plugin {
                    track: track.clone(),
                    kind: PluginKind::Lv2,
                    index: idx,
                };
                let plugin_key = Self::task_key(&plugin_task);
                let id = t.lv2_plugins[idx].id;
                source_keys.insert(ConnectableRef::Lv2Plugin(id), plugin_key.clone());
                target_keys.insert(ConnectableRef::Lv2Plugin(id), plugin_key.clone());
                tasks.push(plugin_task);
                deps.insert(plugin_key.clone(), vec![folder_input_key.clone()]);
                plugin_keys.push(plugin_key);
            }

            let mut child_keys = Vec::new();
            for child_track in &t.child_tracks {
                let (child_first, child_last) = self.append_track_tasks(
                    child_track.clone(),
                    Some(folder_input_key.clone()),
                    tasks,
                    deps,
                );
                let child_name = child_track.lock().name.clone();
                source_keys.insert(
                    ConnectableRef::ChildTrack(child_name.clone()),
                    child_last.clone(),
                );
                target_keys.insert(ConnectableRef::ChildTrack(child_name), child_first.clone());
                child_keys.push((child_first, child_last.clone()));
            }

            let folder_output = ProcessTask::FolderOutput(track.clone());
            let folder_output_key = Self::task_key(&folder_output);
            source_keys.insert(ConnectableRef::TrackOutput, folder_output_key.clone());
            target_keys.insert(ConnectableRef::TrackOutput, folder_output_key.clone());
            tasks.push(folder_output.clone());
            let mut folder_output_deps = vec![folder_input_key.clone()];
            folder_output_deps.extend(plugin_keys);
            folder_output_deps.extend(child_keys.iter().map(|(_, last)| last.clone()));
            deps.insert(folder_output_key.clone(), folder_output_deps);

            // Add cross-connectable dependencies based on the track's routing graph.
            // This includes child->plugin, plugin->folder output, plugin->plugin, etc.
            for conn in t.connectable_connections() {
                let Some(source_key) = source_keys.get(&conn.from) else {
                    continue;
                };
                let Some(target_key) = target_keys.get(&conn.to) else {
                    continue;
                };
                if source_key == target_key {
                    continue;
                }
                let entry = deps.entry(target_key.clone()).or_default();
                if !entry.contains(source_key) {
                    entry.push(source_key.clone());
                }
            }

            (folder_input_key, folder_output_key)
        } else {
            let task = ProcessTask::Track(track.clone());
            let task_key = Self::task_key(&task);
            tasks.push(task.clone());
            deps.insert(
                task_key.clone(),
                predecessor.into_iter().collect::<Vec<_>>(),
            );
            (task_key.clone(), task_key)
        }
    }

    pub(crate) fn task_track_name(task: &ProcessTask) -> String {
        match task {
            ProcessTask::Track(t) | ProcessTask::FolderInput(t) | ProcessTask::FolderOutput(t) => {
                t.lock().name.clone()
            }
            ProcessTask::Plugin { track, .. } => track.lock().name.clone(),
        }
    }

    pub(crate) fn task_key(task: &ProcessTask) -> String {
        match task {
            ProcessTask::Track(t) => format!("Track:{:p}", std::sync::Arc::as_ptr(t)),
            ProcessTask::FolderInput(t) => {
                format!("FolderInput:{:p}", std::sync::Arc::as_ptr(t))
            }
            ProcessTask::FolderOutput(t) => {
                format!("FolderOutput:{:p}", std::sync::Arc::as_ptr(t))
            }
            ProcessTask::Plugin { track, kind, index } => format!(
                "Plugin:{:?}:{:p}:{}",
                kind,
                std::sync::Arc::as_ptr(track),
                index
            ),
        }
    }

    pub(crate) fn task_running_finished_contains(
        haystack: &[ProcessTask],
        needle: &ProcessTask,
    ) -> bool {
        let needle_key = Self::task_key(needle);
        haystack.iter().any(|t| Self::task_key(t) == needle_key)
    }

    pub(crate) fn task_ready(&self, task: &ProcessTask) -> bool {
        match task {
            ProcessTask::Track(t) | ProcessTask::FolderInput(t) => {
                let track = t.lock();
                let ready = track.audio.ready();
                if !ready {
                    let task_kind = match task {
                        ProcessTask::Track(_) => "Track",
                        ProcessTask::FolderInput(_) => "FolderInput",
                        _ => "?",
                    };
                    let mut input_status = Vec::new();
                    for (idx, input) in track.audio.ins.iter().enumerate() {
                        let finished = *input.finished.lock();
                        let conn_count = input.connection_count.load(Ordering::Relaxed);
                        let mut pending = Vec::new();
                        for conn in input.connections.lock().iter() {
                            pending.push(*conn.finished.lock());
                        }
                        input_status.push(format!(
                            "in{}: finished={} conns={} pending_finished={:?}",
                            idx, finished, conn_count, pending
                        ));
                    }
                    tracing::info!(
                        "task not ready for '{}' ({}): {}",
                        track.name,
                        task_kind,
                        input_status.join(", ")
                    );
                }
                ready
            }
            ProcessTask::Plugin { .. } | ProcessTask::FolderOutput(_) => true,
        }
    }

    pub(crate) fn task_dependencies_satisfied(&self, task: &ProcessTask) -> bool {
        let key = Self::task_key(task);
        let Some(deps) = self.cycle_task_deps.get(&key) else {
            return true;
        };
        let finished_keys: std::collections::HashSet<String> = self
            .cycle_tasks_finished
            .iter()
            .map(Self::task_key)
            .collect();
        deps.iter().all(|d| finished_keys.contains(d))
    }

    pub(crate) fn prepare_task_track(&self, task: &ProcessTask) {
        let track = match task {
            ProcessTask::Track(t) | ProcessTask::FolderInput(t) | ProcessTask::FolderOutput(t) => t,
            ProcessTask::Plugin { track, .. } => track,
        };
        let t = track.lock();
        let transport_sample = if self.session_clip_playback_enabled && self.playing {
            self.session_transport_sample
        } else {
            self.transport_sample
        };
        t.set_transport_sample(transport_sample);
        t.set_loop_config(self.loop_enabled, self.loop_range_samples);
        t.set_transport_timing(self.tempo_bpm, self.tsig_num, self.tsig_denom);
        t.process_epoch = self.track_process_epoch;
        t.set_clip_playback_enabled(self.clip_playback_enabled && self.playing);
        t.set_session_clip_playback_enabled(self.session_clip_playback_enabled && self.playing);
        t.set_record_tap_enabled(self.playing && self.record_enabled);
        t.audio.processing = true;
    }

    pub(crate) async fn send_tasks(&mut self) -> bool {
        if !self.playing {
            return false;
        }
        self.refresh_realtime_infection();
        self.force_stalled_task_completions();

        if self.cycle_tasks.is_empty() {
            let (tasks, deps) = self.build_task_graph();
            let task_names: Vec<String> = tasks.iter().map(Self::task_track_name).collect();
            tracing::debug!(
                "send_tasks rebuilt graph: {} tasks ({:?})",
                tasks.len(),
                task_names
            );
            self.cycle_tasks = tasks;
            self.cycle_task_deps = deps;
            self.cycle_tasks_running.clear();
            self.cycle_tasks_finished.clear();
        }

        let mut finished = true;
        let mut dispatched = 0;
        loop {
            let next_task = {
                let mut next = None;
                tracing::debug!(
                    "selecting next: cycle={} running={} finished={}",
                    self.cycle_tasks.len(),
                    self.cycle_tasks_running.len(),
                    self.cycle_tasks_finished.len()
                );
                for task in &self.cycle_tasks {
                    let in_running =
                        Self::task_running_finished_contains(&self.cycle_tasks_running, task);
                    let in_finished =
                        Self::task_running_finished_contains(&self.cycle_tasks_finished, task);
                    tracing::debug!(
                        "checking task {} in_running={} in_finished={}",
                        Self::task_track_name(task),
                        in_running,
                        in_finished
                    );
                    if in_finished || in_running {
                        continue;
                    }
                    finished = false;
                    if !self.task_dependencies_satisfied(task) {
                        continue;
                    }
                    if !self.task_ready(task) {
                        continue;
                    }
                    next = Some(task.clone());
                    break;
                }
                next
            };

            let Some(task) = next_task else {
                if !finished && dispatched == 0 {
                    tracing::info!(
                        "send_tasks returning finished={} (dispatched {})",
                        finished,
                        dispatched
                    );
                } else {
                    tracing::debug!(
                        "send_tasks returning finished={} (dispatched {})",
                        finished,
                        dispatched
                    );
                }
                return finished;
            };
            let Some(worker_index) = self.take_ready_worker_index() else {
                self.force_stalled_task_completions();
                tracing::debug!(
                    "send_tasks returning false (no ready worker; dispatched {})",
                    dispatched
                );
                return false;
            };

            if Self::task_running_finished_contains(&self.cycle_tasks_finished, &task)
                || Self::task_running_finished_contains(&self.cycle_tasks_running, &task)
            {
                continue;
            }
            dispatched += 1;
            let task_key = Self::task_key(&task);
            tracing::info!(
                "send_tasks dispatching {} (running={} finished={})",
                Self::task_track_name(&task),
                self.cycle_tasks_running.len(),
                self.cycle_tasks_finished.len()
            );
            self.prepare_task_track(&task);
            self.cycle_tasks_running.push(task.clone());
            tracing::debug!(
                "inserted task {} -> running_size={}",
                Self::task_track_name(&task),
                self.cycle_tasks_running.len()
            );
            self.task_processing_started_at
                .insert(task_key.clone(), Instant::now());
            let worker = &self.workers[worker_index];
            if let Err(e) = worker.tx.send(Message::ProcessTask(task.clone())).await {
                self.cycle_tasks_running
                    .retain(|t| Self::task_key(t) != task_key);
                self.task_processing_started_at.remove(&task_key);
                self.notify_clients(Err(format!("Failed to send task to worker: {}", e)))
                    .await;
            }
        }
    }

    pub(crate) async fn on_all_tracks_finished(&mut self) {
        if self.transport_restart_pending {
            let state = self.state.lock();
            for track in state.tracks.values() {
                track.lock().take_hw_midi_out_events();
            }
        } else if self.hw_worker.is_some() {
            self.active_hw_notes_cycle_start = self.active_hw_notes_by_track.clone();
            let mut out_events = self.collect_hw_midi_output_events_by_device();
            if self.loop_enabled
                && let Some((_, loop_end)) = self.loop_range_samples
            {
                let cycle_end = self
                    .transport_sample
                    .saturating_add(self.current_cycle_samples());
                if self.transport_sample < loop_end && cycle_end >= loop_end {
                    let wrap_frame = loop_end
                        .saturating_sub(self.transport_sample)
                        .min(self.current_cycle_samples())
                        as u32;
                    out_events.extend(self.note_off_events_for_active_snapshot(
                        &self.active_hw_notes_cycle_start,
                        wrap_frame,
                    ));
                    out_events.sort_by(|a, b| {
                        a.event
                            .frame
                            .cmp(&b.event.frame)
                            .then_with(|| a.device.cmp(&b.device))
                    });
                }
            }
            self.pending_hw_midi_out_events_by_device.extend(out_events);
        } else {
            self.pending_hw_midi_out_events = self.collect_hw_midi_output_events();
        }
        self.request_hw_cycle().await;
    }

    pub(crate) fn take_ready_worker_index(&mut self) -> Option<usize> {
        while !self.ready_workers.is_empty() {
            let worker_index = self.ready_workers.remove(0);
            if worker_index < self.workers.len() {
                return Some(worker_index);
            }
        }
        None
    }

    pub(crate) fn push_ready_worker(&mut self, worker_index: usize) {
        self.ready_workers.push(worker_index);
    }

    pub(crate) async fn publish_track_meters(&mut self) {
        if !self.should_publish_track_meters() {
            return;
        }
        let tracks: Vec<(String, Arc<UnsafeMutex<Box<Track>>>)> = self
            .state
            .lock()
            .tracks
            .iter()
            .map(|(name, track)| (name.clone(), track.clone()))
            .collect();
        let mut snapshot = Vec::with_capacity(tracks.len());
        for (name, track) in &tracks {
            let linear = self
                .track_meter_linear_by_track
                .get(name)
                .cloned()
                .unwrap_or_else(|| track.lock().output_meter_linear());
            let output_db = linear
                .iter()
                .copied()
                .map(Self::meter_linear_to_db)
                .collect::<Vec<_>>();
            snapshot.push((name.clone(), output_db));
        }
        self.latest_track_meter_snapshot = Arc::new(snapshot);
        let meters = self.collect_changed_track_meters(&tracks);
        for (track_name, output_db) in meters {
            self.notify_clients(Ok(Action::TrackMeters {
                track_name,
                output_db,
            }))
            .await;
        }
    }

    pub(crate) async fn publish_session_runtime_reports(&mut self) {
        if self
            .last_session_report_publish
            .is_some_and(|t| t.elapsed() < Self::SESSION_RUNTIME_REPORT_INTERVAL)
        {
            return;
        }

        let mut current = HashMap::<(String, usize), (SessionSlotState, usize, usize)>::new();
        {
            let state = self.state.lock();
            for (track_name, track) in &state.tracks {
                let track = track.lock();
                for launch in &track.pending_session_launches {
                    current.insert(
                        (track_name.clone(), launch.scene_index),
                        (SessionSlotState::Queued, 0, 0),
                    );
                }
                for clip in &track.playing_session_clips {
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

        let previous_keys: Vec<(String, usize)> =
            self.session_report_state.keys().cloned().collect();
        for key in previous_keys {
            if current.contains_key(&key) {
                continue;
            }
            let (track_name, scene_index) = key;
            self.notify_clients(Ok(Action::SessionRuntimeReport {
                track_name,
                scene_index,
                state: SessionSlotState::Stopped,
                play_position_samples: 0,
                elapsed_samples: 0,
            }))
            .await;
        }

        for ((track_name, scene_index), (state, play_position_samples, elapsed_samples)) in &current
        {
            self.notify_clients(Ok(Action::SessionRuntimeReport {
                track_name: track_name.clone(),
                scene_index: *scene_index,
                state: *state,
                play_position_samples: *play_position_samples,
                elapsed_samples: *elapsed_samples,
            }))
            .await;
        }

        self.session_report_state = current.into_iter().map(|(k, (s, _, _))| (k, s)).collect();
        self.last_session_report_publish = Some(Instant::now());
    }

    pub(crate) async fn publish_clap_state_dirty(&mut self) {
        let tracks: Vec<(String, Arc<UnsafeMutex<Box<Track>>>)> = self
            .state
            .lock()
            .tracks
            .iter()
            .map(|(name, track)| (name.clone(), track.clone()))
            .collect();
        for (track_name, track) in &tracks {
            let dirty = track.lock().take_dirty_clap_instances();
            for instance_id in dirty {
                self.notify_clients(Ok(Action::TrackClapStateDirty {
                    track_name: track_name.clone(),
                    instance_id,
                }))
                .await;
            }
        }
    }

    pub(crate) fn reset_meters_after_stop(&mut self) {
        self.last_hw_out_meter_publish = None;
        self.last_track_meter_publish = None;
        self.hw_out_peak_hold_linear.fill(0.0);
        #[cfg(any(target_os = "freebsd", target_os = "linux", target_os = "openbsd"))]
        {
            self.last_hw_out_meter_linear.clear();
        }
        let hw_channels = self.latest_hw_out_meter_db.len();
        self.latest_hw_out_meter_db = Arc::new(vec![-90.0; hw_channels]);

        let tracks: Vec<(String, Arc<UnsafeMutex<Box<Track>>>)> = self
            .state
            .lock()
            .tracks
            .iter()
            .map(|(name, track)| (name.clone(), track.clone()))
            .collect();
        self.track_meter_linear_by_track.clear();
        let mut snapshot = Vec::with_capacity(tracks.len());
        for (name, track) in tracks {
            let t = track.lock();
            t.clear_output_meters();
            let width = t.output_meter_linear().len();
            let zero_linear = vec![0.0; width];
            self.track_meter_linear_by_track
                .insert(name.clone(), zero_linear);
            snapshot.push((name, vec![-90.0; width]));
        }
        self.latest_track_meter_snapshot = Arc::new(snapshot);
    }

    pub(crate) async fn handle_request(&mut self, a: Action) {
        match a {
            Action::Log { source, message } => {
                self.notify_clients(Ok(Action::Log { source, message }))
                    .await;
            }
            Action::Undo => {
                let actions = match self.history.undo() {
                    Some(actions) => actions,
                    None => {
                        self.notify_clients(Ok(Action::Undo)).await;
                        self.notify_clients(Ok(Action::HistoryState {
                            dirty: self.history.is_dirty(),
                        }))
                        .await;
                        return;
                    }
                };

                let was_suspended = self.history_suspended;
                self.history_suspended = true;
                for action in actions {
                    self.handle_request_inner(action, false).await;
                }
                self.history_suspended = was_suspended;
                self.notify_clients(Ok(Action::Undo)).await;
                self.notify_clients(Ok(Action::HistoryState {
                    dirty: self.history.is_dirty(),
                }))
                .await;
            }
            Action::Redo => {
                let actions = match self.history.redo() {
                    Some(actions) => actions,
                    None => {
                        self.notify_clients(Ok(Action::Redo)).await;
                        self.notify_clients(Ok(Action::HistoryState {
                            dirty: self.history.is_dirty(),
                        }))
                        .await;
                        return;
                    }
                };

                let was_suspended = self.history_suspended;
                self.history_suspended = true;
                for action in actions {
                    self.handle_request_inner(action, false).await;
                }
                self.history_suspended = was_suspended;
                self.notify_clients(Ok(Action::Redo)).await;
                self.notify_clients(Ok(Action::HistoryState {
                    dirty: self.history.is_dirty(),
                }))
                .await;
            }
            Action::ApplyGroupedActions(actions) => {
                self.handle_request_inner(Action::BeginHistoryGroup, true)
                    .await;
                for action in actions {
                    self.handle_request_inner(action, true).await;
                }
                self.handle_request_inner(Action::EndHistoryGroup, true)
                    .await;
            }
            Action::Session(_) => {
                self.handle_request_inner(a, false).await;
            }
            other => {
                self.handle_request_inner(other, true).await;
            }
        }
    }

    pub(crate) async fn handle_quit(&mut self, a: Action) {
        self.flush_recordings().await;
        // Stop the HW worker before notifying the GUI so the
        // OSS audio channels are halted and closed from the
        // worker's own thread. The GUI calls exit(0) upon
        // receiving the Quit response, which skips Rust
        // destructors. Without this, the kernel's dsp_close
        // drains pending audio buffers for up to CHN_TIMEOUT
        // (5s) during process teardown.
        if let Some(worker) = self.hw_worker.take() {
            if let Some(hw) = &self.hw_driver {
                hw.lock().request_stop();
            }
            // Send MIDI panic (All Sound Off) for any active
            // notes before stopping the worker.
            let panic_events = self.panic_events_for_all_hw_midi_outputs();
            if !panic_events.is_empty() {
                let _ = worker.tx.send(Message::HWMidiOutEvents(panic_events)).await;
            }
            // Send Quit to the worker so it stops its audio
            // cycle loop and releases the driver.
            if let Err(e) = worker.tx.send(Message::Request(a.clone())).await {
                error!("Error sending quit message to HW worker: {e}");
            }
            worker
                .handle
                .await
                .unwrap_or_else(|e| error!("Error waiting for HW worker to quit: {e}"));
        }
        // Explicitly close audio and MIDI fds before sending
        // the Quit response. The GUI calls exit(0) upon
        // receiving it, which skips destructors — any
        // still-open device fd would trigger the kernel's
        // 5-second drain during process teardown.
        if let Some(hw) = &self.hw_driver {
            hw.lock().close_fds();
        }
        self.midi_hub.lock().close_all();
        self.hw_driver = None;
        self.notify_clients(Ok(Action::Quit)).await;
        self.ready_workers.clear();
        while !self.workers.is_empty() {
            let worker = self.workers.remove(0);
            if let Err(e) = worker.tx.send(Message::Request(a.clone())).await {
                error!("Error sending quit message to worker: {e}");
            }
            worker
                .handle
                .await
                .unwrap_or_else(|e| error!("Error waiting for worker to quit: {e}"));
        }
        #[cfg(unix)]
        {
            self.jack_runtime = None;
        }
        self.osc_server = None;
    }

    #[inline]
    pub(crate) fn box_bool<'a>(
        fut: impl std::future::Future<Output = bool> + Send + 'a,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(fut)
    }

    pub(crate) async fn handle_request_inner(
        &mut self,
        mut action_to_process: Action,
        record_history: bool,
    ) {
        let a = action_to_process.clone();
        let suppress_timing_history = self.playing
            && matches!(
                &action_to_process,
                Action::SetTempo(_) | Action::SetTimeSignature { .. } | Action::SetTempoMap { .. }
            );
        let mut inverse_actions = self.prepare_inverse_actions(
            &action_to_process,
            record_history,
            suppress_timing_history,
        );

        match action_to_process {
            Action::Play => {
                if Self::box_bool(self.handle_play(a.clone())).await {
                    return;
                }
            }
            Action::Pause => {
                if Self::box_bool(self.handle_pause(a.clone())).await {
                    return;
                }
            }
            Action::Stop => {
                if Self::box_bool(self.handle_stop(a.clone())).await {
                    return;
                }
            }
            Action::SessionPlay => {
                if Self::box_bool(self.handle_session_play(a.clone())).await {
                    return;
                }
            }
            Action::JumpToEnd => {
                self.transport_sample = self.normalize_transport_sample(self.session_end_sample());
                self.notify_clients(Ok(Action::TransportPosition(self.transport_sample)))
                    .await;
            }
            Action::Panic => {
                if Self::box_bool(self.handle_panic(a.clone())).await {
                    return;
                }
            }
            Action::Session(ref session_action) => {
                self.handle_session_action(session_action.clone()).await;
            }
            Action::SessionRuntimeReport { .. } => {}
            Action::SessionMidiLearnTriggered { .. } => {}
            Action::SetClipPlaybackEnabled(enabled) => {
                self.clip_playback_enabled = enabled;
                for track in self.state.lock().tracks.values() {
                    track.lock().set_clip_playback_enabled(enabled);
                }
            }
            Action::SetSessionClipPlaybackEnabled(enabled) => {
                self.session_clip_playback_enabled = enabled;
                for track in self.state.lock().tracks.values() {
                    track.lock().set_session_clip_playback_enabled(enabled);
                }
            }
            Action::TransportPosition(..) => {
                if Self::box_bool(self.handle_transport_position(a.clone())).await {
                    return;
                }
            }
            Action::SetLoopEnabled(enabled) => {
                self.loop_enabled = enabled && self.loop_range_samples.is_some();
                self.notified_loop_wrap_sample = None;
            }
            Action::SetLoopRange(..) => {
                if Self::box_bool(self.handle_set_loop_range(a.clone())).await {
                    return;
                }
            }
            Action::SetPunchEnabled(enabled) => {
                self.punch_enabled = enabled && self.punch_range_samples.is_some();
            }
            Action::SetPunchRange(range) => {
                self.punch_range_samples = range.and_then(|(start, end)| {
                    if end > start {
                        Some((start, end))
                    } else {
                        None
                    }
                });
                self.punch_enabled = self.punch_range_samples.is_some();
            }
            Action::SetMetronomeEnabled(enabled) => {
                self.metronome_enabled = enabled;
                if enabled {
                    self.ensure_metronome_track().await;
                }
                if let Some(track) = self.state.lock().tracks.get(Self::METRONOME_TRACK).cloned() {
                    track.lock().set_metronome_enabled(enabled);
                }
            }
            Action::SetTempo(bpm) => {
                self.tempo_bpm = bpm.max(1.0);
            }
            Action::SetTimeSignature {
                numerator,
                denominator,
            } => {
                self.tsig_num = numerator.max(1);
                self.tsig_denom = denominator.max(1);
            }
            Action::SetTempoMap {
                ref tempo_points,
                ref time_signature_points,
            } => {
                self.tempo_points = tempo_points.clone();
                self.time_signature_points = time_signature_points.clone();
                self.update_global_tempo_from_map();
            }
            Action::SetOscEnabled(enabled) => {
                if let Err(err) = self.set_osc_enabled_with(enabled, OscServer::start) {
                    self.notify_clients(Err(err)).await;
                }
            }
            Action::SetRecordEnabled(..) => {
                if Self::box_bool(self.handle_set_record_enabled(a.clone())).await {
                    return;
                }
            }
            Action::SetModulators(ref modulators) => {
                self.modulators = modulators.clone();
                let echoes = self.apply_modulators(self.active_transport_sample());
                for action in echoes {
                    self.notify_clients(Ok(action)).await;
                }
            }
            Action::SetTrackAutomationLanes {
                ref track_name,
                ref lanes,
                mode,
            } => {
                if let Some(track) = self.state.lock().tracks.get(track_name) {
                    let track = track.lock();
                    track.automation_lanes = lanes.clone();
                    track.automation_mode = mode;
                }
            }
            Action::TrackAutomationToggleLane { .. } => {
                if Self::box_bool(self.handle_track_automation_toggle_lane(a.clone())).await {
                    return;
                }
            }
            Action::TrackAutomationInsertPoint { .. } => {
                if Self::box_bool(self.handle_track_automation_insert_point(a.clone())).await {
                    return;
                }
            }
            Action::TrackAutomationDeletePoint { .. } => {
                if Self::box_bool(self.handle_track_automation_delete_point(a.clone())).await {
                    return;
                }
            }
            Action::TrackAutomationSetMode {
                ref track_name,
                mode,
            } => {
                if let Some(track) = self.state.lock().tracks.get(track_name).cloned() {
                    track.lock().automation_mode = mode;
                }
            }
            Action::RequestTrackList => {
                let names: Vec<String> = self.state.lock().tracks.keys().cloned().collect();
                self.notify_clients(Ok(Action::TrackList(names))).await;
            }
            Action::TrackList(_) => {}
            Action::RequestTransportState => {
                self.notify_clients(Ok(Action::TransportState {
                    sample: self.transport_sample,
                    tempo_bpm: self.tempo_bpm,
                    playing: self.playing,
                    paused: !self.transport_running && self.playing,
                    tsig_num: self.tsig_num,
                    tsig_denom: self.tsig_denom,
                }))
                .await;
            }
            Action::TransportState { .. } => {}
            Action::SetStepRecording(enabled) => {
                self.step_recording_enabled = enabled;
            }
            Action::BeginHistoryGroup if self.history_group.is_none() => {
                self.history_group = Some(UndoEntry {
                    forward_actions: vec![],
                    inverse_actions: vec![],
                });
            }
            Action::EndHistoryGroup => {
                if let Some(mut group) = self.history_group.take()
                    && !group.forward_actions.is_empty()
                    && !group.inverse_actions.is_empty()
                {
                    let mut add_tracks = Vec::new();
                    let mut connections = Vec::new();
                    let mut rest = Vec::new();
                    for action in group.inverse_actions {
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
                    self.history.record(group);
                }
            }
            Action::SetSessionPath(ref path) => {
                self.session_dir = Some(Path::new(path).to_path_buf());
                self.ensure_session_subdirs();
                #[cfg(all(unix, not(target_os = "macos")))]
                let _lv2_dir = self.session_plugins_dir();
                for track in self.state.lock().tracks.values() {
                    track.lock().set_session_base_dir(self.session_dir.clone());
                }
            }
            Action::MarkHistorySavePoint => {
                self.history.mark_save_point();
                self.notify_clients(Ok(Action::HistoryState {
                    dirty: self.history.is_dirty(),
                }))
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
            Action::Quit => {
                self.handle_quit(a.clone()).await;
                return;
            }
            Action::AddTrack {
                ref name,
                audio_ins,
                midi_ins,
                audio_outs,
                midi_outs,
                folder,
            } => {
                self.handle_add_track(
                    name.clone(),
                    audio_ins,
                    midi_ins,
                    audio_outs,
                    midi_outs,
                    folder,
                )
                .await;
            }
            Action::TrackAddAudioInput(..) => {
                if Self::box_bool(self.handle_track_add_audio_input(a.clone())).await {
                    return;
                }
            }
            Action::TrackAddAudioOutput(..) => {
                if Self::box_bool(self.handle_track_add_audio_output(a.clone())).await {
                    return;
                }
            }
            Action::TrackRemoveAudioInput(..) => {
                if Self::box_bool(self.handle_track_remove_audio_input(a.clone())).await {
                    return;
                }
            }
            Action::TrackRemoveAudioOutput(..) => {
                if Self::box_bool(self.handle_track_remove_audio_output(a.clone())).await {
                    return;
                }
            }
            Action::RenameTrack { .. } => {
                if Self::box_bool(self.handle_rename_track(a.clone())).await {
                    return;
                }
            }
            Action::RemoveTrack(ref name) => {
                self.handle_remove_track(name.clone(), record_history).await;
                inverse_actions = None;
            }
            Action::TrackLevel(ref name, level) => {
                if name == "hw:out" {
                    self.hw_out_level_db = level;
                } else if let Some(track) = self.state.lock().tracks.get(name) {
                    track.lock().set_level(level);
                }
            }
            Action::TrackBalance(ref name, balance) => {
                if name == "hw:out" {
                    self.hw_out_balance = balance.clamp(-1.0, 1.0);
                } else if let Some(track) = self.state.lock().tracks.get(name) {
                    track.lock().set_balance(balance);
                }
            }
            Action::TrackAutomationLevel(ref name, level) => {
                tracing::debug!(%name, level, "engine received TrackAutomationLevel");
                if name == "hw:out" {
                    self.hw_out_level_db = level;
                } else if let Some(track) = self.state.lock().tracks.get(name) {
                    track.lock().set_level(level);
                }
            }
            Action::TrackAutomationBalance(ref name, balance) => {
                if name == "hw:out" {
                    self.hw_out_balance = balance.clamp(-1.0, 1.0);
                } else if let Some(track) = self.state.lock().tracks.get(name) {
                    track.lock().set_balance(balance);
                }
            }
            Action::TrackMidiCc { .. } => {
                if Self::box_bool(self.handle_track_midi_cc(a.clone())).await {
                    return;
                }
            }
            Action::RequestMeterSnapshot => {
                self.notify_clients(Ok(Action::MeterSnapshot {
                    hw_out_db: self.latest_hw_out_meter_db.clone(),
                    track_meters: self.latest_track_meter_snapshot.clone(),
                }))
                .await;
                return;
            }
            Action::TrackMeters { .. } => {}
            Action::MeterSnapshot { .. } => {}
            Action::TrackToggleArm(..) => {
                if Self::box_bool(self.handle_track_toggle_arm(a.clone())).await {
                    return;
                }
            }
            Action::TrackToggleMute(ref name) => {
                if name == "hw:out" {
                    self.hw_out_muted = !self.hw_out_muted;
                } else if let Some(track) = self.state.lock().tracks.get(name) {
                    track.lock().mute();
                }
            }
            Action::TrackTogglePhase(ref name) => {
                if let Some(track) = self.state.lock().tracks.get(name) {
                    track.lock().invert_phase();
                }
            }
            Action::TrackToggleSolo(ref name) => {
                if name == "hw:out" {
                    return;
                }
                if let Some(track) = self.state.lock().tracks.get(name) {
                    track.lock().solo();
                }
            }
            Action::TrackToggleMaster(ref name) => {
                if let Some(track) = self.state.lock().tracks.get(name) {
                    track.lock().toggle_master();
                }
            }
            Action::TrackToggleInputMonitor {
                ref track_name,
                lane,
            } => {
                if let Some(track) = self.state.lock().tracks.get(track_name) {
                    track.lock().toggle_input_monitor(lane);
                }
            }
            Action::TrackToggleDiskMonitor {
                ref track_name,
                lane,
            } => {
                if let Some(track) = self.state.lock().tracks.get(track_name) {
                    track.lock().toggle_disk_monitor(lane);
                }
            }
            Action::TrackToggleMidiInputMonitor {
                ref track_name,
                lane,
            } => {
                if let Some(track) = self.state.lock().tracks.get(track_name) {
                    track.lock().toggle_midi_input_monitor(lane);
                }
            }
            Action::TrackToggleMidiDiskMonitor {
                ref track_name,
                lane,
            } => {
                if let Some(track) = self.state.lock().tracks.get(track_name) {
                    track.lock().toggle_midi_disk_monitor(lane);
                }
            }
            Action::TrackSetColor {
                ref track_name,
                color,
            } => {
                if let Some(track) = self.state.lock().tracks.get(track_name) {
                    track.lock().color = color;
                }
            }
            Action::TrackArmMidiLearn {
                ref track_name,
                target,
            } => {
                if let Err(e) = self.track_handle_or_err(track_name) {
                    self.notify_clients(Err(e)).await;
                    return;
                }
                self.pending_midi_learn = Some((track_name.clone(), target, None));
            }
            Action::GlobalArmMidiLearn { target } => {
                self.pending_global_midi_learn = Some(target);
            }
            Action::SessionArmMidiLearn { ref target } => {
                self.pending_session_midi_learn = Some(target.clone());
            }
            Action::TrackSetMidiLearnBinding { .. } => {
                if Self::box_bool(self.handle_track_set_midi_learn_binding(a.clone())).await {
                    return;
                }
            }
            Action::SetGlobalMidiLearnBinding { .. } => {
                if Self::box_bool(self.handle_set_global_midi_learn_binding(a.clone())).await {
                    return;
                }
            }
            Action::SetSessionMidiLearnBinding { .. } => {
                if Self::box_bool(self.handle_set_session_midi_learn_binding(a.clone())).await {
                    return;
                }
            }
            Action::TrackSetFolder { .. } => {
                if Self::box_bool(self.handle_track_set_folder(a.clone())).await {
                    return;
                }
            }
            Action::TrackSetParent {
                ref track_name,
                ref parent_name,
            } => {
                self.handle_track_set_parent(track_name.as_str(), parent_name.as_deref())
                    .await;
            }
            Action::TrackToggleFolder { .. } => {
                if Self::box_bool(self.handle_track_toggle_folder(a.clone())).await {
                    return;
                }
            }
            Action::TrackSetMidiLaneChannel { .. } => {
                if Self::box_bool(self.handle_track_set_midi_lane_channel(a.clone())).await {
                    return;
                }
            }
            Action::TrackSetFrozen { .. } => {
                if Self::box_bool(self.handle_track_set_frozen(a.clone())).await {
                    return;
                }
            }
            Action::TrackSetSessionSlot { .. } => {
                if Self::box_bool(self.handle_track_set_session_slot(a.clone())).await {
                    return;
                }
            }
            Action::TrackSetSessionSlotPlayEnabled { .. } => {
                if self
                    .handle_track_set_session_slot_play_enabled(a.clone())
                    .await
                {
                    return;
                }
            }
            Action::TrackOfflineBounce { .. } => {
                self.handle_track_offline_bounce(action_to_process).await;
                return;
            }
            Action::TrackOfflineBounceCancel { .. } => {}
            Action::TrackOfflineBounceCancelAll => {}
            Action::TrackOfflineBounceCanceled { .. } => {}
            Action::TrackOfflineBounceProgress { .. } => {}
            Action::PianoKey {
                ref track_name,
                note,
                velocity,
                on,
            } => {
                if let Some(track) = self.state.lock().tracks.get(track_name) {
                    let status = if on { 0x90 } else { 0x80 };
                    let event = MidiEvent::new(0, vec![status, note.min(127), velocity.min(127)]);
                    track.lock().push_hw_midi_events(&[event]);
                }
            }
            Action::ModifyMidiNotes { .. }
            | Action::ModifyMidiControllers { .. }
            | Action::DeleteMidiControllers { .. }
            | Action::InsertMidiControllers { .. }
            | Action::DeleteMidiNotes { .. }
            | Action::InsertMidiNotes { .. } => {
                if let Err(e) = self.apply_midi_edit_action(&action_to_process) {
                    self.notify_clients(Err(e)).await;
                    return;
                }
            }
            Action::SetMidiSysExEvents { .. } => {
                if let Err(e) = self.apply_midi_edit_action(&action_to_process) {
                    self.notify_clients(Err(e)).await;
                    return;
                }
            }
            Action::TrackClearDefaultPassthrough { .. } => {
                if Self::box_bool(self.handle_track_clear_default_passthrough(a.clone())).await {
                    return;
                }
            }
            Action::TrackClearPlugins { .. } => {
                if Self::box_bool(self.handle_track_clear_plugins(a.clone())).await {
                    return;
                }
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            Action::ClipSetLv2PluginState { ref track_name, .. } => {
                self.notify_clients(Err(format!(
                    "Track '{}': clip LV2 plugin state changes are not supported",
                    track_name
                )))
                .await;
            }
            Action::TrackGetClapNoteNames { .. } => {
                if Self::box_bool(self.handle_track_get_clap_note_names(a.clone())).await {
                    return;
                }
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            Action::TrackGetLv2Midnam { .. } => {
                if Self::box_bool(self.handle_track_get_lv2_midnam(a.clone())).await {
                    return;
                }
            }
            Action::TrackGetPluginGraph { .. } => {
                if Self::box_bool(self.handle_track_get_plugin_graph(a.clone())).await {
                    return;
                }
            }
            Action::TrackPluginGraph { .. } => {}
            Action::TrackConnectPluginAudio { .. } => {
                if Self::box_bool(self.handle_track_connect_plugin_audio(a.clone())).await {
                    return;
                }
            }
            Action::TrackConnectPluginMidi { .. } => {
                if Self::box_bool(self.handle_track_connect_plugin_midi(a.clone())).await {
                    return;
                }
            }
            Action::TrackDisconnectPluginAudio { .. } => {
                if Self::box_bool(self.handle_track_disconnect_plugin_audio(a.clone())).await {
                    return;
                }
            }
            Action::TrackDisconnectPluginMidi { .. } => {
                if Self::box_bool(self.handle_track_disconnect_plugin_midi(a.clone())).await {
                    return;
                }
            }
            Action::TrackConnectAudio { .. } => {
                if Self::box_bool(self.handle_track_connect_audio(a.clone())).await {
                    return;
                }
            }
            Action::TrackDisconnectAudio { .. } => {
                if Self::box_bool(self.handle_track_disconnect_audio(a.clone())).await {
                    return;
                }
            }
            Action::TrackConnectMidi { .. } => {
                if Self::box_bool(self.handle_track_connect_midi(a.clone())).await {
                    return;
                }
            }
            Action::TrackDisconnectMidi { .. } => {
                if Self::box_bool(self.handle_track_disconnect_midi(a.clone())).await {
                    return;
                }
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            Action::ListLv2Plugins => {
                if Self::box_bool(self.handle_list_lv2_plugins(a.clone())).await {
                    return;
                }
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            Action::Lv2Plugins(_) => {}
            #[cfg(all(unix, not(target_os = "macos")))]
            Action::Lv2PluginsUnavailable { .. } => {}
            Action::ListVst3Plugins => {
                if Self::box_bool(self.handle_list_vst3_plugins(a.clone())).await {
                    return;
                }
            }
            Action::Vst3Plugins(_) => {}
            Action::Vst3PluginsUnavailable { .. } => {}
            Action::ListClapPlugins => {
                if Self::box_bool(self.handle_list_clap_plugins(a.clone())).await {
                    return;
                }
            }
            Action::ListClapPluginsWithCapabilities => {
                if self
                    .handle_list_clap_plugins_with_capabilities(a.clone())
                    .await
                {
                    return;
                }
            }
            Action::ClapPlugins(_) => {}
            Action::ClapPluginsUnavailable { .. } => {}
            Action::TrackLoadClapPlugin {
                ref track_name,
                ref plugin_id,
                instance_id,
            } => {
                if self
                    .handle_track_load_clap_plugin(
                        track_name.as_str(),
                        plugin_id.as_str(),
                        instance_id,
                    )
                    .await
                {
                    return;
                }
            }
            Action::TrackUnloadClapPlugin {
                ref track_name,
                ref plugin_id,
            } => {
                if self
                    .handle_track_unload_clap_plugin(track_name.as_str(), plugin_id.as_str())
                    .await
                {
                    return;
                }
            }
            Action::TrackUnloadClapPluginInstance {
                ref track_name,
                instance_id,
            } => {
                if self
                    .handle_track_unload_clap_plugin_instance(track_name.as_str(), instance_id)
                    .await
                {
                    return;
                }
            }
            Action::TrackShowClapGui { .. } => {
                if Self::box_bool(self.handle_track_show_clap_gui(a.clone())).await {
                    return;
                }
            }
            Action::TrackLoadVst3Plugin {
                ref track_name,
                ref plugin_id,
                instance_id,
            } => {
                if self
                    .handle_track_load_vst3_plugin(
                        track_name.as_str(),
                        plugin_id.as_str(),
                        instance_id,
                    )
                    .await
                {
                    return;
                }
            }
            Action::TrackUnloadVst3Plugin {
                ref track_name,
                ref plugin_id,
            } => {
                if self
                    .handle_track_unload_vst3_plugin(track_name.as_str(), plugin_id.as_str())
                    .await
                {
                    return;
                }
            }
            Action::TrackUnloadVst3PluginInstance {
                ref track_name,
                instance_id,
            } => {
                if self
                    .handle_track_unload_vst3_plugin_instance(track_name.as_str(), instance_id)
                    .await
                {
                    return;
                }
            }
            Action::TrackShowVst3Gui { .. } => {
                if Self::box_bool(self.handle_track_show_vst3_gui(a.clone())).await {
                    return;
                }
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            Action::TrackLoadLv2Plugin {
                ref track_name,
                ref plugin_uri,
                instance_id,
            } => {
                if self
                    .handle_track_load_lv2_plugin(
                        track_name.as_str(),
                        plugin_uri.as_str(),
                        instance_id,
                    )
                    .await
                {
                    return;
                }
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            Action::TrackUnloadLv2Plugin {
                ref track_name,
                ref plugin_uri,
            } => {
                if self
                    .handle_track_unload_lv2_plugin(track_name.as_str(), plugin_uri.as_str())
                    .await
                {
                    return;
                }
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            Action::TrackUnloadLv2PluginInstance {
                ref track_name,
                instance_id,
            } => {
                if self
                    .handle_track_unload_lv2_plugin_instance(track_name.as_str(), instance_id)
                    .await
                {
                    return;
                }
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            Action::TrackShowLv2Gui { .. } => {
                if Self::box_bool(self.handle_track_show_lv2_gui(a.clone())).await {
                    return;
                }
            }
            Action::TrackSetPluginResourceDir { .. } => {
                if Self::box_bool(self.handle_track_set_plugin_resource_dir(a.clone())).await {
                    return;
                }
            }
            Action::TrackClapFileReferences { .. } => {
                if Self::box_bool(self.handle_track_clap_file_references(a.clone())).await {
                    return;
                }
            }
            Action::TrackUpdateClapFileReference { .. } => {
                if self
                    .handle_track_update_clap_file_reference(a.clone())
                    .await
                {
                    return;
                }
            }
            Action::ClipSetPluginResourceDir { .. } => {
                if Self::box_bool(self.handle_clip_set_plugin_resource_dir(a.clone())).await {
                    return;
                }
            }
            Action::ClipClapFileReferences { .. } => {
                if Self::box_bool(self.handle_clip_clap_file_references(a.clone())).await {
                    return;
                }
            }
            Action::ClipUpdateClapFileReference { .. } => {
                if Self::box_bool(self.handle_clip_update_clap_file_reference(a.clone())).await {
                    return;
                }
            }
            Action::TrackSetClapParameter { .. } => {
                if Self::box_bool(self.handle_track_set_clap_parameter(a.clone())).await {
                    return;
                }
            }
            Action::ClipSetClapParameter { .. } => {
                if Self::box_bool(self.handle_clip_set_clap_parameter(a.clone())).await {
                    return;
                }
            }
            Action::TrackSetClapParameterAt { .. } => {
                if Self::box_bool(self.handle_track_set_clap_parameter_at(a.clone())).await {
                    return;
                }
            }
            Action::TrackBeginClapParameterEdit { .. } => {
                if Self::box_bool(self.handle_track_begin_clap_parameter_edit(a.clone())).await {
                    return;
                }
            }
            Action::TrackEndClapParameterEdit { .. } => {
                if Self::box_bool(self.handle_track_end_clap_parameter_edit(a.clone())).await {
                    return;
                }
            }
            Action::TrackGetClapParameters { .. } => {
                if Self::box_bool(self.handle_track_get_clap_parameters(a.clone())).await {
                    return;
                }
            }
            Action::TrackClapParameters { .. } => {}
            Action::TrackClapSnapshotState { .. } => {
                if Self::box_bool(self.handle_track_clap_snapshot_state(a.clone())).await {
                    return;
                }
            }
            Action::ClipClapSnapshotState { .. } => {
                if Self::box_bool(self.handle_clip_clap_snapshot_state(a.clone())).await {
                    return;
                }
            }
            Action::TrackClapStateSnapshot { .. } => {}
            Action::ClipClapStateSnapshot { .. } => {}
            Action::TrackClapStateDirty { .. } => {}
            Action::ClipClapStateDirty { .. } => {}
            Action::TrackClapRestoreState { .. } => {
                if Self::box_bool(self.handle_track_clap_restore_state(a.clone())).await {
                    return;
                }
            }
            Action::ClipClapRestoreState { .. } => {
                if Self::box_bool(self.handle_clip_clap_restore_state(a.clone())).await {
                    return;
                }
            }
            Action::TrackSnapshotAllClapStates { .. } => {
                if Self::box_bool(self.handle_track_snapshot_all_clap_states(a.clone())).await {
                    return;
                }
            }
            Action::TrackSnapshotAllClapStatesDone { .. } => {}
            Action::TrackGetVst3Graph { .. } => {
                if Self::box_bool(self.handle_track_get_vst3_graph(a.clone())).await {
                    return;
                }
            }
            Action::TrackVst3Graph { .. } => {}
            Action::TrackSetVst3Parameter { .. } => {
                if Self::box_bool(self.handle_track_set_vst3_parameter(a.clone())).await {
                    return;
                }
            }
            Action::TrackSetPluginBypassed { .. } => {
                if Self::box_bool(self.handle_track_set_plugin_bypassed(a.clone())).await {
                    return;
                }
            }
            Action::TrackGetVst3Parameters { .. } => {
                if Self::box_bool(self.handle_track_get_vst3_parameters(a.clone())).await {
                    return;
                }
            }
            Action::TrackVst3Parameters { .. } => {}
            #[cfg(all(unix, not(target_os = "macos")))]
            Action::TrackGetLv2PluginControls { .. } => {
                if Self::box_bool(self.handle_track_get_lv2_plugin_controls(a.clone())).await {
                    return;
                }
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            Action::TrackLv2SnapshotState { .. } => {
                if Self::box_bool(self.handle_track_lv2_snapshot_state(a.clone())).await {
                    return;
                }
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            Action::ClipLv2SnapshotState { .. } => {
                if Self::box_bool(self.handle_clip_lv2_snapshot_state(a.clone())).await {
                    return;
                }
            }
            Action::TrackVst3SnapshotState { .. } => {
                if Self::box_bool(self.handle_track_vst3_snapshot_state(a.clone())).await {
                    return;
                }
            }
            Action::ClipVst3SnapshotState { .. } => {
                if Self::box_bool(self.handle_clip_vst3_snapshot_state(a.clone())).await {
                    return;
                }
            }
            Action::TrackVst3StateSnapshot { .. } => {}
            Action::ClipVst3StateSnapshot { .. } => {}
            Action::TrackVst3RestoreState { .. } => {
                if Self::box_bool(self.handle_track_vst3_restore_state(a.clone())).await {
                    return;
                }
            }
            Action::TrackConnectVst3Audio { .. } => {
                if Self::box_bool(self.handle_track_connect_vst3_audio(a.clone())).await {
                    return;
                }
            }
            Action::TrackDisconnectVst3Audio { .. } => {
                if Self::box_bool(self.handle_track_disconnect_vst3_audio(a.clone())).await {
                    return;
                }
            }
            Action::ClipMove { .. } => {
                self.handle_clip_move(a.clone()).await;
            }
            Action::AddClip { .. } => {
                if Self::box_bool(self.handle_add_clip(a.clone())).await {
                    return;
                }
            }
            Action::AddGroupedClip { .. } => {
                if Self::box_bool(self.handle_add_grouped_clip(a.clone())).await {
                    return;
                }
            }
            Action::RemoveClip {
                ref track_name,
                kind,
                ref clip_indices,
            } => {
                self.remove_clips_from_track(track_name, kind, clip_indices);
            }
            Action::RenameClip {
                ref track_name,
                kind,
                clip_index,
                ref new_name,
            } => {
                self.rename_clip_references(track_name, kind, clip_index, new_name);
            }
            Action::SetClipSourceName {
                ref track_name,
                kind,
                clip_index,
                ref name,
            } => {
                self.set_clip_source_name(track_name, clip_index, kind, name.clone());
            }
            Action::SetClipFade { .. } => {
                if Self::box_bool(self.handle_set_clip_fade(a.clone())).await {
                    return;
                }
            }
            Action::SetClipBounds {
                ref track_name,
                clip_index,
                kind,
                start,
                length,
                offset,
            } => {
                self.set_clip_bounds(track_name, clip_index, kind, start, length, offset);
            }
            Action::SyncClipBounds {
                ref track_name,
                clip_index,
                kind,
                start,
                length,
                offset,
            } => {
                self.set_clip_bounds(track_name, clip_index, kind, start, length, offset);
            }
            Action::SetClipMuted {
                ref track_name,
                clip_index,
                kind,
                muted,
            } => {
                self.set_clip_muted(track_name, clip_index, kind, muted);
            }
            Action::SetClipPluginGraphJson {
                ref track_name,
                clip_index,
                ref plugin_graph_json,
            } => {
                self.set_clip_plugin_graph_json(track_name, clip_index, plugin_graph_json.clone());
            }
            Action::SetClipPitchCorrection { .. } => {
                if Self::box_bool(self.handle_set_clip_pitch_correction(a.clone())).await {
                    return;
                }
            }
            Action::Connect {
                ref from_track,
                from_port,
                ref to_track,
                to_port,
                kind,
            } => {
                self.handle_connect(
                    from_track.as_str(),
                    from_port,
                    to_track.as_str(),
                    to_port,
                    kind,
                )
                .await;
            }
            Action::Disconnect { .. } => {
                self.handle_disconnect(a.clone()).await;
            }
            Action::OpenAudioDevice { .. } => {
                let (done, updated) = self.handle_open_audio_device(a.clone()).await;
                if done {
                    return;
                }
                if let Some(action) = updated {
                    action_to_process = action;
                }
            }
            Action::JackAddAudioInputPort => {
                if Self::box_bool(self.handle_jack_add_audio_input_port(a.clone())).await {
                    return;
                }
            }
            Action::JackRemoveAudioInputPort(_removed_port) => {
                if self
                    .handle_jack_remove_audio_input_port(_removed_port, a.clone())
                    .await
                {
                    return;
                }
            }
            Action::JackAddAudioOutputPort => {
                if Self::box_bool(self.handle_jack_add_audio_output_port(a.clone())).await {
                    return;
                }
            }
            Action::JackRemoveAudioOutputPort(_removed_port) => {
                if self
                    .handle_jack_remove_audio_output_port(_removed_port, a.clone())
                    .await
                {
                    return;
                }
            }
            Action::OpenMidiInputDevice(ref device) => {
                let midi_hub = self.midi_hub.lock();
                if let Err(e) = midi_hub.open_input(device) {
                    self.notify_clients(Err(e)).await;
                    return;
                }
            }
            Action::OpenMidiOutputDevice(ref device) => {
                let midi_hub = self.midi_hub.lock();
                if let Err(e) = midi_hub.open_output(device) {
                    self.notify_clients(Err(e)).await;
                    return;
                }
            }
            Action::RequestSessionDiagnostics => {
                self.handle_request_session_diagnostics().await;
            }
            Action::RequestMidiLearnMappingsReport => {
                self.handle_request_midi_learn_mappings_report().await;
            }
            Action::ClearAllMidiLearnBindings => {
                if Self::box_bool(self.handle_clear_all_midi_learn_bindings(a.clone())).await {
                    return;
                }
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            Action::TrackLv2PluginControls { .. } => {}
            #[cfg(all(unix, not(target_os = "macos")))]
            Action::ClipLv2PluginControls { .. } => {}
            #[cfg(all(unix, not(target_os = "macos")))]
            Action::TrackLv2StateSnapshot { .. } => {}
            #[cfg(all(unix, not(target_os = "macos")))]
            Action::ClipLv2StateSnapshot { .. } => {}
            #[cfg(all(unix, not(target_os = "macos")))]
            Action::TrackLv2Midnam { .. } => {}
            Action::TrackClapNoteNames { .. } => {}
            Action::SessionDiagnosticsReport { .. } => {}
            Action::MidiLearnMappingsReport { .. } => {}
            Action::HWInfo { .. } => {}
            Action::HistoryState { .. } => {}
            Action::Undo => {}
            Action::Redo => {}
            Action::ApplyGroupedActions(_) => {}
            _ => {}
        }

        if let Some(inverse) = inverse_actions {
            if let Some(group) = self.history_group.as_mut() {
                group.forward_actions.push(action_to_process.clone());
                group.inverse_actions.splice(0..0, inverse);
            } else {
                self.history.record(UndoEntry {
                    forward_actions: vec![action_to_process.clone()],
                    inverse_actions: inverse,
                });
            }
        }

        self.notify_clients(Ok(action_to_process)).await;
    }
    pub async fn work(&mut self) {
        while let Some(message) = self.rx.recv().await {
            match message {
                Message::Ready(id) => self.push_ready_worker(id),
                Message::Finished {
                    worker_id,
                    task,
                    output_linear,
                    process_epoch,
                    parameter_updates,
                } => {
                    tracing::debug!(
                        "engine received Finished from worker {} for task {:?} (epoch {} vs {})",
                        worker_id,
                        task,
                        process_epoch,
                        self.track_process_epoch
                    );
                    self.push_ready_worker(worker_id);
                    let task_key = Self::task_key(&task);
                    self.task_processing_started_at.remove(&task_key);
                    if process_epoch != self.track_process_epoch {
                        if let Some(track) = self
                            .state
                            .lock()
                            .tracks
                            .get(&Self::task_track_name(&task))
                            .cloned()
                        {
                            let t = track.lock();
                            t.audio.finished = false;
                            t.audio.processing = false;
                        }
                        continue;
                    }
                    self.cycle_tasks_running
                        .retain(|t| Self::task_key(t) != task_key);
                    self.cycle_tasks_finished.push(task.clone());
                    let track_name = Self::task_track_name(&task);
                    let peak = output_linear.iter().copied().fold(0.0_f32, |a, b| a.max(b));
                    tracing::debug!(
                        "Finished task for '{}' epoch={} output_peak={}",
                        track_name,
                        process_epoch,
                        peak
                    );
                    self.track_meter_linear_by_track
                        .insert(track_name.clone(), output_linear);
                    for action in parameter_updates {
                        self.notify_clients(Ok(action)).await;
                    }
                    self.force_stalled_task_completions();
                    let all_finished = self.send_tasks().await;
                    tracing::debug!(
                        "engine after Finished for {}: all_finished={}",
                        track_name,
                        all_finished
                    );
                    if all_finished {
                        self.on_all_tracks_finished().await;
                    }
                }
                Message::Channel(s) => {
                    self.clients.push(s);
                }

                Message::Request(a) => {
                    self.dispatch_request(a).await;
                }
                Message::OscRequest { action, reply_to } => {
                    self.osc_reply_target = Some(reply_to);
                    self.dispatch_request(action).await;
                    self.osc_reply_target = None;
                }
                Message::OfflineBounceFinished { result } => {
                    if let Ok(Action::TrackOfflineBounce { track_name, .. }) = &result {
                        self.offline_bounce_jobs.remove(track_name);
                    }
                    self.notify_clients(result).await;
                    if self.offline_bounce_jobs.is_empty() {
                        while let Some(next) = self.pending_requests.pop_front() {
                            self.handle_request(next).await;
                        }
                    }
                }
                Message::HWFinished => {
                    if !self.awaiting_hwfinished {
                        tracing::debug!("HWFinished ignored (not awaiting)");
                        continue;
                    }
                    tracing::debug!("HWFinished handling; playing={}", self.playing);
                    self.handling_hwfinished = true;
                    self.awaiting_hwfinished = false;
                    #[cfg(unix)]
                    {
                        if let Some(jack) = &self.jack_runtime {
                            if !self.pending_hw_midi_out_events.is_empty() {
                                let out_events =
                                    std::mem::take(&mut self.pending_hw_midi_out_events);
                                jack.lock().write_events(&out_events);
                            }
                            let mut in_events = vec![];
                            jack.lock().read_events_into(&mut in_events);
                            if !in_events.is_empty() {
                                self.pending_hw_midi_events.extend(in_events);
                            }
                        }
                    }
                    #[cfg(unix)]
                    if self.jack_runtime.is_some() {
                        self.sync_from_jack_transport().await;
                    }
                    while let Some(a) = self.pending_requests.pop_front() {
                        self.handle_request(a).await;
                    }
                    self.apply_mute_solo_policy();
                    self.append_recorded_cycle();
                    self.flush_completed_recordings().await;
                    let hw_in_routes = self.midi_hw_in_routes.clone();
                    let pending_hw_in_by_device = self.pending_hw_midi_events_by_device.clone();
                    let mut reconfigured_tracks = Vec::new();
                    for (track_name, track) in self.state.lock().tracks.iter() {
                        let track_lock = track.lock();
                        if self.jack_runtime_is_some() {
                            if !self.pending_hw_midi_events.is_empty() {
                                track_lock.push_hw_midi_events(&self.pending_hw_midi_events);
                            }
                        } else {
                            for route in hw_in_routes.iter().filter(|r| &r.to_track == track_name) {
                                if let Some(events) = pending_hw_in_by_device.get(&route.device) {
                                    track_lock.push_hw_midi_events_to_port(route.to_port, events);
                                }
                            }
                        }
                        if track_lock.setup() {
                            reconfigured_tracks.push(track_name.clone());
                        }
                    }
                    self.publish_track_meters().await;
                    self.publish_session_runtime_reports().await;
                    self.publish_clap_state_dirty().await;
                    for track_name in reconfigured_tracks {
                        let track = self.state.lock().tracks.get(&track_name).cloned();
                        if let Some(track) = track {
                            let (plugins, connections, connectable_connections) = {
                                let track_lock = track.lock();
                                (
                                    track_lock.plugin_graph_plugins(false),
                                    track_lock.plugin_graph_connections(),
                                    track_lock.connectable_connections(),
                                )
                            };
                            self.notify_clients(Ok(Action::TrackPluginGraph {
                                track_name: track_name.clone(),
                                plugins,
                                connections,
                                connectable_connections,
                            }))
                            .await;
                        }
                    }
                    self.pending_hw_midi_events.clear();
                    self.pending_hw_midi_events_by_device.clear();
                    if self.transport_running {
                        if self.transport_panic_flush_pending {
                            self.transport_panic_flush_pending = false;
                        } else if self.transport_restart_pending {
                            self.transport_restart_pending = false;
                        } else {
                            let next = self
                                .transport_sample
                                .saturating_add(self.current_cycle_samples());
                            let normalized = self.normalize_transport_sample(next);
                            let wrapped = normalized != next;
                            self.transport_sample = normalized;
                            if wrapped {
                                if self.notified_loop_wrap_sample == Some(self.transport_sample) {
                                    self.notified_loop_wrap_sample = None;
                                } else {
                                    self.notify_clients(Ok(Action::TransportPosition(
                                        self.transport_sample,
                                    )))
                                    .await;
                                }
                            }
                        }
                    }
                    if self.session_clip_playback_enabled && self.playing {
                        self.session_transport_sample = self
                            .session_transport_sample
                            .saturating_add(self.current_cycle_samples());
                    }
                    {
                        let echoes = self.apply_modulators(self.active_transport_sample());
                        for action in echoes {
                            self.notify_clients(Ok(action)).await;
                        }
                    }
                    self.invalidate_track_cycle_state();
                    let all_finished = self.send_tasks().await;
                    tracing::debug!(
                        "HWFinished send_tasks finished={} hw_worker={}",
                        all_finished,
                        self.hw_worker.is_some()
                    );
                    if all_finished && self.hw_worker.is_some() {
                        self.request_hw_cycle().await;
                    }
                    #[cfg(unix)]
                    {
                        if self.jack_runtime.is_some() {
                            self.awaiting_hwfinished = true;
                        }
                    }
                    self.handling_hwfinished = false;
                }
                Message::HWMidiEvents(events) => {
                    for hw_event in events {
                        let thru_targets: Vec<String> = self
                            .midi_hw_thru_routes
                            .iter()
                            .filter(|route| route.from_device == hw_event.device)
                            .map(|route| route.to_device.clone())
                            .collect();
                        for device in thru_targets {
                            self.pending_hw_midi_out_events_by_device.push(HwMidiEvent {
                                device,
                                event: hw_event.event.clone(),
                            });
                        }
                        if hw_event.event.data.len() >= 3 {
                            let status = hw_event.event.data[0];
                            if status & 0xF0 == 0xB0 {
                                let channel = status & 0x0F;
                                let cc = hw_event.event.data[1];
                                let value = hw_event.event.data[2];
                                self.handle_incoming_hw_cc(&hw_event.device, channel, cc, value)
                                    .await;
                            }
                            if self.step_recording_enabled && status & 0xF0 == 0x90 {
                                let channel = status & 0x0F;
                                let pitch = hw_event.event.data[1];
                                let velocity = hw_event.event.data[2];
                                if velocity > 0 {
                                    self.notify_clients(Ok(Action::StepRecordMidiNote {
                                        device: hw_event.device.clone(),
                                        channel,
                                        pitch,
                                        velocity,
                                    }))
                                    .await;
                                }
                            }
                        }
                        self.pending_hw_midi_events_by_device
                            .entry(hw_event.device)
                            .or_default()
                            .push(hw_event.event);
                    }
                }
                _ => {}
            }
        }
    }

    pub(crate) fn collect_hw_midi_output_events(&self) -> Vec<MidiEvent> {
        let mut events = vec![];
        for track in self.state.lock().tracks.values() {
            events.extend(
                track
                    .lock()
                    .take_hw_midi_out_events()
                    .into_iter()
                    .map(|evt| evt.event),
            );
        }
        events.sort_by_key(|a| a.frame);
        events
    }

    pub(crate) fn collect_hw_midi_output_events_by_device(&mut self) -> Vec<HwMidiEvent> {
        let mut events = Vec::<HwMidiEvent>::new();
        let routes = self.midi_hw_out_routes.clone();
        let mut events_by_track = HashMap::<String, Vec<crate::track::HwMidiOutEvent>>::new();
        {
            let state = self.state.lock();
            for route in &routes {
                if events_by_track.contains_key(&route.from_track) {
                    continue;
                }
                let Some(track) = state.tracks.get(&route.from_track) else {
                    continue;
                };
                events_by_track.insert(
                    route.from_track.clone(),
                    track.lock().take_hw_midi_out_events(),
                );
            }
        }

        for route in routes {
            let Some(track_events) = events_by_track.get(&route.from_track) else {
                continue;
            };
            for hw_event in track_events
                .iter()
                .filter(|evt| evt.port == route.from_port)
            {
                self.update_active_hw_notes_for_track(
                    &route.from_track,
                    &route.device,
                    &hw_event.event.data,
                );
                events.push(HwMidiEvent {
                    device: route.device.clone(),
                    event: hw_event.event.clone(),
                });
            }
        }
        events.sort_by(|a, b| {
            a.event
                .frame
                .cmp(&b.event.frame)
                .then_with(|| a.device.cmp(&b.device))
        });
        events
    }
}
