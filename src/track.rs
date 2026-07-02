use super::{audio::track::AudioTrack, midi::track::MIDITrack};
#[cfg(target_os = "macos")]
use crate::clap::ClapMidiOutputEvent;
use crate::connectable::{ConnectableConnection, ConnectableRef};
use crate::message::{PluginGraphConnection, PluginGraphNode, PluginGraphPlugin, PluginKind};
use crate::mutex::UnsafeMutex;
#[cfg(unix)]
use crate::rubberband::LivePitchShifter;

use crate::{
    audio::io::AudioIO,
    midi::io::{MIDIIO, MidiEvent},
};
use crate::{kind::Kind, routing};
use midly::{MetaMessage, Smf, Timing, TrackEventKind, live::LiveEvent};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, atomic::Ordering},
};

type MidiClipEvents = Arc<Vec<(usize, Vec<u8>)>>;

struct TrackIoCounts {
    audio_ins: usize,
    audio_outs: usize,
    midi_ins: usize,
    midi_outs: usize,
}

pub struct ClapInstance {
    pub id: usize,
    pub processor: crate::clap_proc::SharedClapProcessor,
}

impl std::fmt::Debug for ClapInstance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClapInstance")
            .field("id", &self.id)
            .field("processor", &"<SharedClapProcessor>")
            .finish()
    }
}

impl ClapInstance {
    fn new(id: usize, processor: crate::clap_proc::SharedClapProcessor) -> Self {
        Self { id, processor }
    }
}

pub struct Vst3Instance {
    pub id: usize,
    pub processor: crate::vst3_proc::SharedVst3Processor,
}

impl std::fmt::Debug for Vst3Instance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vst3Instance")
            .field("id", &self.id)
            .field("processor", &"<SharedVst3Processor>")
            .finish()
    }
}

impl Vst3Instance {
    fn new(id: usize, processor: crate::vst3_proc::SharedVst3Processor) -> Self {
        Self { id, processor }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
pub struct Lv2Instance {
    pub id: usize,
    pub processor: crate::lv2_proc::SharedLv2Processor,
}

#[cfg(all(unix, not(target_os = "macos")))]
impl std::fmt::Debug for Lv2Instance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lv2Instance")
            .field("id", &self.id)
            .field("processor", &"<SharedLv2Processor>")
            .finish()
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
impl Lv2Instance {
    fn new(id: usize, processor: crate::lv2_proc::SharedLv2Processor) -> Self {
        Self { id, processor }
    }
}

#[derive(Debug, Clone)]
struct AudioClipBuffer {
    channels: usize,
    samples: Vec<f32>,
}

#[cfg(unix)]
#[derive(Debug)]
pub(crate) struct ClipPitchShifter {
    shifter: LivePitchShifter,
}

#[derive(Debug)]
struct ClipPluginRuntime {
    input_sources: Vec<Arc<AudioIO>>,
    outputs: Vec<Arc<AudioIO>>,
    clap_plugins: Vec<ClapInstance>,
    vst3_plugins: Vec<Vst3Instance>,
    #[cfg(all(unix, not(target_os = "macos")))]
    lv2_plugins: Vec<Lv2Instance>,
    plugin_midi_connections: Vec<PluginGraphConnection>,
}

#[derive(Clone, Copy)]
struct ClipRuntimeProcessContext {}

impl ClipPluginRuntime {
    fn setup_ports(&self) {
        for source in &self.input_sources {
            source.setup();
        }
        for output in &self.outputs {
            output.setup();
        }
    }

    fn connect_audio(
        &mut self,
        from_node: PluginGraphNode,
        from_port: usize,
        to_node: PluginGraphNode,
        to_port: usize,
    ) -> Result<(), String> {
        let source = self.source_io(&from_node, from_port)?;
        let target = self.target_io(&to_node, to_port)?;
        Track::connect_directed_audio(&source, &target);
        Ok(())
    }

    fn connect_midi(
        &mut self,
        from_node: PluginGraphNode,
        from_port: usize,
        to_node: PluginGraphNode,
        to_port: usize,
    ) {
        self.plugin_midi_connections.push(PluginGraphConnection {
            from_node,
            from_port,
            to_node,
            to_port,
            kind: Kind::MIDI,
        });
    }

    fn source_io(&self, node: &PluginGraphNode, port: usize) -> Result<Arc<AudioIO>, String> {
        match node {
            PluginGraphNode::TrackInput => self
                .input_sources
                .get(port)
                .cloned()
                .ok_or_else(|| format!("Invalid clip input port: {port}")),

            PluginGraphNode::ClapPluginInstance(id) => self
                .clap_plugins
                .iter()
                .find(|instance| instance.id == *id)
                .and_then(|instance| instance.processor.lock().audio_outputs().get(port).cloned())
                .ok_or_else(|| format!("Invalid clip CLAP output port: {id}:{port}")),
            PluginGraphNode::Vst3PluginInstance(id) => self
                .vst3_plugins
                .iter()
                .find(|instance| instance.id == *id)
                .and_then(|instance| instance.processor.lock().audio_outputs().get(port).cloned())
                .ok_or_else(|| format!("Invalid clip VST3 output port: {id}:{port}")),
            #[cfg(all(unix, not(target_os = "macos")))]
            PluginGraphNode::Lv2PluginInstance(id) => self
                .lv2_plugins
                .iter()
                .find(|instance| instance.id == *id)
                .and_then(|instance| instance.processor.lock().audio_outputs().get(port).cloned())
                .ok_or_else(|| format!("Invalid clip LV2 output port: {id}:{port}")),
            PluginGraphNode::TrackOutput => Err("Clip output cannot be audio source".to_string()),
        }
    }

    fn target_io(&self, node: &PluginGraphNode, port: usize) -> Result<Arc<AudioIO>, String> {
        match node {
            PluginGraphNode::TrackOutput => self
                .outputs
                .get(port)
                .cloned()
                .ok_or_else(|| format!("Invalid clip output port: {port}")),

            PluginGraphNode::ClapPluginInstance(id) => self
                .clap_plugins
                .iter()
                .find(|instance| instance.id == *id)
                .and_then(|instance| instance.processor.lock().audio_inputs().get(port).cloned())
                .ok_or_else(|| format!("Invalid clip CLAP input port: {id}:{port}")),
            PluginGraphNode::Vst3PluginInstance(id) => self
                .vst3_plugins
                .iter()
                .find(|instance| instance.id == *id)
                .and_then(|instance| instance.processor.lock().audio_inputs().get(port).cloned())
                .ok_or_else(|| format!("Invalid clip VST3 input port: {id}:{port}")),
            #[cfg(all(unix, not(target_os = "macos")))]
            PluginGraphNode::Lv2PluginInstance(id) => self
                .lv2_plugins
                .iter()
                .find(|instance| instance.id == *id)
                .and_then(|instance| instance.processor.lock().audio_inputs().get(port).cloned())
                .ok_or_else(|| format!("Invalid clip LV2 input port: {id}:{port}")),
            PluginGraphNode::TrackInput => Err("Clip input cannot be audio target".to_string()),
        }
    }

    fn process(
        &mut self,
        input_blocks: &[Vec<f32>],
        request_len: usize,
        _context: ClipRuntimeProcessContext,
    ) -> Vec<Vec<f32>> {
        self.setup_ports();
        for (source, samples) in self.input_sources.iter().zip(input_blocks.iter()) {
            let buffer = source.buffer.lock();
            let len = buffer.len().min(request_len);
            buffer.fill(0.0);
            buffer[..len].copy_from_slice(&samples[..len]);
            *source.finished.lock() = true;
        }
        for source in self.input_sources.iter().skip(input_blocks.len()) {
            source.buffer.lock().fill(0.0);
            *source.finished.lock() = true;
        }

        self.process_plugins_in_graph_order(request_len, &[], &mut HashMap::new());

        let mut outputs = Vec::with_capacity(self.outputs.len());
        for output in &self.outputs {
            if output.ready() {
                output.process();
            } else {
                output.buffer.lock().fill(0.0);
                *output.finished.lock() = true;
            }
            let buffer = output.buffer.lock();
            outputs.push(
                buffer
                    .iter()
                    .take(request_len)
                    .copied()
                    .collect::<Vec<f32>>(),
            );
        }
        outputs
    }

    fn process_plugins_in_graph_order(
        &self,
        frames: usize,
        _track_input_events: &[Vec<MidiEvent>],
        midi_node_events: &mut HashMap<(PluginGraphNode, usize), Vec<MidiEvent>>,
    ) {
        let mut clap_processed = vec![false; self.clap_plugins.len()];
        let mut vst3_processed = vec![false; self.vst3_plugins.len()];
        #[cfg(all(unix, not(target_os = "macos")))]
        let mut lv2_processed = vec![false; self.lv2_plugins.len()];
        #[cfg(all(unix, not(target_os = "macos")))]
        let mut remaining = clap_processed.len() + vst3_processed.len() + lv2_processed.len();
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        let mut remaining = clap_processed.len() + vst3_processed.len();

        while remaining > 0 {
            let mut progressed = false;

            for (idx, done) in clap_processed.iter_mut().enumerate() {
                if *done {
                    continue;
                }
                let processor = self.clap_plugins[idx].processor.lock();
                let audio_ready = processor.audio_inputs().iter().all(|input| input.ready());
                let midi_ready = Self::plugin_midi_inputs_ready(processor.midi_input_ports());
                let node = PluginGraphNode::ClapPluginInstance(self.clap_plugins[idx].id);
                if !audio_ready || !midi_ready {
                    continue;
                }
                for input in processor.audio_inputs() {
                    input.process();
                }
                let _midi_inputs = Self::prepare_plugin_midi_inputs(processor.midi_input_ports());
                let outputs = processor.process_with_midi(
                    frames,
                    &[],
                    crate::plugins::types::ClapTransportInfo::default(),
                );
                for evt in outputs {
                    midi_node_events
                        .entry((node.clone(), evt.port))
                        .or_default()
                        .push(evt.event);
                }
                *done = true;
                remaining = remaining.saturating_sub(1);
                progressed = true;
            }

            for (idx, done) in vst3_processed.iter_mut().enumerate() {
                if *done {
                    continue;
                }
                let processor = self.vst3_plugins[idx].processor.lock();
                let audio_ready = processor.audio_inputs().iter().all(|input| input.ready());
                let midi_ready = Self::plugin_midi_inputs_ready(processor.midi_input_ports());
                let node = PluginGraphNode::Vst3PluginInstance(self.vst3_plugins[idx].id);
                if !audio_ready || !midi_ready {
                    continue;
                }
                for input in processor.audio_inputs() {
                    input.process();
                }
                let midi_inputs = Self::prepare_plugin_midi_inputs(processor.midi_input_ports());
                let vst3_input = midi_inputs.first().cloned().unwrap_or_default();
                let outputs = processor.process_with_midi(frames, &vst3_input);
                if !outputs.is_empty() {
                    midi_node_events.insert((node.clone(), 0), outputs);
                }
                *done = true;
                remaining = remaining.saturating_sub(1);
                progressed = true;
            }

            #[cfg(all(unix, not(target_os = "macos")))]
            for (idx, done) in lv2_processed.iter_mut().enumerate() {
                if *done {
                    continue;
                }
                let processor = self.lv2_plugins[idx].processor.lock();
                let audio_ready = processor.audio_inputs().iter().all(|input| input.ready());
                let midi_ready = Self::plugin_midi_inputs_ready(processor.midi_input_ports());
                let node = PluginGraphNode::Lv2PluginInstance(self.lv2_plugins[idx].id);
                if !audio_ready || !midi_ready {
                    continue;
                }
                for input in processor.audio_inputs() {
                    input.process();
                }
                let midi_inputs = Self::prepare_plugin_midi_inputs(processor.midi_input_ports());
                let lv2_input = midi_inputs.first().cloned().unwrap_or_default();
                let outputs = processor.process_with_midi(frames, &lv2_input);
                if !outputs.is_empty() {
                    midi_node_events.insert((node.clone(), 0), outputs);
                }
                *done = true;
                remaining = remaining.saturating_sub(1);
                progressed = true;
            }

            if !progressed {
                break;
            }
        }
    }

    fn plugin_midi_inputs_ready(ports: &[Arc<UnsafeMutex<Box<MIDIIO>>>]) -> bool {
        ports.iter().all(|port| port.lock().ready())
    }

    fn prepare_plugin_midi_inputs(ports: &[Arc<UnsafeMutex<Box<MIDIIO>>>]) -> Vec<Vec<MidiEvent>> {
        ports
            .iter()
            .map(|port| {
                let lock = port.lock();
                lock.process();
                lock.buffer.clone()
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct HwMidiOutEvent {
    pub port: usize,
    pub event: MidiEvent,
}

#[derive(Debug)]
pub struct Track {
    pub name: String,
    pub level: f32,
    pub balance: f32,
    pub armed: bool,
    pub muted: bool,
    pub phase_inverted: bool,
    pub soloed: bool,
    pub is_master: bool,
    pub input_monitor: Vec<bool>,
    pub disk_monitor: Vec<bool>,
    pub midi_input_monitor: Vec<bool>,
    pub midi_disk_monitor: Vec<bool>,
    pub color: Option<crate::message::TrackColor>,
    pub midi_learn_volume: Option<crate::message::MidiLearnBinding>,
    pub midi_learn_balance: Option<crate::message::MidiLearnBinding>,
    pub midi_learn_mute: Option<crate::message::MidiLearnBinding>,
    pub midi_learn_solo: Option<crate::message::MidiLearnBinding>,
    pub midi_learn_arm: Option<crate::message::MidiLearnBinding>,
    pub midi_learn_input_monitor: Option<crate::message::MidiLearnBinding>,
    pub midi_learn_disk_monitor: Option<crate::message::MidiLearnBinding>,
    pub is_folder: bool,
    pub folder_open: bool,
    pub parent_track: Option<String>,
    pub child_tracks: Vec<Arc<UnsafeMutex<Box<Track>>>>,
    pub frozen: bool,
    pub midi_lane_channels: Vec<Option<u8>>,
    primary_audio_ins: usize,
    primary_audio_outs: usize,
    pub audio: AudioTrack,
    pub midi: MIDITrack,
    pub clap_plugins: Vec<ClapInstance>,
    pub vst3_plugins: Vec<Vst3Instance>,
    #[cfg(all(unix, not(target_os = "macos")))]
    pub lv2_plugins: Vec<Lv2Instance>,
    pub plugin_midi_connections: Vec<PluginGraphConnection>,

    pub echoed_parameter_updates: UnsafeMutex<Vec<crate::message::Action>>,
    pub pending_hw_midi_out_events: Vec<HwMidiOutEvent>,
    pub pending_modulator_midi_events: Vec<MidiEvent>,
    pub pending_automation_midi_events: Vec<MidiEvent>,
    pub next_clap_instance_id: usize,
    pub next_vst3_instance_id: usize,
    #[cfg(all(unix, not(target_os = "macos")))]
    pub next_lv2_instance_id: usize,
    pub next_plugin_instance_id: usize,
    pub sample_rate: f64,
    process_block_size: usize,
    force_realtime_domain: bool,
    shared_realtime_mixed: bool,
    last_render_block_silent: bool,
    pub output_enabled: bool,
    pub process_epoch: usize,
    pub transport_sample: usize,
    pub loop_enabled: bool,
    pub loop_range_samples: Option<(usize, usize)>,
    pub tempo_bpm: f64,
    pub tsig_num: u16,
    pub tsig_denom: u16,
    pub clip_playback_enabled: bool,
    pub metronome_enabled: bool,
    output_meter_linear_cache: Vec<f32>,
    meter_peak_hold_linear: Vec<f32>,
    pub record_tap_outs: Vec<Vec<f32>>,
    pub record_tap_midi_in: Vec<MidiEvent>,
    pub session_base_dir: Option<PathBuf>,
    record_tap_enabled: bool,
    audio_clip_cache: HashMap<String, Arc<AudioClipBuffer>>,
    clip_plugin_tracks: HashMap<String, ClipPluginRuntime>,
    #[cfg(unix)]
    pub(crate) clip_pitch_shifters: HashMap<String, ClipPitchShifter>,
    midi_clip_cache: HashMap<String, MidiClipEvents>,
    internal_output_routes_cache: Vec<Vec<Arc<AudioIO>>>,
    audio_route_cache_dirty: bool,
    metronome_source: Option<Arc<AudioIO>>,
    midi_input_to_out_routes_cache: Vec<Vec<usize>>,
    midi_out_external_targets_cache: Vec<Vec<Arc<UnsafeMutex<Box<MIDIIO>>>>>,
    midi_route_cache_dirty: bool,

    folder_input_midi_events: Vec<Vec<MidiEvent>>,
    folder_plugin_midi_node_events: HashMap<(PluginGraphNode, usize), Vec<MidiEvent>>,
    folder_processed_midi_plugins: HashSet<PluginGraphNode>,
    folder_clip_playback_active: bool,
    folder_record_tap_input_snapshots: Vec<Vec<f32>>,
}

impl Track {
    const METRONOME_DEFAULT_LEVEL_DB: f32 = -10.0;

    fn new_raw(
        name: String,
        io: TrackIoCounts,
        buffer_size: usize,
        sample_rate: f64,
        is_folder: bool,
    ) -> Self {
        Self {
            name,
            level: 0.0,
            balance: 0.0,
            armed: false,
            muted: false,
            phase_inverted: false,
            soloed: false,
            is_master: false,
            input_monitor: vec![false; io.audio_ins],
            disk_monitor: vec![true; io.audio_ins],
            midi_input_monitor: vec![false; io.midi_ins],
            midi_disk_monitor: vec![true; io.midi_ins],
            color: None,
            midi_learn_volume: None,
            midi_learn_balance: None,
            midi_learn_mute: None,
            midi_learn_solo: None,
            midi_learn_arm: None,
            midi_learn_input_monitor: None,
            midi_learn_disk_monitor: None,
            is_folder,
            folder_open: true,
            parent_track: None,
            child_tracks: Vec::new(),
            frozen: false,
            midi_lane_channels: vec![None; io.midi_ins],
            primary_audio_ins: io.audio_ins,
            primary_audio_outs: io.audio_outs,
            audio: AudioTrack::new(io.audio_ins, io.audio_outs, buffer_size),
            midi: MIDITrack::new(io.midi_ins, io.midi_outs),
            clap_plugins: Vec::new(),
            vst3_plugins: Vec::new(),
            #[cfg(all(unix, not(target_os = "macos")))]
            lv2_plugins: Vec::new(),
            plugin_midi_connections: Vec::new(),
            echoed_parameter_updates: UnsafeMutex::new(Vec::new()),
            pending_hw_midi_out_events: vec![],
            pending_modulator_midi_events: vec![],
            pending_automation_midi_events: vec![],
            next_clap_instance_id: 0,
            next_vst3_instance_id: 0,
            #[cfg(all(unix, not(target_os = "macos")))]
            next_lv2_instance_id: 0,
            next_plugin_instance_id: 0,
            sample_rate,
            process_block_size: buffer_size.max(1),
            force_realtime_domain: false,
            shared_realtime_mixed: false,
            last_render_block_silent: true,
            output_enabled: true,
            process_epoch: 0,
            transport_sample: 0,
            loop_enabled: false,
            loop_range_samples: None,
            tempo_bpm: 120.0,
            tsig_num: 4,
            tsig_denom: 4,
            clip_playback_enabled: true,
            metronome_enabled: false,
            output_meter_linear_cache: vec![0.0; io.audio_outs],
            meter_peak_hold_linear: vec![0.0; io.audio_outs],
            record_tap_outs: vec![vec![0.0; buffer_size]; io.audio_outs],
            record_tap_midi_in: vec![],
            session_base_dir: None,
            record_tap_enabled: false,
            audio_clip_cache: HashMap::new(),
            clip_plugin_tracks: HashMap::new(),
            #[cfg(unix)]
            clip_pitch_shifters: HashMap::new(),
            midi_clip_cache: HashMap::new(),
            internal_output_routes_cache: Vec::new(),
            audio_route_cache_dirty: true,
            metronome_source: None,
            midi_input_to_out_routes_cache: Vec::new(),
            midi_out_external_targets_cache: Vec::new(),
            midi_route_cache_dirty: true,

            folder_input_midi_events: Vec::new(),
            folder_plugin_midi_node_events: HashMap::new(),
            folder_processed_midi_plugins: HashSet::new(),
            folder_clip_playback_active: false,
            folder_record_tap_input_snapshots: Vec::new(),
        }
    }

    pub fn new(
        name: String,
        audio_ins: usize,
        audio_outs: usize,
        midi_ins: usize,
        midi_outs: usize,
        buffer_size: usize,
        sample_rate: f64,
    ) -> Self {
        Self::new_raw(
            name,
            TrackIoCounts {
                audio_ins,
                audio_outs,
                midi_ins,
                midi_outs,
            },
            buffer_size,
            sample_rate,
            false,
        )
        .with_default_passthrough()
    }

    pub fn new_folder(
        name: String,
        audio_ins: usize,
        audio_outs: usize,
        midi_ins: usize,
        midi_outs: usize,
        buffer_size: usize,
        sample_rate: f64,
    ) -> Self {
        Self::new_raw(
            name,
            TrackIoCounts {
                audio_ins,
                audio_outs,
                midi_ins,
                midi_outs,
            },
            buffer_size,
            sample_rate,
            true,
        )
        .with_default_passthrough()
    }

    fn alloc_plugin_instance_id(&mut self) -> usize {
        let id = self.next_plugin_instance_id;
        self.next_plugin_instance_id = self.next_plugin_instance_id.saturating_add(1);
        id
    }

    pub fn setup(&mut self) -> bool {
        self.audio.setup();
        let mut reconfigured = false;
        for runtime in self.clip_plugin_tracks.values() {
            for instance in &runtime.clap_plugins {
                instance.processor.lock().run_host_callbacks_main_thread();
                match instance.processor.lock().reconfigure_ports_if_needed() {
                    Ok(true) => reconfigured = true,
                    Err(e) => {
                        tracing::warn!(
                            "CLAP port reconfiguration failed for '{}': {}",
                            instance.processor.lock().name(),
                            e
                        );
                    }
                    Ok(false) => {}
                }
            }
        }
        reconfigured
    }

    pub fn connect_directed_audio(from: &Arc<AudioIO>, to: &Arc<AudioIO>) {
        let new_len = {
            let conns = to.connections.lock();
            if !conns.iter().any(|conn| Arc::ptr_eq(conn, from)) {
                conns.push(from.clone());
            }
            conns.len()
        };
        to.connection_count.store(new_len, Ordering::Relaxed);
    }

    pub fn invalidate_audio_route_cache(&mut self) {
        self.audio_route_cache_dirty = true;
    }

    pub fn primary_audio_ins(&self) -> usize {
        self.primary_audio_ins.min(self.audio.ins.len())
    }

    pub fn primary_audio_outs(&self) -> usize {
        self.primary_audio_outs.min(self.audio.outs.len())
    }

    fn ensure_audio_route_cache(&mut self) {
        if !self.audio_route_cache_dirty
            && self.internal_output_routes_cache.len() == self.audio.outs.len()
        {
            return;
        }
        let internal_sources = self.internal_audio_sources();
        let mut routes = Vec::with_capacity(self.audio.outs.len());
        for audio_out in &self.audio.outs {
            let connections = audio_out.connections.lock();
            let mut route_sources = Vec::new();
            for source in connections.iter() {
                if internal_sources
                    .iter()
                    .any(|candidate| Arc::ptr_eq(candidate, source))
                {
                    route_sources.push(source.clone());
                }
            }
            routes.push(route_sources);
        }
        self.internal_output_routes_cache = routes;
        self.audio_route_cache_dirty = false;
    }

    pub fn invalidate_midi_route_cache(&mut self) {
        self.midi_route_cache_dirty = true;
    }

    fn ensure_midi_route_cache(&mut self) {
        if !self.midi_route_cache_dirty
            && self.midi_input_to_out_routes_cache.len() == self.midi.ins.len()
            && self.midi_out_external_targets_cache.len() == self.midi.outs.len()
        {
            return;
        }

        let mut input_to_out = vec![Vec::<usize>::new(); self.midi.ins.len()];
        let mut out_external_targets =
            vec![Vec::<Arc<UnsafeMutex<Box<MIDIIO>>>>::new(); self.midi.outs.len()];

        for (out_idx, out) in self.midi.outs.iter().enumerate() {
            let out_lock = out.lock();
            for source in &out_lock.sources {
                if let Some(input_idx) = self
                    .midi
                    .ins
                    .iter()
                    .position(|input| Arc::ptr_eq(input, source))
                {
                    input_to_out[input_idx].push(out_idx);
                }
            }
            for target in &out_lock.connections {
                out_external_targets[out_idx].push(target.clone());
            }
        }

        self.midi_input_to_out_routes_cache = input_to_out;
        self.midi_out_external_targets_cache = out_external_targets;
        self.midi_route_cache_dirty = false;
    }

    #[inline(always)]
    fn copy_unity_with_zero_tail(dst: &mut [f32], src: &[f32]) {
        let len = dst.len().min(src.len());
        unsafe {
            std::ptr::copy_nonoverlapping(src.as_ptr(), dst.as_mut_ptr(), len);
        }
        if len < dst.len() {
            dst[len..].fill(0.0);
        }
    }

    #[inline(always)]
    fn copy_scaled_with_zero_tail(dst: &mut [f32], src: &[f32], gain: f32) {
        let len = dst.len().min(src.len());
        crate::simd::copy_scaled_inplace(&mut dst[..len], &src[..len], gain);
        if len < dst.len() {
            dst[len..].fill(0.0);
        }
    }

    #[inline(always)]
    fn add_unity(dst: &mut [f32], src: &[f32]) {
        crate::simd::add_inplace(dst, src);
    }

    #[inline(always)]
    fn add_scaled(dst: &mut [f32], src: &[f32], gain: f32) {
        crate::simd::add_scaled_inplace(dst, src, gain);
    }

    fn ensure_metronome_source(&mut self, frames: usize) -> Option<Arc<AudioIO>> {
        if self.name != "metronome" || self.audio.outs.is_empty() {
            return None;
        }
        let needed = frames.max(1);
        let needs_new = self
            .metronome_source
            .as_ref()
            .is_none_or(|src| src.buffer.lock().len() < needed);
        if needs_new {
            self.metronome_source = Some(Arc::new(AudioIO::new(needed)));
            self.invalidate_audio_route_cache();
        }
        let src = self.metronome_source.clone()?;
        let mut route_changed = false;
        for out in &self.audio.outs {
            if !out
                .connections
                .lock()
                .iter()
                .any(|conn| Arc::ptr_eq(conn, &src))
            {
                Self::connect_directed_audio(&src, out);
                route_changed = true;
            }
        }
        if route_changed {
            self.invalidate_audio_route_cache();
        }
        Some(src)
    }

    fn synthesize_metronome_into(&mut self, dst: &Arc<AudioIO>, frames: usize) {
        let buf = dst.buffer.lock();
        buf.fill(0.0);
        if !self.metronome_enabled || !self.clip_playback_enabled || frames == 0 {
            return;
        }
        let metronome_gain = 10.0_f32.powf((-Self::METRONOME_DEFAULT_LEVEL_DB) / 20.0);
        let sample_rate = self.sample_rate.max(1.0);
        let denom = self.tsig_denom.max(1) as f64;
        let beats_per_bar = self.tsig_num.max(1) as u64;
        let samples_per_beat = ((sample_rate * 60.0) / self.tempo_bpm.max(1.0)) * (4.0 / denom);
        if !samples_per_beat.is_finite() || samples_per_beat <= 1.0 {
            return;
        }
        let segments = self.cycle_segments(frames);
        for (seg_start, seg_end, frame_offset) in segments {
            if seg_end <= seg_start {
                continue;
            }
            let mut beat_idx = ((seg_start as f64) / samples_per_beat).ceil() as u64;
            loop {
                let beat_sample = (beat_idx as f64 * samples_per_beat).round() as usize;
                if beat_sample >= seg_end {
                    break;
                }
                if beat_sample >= seg_start {
                    let hit_frame = frame_offset + (beat_sample - seg_start);
                    if hit_frame < frames {
                        let accented = beat_idx.is_multiple_of(beats_per_bar);
                        let freq = if accented { 1_760.0_f32 } else { 1_320.0_f32 };
                        let amp = if accented { 0.30_f32 } else { 0.22_f32 } * metronome_gain;
                        let click_len = ((sample_rate as usize) / 50).max(64);
                        let phase_step = core::f32::consts::TAU * (freq / sample_rate as f32);
                        let end = (hit_frame + click_len).min(frames).min(buf.len());
                        for (n, buf_n) in buf
                            .iter_mut()
                            .enumerate()
                            .skip(hit_frame)
                            .take(end - hit_frame)
                        {
                            let t = (n - hit_frame) as f32;
                            let env = (-t / (click_len as f32 * 0.28)).exp();
                            let s = (t * phase_step).sin() * amp * env;
                            *buf_n = (*buf_n + s).clamp(-1.0, 1.0);
                        }
                    }
                }
                beat_idx = beat_idx.saturating_add(1);
            }
        }
    }

    fn process_render_block(&mut self) -> usize {
        let live_mode = self.is_realtime_domain();
        let t0 = std::time::Instant::now();
        self.process_folder_input();
        let t5 = std::time::Instant::now();
        let frames = self.compute_process_frames();

        {
            let track_name = self.name.clone();
            let track_input_events = self.folder_input_midi_events.clone();
            let can_skip_plugins = !live_mode
                && self.last_render_block_silent
                && track_input_events.is_empty()
                && self.audio.ins.iter().all(|audio_in| {
                    let buf = audio_in.buffer.lock();
                    buf.iter().all(|&s| s == 0.0)
                });
            if can_skip_plugins {
                for instance in &self.clap_plugins {
                    for output in instance.processor.lock().audio_outputs() {
                        output.buffer.lock().fill(0.0);
                    }
                }
                for instance in &self.vst3_plugins {
                    for output in instance.processor.lock().audio_outputs() {
                        output.buffer.lock().fill(0.0);
                    }
                }
                #[cfg(all(unix, not(target_os = "macos")))]
                for instance in &self.lv2_plugins {
                    for output in instance.processor.lock().audio_outputs() {
                        output.buffer.lock().fill(0.0);
                    }
                }
                self.echoed_parameter_updates.lock().clear();
                self.folder_plugin_midi_node_events.clear();
                self.folder_processed_midi_plugins.clear();
            } else {
                self.process_track_plugins_in_graph_order(frames);
            }
            let t6 = std::time::Instant::now();
            let t9 = std::time::Instant::now();
            let total = t9.duration_since(t0).as_secs_f64() * 1000.0;
            if total > 20.0 {
                let _clap_count = self.clap_plugins.len();
                let _vst3_count = self.vst3_plugins.len();
                #[cfg(all(unix, not(target_os = "macos")))]
                let _lv2_count = self.lv2_plugins.len();
                #[cfg(not(all(unix, not(target_os = "macos"))))]
                let _lv2_count = 0;
                tracing::warn!(
                    "Track '{}' process breakdown: total={:.1}ms clip_mix={:.1}ms plugins={:.1}ms midi_route={:.1}ms",
                    track_name,
                    total,
                    t5.duration_since(t0).as_secs_f64() * 1000.0,
                    t6.duration_since(t5).as_secs_f64() * 1000.0,
                    t9.duration_since(t6).as_secs_f64() * 1000.0,
                );
            }
        }

        self.process_folder_output();
        frames
    }

    pub fn process(&mut self) {
        let _ = self.process_render_block();
    }

    pub fn process_folder_input(&mut self) {
        for audio_in in &self.audio.ins {
            audio_in.process();
        }
        let frames = self
            .audio
            .ins
            .first()
            .map(|audio_in| audio_in.buffer.lock().len())
            .or_else(|| {
                self.audio
                    .outs
                    .first()
                    .map(|audio_out| audio_out.buffer.lock().len())
            })
            .unwrap_or(self.process_block_size);
        if let Some(source) = self.ensure_metronome_source(frames) {
            self.synthesize_metronome_into(&source, frames);
        }
        let audio_disk_active = self.disk_monitor.iter().any(|&m| m);
        let midi_disk_active = self.midi_disk_monitor.iter().any(|&m| m);
        self.folder_clip_playback_active =
            (audio_disk_active || midi_disk_active) && self.clip_playback_enabled;
        tracing::debug!(
            "process_folder_input for '{}' active={} disk={:?} clip_enabled={}",
            self.name,
            self.folder_clip_playback_active,
            self.disk_monitor,
            self.clip_playback_enabled
        );
        self.folder_record_tap_input_snapshots = if self.armed && self.record_tap_enabled {
            self.audio
                .ins
                .iter()
                .map(|audio_in| audio_in.buffer.lock().to_vec())
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        let mut track_input_midi_events = self.collect_track_input_midi_events();
        if self.folder_clip_playback_active {
            self.mix_clip_midi_into_inputs(&mut track_input_midi_events, frames);
            for (lane, input) in self.midi.ins.iter().enumerate() {
                let lock = input.lock();
                lock.buffer.clear();
                if let Some(events) = track_input_midi_events.get(lane) {
                    lock.buffer.extend_from_slice(events);
                }
                lock.buffer.sort_by_key(|event| event.frame);
                lock.mark_finished();
            }
            for (lane, audio_in) in self.audio.ins.iter().enumerate() {
                if !self.input_monitor.get(lane).copied().unwrap_or(false) {
                    audio_in.buffer.lock().fill(0.0);
                }
            }
            let mix_start = std::time::Instant::now();
            self.mix_clip_audio_into_inputs();
            let mix_elapsed = mix_start.elapsed().as_secs_f64() * 1000.0;
            if mix_elapsed > 1.0 {
                tracing::warn!(
                    "mix_clip_audio_into_inputs for '{}' took {:.2}ms",
                    self.name,
                    mix_elapsed
                );
            }
        }

        self.folder_input_midi_events = track_input_midi_events.clone();

        // Folder children receive the same input MIDI events as the folder.
        if !self.child_tracks.is_empty() {
            for child in &self.child_tracks {
                let child = child.lock();
                for (i, events) in track_input_midi_events.iter().enumerate() {
                    if let Some(child_in) = child.midi.ins.get(i) {
                        child_in.lock().buffer.extend_from_slice(events);
                    }
                }
            }
        }

        self.folder_plugin_midi_node_events.clear();
        self.folder_processed_midi_plugins.clear();
    }

    fn compute_process_frames(&self) -> usize {
        self.audio
            .ins
            .first()
            .map(|audio_in| audio_in.buffer.lock().len())
            .or_else(|| {
                self.audio
                    .outs
                    .first()
                    .map(|audio_out| audio_out.buffer.lock().len())
            })
            .unwrap_or(self.process_block_size)
    }

    pub fn process_plugin(&mut self, kind: PluginKind, index: usize) {
        let frames = self.compute_process_frames();
        let track_input_events = self.folder_input_midi_events.clone();

        match kind {
            PluginKind::Clap => {
                if index >= self.clap_plugins.len() {
                    return;
                }
                let processor = self.clap_plugins[index].processor.lock();
                let ready = processor.audio_inputs().iter().all(|input| input.ready());
                let node = PluginGraphNode::ClapPluginInstance(self.clap_plugins[index].id);
                if !ready || !self.plugin_midi_ready(&node, &self.folder_processed_midi_plugins) {
                    return;
                }
                for input in processor.audio_inputs() {
                    input.process();
                }
                self.plugin_midi_input_events(
                    &node,
                    processor.midi_input_count(),
                    &track_input_events,
                    &self.folder_plugin_midi_node_events,
                );
                let outputs = processor.process_with_midi(
                    frames,
                    &[],
                    crate::plugins::types::ClapTransportInfo {
                        transport_sample: self.transport_sample,
                        playing: (self.disk_monitor.iter().any(|&m| m)
                            || self.midi_disk_monitor.iter().any(|&m| m))
                            && self.clip_playback_enabled,
                        loop_enabled: self.loop_enabled,
                        loop_range_samples: self.loop_range_samples,
                        bpm: self.tempo_bpm,
                        tsig_num: self.tsig_num,
                        tsig_denom: self.tsig_denom,
                    },
                );
                let track_name = self.name.clone();
                for ev in processor.drain_echoed_parameters() {
                    self.echoed_parameter_updates.lock().push(
                        crate::message::Action::TrackSetClapParameter {
                            track_name: track_name.clone(),
                            instance_id: self.clap_plugins[index].id,
                            param_id: ev.param_index,
                            value: ev.value as f64,
                        },
                    );
                }
                for evt in outputs {
                    self.folder_plugin_midi_node_events
                        .entry((node.clone(), evt.port))
                        .or_default()
                        .push(evt.event);
                }
                self.folder_processed_midi_plugins.insert(node);
            }
            PluginKind::Vst3 => {
                if index >= self.vst3_plugins.len() {
                    return;
                }
                let processor = self.vst3_plugins[index].processor.lock();
                let ready = processor.audio_inputs().iter().all(|input| input.ready());
                let node = PluginGraphNode::Vst3PluginInstance(self.vst3_plugins[index].id);
                if !ready || !self.plugin_midi_ready(&node, &self.folder_processed_midi_plugins) {
                    return;
                }
                for input in processor.audio_inputs() {
                    input.process();
                }
                let midi_inputs = self.plugin_midi_input_events(
                    &node,
                    processor.midi_input_count(),
                    &track_input_events,
                    &self.folder_plugin_midi_node_events,
                );
                let vst3_input = midi_inputs.first().cloned().unwrap_or_default();
                let outputs = processor.process_with_midi(frames, &vst3_input);
                let track_name = self.name.clone();
                for ev in processor.drain_echoed_parameters() {
                    self.echoed_parameter_updates.lock().push(
                        crate::message::Action::TrackSetVst3Parameter {
                            track_name: track_name.clone(),
                            instance_id: self.vst3_plugins[index].id,
                            param_id: ev.param_index,
                            value: ev.value,
                        },
                    );
                }
                if !outputs.is_empty() {
                    self.folder_plugin_midi_node_events
                        .insert((node.clone(), 0), outputs);
                }
                self.folder_processed_midi_plugins.insert(node);
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            PluginKind::Lv2 => {
                if index >= self.lv2_plugins.len() {
                    return;
                }
                let processor = self.lv2_plugins[index].processor.lock();
                let ready = processor.audio_inputs().iter().all(|input| input.ready());
                let node = PluginGraphNode::Lv2PluginInstance(self.lv2_plugins[index].id);
                if !ready || !self.plugin_midi_ready(&node, &self.folder_processed_midi_plugins) {
                    return;
                }
                for input in processor.audio_inputs() {
                    input.process();
                }
                let midi_inputs = self.plugin_midi_input_events(
                    &node,
                    processor.midi_input_count(),
                    &track_input_events,
                    &self.folder_plugin_midi_node_events,
                );
                let lv2_input = midi_inputs.first().cloned().unwrap_or_default();
                let outputs = processor.process_with_midi(frames, &lv2_input);
                let track_name = self.name.clone();
                for ev in processor.drain_echoed_parameters() {
                    self.echoed_parameter_updates.lock().push(
                        crate::message::Action::TrackSetLv2ControlValue {
                            track_name: track_name.clone(),
                            instance_id: self.lv2_plugins[index].id,
                            index: ev.param_index,
                            value: ev.value,
                        },
                    );
                }
                if !outputs.is_empty() {
                    self.folder_plugin_midi_node_events
                        .insert((node.clone(), 0), outputs);
                }
                self.folder_processed_midi_plugins.insert(node);
            }
        }
    }

    pub fn process_folder_output(&mut self) {
        let track_input_events = self.folder_input_midi_events.clone();
        let midi_node_events = self.folder_plugin_midi_node_events.clone();

        self.ensure_midi_route_cache();
        self.route_track_inputs_to_track_outputs(&track_input_events);
        self.route_plugin_midi_to_track_outputs_graph(&track_input_events, &midi_node_events);

        // Sum child-track MIDI outputs into the folder's MIDI outputs.
        for child in &self.child_tracks {
            let child = child.lock();
            for (out_idx, child_out) in child.midi.outs.iter().enumerate() {
                if let Some(folder_out) = self.midi.outs.get(out_idx) {
                    let events = {
                        let child_out_lock = child_out.lock();
                        child_out_lock.buffer.clone()
                    };
                    if !events.is_empty() {
                        folder_out.lock().buffer.extend_from_slice(&events);
                    }
                }
            }
        }

        self.route_modulator_midi_to_track_outputs();
        self.route_automation_midi_to_track_outputs();
        self.collect_hw_midi_output_events();
        self.clear_local_midi_inputs();

        let linear_gain = 10.0_f32.powf(self.level / 20.0);
        let phase_multiplier = if self.phase_inverted { -1.0 } else { 1.0 };
        let (left_balance, right_balance) = if self.audio.outs.len() == 2 {
            let b = self.balance.clamp(-1.0, 1.0);
            ((1.0 - b).clamp(0.0, 1.0), (1.0 + b).clamp(0.0, 1.0))
        } else {
            (1.0, 1.0)
        };

        self.ensure_audio_route_cache();
        if self.output_meter_linear_cache.len() != self.audio.outs.len() {
            self.output_meter_linear_cache
                .resize(self.audio.outs.len(), 0.0);
        }
        if self.meter_peak_hold_linear.len() != self.audio.outs.len() {
            self.meter_peak_hold_linear
                .resize(self.audio.outs.len(), 0.0);
        }
        let clip_playback_active = self.folder_clip_playback_active;
        let record_tap_input_snapshots = self.folder_record_tap_input_snapshots.clone();
        let mut all_outputs_zero = true;
        for out_idx in 0..self.audio.outs.len() {
            let audio_out = self.audio.outs[out_idx].clone();
            let out_samples = audio_out.buffer.lock();
            let capture_record_tap = self.armed && self.record_tap_enabled;
            if capture_record_tap {
                if self.record_tap_outs.len() <= out_idx {
                    self.record_tap_outs.push(vec![0.0; out_samples.len()]);
                }
                if self.record_tap_outs[out_idx].len() != out_samples.len() {
                    self.record_tap_outs[out_idx].resize(out_samples.len(), 0.0);
                }
            }
            let balance_gain = if self.audio.outs.len() == 2 {
                if out_idx == 0 {
                    left_balance
                } else {
                    right_balance
                }
            } else {
                1.0
            };
            let output_gain = linear_gain * balance_gain * phase_multiplier;
            let unity_output_gain = (output_gain - 1.0).abs() <= f32::EPSILON;
            let sources = self.internal_output_routes_cache.get(out_idx);
            let has_sources = sources.is_some_and(|s| !s.is_empty());
            let mut wrote_output = false;
            if self.output_enabled
                && let Some(sources) = sources
            {
                let mut seeded = false;
                for source in sources {
                    let source_input_monitor = self
                        .audio
                        .ins
                        .iter()
                        .position(|input| Arc::ptr_eq(input, source))
                        .and_then(|idx| self.input_monitor.get(idx).copied())
                        .unwrap_or(false);
                    if !source_input_monitor
                        && !clip_playback_active
                        && self.is_track_input_source(source)
                    {
                        continue;
                    }
                    let source_buf = source.buffer.lock();
                    if !seeded {
                        if unity_output_gain {
                            Self::copy_unity_with_zero_tail(out_samples, source_buf);
                        } else {
                            Self::copy_scaled_with_zero_tail(out_samples, source_buf, output_gain);
                        }
                        seeded = true;
                        wrote_output = true;
                    } else if unity_output_gain {
                        Self::add_unity(out_samples, source_buf);
                    } else {
                        Self::add_scaled(out_samples, source_buf, output_gain);
                    }
                }
            }
            if !wrote_output {
                out_samples.fill(0.0);
            }

            if capture_record_tap {
                let tap = &mut self.record_tap_outs[out_idx];
                if has_sources {
                    if let Some(sources) = sources {
                        let first_idx = self
                            .audio
                            .ins
                            .iter()
                            .position(|input| Arc::ptr_eq(input, &sources[0]));
                        if let Some(idx) = first_idx
                            .filter(|idx| !self.input_monitor.get(*idx).copied().unwrap_or(false))
                        {
                            Self::copy_unity_with_zero_tail(tap, &record_tap_input_snapshots[idx]);
                        } else {
                            let first = sources[0].buffer.lock();
                            Self::copy_unity_with_zero_tail(tap, first);
                        }
                        for source in &sources[1..] {
                            if let Some(idx) = self
                                .audio
                                .ins
                                .iter()
                                .position(|input| Arc::ptr_eq(input, source))
                                .filter(|idx| {
                                    !self.input_monitor.get(*idx).copied().unwrap_or(false)
                                })
                            {
                                Self::add_unity(tap, &record_tap_input_snapshots[idx]);
                            } else {
                                let source_buf = source.buffer.lock();
                                Self::add_unity(tap, source_buf);
                            }
                        }
                    }
                } else if let Some(source) = record_tap_input_snapshots
                    .get(out_idx.min(record_tap_input_snapshots.len().saturating_sub(1)))
                {
                    Self::copy_unity_with_zero_tail(tap, source);
                } else {
                    tap.fill(0.0);
                }
            }
            let peak_now = crate::simd::peak_abs(out_samples);
            if peak_now > 0.0 {
                all_outputs_zero = false;
            }

            let held = self.meter_peak_hold_linear[out_idx] * 0.92;
            let next = peak_now.max(held);
            self.meter_peak_hold_linear[out_idx] = next;
            self.output_meter_linear_cache[out_idx] = next;
            *audio_out.finished.lock() = true;
        }

        self.last_render_block_silent = all_outputs_zero;
        self.audio.finished = true;
        self.audio.processing = false;
    }

    pub fn name(&self) -> String {
        self.name.clone()
    }
    pub fn set_name(&mut self, name: String) {
        self.name = name;
    }

    pub fn level(&self) -> f32 {
        self.level
    }
    pub fn set_level(&mut self, level: f32) {
        self.level = level;
    }
    pub fn set_balance(&mut self, balance: f32) {
        self.balance = balance.clamp(-1.0, 1.0);
    }

    pub fn output_meter_linear(&self) -> Vec<f32> {
        self.output_meter_linear_cache.clone()
    }

    pub fn clear_output_meters(&mut self) {
        for value in &mut self.output_meter_linear_cache {
            *value = 0.0;
        }
        for value in &mut self.meter_peak_hold_linear {
            *value = 0.0;
        }
    }

    pub fn arm(&mut self) {
        self.armed = !self.armed;
    }

    pub fn set_output_enabled(&mut self, enabled: bool) {
        self.output_enabled = enabled;
    }
    pub fn set_transport_sample(&mut self, sample: usize) {
        self.transport_sample = sample;
    }
    pub fn set_loop_config(&mut self, enabled: bool, range: Option<(usize, usize)>) {
        self.loop_enabled = enabled;
        self.loop_range_samples = range;
    }
    pub fn set_transport_timing(&mut self, tempo_bpm: f64, tsig_num: u16, tsig_denom: u16) {
        self.tempo_bpm = tempo_bpm.max(1.0);
        self.tsig_num = tsig_num.max(1);
        self.tsig_denom = tsig_denom.max(1);
    }
    pub fn set_clip_playback_enabled(&mut self, enabled: bool) {
        self.clip_playback_enabled = enabled;
    }
    pub fn set_metronome_enabled(&mut self, enabled: bool) {
        self.metronome_enabled = enabled;
    }
    pub fn set_record_tap_enabled(&mut self, enabled: bool) {
        self.record_tap_enabled = enabled;
    }

    pub fn set_midi_lane_channel(&mut self, lane: usize, channel: Option<u8>) {
        if let Some(slot) = self.midi_lane_channels.get_mut(lane) {
            *slot = channel.map(|channel| channel.min(15));
        }
    }
    pub fn mute(&mut self) {
        self.muted = !self.muted;
    }
    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
    }
    pub fn invert_phase(&mut self) {
        self.phase_inverted = !self.phase_inverted;
    }
    pub fn set_phase_inverted(&mut self, phase_inverted: bool) {
        self.phase_inverted = phase_inverted;
    }
    pub fn solo(&mut self) {
        self.soloed = !self.soloed;
    }
    pub fn toggle_master(&mut self) {
        // A folder track can never become master; an already-master folder
        // is allowed to toggle off to recover from invalid legacy states.
        if !self.is_master && self.is_folder {
            return;
        }
        self.is_master = !self.is_master;
    }
    pub fn set_master(&mut self, master: bool) {
        if master && self.is_folder {
            return;
        }
        self.is_master = master;
    }
    pub fn toggle_input_monitor(&mut self, lane: usize) {
        if let Some(monitor) = self.input_monitor.get_mut(lane) {
            *monitor = !*monitor;
        }
    }
    pub fn toggle_disk_monitor(&mut self, lane: usize) {
        if let Some(monitor) = self.disk_monitor.get_mut(lane) {
            *monitor = !*monitor;
        }
    }
    pub fn toggle_midi_input_monitor(&mut self, lane: usize) {
        if let Some(monitor) = self.midi_input_monitor.get_mut(lane) {
            *monitor = !*monitor;
        }
    }
    pub fn toggle_midi_disk_monitor(&mut self, lane: usize) {
        if let Some(monitor) = self.midi_disk_monitor.get_mut(lane) {
            *monitor = !*monitor;
        }
    }

    pub fn set_session_base_dir(&mut self, base_dir: Option<PathBuf>) {
        if self.session_base_dir != base_dir {
            tracing::warn!(
                "Clearing clip caches for track '{}' because session base dir changed from {:?} to {:?}",
                self.name,
                self.session_base_dir,
                base_dir
            );
            self.session_base_dir = base_dir;

            self.audio_clip_cache.clear();
            self.midi_clip_cache.clear();
        }
    }

    pub fn frozen(&self) -> bool {
        self.frozen
    }

    pub fn set_frozen(&mut self, frozen: bool) {
        self.frozen = frozen;
    }

    pub fn offline_bounce_interleaved(
        &mut self,
        start_sample: usize,
        length_samples: usize,
    ) -> (usize, Vec<f32>) {
        let channels = self.audio.outs.len().max(1);
        if length_samples == 0 {
            return (channels, vec![]);
        }
        let block_size = self
            .audio
            .outs
            .first()
            .map(|io| io.buffer.lock().len())
            .or_else(|| self.audio.ins.first().map(|io| io.buffer.lock().len()))
            .unwrap_or(0)
            .max(1);

        let saved_transport = self.transport_sample;
        let saved_disk_monitor = self.disk_monitor.clone();
        let saved_input_monitor = self.input_monitor.clone();
        let saved_midi_disk_monitor = self.midi_disk_monitor.clone();
        let saved_midi_input_monitor = self.midi_input_monitor.clone();
        let saved_clip_playback_enabled = self.clip_playback_enabled;
        let saved_record_tap_enabled = self.record_tap_enabled;
        let saved_armed = self.armed;
        let saved_output_enabled = self.output_enabled;
        let saved_loop_enabled = self.loop_enabled;
        let saved_loop_range = self.loop_range_samples;
        let saved_pending_hw = self.pending_hw_midi_out_events.clone();

        let audio_in_count = self.audio.ins.len();
        let midi_in_count = self.midi.ins.len();
        self.disk_monitor = vec![true; audio_in_count];
        self.input_monitor = vec![false; audio_in_count];
        self.midi_disk_monitor = vec![true; midi_in_count];
        self.midi_input_monitor = vec![false; midi_in_count];
        self.clip_playback_enabled = true;
        self.record_tap_enabled = false;
        self.armed = false;
        self.output_enabled = true;
        self.loop_enabled = false;
        self.loop_range_samples = None;
        self.pending_hw_midi_out_events.clear();

        let mut rendered = vec![0.0_f32; length_samples.saturating_mul(channels)];
        let mut cursor = 0usize;
        while cursor < length_samples {
            self.transport_sample = start_sample.saturating_add(cursor);
            self.process();
            let step = (length_samples - cursor).min(block_size);
            if channels == 2 {
                let out_l = self.audio.outs[0].buffer.lock();
                let out_r = self.audio.outs[1].buffer.lock();
                let copy_len = step.min(out_l.len()).min(out_r.len());
                #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
                unsafe {
                    if std::arch::is_x86_feature_detected!("sse") {
                        let n = copy_len / 4;
                        for i in 0..n {
                            let l = std::arch::x86_64::_mm_loadu_ps(out_l.as_ptr().add(i * 4));
                            let r = std::arch::x86_64::_mm_loadu_ps(out_r.as_ptr().add(i * 4));
                            let lr0 = std::arch::x86_64::_mm_unpacklo_ps(l, r);
                            let lr1 = std::arch::x86_64::_mm_unpackhi_ps(l, r);
                            let dst = (cursor + i * 4) * 2;
                            std::arch::x86_64::_mm_storeu_ps(rendered.as_mut_ptr().add(dst), lr0);
                            std::arch::x86_64::_mm_storeu_ps(
                                rendered.as_mut_ptr().add(dst + 4),
                                lr1,
                            );
                        }
                        for i in n * 4..copy_len {
                            let dst = (cursor + i) * 2;
                            rendered[dst] = out_l[i];
                            rendered[dst + 1] = out_r[i];
                        }
                    } else {
                        for i in 0..copy_len {
                            let dst = (cursor + i) * 2;
                            rendered[dst] = out_l[i];
                            rendered[dst + 1] = out_r[i];
                        }
                    }
                }
                #[cfg(not(any(target_arch = "x86_64", target_arch = "x86")))]
                {
                    for i in 0..copy_len {
                        let dst = (cursor + i) * 2;
                        rendered[dst] = out_l[i];
                        rendered[dst + 1] = out_r[i];
                    }
                }
            } else {
                for ch in 0..channels {
                    let out = self.audio.outs[ch].buffer.lock();
                    let copy_len = step.min(out.len());
                    for (i, out_i) in out.iter().enumerate().take(copy_len) {
                        let dst = (cursor + i) * channels + ch;
                        rendered[dst] = *out_i;
                    }
                }
            }
            cursor = cursor.saturating_add(step);
            self.pending_hw_midi_out_events.clear();
        }

        self.transport_sample = saved_transport;
        self.disk_monitor = saved_disk_monitor;
        self.input_monitor = saved_input_monitor;
        self.midi_disk_monitor = saved_midi_disk_monitor;
        self.midi_input_monitor = saved_midi_input_monitor;
        self.clip_playback_enabled = saved_clip_playback_enabled;
        self.record_tap_enabled = saved_record_tap_enabled;
        self.armed = saved_armed;
        self.output_enabled = saved_output_enabled;
        self.loop_enabled = saved_loop_enabled;
        self.loop_range_samples = saved_loop_range;
        self.pending_hw_midi_out_events = saved_pending_hw;

        (channels, rendered)
    }

    pub(crate) fn resolve_clip_path(&self, clip_name: &str) -> PathBuf {
        let clip_path = Path::new(clip_name);
        if clip_path.is_absolute() {
            clip_path.to_path_buf()
        } else {
            if let Some(base) = &self.session_base_dir {
                let candidate = base.join(clip_path);
                if candidate.exists() {
                    return candidate;
                }
            }

            let cwd_candidate = clip_path.to_path_buf();
            if cwd_candidate.exists() {
                return cwd_candidate;
            }

            if let Ok(session_root) = std::env::var("MAOLAN_SESSION_PATH") {
                let candidate = Path::new(&session_root).join(clip_path);
                if candidate.exists() {
                    return candidate;
                }
            }

            if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
                let candidate = Path::new(&home).join("recordings").join(clip_path);
                if candidate.exists() {
                    return candidate;
                }
            }

            if let Some(base) = &self.session_base_dir {
                base.join(clip_path)
            } else {
                cwd_candidate
            }
        }
    }

    pub(crate) fn invalidate_midi_clip_cache(&mut self, clip_name: &str) {
        self.midi_clip_cache.remove(clip_name);
    }

    fn load_audio_clip_buffer(path: &Path) -> Option<AudioClipBuffer> {
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

    fn clip_buffer(&mut self, clip_name: &str) -> Option<Arc<AudioClipBuffer>> {
        if let Some(cached) = self.audio_clip_cache.get(clip_name) {
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
        self.audio_clip_cache
            .insert(clip_name.to_string(), loaded.clone());
        Some(loaded)
    }

    fn clip_playback_name(clip: &crate::audio::clip::AudioClip) -> &str {
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
    fn clip_pitch_key(clip: &crate::audio::clip::AudioClip) -> String {
        format!(
            "{}:{}:{}:{}:{}",
            clip.name, clip.start, clip.end, clip.offset, clip.input_channel
        )
    }

    fn clip_plugin_runtime_key(
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

    fn clip_plugin_runtime_node_from_json(
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

    fn clip_graph_uses_plugin_runtime(graph: &Value) -> bool {
        graph
            .get("plugins")
            .and_then(Value::as_array)
            .is_some_and(|plugins| !plugins.is_empty())
    }

    fn clip_graph_track_io_node(value: &Value) -> Option<bool> {
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

    fn process_direct_clip_graph(
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

    fn build_clip_plugin_runtime(
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
                        let processor = match crate::clap_proc::ClapProcessor::new(
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
                            .clap_plugins
                            .push(ClapInstance::new(id, Arc::new(UnsafeMutex::new(processor))));
                        runtime_nodes.push(PluginGraphNode::ClapPluginInstance(id));
                    }
                    "VST3" | "vst3" => {
                        let host_binary = match crate::plugins::ipc::find_plugin_host_binary() {
                            Some(b) => b,
                            None => continue,
                        };
                        let processor = match crate::vst3_proc::Vst3Processor::new(
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
                            .vst3_plugins
                            .push(Vst3Instance::new(id, Arc::new(UnsafeMutex::new(processor))));
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
                            .push(Lv2Instance::new(id, Arc::new(UnsafeMutex::new(processor))));
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

    fn process_clip_plugin_runtime_segment(
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
        if !self.clip_plugin_tracks.contains_key(&runtime_key) {
            let runtime =
                self.build_clip_plugin_runtime(clip, input_count, self.process_block_size)?;
            self.clip_plugin_tracks.insert(runtime_key.clone(), runtime);
        }
        let runtime = self
            .clip_plugin_tracks
            .get_mut(&runtime_key)
            .ok_or_else(|| "Missing clip plugin runtime".to_string())?;

        Ok(runtime.process(input_blocks, request_len, ClipRuntimeProcessContext {}))
    }

    fn apply_audio_clip_fades(
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

    fn render_audio_clip_segment(
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
        let buffer = match self.audio_clip_cache.get(playback_name).cloned() {
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
                    self.clip_pitch_shifters
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

    fn collect_midi_clip_events_recursive(
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
            .midi_disk_monitor
            .get(input_lane)
            .copied()
            .unwrap_or(true)
        {
            return;
        }
        let clip_end = clip_start.saturating_add(clip_len);
        let Some(events) = self.midi_clip_cache.get(&clip.name) else {
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

    fn is_midi_note_off(data: &[u8]) -> bool {
        let Some(status) = data.first().copied() else {
            return false;
        };
        match status & 0xF0 {
            0x80 => true,
            0x90 => data.get(2).copied().unwrap_or(0) == 0,
            _ => false,
        }
    }

    fn synthetic_note_offs_at_clip_end(
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

    fn ensure_clip_plugin_runtime(
        &mut self,
        clip_idx: usize,
        channels: usize,
    ) -> Result<&mut ClipPluginRuntime, String> {
        let clip = self.audio.clips.get(clip_idx).cloned().ok_or_else(|| {
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
        if !self.clip_plugin_tracks.contains_key(&runtime_key) {
            let runtime =
                self.build_clip_plugin_runtime(&clip, channels, self.process_block_size)?;
            self.clip_plugin_tracks.insert(runtime_key.clone(), runtime);
        }
        let runtime = self
            .clip_plugin_tracks
            .get_mut(&runtime_key)
            .ok_or_else(|| "Missing clip plugin runtime".to_string())?;
        Ok(runtime)
    }

    #[cfg(unix)]
    fn pitch_shift_for_sample(
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

    fn preload_audio_clip_cache(&mut self) {
        let missing: Vec<String> = self
            .audio
            .clips
            .iter()
            .filter_map(|clip| {
                let clip_name = Self::clip_playback_name(clip);
                if self.audio_clip_cache.contains_key(clip_name) {
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
                    self.audio.clips.len()
                );
            }
        }
    }

    fn load_midi_clip_events(path: &Path, sample_rate: f64) -> Option<Vec<(usize, Vec<u8>)>> {
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

    fn midi_clip_events(&mut self, clip_name: &str) -> Option<MidiClipEvents> {
        if let Some(cached) = self.midi_clip_cache.get(clip_name) {
            return Some(cached.clone());
        }
        let path = self.resolve_clip_path(clip_name);
        let loaded = Self::load_midi_clip_events(&path, self.sample_rate)?;
        let loaded = Arc::new(loaded);
        self.midi_clip_cache
            .insert(clip_name.to_string(), loaded.clone());
        Some(loaded)
    }

    fn preload_midi_clip_cache(&mut self) {
        let missing: Vec<String> = self
            .midi
            .clips
            .iter()
            .filter_map(|clip| {
                if self.midi_clip_cache.contains_key(&clip.name) {
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
                    self.midi.clips.len()
                );
            }
        }
    }

    pub fn preload_clips(&mut self) {
        self.preload_audio_clip_cache();
        self.preload_midi_clip_cache();
    }

    fn cycle_segments(&self, frames: usize) -> Vec<(usize, usize, usize)> {
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

    fn mix_clip_audio_into_inputs(&mut self) {
        let frames = self
            .audio
            .ins
            .first()
            .map(|audio_in| audio_in.buffer.lock().len())
            .unwrap_or(0);
        tracing::debug!(
            "mix_clip_audio_into_inputs for '{}' frames={} clips={}",
            self.name,
            frames,
            self.audio.clips.len()
        );
        if frames == 0 || self.audio.ins.is_empty() {
            return;
        }

        let mut active_clip_plugin_keys = HashSet::new();
        let segments = self.cycle_segments(frames);
        for clip in self.audio.clips.clone() {
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
                    self.transport_sample
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
                let copy_len = to.saturating_sub(from).min(
                    self.audio
                        .ins
                        .first()
                        .map(|audio_in| audio_in.buffer.lock().len().saturating_sub(track_idx))
                        .unwrap_or(0),
                );
                if copy_len == 0 {
                    tracing::debug!("mix_clip_audio_into_inputs clip '{}' copy_len=0", clip.name);
                    continue;
                }
                let render_start = std::time::Instant::now();
                let Some(processed_blocks) = self.render_audio_clip_segment(
                    &clip,
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
                for in_channel in 0..self.audio.ins.len() {
                    if !self.disk_monitor.get(in_channel).copied().unwrap_or(false) {
                        continue;
                    }
                    let in_samples = self.audio.ins[in_channel].buffer.lock();
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
        self.clip_plugin_tracks
            .retain(|key, _| active_clip_plugin_keys.contains(key));
    }

    fn mix_clip_midi_into_inputs(&mut self, input_events: &mut [Vec<MidiEvent>], frames: usize) {
        if frames == 0 || input_events.is_empty() {
            return;
        }
        let segments = self.cycle_segments(frames);
        for clip in &self.midi.clips {
            self.collect_midi_clip_events_recursive(clip, 0, input_events, frames, &segments);
        }
        for events in input_events.iter_mut() {
            events.sort_by_key(|event| event.frame);
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub fn load_lv2_plugin(&mut self, uri: &str, instance_id: Option<usize>) -> Result<(), String> {
        let buffer_size = self
            .audio
            .ins
            .first()
            .map(|io| io.buffer.lock().len())
            .or_else(|| self.audio.outs.first().map(|io| io.buffer.lock().len()))
            .unwrap_or(0);
        let host_binary = crate::plugins::ipc::find_plugin_host_binary()
            .ok_or_else(|| "maolan-plugin-host binary not found".to_string())?;
        let processor = crate::lv2_proc::Lv2Processor::new(
            self.sample_rate,
            buffer_size,
            uri,
            self.audio.ins.len().max(1),
            self.audio.outs.len().max(1),
            host_binary,
        )?;
        let id = instance_id
            .filter(|&id| {
                !self.vst3_plugins.iter().any(|i| i.id == id)
                    && !self.clap_plugins.iter().any(|i| i.id == id)
                    && !self.lv2_instance_id_exists(id)
            })
            .unwrap_or_else(|| self.alloc_plugin_instance_id());
        self.next_lv2_instance_id = self.next_lv2_instance_id.max(id.saturating_add(1));
        self.next_plugin_instance_id = self.next_plugin_instance_id.max(id.saturating_add(1));
        self.lv2_plugins.push(Lv2Instance {
            id,
            processor: Arc::new(UnsafeMutex::new(processor)),
        });
        self.invalidate_audio_route_cache();
        Ok(())
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub fn unload_lv2_plugin(&mut self, uri: &str) -> Result<(), String> {
        let Some(index) = self
            .lv2_plugins
            .iter()
            .position(|instance| instance.processor.lock().uri() == uri)
        else {
            return Err(format!(
                "Track '{}' does not have LV2 plugin loaded: {uri}",
                self.name
            ));
        };
        self.remove_lv2_instance(index);
        Ok(())
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub fn unload_lv2_plugin_instance(&mut self, instance_id: usize) -> Result<(), String> {
        let Some(index) = self
            .lv2_plugins
            .iter()
            .position(|instance| instance.id == instance_id)
        else {
            return Err(format!(
                "Track '{}' does not have LV2 instance id: {}",
                self.name, instance_id
            ));
        };
        self.remove_lv2_instance(index);
        Ok(())
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    fn remove_lv2_instance(&mut self, index: usize) {
        let removed = self.lv2_plugins.remove(index);
        for port in removed.processor.lock().audio_inputs() {
            Self::disconnect_all(port);
        }
        for port in removed.processor.lock().audio_outputs() {
            Self::disconnect_all(port);
        }
        self.plugin_midi_connections.retain(|conn| {
            conn.from_node != PluginGraphNode::Lv2PluginInstance(removed.id)
                && conn.to_node != PluginGraphNode::Lv2PluginInstance(removed.id)
        });
        self.invalidate_audio_route_cache();
    }

    fn prune_plugin_midi_connections(&mut self, node: PluginGraphNode) {
        self.plugin_midi_connections
            .retain(|conn| conn.from_node != node && conn.to_node != node);
    }

    fn push_plugin_graph_plugin(plugins: &mut Vec<PluginGraphPlugin>, plugin: PluginGraphPlugin) {
        plugins.push(plugin);
    }

    pub fn plugin_graph_plugins(&self) -> Vec<PluginGraphPlugin> {
        let mut plugins = Vec::new();
        #[cfg(all(unix, not(target_os = "macos")))]
        for instance in &self.lv2_plugins {
            let proc = instance.processor.lock();
            Self::push_plugin_graph_plugin(
                &mut plugins,
                PluginGraphPlugin {
                    #[cfg(all(unix, not(target_os = "macos")))]
                    node: PluginGraphNode::Lv2PluginInstance(instance.id),
                    instance_id: instance.id,
                    format: "LV2".to_string(),
                    uri: proc.uri().to_string(),
                    plugin_id: proc.uri().to_string(),
                    name: proc.name().to_string(),
                    main_audio_inputs: proc.main_audio_input_count(),
                    main_audio_outputs: proc.main_audio_output_count(),
                    audio_inputs: proc.audio_inputs().len(),
                    audio_outputs: proc.audio_outputs().len(),
                    midi_inputs: proc.midi_input_count(),
                    midi_outputs: proc.midi_output_count(),
                    state: serde_json::to_value(proc.snapshot_state()).ok(),
                    bypassed: proc.is_bypassed(),
                },
            );
        }
        for instance in &self.vst3_plugins {
            let proc = instance.processor.lock();
            Self::push_plugin_graph_plugin(
                &mut plugins,
                PluginGraphPlugin {
                    node: PluginGraphNode::Vst3PluginInstance(instance.id),
                    instance_id: instance.id,
                    format: "VST3".to_string(),
                    uri: proc.path().to_string(),
                    plugin_id: proc.path().to_string(),
                    name: proc.name().to_string(),
                    main_audio_inputs: proc.main_audio_input_count(),
                    main_audio_outputs: proc.main_audio_output_count(),
                    audio_inputs: proc.audio_inputs().len(),
                    audio_outputs: proc.audio_outputs().len(),
                    midi_inputs: proc.midi_input_count(),
                    midi_outputs: proc.midi_output_count(),
                    state: None,
                    bypassed: proc.is_bypassed(),
                },
            );
        }
        for instance in &self.clap_plugins {
            let proc = instance.processor.lock();
            Self::push_plugin_graph_plugin(
                &mut plugins,
                PluginGraphPlugin {
                    node: PluginGraphNode::ClapPluginInstance(instance.id),
                    instance_id: instance.id,
                    format: "CLAP".to_string(),
                    uri: proc.path().to_string(),
                    plugin_id: proc.plugin_id().to_string(),
                    name: proc.name().to_string(),
                    main_audio_inputs: proc.main_audio_input_count(),
                    main_audio_outputs: proc.main_audio_output_count(),
                    audio_inputs: proc.audio_inputs().len(),
                    audio_outputs: proc.audio_outputs().len(),
                    midi_inputs: proc.midi_input_count(),
                    midi_outputs: proc.midi_output_count(),
                    state: proc
                        .snapshot_state()
                        .ok()
                        .and_then(|state| serde_json::to_value(state).ok()),
                    bypassed: proc.is_bypassed(),
                },
            );
        }
        plugins
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub fn set_lv2_plugin_bypassed(
        &self,
        instance_id: usize,
        bypassed: bool,
    ) -> Result<(), String> {
        let Some(instance) = self
            .lv2_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
        else {
            return Err(format!(
                "Track '{}' does not have LV2 instance id: {}",
                self.name, instance_id
            ));
        };
        instance.processor.lock().set_bypassed(bypassed);
        Ok(())
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub fn set_lv2_control_value(
        &self,
        instance_id: usize,
        index: usize,
        param_value: f64,
    ) -> Result<(), String> {
        let Some(instance) = self
            .lv2_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
        else {
            return Err(format!(
                "Track '{}' does not have LV2 instance id: {}",
                self.name, instance_id
            ));
        };
        instance
            .processor
            .lock()
            .set_parameter(index as u32, param_value)
    }

    fn normalize_clap_path(path: &str) -> String {
        if let Some(pos) = path.rfind("::") {
            format!("{}::{}", &path[..pos], &path[pos + 2..])
        } else if let Some(pos) = path.rfind('#') {
            format!("{}::{}", &path[..pos], &path[pos + 1..])
        } else {
            path.to_string()
        }
    }

    pub fn load_clap_plugin(
        &mut self,
        plugin_path: &str,
        instance_id: Option<usize>,
    ) -> Result<(), String> {
        let normalized = Self::normalize_clap_path(plugin_path);
        let bundle_path = normalized
            .split_once("::")
            .map(|(path, _)| path)
            .unwrap_or(&normalized);
        let path = Path::new(bundle_path);
        if !path.exists() {
            return Err(format!("CLAP plugin not found: {plugin_path}"));
        }
        if !crate::clap::is_supported_clap_binary(path) {
            return Err(format!("Not a CLAP plugin path: {plugin_path}"));
        }
        let id = instance_id
            .filter(|&id| {
                !self.vst3_plugins.iter().any(|i| i.id == id)
                    && !self.clap_plugins.iter().any(|i| i.id == id)
                    && !self.lv2_instance_id_exists(id)
            })
            .unwrap_or_else(|| self.alloc_plugin_instance_id());
        self.next_clap_instance_id = self.next_clap_instance_id.max(id.saturating_add(1));
        self.next_plugin_instance_id = self.next_plugin_instance_id.max(id.saturating_add(1));
        let buffer_size = self
            .audio
            .ins
            .first()
            .map(|io| io.buffer.lock().len())
            .or_else(|| self.audio.outs.first().map(|io| io.buffer.lock().len()))
            .unwrap_or(0);
        let input_count = self.audio.ins.len().max(1);
        let output_count = self.audio.outs.len().max(1);
        let host_binary = crate::plugins::ipc::find_plugin_host_binary()
            .ok_or_else(|| "maolan-plugin-host binary not found".to_string())?;
        let processor = Arc::new(UnsafeMutex::new(crate::clap_proc::ClapProcessor::new(
            self.sample_rate,
            buffer_size,
            plugin_path,
            input_count,
            output_count,
            host_binary,
        )?));
        self.clap_plugins.push(ClapInstance::new(id, processor));
        self.invalidate_audio_route_cache();
        Ok(())
    }

    pub fn unload_clap_plugin(&mut self, plugin_path: &str) -> Result<(), String> {
        let normalized = Self::normalize_clap_path(plugin_path);
        let Some(index) = self.clap_plugins.iter().position(|instance| {
            Self::normalize_clap_path(instance.processor.lock().path()) == normalized
        }) else {
            return Err(format!(
                "Track '{}' does not have CLAP plugin loaded: {}",
                self.name, plugin_path
            ));
        };
        let removed_id = self.clap_plugins[index].id;
        self.clap_plugins.remove(index);
        self.prune_plugin_midi_connections(PluginGraphNode::ClapPluginInstance(removed_id));
        self.invalidate_audio_route_cache();
        Ok(())
    }

    pub fn unload_clap_plugin_instance(&mut self, instance_id: usize) -> Result<(), String> {
        let Some(index) = self
            .clap_plugins
            .iter()
            .position(|instance| instance.id == instance_id)
        else {
            return Err(format!(
                "Track '{}' does not have CLAP instance id: {}",
                self.name, instance_id
            ));
        };
        self.clap_plugins.remove(index);
        self.prune_plugin_midi_connections(PluginGraphNode::ClapPluginInstance(instance_id));
        self.invalidate_audio_route_cache();
        Ok(())
    }

    pub fn show_clap_gui(&self, instance_id: usize) -> Result<(), String> {
        if let Some(instance) = self.clap_plugins.iter().find(|i| i.id == instance_id) {
            let processor = instance.processor.lock();
            processor.gui_set_parent_x11(0)?;
            return processor.gui_show();
        }
        Err(format!(
            "Track '{}' does not have CLAP instance id: {}",
            self.name, instance_id
        ))
    }

    pub fn show_vst3_gui(&self, instance_id: usize) -> Result<(), String> {
        if let Some(instance) = self.vst3_plugins.iter().find(|i| i.id == instance_id) {
            return instance.processor.lock().gui_show();
        }
        Err(format!(
            "Track '{}' does not have VST3 instance id: {}",
            self.name, instance_id
        ))
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub fn show_lv2_gui(&self, instance_id: usize) -> Result<(), String> {
        if let Some(instance) = self.lv2_plugins.iter().find(|i| i.id == instance_id) {
            return instance.processor.lock().gui_show();
        }
        Err(format!(
            "Track '{}' does not have LV2 instance id: {}",
            self.name, instance_id
        ))
    }

    pub fn set_clap_plugin_bypassed(
        &self,
        instance_id: usize,
        bypassed: bool,
    ) -> Result<(), String> {
        if let Some(instance) = self.clap_plugins.iter().find(|i| i.id == instance_id) {
            instance.processor.lock().set_bypassed(bypassed);
            return Ok(());
        }
        Err(format!(
            "Track '{}' does not have CLAP instance id: {}",
            self.name, instance_id
        ))
    }

    pub fn set_clap_parameter(
        &self,
        instance_id: usize,
        param_id: u32,
        value: f64,
    ) -> Result<(), String> {
        if let Some(instance) = self.clap_plugins.iter().find(|i| i.id == instance_id) {
            return instance.processor.lock().set_parameter(param_id, value);
        }
        Err(format!(
            "Track '{}' does not have CLAP instance id: {}",
            self.name, instance_id
        ))
    }

    pub fn clip_set_clap_parameter(
        &mut self,
        clip_idx: usize,
        instance_id: usize,
        param_id: u32,
        value: f64,
    ) -> Result<(), String> {
        let channels = self.audio.ins.len().max(1);
        let runtime = self.ensure_clip_plugin_runtime(clip_idx, channels)?;
        let instance = runtime
            .clap_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .ok_or_else(|| format!("Clip CLAP instance {} not found", instance_id))?;
        instance.processor.lock().set_parameter(param_id, value)
    }

    pub fn set_clap_parameter_at(
        &self,
        instance_id: usize,
        param_id: u32,
        value: f64,
        frame: u32,
    ) -> Result<(), String> {
        if let Some(instance) = self.clap_plugins.iter().find(|i| i.id == instance_id) {
            return instance
                .processor
                .lock()
                .set_parameter_at(param_id, value, frame);
        }
        Err(format!(
            "Track '{}' does not have CLAP instance id: {}",
            self.name, instance_id
        ))
    }

    pub fn begin_clap_parameter_edit(
        &self,
        instance_id: usize,
        param_id: u32,
        frame: u32,
    ) -> Result<(), String> {
        let instance = self
            .clap_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .ok_or_else(|| {
                format!(
                    "Track '{}' does not have CLAP instance id: {}",
                    self.name, instance_id
                )
            })?;
        instance
            .processor
            .lock()
            .begin_parameter_edit_at(param_id, frame)
    }

    pub fn end_clap_parameter_edit(
        &self,
        instance_id: usize,
        param_id: u32,
        frame: u32,
    ) -> Result<(), String> {
        let instance = self
            .clap_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .ok_or_else(|| {
                format!(
                    "Track '{}' does not have CLAP instance id: {}",
                    self.name, instance_id
                )
            })?;
        instance
            .processor
            .lock()
            .end_parameter_edit_at(param_id, frame)
    }

    pub fn get_clap_parameters(
        &self,
        instance_id: usize,
    ) -> Result<Vec<crate::clap::ClapParameterInfo>, String> {
        let instance = self
            .clap_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .ok_or_else(|| {
                format!(
                    "Track '{}' does not have CLAP instance id: {}",
                    self.name, instance_id
                )
            })?;
        Ok(instance.processor.lock().parameter_infos())
    }

    pub fn get_clap_note_names(&self) -> std::collections::HashMap<u8, String> {
        let mut result = std::collections::HashMap::new();
        for instance in &self.clap_plugins {
            for (k, v) in instance.processor.lock().note_names() {
                result.insert(k, v);
            }
        }
        result
    }

    pub fn clap_snapshot_state(
        &self,
        instance_id: usize,
    ) -> Result<crate::clap::ClapPluginState, String> {
        if let Some(instance) = self.clap_plugins.iter().find(|i| i.id == instance_id) {
            return instance.processor.lock().snapshot_state();
        }
        Err(format!(
            "Track '{}' does not have CLAP instance id: {}",
            self.name, instance_id
        ))
    }

    pub fn clip_clap_snapshot_state(
        &mut self,
        clip_idx: usize,
        instance_id: usize,
    ) -> Result<(String, crate::clap::ClapPluginState), String> {
        let channels = self.audio.ins.len().max(1);
        let runtime = self.ensure_clip_plugin_runtime(clip_idx, channels)?;
        let instance = runtime
            .clap_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .ok_or_else(|| format!("Clip CLAP instance {} not found", instance_id))?;
        let state = instance.processor.lock().snapshot_state()?;
        Ok((instance.processor.lock().path().to_string(), state))
    }

    pub fn clap_restore_state(
        &self,
        instance_id: usize,
        state: &crate::clap::ClapPluginState,
    ) -> Result<(), String> {
        if let Some(instance) = self.clap_plugins.iter().find(|i| i.id == instance_id) {
            return instance.processor.lock().restore_state(state);
        }
        Err(format!(
            "Track '{}' does not have CLAP instance id: {}",
            self.name, instance_id
        ))
    }

    pub fn clip_clap_restore_state(
        &mut self,
        clip_idx: usize,
        instance_id: usize,
        state: &crate::clap::ClapPluginState,
    ) -> Result<(), String> {
        let channels = self.audio.ins.len().max(1);
        let runtime = self.ensure_clip_plugin_runtime(clip_idx, channels)?;
        let instance = runtime
            .clap_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .ok_or_else(|| format!("Clip CLAP instance {} not found", instance_id))?;
        instance.processor.lock().restore_state(state)
    }

    pub fn clap_snapshot_all_states(&self) -> Vec<(usize, String, crate::clap::ClapPluginState)> {
        self.clap_plugins
            .iter()
            .filter_map(|instance| {
                let proc = instance.processor.lock();
                proc.snapshot_state()
                    .ok()
                    .map(|state| (instance.id, proc.path().to_string(), state))
            })
            .collect()
    }

    pub fn take_dirty_clap_instances(&self) -> Vec<usize> {
        self.clap_plugins
            .iter()
            .filter_map(|instance| {
                if instance.processor.take_state_dirty() {
                    Some(instance.id)
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn set_clap_plugin_resource_dir(
        &self,
        instance_id: usize,
        dir: &std::path::Path,
    ) -> Result<(), String> {
        if let Some(instance) = self.clap_plugins.iter().find(|i| i.id == instance_id) {
            return instance.processor.lock().set_resource_directory(dir);
        }
        Err(format!(
            "Track '{}' does not have CLAP instance id: {}",
            self.name, instance_id
        ))
    }

    pub fn set_lv2_plugin_resource_dir(
        &self,
        instance_id: usize,
        dir: &std::path::Path,
    ) -> Result<(), String> {
        #[cfg(all(unix, not(target_os = "macos")))]
        if let Some(instance) = self.lv2_plugins.iter().find(|i| i.id == instance_id) {
            return instance.processor.lock().set_resource_directory(dir);
        }
        Err(format!(
            "Track '{}' does not have LV2 instance id: {}",
            self.name, instance_id
        ))
    }

    pub fn clap_file_references(
        &self,
        instance_id: usize,
    ) -> Result<Vec<maolan_plugin_protocol::protocol::FileReference>, String> {
        if let Some(instance) = self.clap_plugins.iter().find(|i| i.id == instance_id) {
            return instance.processor.lock().file_references();
        }
        Err(format!(
            "Track '{}' does not have CLAP instance id: {}",
            self.name, instance_id
        ))
    }

    pub fn update_clap_file_reference(
        &self,
        instance_id: usize,
        index: u32,
        path: &str,
    ) -> Result<(), String> {
        if let Some(instance) = self.clap_plugins.iter().find(|i| i.id == instance_id) {
            return instance.processor.lock().update_file_reference(index, path);
        }
        Err(format!(
            "Track '{}' does not have CLAP instance id: {}",
            self.name, instance_id
        ))
    }

    pub fn clip_set_clap_plugin_resource_dir(
        &mut self,
        clip_idx: usize,
        instance_id: usize,
        dir: &std::path::Path,
    ) -> Result<(), String> {
        let channels = self.audio.ins.len().max(1);
        let runtime = self.ensure_clip_plugin_runtime(clip_idx, channels)?;
        let instance = runtime
            .clap_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .ok_or_else(|| format!("Clip CLAP instance {} not found", instance_id))?;
        instance.processor.lock().set_resource_directory(dir)
    }

    pub fn clip_set_lv2_plugin_resource_dir(
        &mut self,
        clip_idx: usize,
        instance_id: usize,
        dir: &std::path::Path,
    ) -> Result<(), String> {
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let channels = self.audio.ins.len().max(1);
            let runtime = self.ensure_clip_plugin_runtime(clip_idx, channels)?;
            let instance = runtime
                .lv2_plugins
                .iter()
                .find(|instance| instance.id == instance_id)
                .ok_or_else(|| format!("Clip LV2 instance {} not found", instance_id))?;
            instance.processor.lock().set_resource_directory(dir)
        }
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        Err("LV2 is not supported on this platform".to_string())
    }

    pub fn clip_clap_file_references(
        &mut self,
        clip_idx: usize,
        instance_id: usize,
    ) -> Result<Vec<maolan_plugin_protocol::protocol::FileReference>, String> {
        let channels = self.audio.ins.len().max(1);
        let runtime = self.ensure_clip_plugin_runtime(clip_idx, channels)?;
        let instance = runtime
            .clap_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .ok_or_else(|| format!("Clip CLAP instance {} not found", instance_id))?;
        instance.processor.lock().file_references()
    }

    pub fn clip_update_clap_file_reference(
        &mut self,
        clip_idx: usize,
        instance_id: usize,
        index: u32,
        path: &str,
    ) -> Result<(), String> {
        let channels = self.audio.ins.len().max(1);
        let runtime = self.ensure_clip_plugin_runtime(clip_idx, channels)?;
        let instance = runtime
            .clap_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .ok_or_else(|| format!("Clip CLAP instance {} not found", instance_id))?;
        instance.processor.lock().update_file_reference(index, path)
    }

    pub fn load_vst3_plugin(
        &mut self,
        plugin_path: &str,
        instance_id: Option<usize>,
    ) -> Result<(), String> {
        let buffer_size = self
            .audio
            .ins
            .first()
            .map(|io| io.buffer.lock().len())
            .or_else(|| self.audio.outs.first().map(|io| io.buffer.lock().len()))
            .unwrap_or(64)
            .max(1);
        let input_count = self.audio.ins.len().max(1);
        let output_count = self.audio.outs.len().max(1);
        let host_binary = crate::plugins::ipc::find_plugin_host_binary()
            .ok_or_else(|| "maolan-plugin-host binary not found".to_string())?;
        let processor = crate::vst3_proc::Vst3Processor::new(
            self.sample_rate,
            buffer_size,
            plugin_path,
            input_count,
            output_count,
            host_binary,
        )?;
        let id = instance_id
            .filter(|&id| {
                !self.vst3_plugins.iter().any(|i| i.id == id)
                    && !self.clap_plugins.iter().any(|i| i.id == id)
                    && !self.lv2_instance_id_exists(id)
            })
            .unwrap_or_else(|| self.alloc_plugin_instance_id());
        self.next_vst3_instance_id = self.next_vst3_instance_id.max(id.saturating_add(1));
        self.next_plugin_instance_id = self.next_plugin_instance_id.max(id.saturating_add(1));
        self.vst3_plugins.push(Vst3Instance {
            id,
            processor: Arc::new(UnsafeMutex::new(processor)),
        });
        self.invalidate_audio_route_cache();
        Ok(())
    }

    pub fn unload_vst3_plugin(&mut self, plugin_path: &str) -> Result<(), String> {
        let Some(index) = self.vst3_plugins.iter().position(|instance| {
            instance
                .processor
                .lock()
                .path()
                .eq_ignore_ascii_case(plugin_path)
        }) else {
            return Err(format!(
                "Track '{}' does not have VST3 plugin loaded: {}",
                self.name, plugin_path
            ));
        };
        let removed = self.vst3_plugins.remove(index);
        let removed_id = removed.id;
        for port in removed.processor.lock().audio_inputs() {
            Self::disconnect_all(port);
        }
        for port in removed.processor.lock().audio_outputs() {
            Self::disconnect_all(port);
        }
        self.prune_plugin_midi_connections(PluginGraphNode::Vst3PluginInstance(removed_id));
        self.invalidate_audio_route_cache();
        Ok(())
    }

    pub fn unload_vst3_plugin_instance(&mut self, instance_id: usize) -> Result<(), String> {
        let Some(index) = self
            .vst3_plugins
            .iter()
            .position(|instance| instance.id == instance_id)
        else {
            return Err(format!(
                "Track '{}' does not have VST3 instance id: {}",
                self.name, instance_id
            ));
        };
        let removed = self.vst3_plugins.remove(index);
        for port in removed.processor.lock().audio_inputs() {
            Self::disconnect_all(port);
        }
        for port in removed.processor.lock().audio_outputs() {
            Self::disconnect_all(port);
        }
        self.prune_plugin_midi_connections(PluginGraphNode::Vst3PluginInstance(instance_id));
        self.invalidate_audio_route_cache();
        Ok(())
    }

    pub fn clear_plugins(&mut self) {
        let clap_ids: Vec<usize> = self.clap_plugins.iter().map(|i| i.id).collect();
        for id in clap_ids {
            let _ = self.unload_clap_plugin_instance(id);
        }
        let vst3_ids: Vec<usize> = self.vst3_plugins.iter().map(|i| i.id).collect();
        for id in vst3_ids {
            let _ = self.unload_vst3_plugin_instance(id);
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let lv2_ids: Vec<usize> = self.lv2_plugins.iter().map(|i| i.id).collect();
            for id in lv2_ids {
                let _ = self.unload_lv2_plugin_instance(id);
            }
        }
        self.plugin_midi_connections.clear();
        self.invalidate_audio_route_cache();
        self.invalidate_midi_route_cache();
    }

    pub fn vst3_graph_plugins(&self) -> Vec<crate::message::Vst3GraphPlugin> {
        use crate::message::Vst3GraphPlugin;

        self.vst3_plugins
            .iter()
            .map(|instance| {
                let proc = instance.processor.lock();
                Vst3GraphPlugin {
                    instance_id: instance.id,
                    name: proc.name().to_string(),
                    path: proc.path().to_string(),
                    audio_inputs: proc.audio_inputs().len(),
                    audio_outputs: proc.audio_outputs().len(),
                    parameters: proc.parameter_infos(),
                }
            })
            .collect()
    }

    pub fn vst3_graph_connections(&self) -> Vec<crate::message::Vst3GraphConnection> {
        use crate::kind::Kind;
        use crate::message::{Vst3GraphConnection, Vst3GraphNode};

        let mut connections = Vec::new();

        for instance in &self.vst3_plugins {
            let proc = instance.processor.lock();
            for (port_idx, input) in proc.audio_inputs().iter().enumerate() {
                let conns = input.connections.lock();
                for conn in conns.iter() {
                    let from_node = self.find_vst3_audio_source_node(conn.as_ref());
                    if let Some((node, from_port)) = from_node {
                        connections.push(Vst3GraphConnection {
                            from_node: node,
                            from_port,
                            to_node: Vst3GraphNode::PluginInstance(instance.id),
                            to_port: port_idx,
                            kind: Kind::Audio,
                        });
                    }
                }
            }

            for (port_idx, output) in proc.audio_outputs().iter().enumerate() {
                let conns = output.connections.lock();
                for conn in conns.iter() {
                    if self.audio.outs.iter().any(|out| Arc::ptr_eq(out, conn)) {
                        let to_port = self
                            .audio
                            .outs
                            .iter()
                            .position(|out| Arc::ptr_eq(out, conn))
                            .unwrap();

                        connections.push(Vst3GraphConnection {
                            from_node: Vst3GraphNode::PluginInstance(instance.id),
                            from_port: port_idx,
                            to_node: Vst3GraphNode::TrackOutput,
                            to_port,
                            kind: Kind::Audio,
                        });
                    }
                }
            }
        }

        connections
    }

    fn find_vst3_audio_source_node(
        &self,
        audio_io: &crate::audio::io::AudioIO,
    ) -> Option<(crate::message::Vst3GraphNode, usize)> {
        use crate::message::Vst3GraphNode;

        for (idx, input) in self.audio.ins.iter().enumerate() {
            if std::ptr::eq(input.as_ref(), audio_io) {
                return Some((Vst3GraphNode::TrackInput, idx));
            }
        }

        for instance in &self.vst3_plugins {
            for (port_idx, output) in instance.processor.lock().audio_outputs().iter().enumerate() {
                if std::ptr::eq(output.as_ref(), audio_io) {
                    return Some((Vst3GraphNode::PluginInstance(instance.id), port_idx));
                }
            }
        }

        None
    }

    pub fn set_vst3_plugin_bypassed(
        &self,
        instance_id: usize,
        bypassed: bool,
    ) -> Result<(), String> {
        let instance = self
            .vst3_plugins
            .iter()
            .find(|i| i.id == instance_id)
            .ok_or_else(|| format!("VST3 instance {} not found", instance_id))?;
        instance.processor.lock().set_bypassed(bypassed);
        Ok(())
    }

    pub fn set_vst3_parameter(
        &mut self,
        instance_id: usize,
        param_id: u32,
        value: f32,
    ) -> Result<(), String> {
        let instance = self
            .vst3_plugins
            .iter()
            .find(|i| i.id == instance_id)
            .ok_or_else(|| format!("VST3 instance {} not found", instance_id))?;

        instance
            .processor
            .lock()
            .set_parameter(param_id, value as f64)
    }

    pub fn get_vst3_parameters(
        &self,
        instance_id: usize,
    ) -> Result<Vec<crate::vst3::port::ParameterInfo>, String> {
        let instance = self
            .vst3_plugins
            .iter()
            .find(|i| i.id == instance_id)
            .ok_or_else(|| format!("VST3 instance {} not found", instance_id))?;

        Ok(instance.processor.lock().parameter_infos())
    }

    pub fn vst3_snapshot_state(
        &self,
        instance_id: usize,
    ) -> Result<crate::vst3::state::Vst3PluginState, String> {
        let instance = self
            .vst3_plugins
            .iter()
            .find(|i| i.id == instance_id)
            .ok_or_else(|| format!("VST3 instance {} not found", instance_id))?;

        instance.processor.lock().snapshot_state()
    }

    pub fn clip_vst3_snapshot_state(
        &mut self,
        clip_idx: usize,
        instance_id: usize,
    ) -> Result<crate::vst3::state::Vst3PluginState, String> {
        let channels = self.audio.ins.len().max(1);
        let runtime = self.ensure_clip_plugin_runtime(clip_idx, channels)?;
        let instance = runtime
            .vst3_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .ok_or_else(|| format!("Clip VST3 instance {} not found", instance_id))?;
        instance.processor.lock().snapshot_state()
    }

    pub fn vst3_restore_state(
        &mut self,
        instance_id: usize,
        state: &crate::vst3::state::Vst3PluginState,
    ) -> Result<(), String> {
        let instance = self
            .vst3_plugins
            .iter()
            .find(|i| i.id == instance_id)
            .ok_or_else(|| format!("VST3 instance {} not found", instance_id))?;

        instance.processor.lock().restore_state(state)
    }

    pub fn connect_vst3_audio(
        &mut self,
        from_node: &crate::message::Vst3GraphNode,
        from_port: usize,
        to_node: &crate::message::Vst3GraphNode,
        to_port: usize,
    ) -> Result<(), String> {
        use crate::message::Vst3GraphNode;

        let from_io = match from_node {
            Vst3GraphNode::TrackInput => self
                .audio
                .ins
                .get(from_port)
                .ok_or("Invalid track input port")?
                .clone(),
            Vst3GraphNode::PluginInstance(id) => {
                let instance = self
                    .vst3_plugins
                    .iter()
                    .find(|i| i.id == *id)
                    .ok_or("VST3 instance not found")?;
                instance
                    .processor
                    .lock()
                    .audio_outputs()
                    .get(from_port)
                    .ok_or("Invalid plugin output port")?
                    .clone()
            }
            Vst3GraphNode::TrackOutput => {
                return Err("Cannot connect from track output".to_string());
            }
        };

        let to_io = match to_node {
            Vst3GraphNode::PluginInstance(id) => {
                let instance = self
                    .vst3_plugins
                    .iter()
                    .find(|i| i.id == *id)
                    .ok_or("VST3 instance not found")?;
                instance
                    .processor
                    .lock()
                    .audio_inputs()
                    .get(to_port)
                    .ok_or("Invalid plugin input port")?
            }
            Vst3GraphNode::TrackOutput => self
                .audio
                .outs
                .get(to_port)
                .ok_or("Invalid track output port")?,
            Vst3GraphNode::TrackInput => return Err("Cannot connect to track input".to_string()),
        };

        to_io.connections.lock().push(from_io);
        self.invalidate_audio_route_cache();
        Ok(())
    }

    pub fn disconnect_vst3_audio(
        &mut self,
        from_node: &crate::message::Vst3GraphNode,
        from_port: usize,
        to_node: &crate::message::Vst3GraphNode,
        to_port: usize,
    ) -> Result<(), String> {
        use crate::message::Vst3GraphNode;

        let from_io = match from_node {
            Vst3GraphNode::TrackInput => self
                .audio
                .ins
                .get(from_port)
                .ok_or("Invalid track input port")?
                .clone(),
            Vst3GraphNode::PluginInstance(id) => {
                let instance = self
                    .vst3_plugins
                    .iter()
                    .find(|i| i.id == *id)
                    .ok_or("VST3 instance not found")?;
                instance
                    .processor
                    .lock()
                    .audio_outputs()
                    .get(from_port)
                    .ok_or("Invalid plugin output port")?
                    .clone()
            }
            Vst3GraphNode::TrackOutput => {
                return Err("Cannot disconnect from track output".to_string());
            }
        };

        let to_io = match to_node {
            Vst3GraphNode::PluginInstance(id) => {
                let instance = self
                    .vst3_plugins
                    .iter()
                    .find(|i| i.id == *id)
                    .ok_or("VST3 instance not found")?;
                instance
                    .processor
                    .lock()
                    .audio_inputs()
                    .get(to_port)
                    .ok_or("Invalid plugin input port")?
            }
            Vst3GraphNode::TrackOutput => self
                .audio
                .outs
                .get(to_port)
                .ok_or("Invalid track output port")?,
            Vst3GraphNode::TrackInput => return Err("Cannot disconnect to track input".to_string()),
        };

        to_io
            .connections
            .lock()
            .retain(|conn| !Arc::ptr_eq(conn, &from_io));
        self.invalidate_audio_route_cache();
        Ok(())
    }

    pub fn clear_default_passthrough(&mut self) {
        for (audio_in, audio_out) in self.audio.ins.iter().zip(self.audio.outs.iter()) {
            let _ = AudioIO::disconnect(audio_in, audio_out);
            let _ = AudioIO::disconnect(audio_out, audio_in);
        }
        for (midi_in, midi_out) in self.midi.ins.iter().zip(self.midi.outs.iter()) {
            let _ = MIDIIO::disconnect(midi_out, midi_in);
        }
        self.invalidate_audio_route_cache();
        self.invalidate_midi_route_cache();
    }

    fn current_buffer_size(&self) -> usize {
        self.audio
            .ins
            .first()
            .map(|io| io.buffer.lock().len())
            .or_else(|| self.audio.outs.first().map(|io| io.buffer.lock().len()))
            .unwrap_or(self.process_block_size)
    }

    pub fn set_force_realtime_domain(&mut self, forced: bool) {
        self.force_realtime_domain = forced;
    }

    pub fn set_shared_realtime_mixed(&mut self, mixed: bool) {
        self.shared_realtime_mixed = mixed;
    }

    pub fn is_realtime_domain(&self) -> bool {
        (self.armed
            && (self.input_monitor.iter().any(|&m| m)
                || self.midi_input_monitor.iter().any(|&m| m)))
            || self.force_realtime_domain
    }

    pub fn add_audio_input(&mut self) -> Result<(), String> {
        let buffer_size = self.current_buffer_size();
        if buffer_size == 0 {
            return Err(format!("Track '{}' has no audio buffer size", self.name));
        }
        let _ = self.audio.add_input(buffer_size);
        self.invalidate_audio_route_cache();
        Ok(())
    }

    pub fn add_audio_output(&mut self) -> Result<(), String> {
        let buffer_size = self.current_buffer_size();
        if buffer_size == 0 {
            return Err(format!("Track '{}' has no audio buffer size", self.name));
        }
        let _ = self.audio.add_output(buffer_size);
        self.record_tap_outs.push(vec![0.0; buffer_size]);
        self.output_meter_linear_cache.push(0.0);
        self.meter_peak_hold_linear.push(0.0);
        self.invalidate_audio_route_cache();
        Ok(())
    }

    pub fn remove_audio_input(&mut self) -> Result<(), String> {
        if self.audio.ins.len() <= self.primary_audio_ins() {
            return Err(format!(
                "Track '{}' has no removable return inputs",
                self.name
            ));
        }
        if let Some(input) = self.audio.ins.pop() {
            Self::disconnect_all(&input);
            for output in &self.audio.outs {
                let conns = output.connections.lock();
                conns.retain(|source| !Arc::ptr_eq(source, &input));
            }
            self.invalidate_audio_route_cache();
            Ok(())
        } else {
            Err(format!("Track '{}' input removal failed", self.name))
        }
    }

    pub fn remove_audio_output(
        &mut self,
        hw_outputs: &[Arc<AudioIO>],
        track_inputs: &[Arc<AudioIO>],
    ) -> Result<(), String> {
        if self.audio.outs.len() <= self.primary_audio_outs() {
            return Err(format!(
                "Track '{}' has no removable send outputs",
                self.name
            ));
        }
        let Some(output) = self.audio.outs.pop() else {
            return Err(format!("Track '{}' output removal failed", self.name));
        };
        for target in hw_outputs.iter().chain(track_inputs.iter()) {
            let _ = AudioIO::disconnect(&output, target);
        }
        self.record_tap_outs.truncate(self.audio.outs.len());
        self.output_meter_linear_cache
            .truncate(self.audio.outs.len());
        self.meter_peak_hold_linear.truncate(self.audio.outs.len());
        self.invalidate_audio_route_cache();
        Ok(())
    }

    pub fn plugin_graph_connections(&self) -> Vec<PluginGraphConnection> {
        let mut source_ports: Vec<(PluginGraphNode, usize, Arc<AudioIO>)> = self
            .audio
            .ins
            .iter()
            .enumerate()
            .map(|(idx, io)| (PluginGraphNode::TrackInput, idx, io.clone()))
            .collect();
        #[cfg(all(unix, not(target_os = "macos")))]
        for instance in &self.lv2_plugins {
            source_ports.extend(
                instance
                    .processor
                    .lock()
                    .audio_outputs()
                    .iter()
                    .enumerate()
                    .map(|(idx, io)| {
                        (
                            #[cfg(all(unix, not(target_os = "macos")))]
                            PluginGraphNode::Lv2PluginInstance(instance.id),
                            idx,
                            io.clone(),
                        )
                    }),
            );
        }
        for instance in &self.vst3_plugins {
            source_ports.extend(
                instance
                    .processor
                    .lock()
                    .audio_outputs()
                    .iter()
                    .enumerate()
                    .map(|(idx, io)| {
                        (
                            PluginGraphNode::Vst3PluginInstance(instance.id),
                            idx,
                            io.clone(),
                        )
                    }),
            );
        }
        for instance in &self.clap_plugins {
            source_ports.extend(
                instance
                    .processor
                    .lock()
                    .audio_outputs()
                    .iter()
                    .enumerate()
                    .map(|(idx, io)| {
                        (
                            PluginGraphNode::ClapPluginInstance(instance.id),
                            idx,
                            io.clone(),
                        )
                    }),
            );
        }

        let mut connections = vec![];
        for (to_port, to_io) in self.audio.outs.iter().enumerate() {
            for conn in to_io.connections.lock().iter() {
                if let Some((from_node, from_port, _)) = source_ports
                    .iter()
                    .find(|(_, _, source_io)| Arc::ptr_eq(source_io, conn))
                {
                    connections.push(PluginGraphConnection {
                        from_node: from_node.clone(),
                        from_port: *from_port,
                        to_node: PluginGraphNode::TrackOutput,
                        to_port,
                        kind: Kind::Audio,
                    });
                }
            }
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        for instance in &self.lv2_plugins {
            for (to_port, to_io) in instance.processor.lock().audio_inputs().iter().enumerate() {
                for conn in to_io.connections.lock().iter() {
                    if let Some((from_node, from_port, _)) = source_ports
                        .iter()
                        .find(|(_, _, source_io)| Arc::ptr_eq(source_io, conn))
                    {
                        connections.push(PluginGraphConnection {
                            from_node: from_node.clone(),
                            from_port: *from_port,
                            #[cfg(all(unix, not(target_os = "macos")))]
                            to_node: PluginGraphNode::Lv2PluginInstance(instance.id),
                            to_port,
                            kind: Kind::Audio,
                        });
                    }
                }
            }
        }
        for instance in &self.vst3_plugins {
            for (to_port, to_io) in instance.processor.lock().audio_inputs().iter().enumerate() {
                for conn in to_io.connections.lock().iter() {
                    if let Some((from_node, from_port, _)) = source_ports
                        .iter()
                        .find(|(_, _, source_io)| Arc::ptr_eq(source_io, conn))
                    {
                        connections.push(PluginGraphConnection {
                            from_node: from_node.clone(),
                            from_port: *from_port,
                            to_node: PluginGraphNode::Vst3PluginInstance(instance.id),
                            to_port,
                            kind: Kind::Audio,
                        });
                    }
                }
            }
        }
        for instance in &self.clap_plugins {
            for (to_port, to_io) in instance.processor.lock().audio_inputs().iter().enumerate() {
                for conn in to_io.connections.lock().iter() {
                    if let Some((from_node, from_port, _)) = source_ports
                        .iter()
                        .find(|(_, _, source_io)| Arc::ptr_eq(source_io, conn))
                    {
                        connections.push(PluginGraphConnection {
                            from_node: from_node.clone(),
                            from_port: *from_port,
                            to_node: PluginGraphNode::ClapPluginInstance(instance.id),
                            to_port,
                            kind: Kind::Audio,
                        });
                    }
                }
            }
        }
        for (from_port, from_io) in self.midi.ins.iter().enumerate() {
            for conn in from_io.lock().connections.iter() {
                if let Some((to_port, _)) = self
                    .midi
                    .outs
                    .iter()
                    .enumerate()
                    .find(|(_, out_io)| Arc::ptr_eq(out_io, conn))
                {
                    connections.push(PluginGraphConnection {
                        from_node: PluginGraphNode::TrackInput,
                        from_port,
                        to_node: PluginGraphNode::TrackOutput,
                        to_port,
                        kind: Kind::MIDI,
                    });
                }
            }
        }
        connections.extend(self.plugin_midi_connections.iter().cloned());
        connections
    }

    pub fn connectable_connections(&self) -> Vec<ConnectableConnection> {
        use crate::connectable::{AudioPorts, MidiPorts};
        let mut connections = Vec::new();

        // --- Audio ---
        let mut audio_sources: Vec<(Arc<AudioIO>, ConnectableRef, usize)> = Vec::new();
        for (port, io) in self.audio.ins.iter().enumerate() {
            audio_sources.push((io.clone(), ConnectableRef::TrackInput, port));
        }
        for (port, io) in self.audio.outs.iter().enumerate() {
            audio_sources.push((io.clone(), ConnectableRef::TrackOutput, port));
        }
        for child in &self.child_tracks {
            let child = child.lock();
            let name = child.name.clone();
            for (port, io) in child.audio_outputs().iter().enumerate() {
                audio_sources.push((io.clone(), ConnectableRef::ChildTrack(name.clone()), port));
            }
        }
        for instance in &self.clap_plugins {
            for (port, io) in instance.audio_outputs().iter().enumerate() {
                audio_sources.push((io.clone(), ConnectableRef::ClapPlugin(instance.id), port));
            }
        }
        for instance in &self.vst3_plugins {
            for (port, io) in instance.audio_outputs().iter().enumerate() {
                audio_sources.push((io.clone(), ConnectableRef::Vst3Plugin(instance.id), port));
            }
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        for instance in &self.lv2_plugins {
            for (port, io) in instance.audio_outputs().iter().enumerate() {
                audio_sources.push((io.clone(), ConnectableRef::Lv2Plugin(instance.id), port));
            }
        }

        let find_audio_source = |io: &Arc<AudioIO>| {
            audio_sources
                .iter()
                .find(|(candidate, _, _)| Arc::ptr_eq(candidate, io))
                .map(|(_, r, p)| (r.clone(), *p))
        };

        let mut report_audio_targets = |targets: Vec<Arc<AudioIO>>, target_ref: ConnectableRef| {
            for (port, target) in targets.iter().enumerate() {
                let source_list = target.connections.lock().clone();
                for source in source_list {
                    if let Some((from_ref, from_port)) = find_audio_source(&source) {
                        connections.push(ConnectableConnection {
                            from: from_ref,
                            from_port,
                            to: target_ref.clone(),
                            to_port: port,
                            kind: Kind::Audio,
                        });
                    }
                }
            }
        };

        report_audio_targets(self.audio_outputs(), ConnectableRef::TrackOutput);
        for child in &self.child_tracks {
            let child = child.lock();
            let name = child.name.clone();
            report_audio_targets(child.audio_inputs(), ConnectableRef::ChildTrack(name));
        }
        for instance in &self.clap_plugins {
            report_audio_targets(
                instance.audio_inputs(),
                ConnectableRef::ClapPlugin(instance.id),
            );
        }
        for instance in &self.vst3_plugins {
            report_audio_targets(
                instance.audio_inputs(),
                ConnectableRef::Vst3Plugin(instance.id),
            );
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        for instance in &self.lv2_plugins {
            report_audio_targets(
                instance.audio_inputs(),
                ConnectableRef::Lv2Plugin(instance.id),
            );
        }

        // --- MIDI ---
        type MidiSource = (Arc<UnsafeMutex<Box<MIDIIO>>>, ConnectableRef, usize);
        let mut midi_sources: Vec<MidiSource> = Vec::new();
        for (port, io) in self.midi.ins.iter().enumerate() {
            midi_sources.push((io.clone(), ConnectableRef::TrackInput, port));
        }
        for (port, io) in self.midi.outs.iter().enumerate() {
            midi_sources.push((io.clone(), ConnectableRef::TrackOutput, port));
        }
        for child in &self.child_tracks {
            let child = child.lock();
            let name = child.name.clone();
            for (port, io) in child.midi_outputs().iter().enumerate() {
                midi_sources.push((io.clone(), ConnectableRef::ChildTrack(name.clone()), port));
            }
        }
        for instance in &self.clap_plugins {
            for (port, io) in instance.midi_outputs().iter().enumerate() {
                midi_sources.push((io.clone(), ConnectableRef::ClapPlugin(instance.id), port));
            }
        }
        for instance in &self.vst3_plugins {
            for (port, io) in instance.midi_outputs().iter().enumerate() {
                midi_sources.push((io.clone(), ConnectableRef::Vst3Plugin(instance.id), port));
            }
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        for instance in &self.lv2_plugins {
            for (port, io) in instance.midi_outputs().iter().enumerate() {
                midi_sources.push((io.clone(), ConnectableRef::Lv2Plugin(instance.id), port));
            }
        }

        let find_midi_source = |io: &Arc<UnsafeMutex<Box<MIDIIO>>>| {
            midi_sources
                .iter()
                .find(|(candidate, _, _)| Arc::ptr_eq(candidate, io))
                .map(|(_, r, p)| (r.clone(), *p))
        };

        let mut report_midi_targets =
            |targets: Vec<Arc<UnsafeMutex<Box<MIDIIO>>>>, target_ref: ConnectableRef| {
                for (port, target) in targets.iter().enumerate() {
                    let source_list = target.lock().sources.clone();
                    for source in source_list {
                        if let Some((from_ref, from_port)) = find_midi_source(&source) {
                            connections.push(ConnectableConnection {
                                from: from_ref,
                                from_port,
                                to: target_ref.clone(),
                                to_port: port,
                                kind: Kind::MIDI,
                            });
                        }
                    }
                }
            };

        report_midi_targets(self.midi_outputs(), ConnectableRef::TrackOutput);
        for child in &self.child_tracks {
            let child = child.lock();
            let name = child.name.clone();
            report_midi_targets(child.midi_inputs(), ConnectableRef::ChildTrack(name));
        }
        for instance in &self.clap_plugins {
            report_midi_targets(
                instance.midi_inputs(),
                ConnectableRef::ClapPlugin(instance.id),
            );
        }
        for instance in &self.vst3_plugins {
            report_midi_targets(
                instance.midi_inputs(),
                ConnectableRef::Vst3Plugin(instance.id),
            );
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        for instance in &self.lv2_plugins {
            report_midi_targets(
                instance.midi_inputs(),
                ConnectableRef::Lv2Plugin(instance.id),
            );
        }

        connections
    }

    fn plugin_process_order(&self) -> Vec<(PluginKind, usize)> {
        let mut entries: Vec<(PluginGraphNode, PluginKind, usize)> = Vec::new();
        for (idx, instance) in self.clap_plugins.iter().enumerate() {
            entries.push((
                PluginGraphNode::ClapPluginInstance(instance.id),
                PluginKind::Clap,
                idx,
            ));
        }
        for (idx, instance) in self.vst3_plugins.iter().enumerate() {
            entries.push((
                PluginGraphNode::Vst3PluginInstance(instance.id),
                PluginKind::Vst3,
                idx,
            ));
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        for (idx, instance) in self.lv2_plugins.iter().enumerate() {
            entries.push((
                PluginGraphNode::Lv2PluginInstance(instance.id),
                PluginKind::Lv2,
                idx,
            ));
        }

        let node_to_index: HashMap<PluginGraphNode, usize> = entries
            .iter()
            .enumerate()
            .map(|(idx, (node, _, _))| (node.clone(), idx))
            .collect();
        let count = entries.len();
        let mut adjacency: Vec<Vec<usize>> = vec![Vec::new(); count];
        let mut in_degree = vec![0usize; count];
        for conn in self.plugin_graph_connections() {
            if let Some(&from_idx) = node_to_index.get(&conn.from_node)
                && let Some(&to_idx) = node_to_index.get(&conn.to_node)
            {
                adjacency[from_idx].push(to_idx);
                in_degree[to_idx] += 1;
            }
        }

        let mut queue: VecDeque<usize> = in_degree
            .iter()
            .enumerate()
            .filter(|(_, d)| **d == 0)
            .map(|(idx, _)| idx)
            .collect();
        let mut order = Vec::with_capacity(count);
        while let Some(idx) = queue.pop_front() {
            order.push((entries[idx].1, entries[idx].2));
            for &next in &adjacency[idx] {
                in_degree[next] = in_degree[next].saturating_sub(1);
                if in_degree[next] == 0 {
                    queue.push_back(next);
                }
            }
        }

        if order.len() < count {
            // Cycle or disconnected graph: fall back to type ordering so every
            // plugin still gets a chance to run.
            order.clear();
            for (idx, _) in self.clap_plugins.iter().enumerate() {
                order.push((PluginKind::Clap, idx));
            }
            for (idx, _) in self.vst3_plugins.iter().enumerate() {
                order.push((PluginKind::Vst3, idx));
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            for (idx, _) in self.lv2_plugins.iter().enumerate() {
                order.push((PluginKind::Lv2, idx));
            }
        }
        order
    }

    pub fn connect_plugin_audio(
        &mut self,
        from_node: PluginGraphNode,
        from_port: usize,
        to_node: PluginGraphNode,
        to_port: usize,
    ) -> Result<(), String> {
        let source = self.plugin_source_io(&from_node, from_port)?;
        let target = self.plugin_target_io(&to_node, to_port)?;
        if routing::would_create_cycle(&from_node, &to_node, |node| {
            self.plugin_connected_neighbors(Kind::Audio, node)
        }) {
            return Err("Circular routing is not allowed!".to_string());
        }
        if matches!(from_node, PluginGraphNode::TrackInput) {
            Self::connect_directed_audio(&source, &target);
        } else {
            AudioIO::connect(&source, &target);
        }
        self.invalidate_audio_route_cache();
        Ok(())
    }

    pub fn disconnect_plugin_audio(
        &mut self,
        from_node: PluginGraphNode,
        from_port: usize,
        to_node: PluginGraphNode,
        to_port: usize,
    ) -> Result<(), String> {
        let source = self.plugin_source_io(&from_node, from_port)?;
        let target = self.plugin_target_io(&to_node, to_port)?;
        AudioIO::disconnect(&source, &target)?;
        self.invalidate_audio_route_cache();
        Ok(())
    }

    pub fn connect_plugin_midi(
        &mut self,
        from_node: PluginGraphNode,
        from_port: usize,
        to_node: PluginGraphNode,
        to_port: usize,
    ) -> Result<(), String> {
        self.validate_plugin_midi_source(&from_node, from_port)?;
        self.validate_plugin_midi_target(&to_node, to_port)?;
        if from_node == to_node && from_port == to_port {
            return Err("Cannot connect a MIDI port to itself".to_string());
        }
        if routing::would_create_cycle(&from_node, &to_node, |node| {
            self.plugin_connected_neighbors(Kind::MIDI, node)
        }) {
            return Err("Circular routing is not allowed!".to_string());
        }

        let source = self.plugin_midi_source_io(&from_node, from_port)?;
        let target = self.plugin_midi_target_io(&to_node, to_port)?;
        MIDIIO::connect(&source, &target);

        if !(matches!(from_node, PluginGraphNode::TrackInput)
            && matches!(to_node, PluginGraphNode::TrackOutput))
        {
            let new_conn = PluginGraphConnection {
                from_node,
                from_port,
                to_node,
                to_port,
                kind: Kind::MIDI,
            };
            if !self.plugin_midi_connections.iter().any(|c| c == &new_conn) {
                self.plugin_midi_connections.push(new_conn);
            }
        }

        self.invalidate_midi_route_cache();
        Ok(())
    }

    pub fn disconnect_plugin_midi(
        &mut self,
        from_node: PluginGraphNode,
        from_port: usize,
        to_node: PluginGraphNode,
        to_port: usize,
    ) -> Result<(), String> {
        let source = self.plugin_midi_source_io(&from_node, from_port)?;
        let target = self.plugin_midi_target_io(&to_node, to_port)?;
        MIDIIO::disconnect(&source, &target)?;

        if !(matches!(from_node, PluginGraphNode::TrackInput)
            && matches!(to_node, PluginGraphNode::TrackOutput))
        {
            let before = self.plugin_midi_connections.len();
            self.plugin_midi_connections.retain(|c| {
                !(c.kind == Kind::MIDI
                    && c.from_node == from_node
                    && c.from_port == from_port
                    && c.to_node == to_node
                    && c.to_port == to_port)
            });
            if self.plugin_midi_connections.len() == before {
                return Err("MIDI plugin graph connection not found".to_string());
            }
        }

        self.invalidate_midi_route_cache();
        Ok(())
    }

    fn connectable_audio_output(
        &self,
        connectable: &ConnectableRef,
        port: usize,
    ) -> Result<Arc<AudioIO>, String> {
        use crate::connectable::AudioPorts;
        match connectable {
            ConnectableRef::TrackInput => {
                Err("Track input cannot be used as an audio source".to_string())
            }
            ConnectableRef::TrackOutput => self
                .audio_outputs()
                .get(port)
                .cloned()
                .ok_or_else(|| format!("Track output audio port {port} not found")),
            ConnectableRef::ChildTrack(name) => self
                .child_tracks
                .iter()
                .find(|child| child.lock().name == *name)
                .and_then(|child| child.lock().audio_outputs().get(port).cloned())
                .ok_or_else(|| format!("Child track '{name}' audio output port {port} not found")),
            ConnectableRef::ClapPlugin(instance_id) => self
                .clap_plugins
                .iter()
                .find(|instance| instance.id == *instance_id)
                .and_then(|instance| instance.audio_outputs().get(port).cloned())
                .ok_or_else(|| format!("CLAP plugin audio output port {port} not found")),
            ConnectableRef::Vst3Plugin(instance_id) => self
                .vst3_plugins
                .iter()
                .find(|instance| instance.id == *instance_id)
                .and_then(|instance| instance.audio_outputs().get(port).cloned())
                .ok_or_else(|| format!("VST3 plugin audio output port {port} not found")),
            #[cfg(all(unix, not(target_os = "macos")))]
            ConnectableRef::Lv2Plugin(instance_id) => self
                .lv2_plugins
                .iter()
                .find(|instance| instance.id == *instance_id)
                .and_then(|instance| instance.audio_outputs().get(port).cloned())
                .ok_or_else(|| format!("LV2 plugin audio output port {port} not found")),
        }
    }

    fn connectable_audio_input(
        &self,
        connectable: &ConnectableRef,
        port: usize,
    ) -> Result<Arc<AudioIO>, String> {
        use crate::connectable::AudioPorts;
        match connectable {
            ConnectableRef::TrackOutput => self
                .audio_outputs()
                .get(port)
                .cloned()
                .ok_or_else(|| format!("Track output audio port {port} not found")),
            ConnectableRef::TrackInput => self
                .audio_inputs()
                .get(port)
                .cloned()
                .ok_or_else(|| format!("Track input audio port {port} not found")),
            ConnectableRef::ChildTrack(name) => self
                .child_tracks
                .iter()
                .find(|child| child.lock().name == *name)
                .and_then(|child| child.lock().audio_inputs().get(port).cloned())
                .ok_or_else(|| format!("Child track '{name}' audio input port {port} not found")),
            ConnectableRef::ClapPlugin(instance_id) => self
                .clap_plugins
                .iter()
                .find(|instance| instance.id == *instance_id)
                .and_then(|instance| instance.audio_inputs().get(port).cloned())
                .ok_or_else(|| format!("CLAP plugin audio input port {port} not found")),
            ConnectableRef::Vst3Plugin(instance_id) => self
                .vst3_plugins
                .iter()
                .find(|instance| instance.id == *instance_id)
                .and_then(|instance| instance.audio_inputs().get(port).cloned())
                .ok_or_else(|| format!("VST3 plugin audio input port {port} not found")),
            #[cfg(all(unix, not(target_os = "macos")))]
            ConnectableRef::Lv2Plugin(instance_id) => self
                .lv2_plugins
                .iter()
                .find(|instance| instance.id == *instance_id)
                .and_then(|instance| instance.audio_inputs().get(port).cloned())
                .ok_or_else(|| format!("LV2 plugin audio input port {port} not found")),
        }
    }

    fn connectable_midi_output(
        &self,
        connectable: &ConnectableRef,
        port: usize,
    ) -> Result<Arc<UnsafeMutex<Box<MIDIIO>>>, String> {
        use crate::connectable::MidiPorts;
        match connectable {
            ConnectableRef::TrackInput => {
                Err("Track input cannot be used as a MIDI source".to_string())
            }
            ConnectableRef::TrackOutput => self
                .midi_outputs()
                .get(port)
                .cloned()
                .ok_or_else(|| format!("Track output MIDI port {port} not found")),
            ConnectableRef::ChildTrack(name) => self
                .child_tracks
                .iter()
                .find(|child| child.lock().name == *name)
                .and_then(|child| child.lock().midi_outputs().get(port).cloned())
                .ok_or_else(|| format!("Child track '{name}' MIDI output port {port} not found")),
            ConnectableRef::ClapPlugin(instance_id) => self
                .clap_plugins
                .iter()
                .find(|instance| instance.id == *instance_id)
                .and_then(|instance| instance.midi_outputs().get(port).cloned())
                .ok_or_else(|| format!("CLAP plugin MIDI output port {port} not found")),
            ConnectableRef::Vst3Plugin(instance_id) => self
                .vst3_plugins
                .iter()
                .find(|instance| instance.id == *instance_id)
                .and_then(|instance| instance.midi_outputs().get(port).cloned())
                .ok_or_else(|| format!("VST3 plugin MIDI output port {port} not found")),
            #[cfg(all(unix, not(target_os = "macos")))]
            ConnectableRef::Lv2Plugin(instance_id) => self
                .lv2_plugins
                .iter()
                .find(|instance| instance.id == *instance_id)
                .and_then(|instance| instance.midi_outputs().get(port).cloned())
                .ok_or_else(|| format!("LV2 plugin MIDI output port {port} not found")),
        }
    }

    fn connectable_midi_input(
        &self,
        connectable: &ConnectableRef,
        port: usize,
    ) -> Result<Arc<UnsafeMutex<Box<MIDIIO>>>, String> {
        use crate::connectable::MidiPorts;
        match connectable {
            ConnectableRef::TrackOutput => self
                .midi_outputs()
                .get(port)
                .cloned()
                .ok_or_else(|| format!("Track output MIDI port {port} not found")),
            ConnectableRef::TrackInput => self
                .midi_inputs()
                .get(port)
                .cloned()
                .ok_or_else(|| format!("Track input MIDI port {port} not found")),
            ConnectableRef::ChildTrack(name) => self
                .child_tracks
                .iter()
                .find(|child| child.lock().name == *name)
                .and_then(|child| child.lock().midi_inputs().get(port).cloned())
                .ok_or_else(|| format!("Child track '{name}' MIDI input port {port} not found")),
            ConnectableRef::ClapPlugin(instance_id) => self
                .clap_plugins
                .iter()
                .find(|instance| instance.id == *instance_id)
                .and_then(|instance| instance.midi_inputs().get(port).cloned())
                .ok_or_else(|| format!("CLAP plugin MIDI input port {port} not found")),
            ConnectableRef::Vst3Plugin(instance_id) => self
                .vst3_plugins
                .iter()
                .find(|instance| instance.id == *instance_id)
                .and_then(|instance| instance.midi_inputs().get(port).cloned())
                .ok_or_else(|| format!("VST3 plugin MIDI input port {port} not found")),
            #[cfg(all(unix, not(target_os = "macos")))]
            ConnectableRef::Lv2Plugin(instance_id) => self
                .lv2_plugins
                .iter()
                .find(|instance| instance.id == *instance_id)
                .and_then(|instance| instance.midi_inputs().get(port).cloned())
                .ok_or_else(|| format!("LV2 plugin MIDI input port {port} not found")),
        }
    }

    pub fn connect_audio_connectable(
        &mut self,
        from: ConnectableRef,
        from_port: usize,
        to: ConnectableRef,
        to_port: usize,
    ) -> Result<(), String> {
        let source = self.connectable_audio_output(&from, from_port)?;
        let target = self.connectable_audio_input(&to, to_port)?;
        if from == to && from_port == to_port {
            return Err("Cannot connect an audio port to itself".to_string());
        }
        AudioIO::connect(&source, &target);
        self.invalidate_audio_route_cache();
        Ok(())
    }

    pub fn disconnect_audio_connectable(
        &mut self,
        from: ConnectableRef,
        from_port: usize,
        to: ConnectableRef,
        to_port: usize,
    ) -> Result<(), String> {
        let source = self.connectable_audio_output(&from, from_port)?;
        let target = self.connectable_audio_input(&to, to_port)?;
        AudioIO::disconnect(&source, &target)?;
        self.invalidate_audio_route_cache();
        Ok(())
    }

    pub fn connect_midi_connectable(
        &mut self,
        from: ConnectableRef,
        from_port: usize,
        to: ConnectableRef,
        to_port: usize,
    ) -> Result<(), String> {
        let source = self.connectable_midi_output(&from, from_port)?;
        let target = self.connectable_midi_input(&to, to_port)?;
        if from == to && from_port == to_port {
            return Err("Cannot connect a MIDI port to itself".to_string());
        }
        MIDIIO::connect(&source, &target);
        self.invalidate_midi_route_cache();
        Ok(())
    }

    pub fn disconnect_midi_connectable(
        &mut self,
        from: ConnectableRef,
        from_port: usize,
        to: ConnectableRef,
        to_port: usize,
    ) -> Result<(), String> {
        let source = self.connectable_midi_output(&from, from_port)?;
        let target = self.connectable_midi_input(&to, to_port)?;
        MIDIIO::disconnect(&source, &target)?;
        self.invalidate_midi_route_cache();
        Ok(())
    }

    fn with_default_passthrough(mut self) -> Self {
        self.ensure_default_audio_passthrough();
        self.ensure_default_midi_passthrough();
        self
    }

    pub fn ensure_default_audio_passthrough(&mut self) {
        if self.is_folder {
            self.disconnect_audio_inputs_from_outputs();
            return;
        }
        if self.audio.ins.is_empty() {
            self.invalidate_audio_route_cache();
            return;
        }

        for audio_in in &self.audio.ins {
            audio_in
                .connections
                .lock()
                .retain(|conn| !self.audio.outs.iter().any(|out| Arc::ptr_eq(out, conn)));
        }

        for (out_idx, audio_out) in self.audio.outs.iter().enumerate() {
            let source_idx = out_idx.min(self.audio.ins.len().saturating_sub(1));
            let audio_in = &self.audio.ins[source_idx];
            let conns = audio_out.connections.lock();
            conns.retain(|conn| !self.audio.ins.iter().any(|input| Arc::ptr_eq(input, conn)));
            if !conns.iter().any(|conn| Arc::ptr_eq(conn, audio_in)) {
                conns.push(audio_in.clone());
            }
        }
        self.invalidate_audio_route_cache();
    }

    fn disconnect_audio_inputs_from_outputs(&mut self) {
        for audio_in in &self.audio.ins {
            audio_in
                .connections
                .lock()
                .retain(|conn| !self.audio.outs.iter().any(|out| Arc::ptr_eq(out, conn)));
        }
        for audio_out in &self.audio.outs {
            audio_out
                .connections
                .lock()
                .retain(|conn| !self.audio.ins.iter().any(|input| Arc::ptr_eq(input, conn)));
        }
        self.invalidate_audio_route_cache();
    }

    pub fn ensure_default_midi_passthrough(&mut self) {
        if self.is_folder {
            self.disconnect_midi_inputs_from_outputs();
            return;
        }
        let count = self.midi.ins.len().min(self.midi.outs.len());
        for port in 0..count {
            let _ = self.connect_plugin_midi(
                PluginGraphNode::TrackInput,
                port,
                PluginGraphNode::TrackOutput,
                port,
            );
        }
    }

    fn disconnect_midi_inputs_from_outputs(&mut self) {
        let count = self.midi.ins.len().min(self.midi.outs.len());
        for port in 0..count {
            let _ = self.disconnect_plugin_midi(
                PluginGraphNode::TrackInput,
                port,
                PluginGraphNode::TrackOutput,
                port,
            );
        }
    }

    pub fn connect_outputs_to_parent(&mut self, parent: &Track) {
        for (out_idx, child_out) in self.audio.outs.iter().enumerate() {
            if let Some(parent_in) = parent.audio.ins.get(out_idx) {
                let already_connected = child_out
                    .connections
                    .lock()
                    .iter()
                    .any(|conn| Arc::ptr_eq(conn, parent_in));
                if !already_connected {
                    AudioIO::connect(child_out, parent_in);
                }
            }
        }
        self.invalidate_audio_route_cache();
    }

    pub fn disconnect_from_parent(&mut self, parent: &Track) {
        // Folder input -> child input
        for (in_idx, child_in) in self.audio.ins.iter().enumerate() {
            if let Some(parent_in) = parent.audio.ins.get(in_idx) {
                let _ = AudioIO::disconnect(parent_in, child_in);
            }
        }
        // Child output -> folder output
        for (out_idx, child_out) in self.audio.outs.iter().enumerate() {
            if let Some(parent_out) = parent.audio.outs.get(out_idx) {
                let _ = AudioIO::disconnect(child_out, parent_out);
            }
        }
        // Folder MIDI input -> child MIDI input
        for (in_idx, child_in) in self.midi.ins.iter().enumerate() {
            if let Some(parent_in) = parent.midi.ins.get(in_idx) {
                let _ = MIDIIO::disconnect(parent_in, child_in);
            }
        }
        // Child MIDI output -> folder MIDI output
        for (out_idx, child_out) in self.midi.outs.iter().enumerate() {
            if let Some(parent_out) = parent.midi.outs.get(out_idx) {
                let _ = MIDIIO::disconnect(child_out, parent_out);
            }
        }
        self.invalidate_audio_route_cache();
        self.invalidate_midi_route_cache();
    }

    fn internal_audio_sources(&self) -> Vec<Arc<AudioIO>> {
        // Folder tracks aggregate their children; their own inputs must not feed their outputs.
        let mut sources = if self.is_folder {
            Vec::new()
        } else {
            self.audio.ins.clone()
        };
        if let Some(src) = &self.metronome_source {
            sources.push(src.clone());
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        for instance in &self.lv2_plugins {
            sources.extend(instance.processor.lock().audio_outputs().iter().cloned());
        }
        for instance in &self.vst3_plugins {
            sources.extend(instance.processor.lock().audio_outputs().iter().cloned());
        }
        for instance in &self.clap_plugins {
            sources.extend(instance.processor.lock().audio_outputs().iter().cloned());
        }
        for child in &self.child_tracks {
            let child = child.lock();
            sources.extend(child.audio.outs.iter().cloned());
        }
        sources
    }

    fn is_track_input_source(&self, source: &Arc<AudioIO>) -> bool {
        self.audio
            .ins
            .iter()
            .any(|input| Arc::ptr_eq(input, source))
    }

    fn disconnect_all(port: &Arc<AudioIO>) {
        let connections = port.connections.lock().clone();
        for other in connections {
            let _ = AudioIO::disconnect(&other, port);
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    fn lv2_audio_output_io(
        &self,
        instance_id: usize,
        _port: usize,
    ) -> Result<Arc<AudioIO>, String> {
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            self.lv2_plugins
                .iter()
                .find(|instance| instance.id == instance_id)
                .and_then(|instance| {
                    instance
                        .processor
                        .lock()
                        .audio_outputs()
                        .get(_port)
                        .cloned()
                })
                .ok_or_else(|| format!("Plugin instance {instance_id} output port {_port} missing"))
        }
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        {
            Err("LV2 plugins are not supported on this platform".to_string())
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    fn lv2_audio_input_io(&self, instance_id: usize, _port: usize) -> Result<Arc<AudioIO>, String> {
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            self.lv2_plugins
                .iter()
                .find(|instance| instance.id == instance_id)
                .and_then(|instance| instance.processor.lock().audio_inputs().get(_port).cloned())
                .ok_or_else(|| format!("Plugin instance {instance_id} input port {_port} missing"))
        }
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        {
            Err("LV2 plugins are not supported on this platform".to_string())
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    fn lv2_validate_midi_output(&self, instance_id: usize, _port: usize) -> Result<(), String> {
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            self.lv2_plugins
                .iter()
                .find(|instance| instance.id == instance_id)
                .and_then(|instance| {
                    (_port < instance.processor.lock().midi_output_count()).then_some(())
                })
                .ok_or_else(|| {
                    format!("Plugin instance {instance_id} MIDI output port {_port} missing")
                })
        }
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        {
            Err("LV2 plugins are not supported on this platform".to_string())
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    fn lv2_validate_midi_input(&self, instance_id: usize, _port: usize) -> Result<(), String> {
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            self.lv2_plugins
                .iter()
                .find(|instance| instance.id == instance_id)
                .and_then(|instance| {
                    (_port < instance.processor.lock().midi_input_count()).then_some(())
                })
                .ok_or_else(|| {
                    format!("Plugin instance {instance_id} MIDI input port {_port} missing")
                })
        }
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        {
            Err("LV2 plugins are not supported on this platform".to_string())
        }
    }

    fn vst3_audio_output_io(
        &self,
        instance_id: usize,
        port: usize,
    ) -> Result<Arc<AudioIO>, String> {
        self.vst3_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .and_then(|instance| instance.processor.lock().audio_outputs().get(port).cloned())
            .ok_or_else(|| format!("VST3 instance {instance_id} output port {port} missing"))
    }

    fn vst3_audio_input_io(&self, instance_id: usize, port: usize) -> Result<Arc<AudioIO>, String> {
        self.vst3_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .and_then(|instance| instance.processor.lock().audio_inputs().get(port).cloned())
            .ok_or_else(|| format!("VST3 instance {instance_id} input port {port} missing"))
    }

    fn clap_audio_output_io(
        &self,
        instance_id: usize,
        port: usize,
    ) -> Result<Arc<AudioIO>, String> {
        self.clap_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .and_then(|instance| instance.processor.lock().audio_outputs().get(port).cloned())
            .ok_or_else(|| format!("CLAP instance {instance_id} output port {port} missing"))
    }

    fn clap_audio_input_io(&self, instance_id: usize, port: usize) -> Result<Arc<AudioIO>, String> {
        self.clap_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .and_then(|instance| instance.processor.lock().audio_inputs().get(port).cloned())
            .ok_or_else(|| format!("CLAP instance {instance_id} input port {port} missing"))
    }

    fn vst3_validate_midi_output(&self, instance_id: usize, port: usize) -> Result<(), String> {
        self.vst3_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .and_then(|instance| {
                (port < instance.processor.lock().midi_output_count()).then_some(())
            })
            .ok_or_else(|| format!("VST3 instance {instance_id} MIDI output port {port} missing"))
    }

    fn clap_validate_midi_output(&self, instance_id: usize, port: usize) -> Result<(), String> {
        self.clap_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .and_then(|instance| {
                (port < instance.processor.lock().midi_output_count()).then_some(())
            })
            .ok_or_else(|| format!("CLAP instance {instance_id} MIDI output port {port} missing"))
    }

    fn vst3_validate_midi_input(&self, instance_id: usize, port: usize) -> Result<(), String> {
        self.vst3_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .and_then(|instance| {
                (port < instance.processor.lock().midi_input_count()).then_some(())
            })
            .ok_or_else(|| format!("VST3 instance {instance_id} MIDI input port {port} missing"))
    }

    fn clap_validate_midi_input(&self, instance_id: usize, port: usize) -> Result<(), String> {
        self.clap_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .and_then(|instance| {
                (port < instance.processor.lock().midi_input_count()).then_some(())
            })
            .ok_or_else(|| format!("CLAP instance {instance_id} MIDI input port {port} missing"))
    }

    fn clap_midi_output_io(
        &self,
        instance_id: usize,
        port: usize,
    ) -> Result<Arc<UnsafeMutex<Box<MIDIIO>>>, String> {
        self.clap_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .and_then(|instance| {
                instance
                    .processor
                    .lock()
                    .midi_output_ports()
                    .get(port)
                    .cloned()
            })
            .ok_or_else(|| format!("CLAP instance {instance_id} MIDI output port {port} missing"))
    }

    fn clap_midi_input_io(
        &self,
        instance_id: usize,
        port: usize,
    ) -> Result<Arc<UnsafeMutex<Box<MIDIIO>>>, String> {
        self.clap_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .and_then(|instance| {
                instance
                    .processor
                    .lock()
                    .midi_input_ports()
                    .get(port)
                    .cloned()
            })
            .ok_or_else(|| format!("CLAP instance {instance_id} MIDI input port {port} missing"))
    }

    fn vst3_midi_output_io(
        &self,
        _instance_id: usize,
        _port: usize,
    ) -> Result<Arc<UnsafeMutex<Box<MIDIIO>>>, String> {
        Err("VST3 MIDI output ports not yet implemented".to_string())
    }

    fn vst3_midi_input_io(
        &self,
        _instance_id: usize,
        _port: usize,
    ) -> Result<Arc<UnsafeMutex<Box<MIDIIO>>>, String> {
        Err("VST3 MIDI input ports not yet implemented".to_string())
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    fn lv2_midi_output_io(
        &self,
        _instance_id: usize,
        _port: usize,
    ) -> Result<Arc<UnsafeMutex<Box<MIDIIO>>>, String> {
        Err("LV2 MIDI output ports not yet implemented".to_string())
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    fn lv2_midi_input_io(
        &self,
        _instance_id: usize,
        _port: usize,
    ) -> Result<Arc<UnsafeMutex<Box<MIDIIO>>>, String> {
        Err("LV2 MIDI input ports not yet implemented".to_string())
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    fn lv2_instance_id_exists(&self, id: usize) -> bool {
        self.lv2_plugins.iter().any(|i| i.id == id)
    }

    #[cfg(not(all(unix, not(target_os = "macos"))))]
    fn lv2_instance_id_exists(&self, _id: usize) -> bool {
        false
    }

    pub fn plugin_source_io(
        &self,
        node: &PluginGraphNode,
        port: usize,
    ) -> Result<Arc<AudioIO>, String> {
        match node {
            PluginGraphNode::TrackInput => self
                .audio
                .ins
                .get(port)
                .cloned()
                .ok_or_else(|| format!("Track input port {port} not found")),
            PluginGraphNode::TrackOutput => Err("Track output node cannot be source".to_string()),
            PluginGraphNode::ClapPluginInstance(instance_id) => {
                self.clap_audio_output_io(*instance_id, port)
            }
            PluginGraphNode::Vst3PluginInstance(instance_id) => {
                self.vst3_audio_output_io(*instance_id, port)
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            PluginGraphNode::Lv2PluginInstance(instance_id) => {
                self.lv2_audio_output_io(*instance_id, port)
            }
        }
    }

    pub fn plugin_target_io(
        &self,
        node: &PluginGraphNode,
        port: usize,
    ) -> Result<Arc<AudioIO>, String> {
        match node {
            PluginGraphNode::TrackInput => Err("Track input node cannot be target".to_string()),
            PluginGraphNode::TrackOutput => self
                .audio
                .outs
                .get(port)
                .cloned()
                .ok_or_else(|| format!("Track output port {port} not found")),
            PluginGraphNode::ClapPluginInstance(instance_id) => {
                self.clap_audio_input_io(*instance_id, port)
            }
            PluginGraphNode::Vst3PluginInstance(instance_id) => {
                self.vst3_audio_input_io(*instance_id, port)
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            PluginGraphNode::Lv2PluginInstance(instance_id) => {
                self.lv2_audio_input_io(*instance_id, port)
            }
        }
    }

    pub fn plugin_midi_source_io(
        &self,
        node: &PluginGraphNode,
        port: usize,
    ) -> Result<Arc<UnsafeMutex<Box<MIDIIO>>>, String> {
        match node {
            PluginGraphNode::TrackInput => self
                .midi
                .ins
                .get(port)
                .cloned()
                .ok_or_else(|| format!("Track MIDI input port {port} not found")),
            PluginGraphNode::TrackOutput => {
                Err("Track output node cannot be MIDI source".to_string())
            }
            PluginGraphNode::ClapPluginInstance(instance_id) => {
                self.clap_midi_output_io(*instance_id, port)
            }
            PluginGraphNode::Vst3PluginInstance(instance_id) => {
                self.vst3_midi_output_io(*instance_id, port)
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            PluginGraphNode::Lv2PluginInstance(instance_id) => {
                self.lv2_midi_output_io(*instance_id, port)
            }
        }
    }

    pub fn plugin_midi_target_io(
        &self,
        node: &PluginGraphNode,
        port: usize,
    ) -> Result<Arc<UnsafeMutex<Box<MIDIIO>>>, String> {
        match node {
            PluginGraphNode::TrackInput => {
                Err("Track input node cannot be MIDI target".to_string())
            }
            PluginGraphNode::TrackOutput => self
                .midi
                .outs
                .get(port)
                .cloned()
                .ok_or_else(|| format!("Track MIDI output port {port} not found")),
            PluginGraphNode::ClapPluginInstance(instance_id) => {
                self.clap_midi_input_io(*instance_id, port)
            }
            PluginGraphNode::Vst3PluginInstance(instance_id) => {
                self.vst3_midi_input_io(*instance_id, port)
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            PluginGraphNode::Lv2PluginInstance(instance_id) => {
                self.lv2_midi_input_io(*instance_id, port)
            }
        }
    }

    fn validate_plugin_midi_source(
        &self,
        node: &PluginGraphNode,
        port: usize,
    ) -> Result<(), String> {
        match node {
            PluginGraphNode::TrackInput => self
                .midi
                .ins
                .get(port)
                .map(|_| ())
                .ok_or_else(|| format!("Track MIDI input port {port} not found")),
            PluginGraphNode::TrackOutput => {
                Err("Track output node cannot be MIDI source".to_string())
            }
            PluginGraphNode::ClapPluginInstance(instance_id) => {
                self.clap_validate_midi_output(*instance_id, port)
            }
            PluginGraphNode::Vst3PluginInstance(instance_id) => {
                self.vst3_validate_midi_output(*instance_id, port)
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            PluginGraphNode::Lv2PluginInstance(instance_id) => {
                self.lv2_validate_midi_output(*instance_id, port)
            }
        }
    }

    fn validate_plugin_midi_target(
        &self,
        node: &PluginGraphNode,
        port: usize,
    ) -> Result<(), String> {
        match node {
            PluginGraphNode::TrackInput => {
                Err("Track input node cannot be MIDI target".to_string())
            }
            PluginGraphNode::TrackOutput => self
                .midi
                .outs
                .get(port)
                .map(|_| ())
                .ok_or_else(|| format!("Track MIDI output port {port} not found")),
            PluginGraphNode::ClapPluginInstance(instance_id) => {
                self.clap_validate_midi_input(*instance_id, port)
            }
            PluginGraphNode::Vst3PluginInstance(instance_id) => {
                self.vst3_validate_midi_input(*instance_id, port)
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            PluginGraphNode::Lv2PluginInstance(instance_id) => {
                self.lv2_validate_midi_input(*instance_id, port)
            }
        }
    }

    fn plugin_connected_neighbors(
        &self,
        kind: Kind,
        current_node: &PluginGraphNode,
    ) -> Vec<PluginGraphNode> {
        let mut nodes = HashSet::new();
        for conn in self.plugin_graph_connections() {
            if conn.kind == kind && &conn.from_node == current_node {
                nodes.insert(conn.to_node);
            }
        }
        nodes.into_iter().collect()
    }

    pub fn push_hw_midi_events(&mut self, events: &[MidiEvent]) {
        let Some(input) = self.midi.ins.first() else {
            return;
        };
        if events.is_empty() {
            return;
        }
        input.lock().buffer.extend_from_slice(events);
    }

    pub fn push_hw_midi_events_to_port(&mut self, port: usize, events: &[MidiEvent]) {
        let Some(input) = self.midi.ins.get(port) else {
            return;
        };
        if events.is_empty() {
            return;
        }
        input.lock().buffer.extend_from_slice(events);
    }

    fn collect_track_input_midi_events(&mut self) -> Vec<Vec<MidiEvent>> {
        let mut events: Vec<Vec<MidiEvent>> = Vec::with_capacity(self.midi.ins.len());
        self.record_tap_midi_in.clear();
        let midi_disk_active = self.midi_disk_monitor.iter().any(|&m| m);
        let clip_playback_active = midi_disk_active && self.clip_playback_enabled;
        for (lane, input) in self.midi.ins.iter().enumerate() {
            let input_lock = input.lock();
            self.record_tap_midi_in
                .extend(input_lock.buffer.iter().cloned());
            let monitor = self.midi_input_monitor.get(lane).copied().unwrap_or(false);
            if clip_playback_active && !monitor {
                input_lock.buffer.clear();
            } else if (monitor || self.record_tap_enabled)
                && let Some(Some(channel)) = self.midi_lane_channels.get(lane)
            {
                input_lock
                    .buffer
                    .retain(|event| Self::event_matches_midi_channel(event, *channel));
            }
            input_lock.buffer.sort_by_key(|event| event.frame);
            input_lock.mark_finished();
            events.push(input_lock.buffer.clone());
        }
        self.record_tap_midi_in.sort_by_key(|e| e.frame);
        events
    }

    fn event_matches_midi_channel(event: &MidiEvent, channel: u8) -> bool {
        let Some(status) = event.data.first().copied() else {
            return true;
        };
        if !(0x80..=0xEF).contains(&status) {
            return true;
        }
        (status & 0x0F) == channel.min(15)
    }

    fn route_track_inputs_to_track_outputs(&mut self, _input_events: &[Vec<MidiEvent>]) {
        for out in &self.midi.outs {
            out.lock().buffer.clear();
        }
        if !self.output_enabled || self.is_folder {
            return;
        }
        for out in &self.midi.outs {
            out.lock().process();
        }
    }

    fn route_modulator_midi_to_track_outputs(&mut self) {
        if self.pending_modulator_midi_events.is_empty() {
            return;
        }
        let events = std::mem::take(&mut self.pending_modulator_midi_events);
        if !self.output_enabled {
            return;
        }
        for out in &self.midi.outs {
            out.lock().buffer.extend_from_slice(&events);
        }
    }

    fn route_automation_midi_to_track_outputs(&mut self) {
        if self.pending_automation_midi_events.is_empty() {
            return;
        }
        let events = std::mem::take(&mut self.pending_automation_midi_events);
        if !self.output_enabled {
            return;
        }
        for out in &self.midi.outs {
            out.lock().buffer.extend_from_slice(&events);
        }
    }

    #[cfg(target_os = "macos")]
    fn route_plugin_midi_to_track_outputs(&self, plugin_events: &[MidiEvent]) {
        if !self.output_enabled || plugin_events.is_empty() {
            return;
        }
        for out in &self.midi.outs {
            out.lock().buffer.extend_from_slice(plugin_events);
        }
    }

    #[cfg(target_os = "macos")]
    fn route_clap_midi_to_track_outputs(&self, plugin_events: &[ClapMidiOutputEvent]) {
        if !self.output_enabled || plugin_events.is_empty() {
            return;
        }
        for event in plugin_events {
            let port = event.port.min(self.midi.outs.len().saturating_sub(1));
            let Some(out) = self.midi.outs.get(port) else {
                continue;
            };
            out.lock().buffer.push(event.event.clone());
        }
    }

    fn process_track_plugins_in_graph_order(&mut self, frames: usize) {
        let track_input_events = self.folder_input_midi_events.clone();
        let order = self.plugin_process_order();
        let mut processed = HashSet::<(PluginKind, usize)>::new();
        self.folder_processed_midi_plugins.clear();
        self.folder_plugin_midi_node_events.clear();
        let echoed = self.echoed_parameter_updates.lock();
        echoed.clear();
        let track_name = self.name.clone();

        while processed.len() < order.len() {
            let mut progressed = false;
            for &(kind, idx) in &order {
                if processed.contains(&(kind, idx)) {
                    continue;
                }
                match kind {
                    PluginKind::Clap => {
                        let processor = self.clap_plugins[idx].processor.lock();
                        if !processor.audio_inputs().iter().all(|input| input.ready()) {
                            continue;
                        }
                        let node = PluginGraphNode::ClapPluginInstance(self.clap_plugins[idx].id);
                        for input in processor.audio_inputs() {
                            input.process();
                        }
                        self.plugin_midi_input_events(
                            &node,
                            processor.midi_input_count(),
                            &track_input_events,
                            &self.folder_plugin_midi_node_events,
                        );
                        let outputs = processor.process_with_midi(
                            frames,
                            &[],
                            crate::plugins::types::ClapTransportInfo {
                                transport_sample: self.transport_sample,
                                playing: (self.disk_monitor.iter().any(|&m| m)
                                    || self.midi_disk_monitor.iter().any(|&m| m))
                                    && self.clip_playback_enabled,
                                loop_enabled: self.loop_enabled,
                                loop_range_samples: self.loop_range_samples,
                                bpm: self.tempo_bpm,
                                tsig_num: self.tsig_num,
                                tsig_denom: self.tsig_denom,
                            },
                        );
                        for ev in processor.drain_echoed_parameters() {
                            echoed.push(crate::message::Action::TrackSetClapParameter {
                                track_name: track_name.clone(),
                                instance_id: self.clap_plugins[idx].id,
                                param_id: ev.param_index,
                                value: ev.value as f64,
                            });
                        }
                        for evt in outputs {
                            self.folder_plugin_midi_node_events
                                .entry((node.clone(), evt.port))
                                .or_default()
                                .push(evt.event);
                        }
                        self.folder_processed_midi_plugins.insert(node);
                    }
                    PluginKind::Vst3 => {
                        let processor = self.vst3_plugins[idx].processor.lock();
                        if !processor.audio_inputs().iter().all(|input| input.ready()) {
                            continue;
                        }
                        let node = PluginGraphNode::Vst3PluginInstance(self.vst3_plugins[idx].id);
                        for input in processor.audio_inputs() {
                            input.process();
                        }
                        let midi_inputs = self.plugin_midi_input_events(
                            &node,
                            processor.midi_input_count(),
                            &track_input_events,
                            &self.folder_plugin_midi_node_events,
                        );
                        let vst3_input = midi_inputs.first().cloned().unwrap_or_default();
                        let outputs = processor.process_with_midi(frames, &vst3_input);
                        for ev in processor.drain_echoed_parameters() {
                            echoed.push(crate::message::Action::TrackSetVst3Parameter {
                                track_name: track_name.clone(),
                                instance_id: self.vst3_plugins[idx].id,
                                param_id: ev.param_index,
                                value: ev.value,
                            });
                        }
                        if !outputs.is_empty() {
                            self.folder_plugin_midi_node_events
                                .insert((node.clone(), 0), outputs);
                        }
                        self.folder_processed_midi_plugins.insert(node);
                    }
                    #[cfg(all(unix, not(target_os = "macos")))]
                    PluginKind::Lv2 => {
                        let processor = self.lv2_plugins[idx].processor.lock();
                        if !processor.audio_inputs().iter().all(|input| input.ready()) {
                            continue;
                        }
                        let node = PluginGraphNode::Lv2PluginInstance(self.lv2_plugins[idx].id);
                        for input in processor.audio_inputs() {
                            input.process();
                        }
                        let midi_inputs = self.plugin_midi_input_events(
                            &node,
                            processor.midi_input_count(),
                            &track_input_events,
                            &self.folder_plugin_midi_node_events,
                        );
                        let lv2_input = midi_inputs.first().cloned().unwrap_or_default();
                        let outputs = processor.process_with_midi(frames, &lv2_input);
                        for ev in processor.drain_echoed_parameters() {
                            echoed.push(crate::message::Action::TrackSetLv2ControlValue {
                                track_name: track_name.clone(),
                                instance_id: self.lv2_plugins[idx].id,
                                index: ev.param_index,
                                value: ev.value,
                            });
                        }
                        if !outputs.is_empty() {
                            self.folder_plugin_midi_node_events
                                .insert((node.clone(), 0), outputs);
                        }
                        self.folder_processed_midi_plugins.insert(node);
                    }
                }
                processed.insert((kind, idx));
                progressed = true;
            }
            if !progressed {
                break;
            }
        }
    }

    fn plugin_midi_ready(
        &self,
        node: &PluginGraphNode,
        processed: &HashSet<PluginGraphNode>,
    ) -> bool {
        self.plugin_midi_connections
            .iter()
            .filter(|conn| {
                if conn.kind != Kind::MIDI || &conn.to_node != node {
                    return false;
                }
                let is_plugin = matches!(
                    conn.from_node,
                    PluginGraphNode::ClapPluginInstance(_) | PluginGraphNode::Vst3PluginInstance(_)
                );
                #[cfg(all(unix, not(target_os = "macos")))]
                let is_plugin =
                    is_plugin || matches!(conn.from_node, PluginGraphNode::Lv2PluginInstance(_));
                is_plugin
            })
            .all(|conn| processed.contains(&conn.from_node))
    }

    fn plugin_midi_input_events(
        &self,
        node: &PluginGraphNode,
        midi_inputs: usize,
        _track_input_events: &[Vec<MidiEvent>],
        _node_events: &HashMap<(PluginGraphNode, usize), Vec<MidiEvent>>,
    ) -> Vec<Vec<MidiEvent>> {
        let ports = self.plugin_midi_input_ports_for_node(node);
        let mut per_port: Vec<Vec<MidiEvent>> = ports
            .iter()
            .map(|port| {
                let lock = port.lock();
                lock.process();
                lock.buffer.clone()
            })
            .collect();
        if per_port.len() < midi_inputs {
            per_port.resize_with(midi_inputs, Vec::new);
        }
        per_port
    }

    fn plugin_midi_input_ports_for_node(
        &self,
        node: &PluginGraphNode,
    ) -> Vec<Arc<UnsafeMutex<Box<MIDIIO>>>> {
        match node {
            PluginGraphNode::ClapPluginInstance(instance_id) => self
                .clap_plugins
                .iter()
                .find(|instance| instance.id == *instance_id)
                .map(|instance| instance.processor.lock().midi_input_ports().to_vec())
                .unwrap_or_default(),
            PluginGraphNode::Vst3PluginInstance(instance_id) => self
                .vst3_plugins
                .iter()
                .find(|instance| instance.id == *instance_id)
                .map(|instance| instance.processor.lock().midi_input_ports().to_vec())
                .unwrap_or_default(),
            #[cfg(all(unix, not(target_os = "macos")))]
            PluginGraphNode::Lv2PluginInstance(instance_id) => self
                .lv2_plugins
                .iter()
                .find(|instance| instance.id == *instance_id)
                .map(|instance| instance.processor.lock().midi_input_ports().to_vec())
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    fn route_plugin_midi_to_track_outputs_graph(
        &self,
        _track_input_events: &[Vec<MidiEvent>],
        node_events: &HashMap<(PluginGraphNode, usize), Vec<MidiEvent>>,
    ) {
        if !self.output_enabled {
            return;
        }
        for conn in self
            .plugin_midi_connections
            .iter()
            .filter(|conn| conn.kind == Kind::MIDI && conn.to_node == PluginGraphNode::TrackOutput)
        {
            // Track input -> output is handled by MIDIIO::process on the output.
            // CLAP plugin outputs already feed track outputs through their MIDIIO ports.
            if conn.from_node == PluginGraphNode::TrackInput
                || matches!(conn.from_node, PluginGraphNode::ClapPluginInstance(_))
            {
                continue;
            }
            let Some(out) = self.midi.outs.get(conn.to_port) else {
                continue;
            };
            if let Some(events) = node_events.get(&(conn.from_node.clone(), conn.from_port)) {
                out.lock().buffer.extend_from_slice(events);
            }
        }
    }

    fn clear_local_midi_inputs(&self) {
        for input in &self.midi.ins {
            input.lock().buffer.clear();
        }
    }

    fn collect_hw_midi_output_events(&mut self) {
        self.pending_hw_midi_out_events.clear();
        for (port, out) in self.midi.outs.iter().enumerate() {
            self.pending_hw_midi_out_events.extend(
                out.lock()
                    .buffer
                    .iter()
                    .cloned()
                    .map(|event| HwMidiOutEvent { port, event }),
            );
        }
    }

    pub fn take_hw_midi_out_events(&mut self) -> Vec<HwMidiOutEvent> {
        std::mem::take(&mut self.pending_hw_midi_out_events)
    }
}

impl crate::connectable::AudioPorts for Track {
    fn audio_inputs(&self) -> Vec<Arc<crate::audio::io::AudioIO>> {
        self.audio.ins.clone()
    }

    fn audio_outputs(&self) -> Vec<Arc<crate::audio::io::AudioIO>> {
        self.audio.outs.clone()
    }
}

impl crate::connectable::MidiPorts for Track {
    fn midi_inputs(&self) -> Vec<Arc<UnsafeMutex<Box<crate::midi::io::MIDIIO>>>> {
        self.midi.ins.clone()
    }

    fn midi_outputs(&self) -> Vec<Arc<UnsafeMutex<Box<crate::midi::io::MIDIIO>>>> {
        self.midi.outs.clone()
    }
}

impl crate::connectable::AudioPorts for ClapInstance {
    fn audio_inputs(&self) -> Vec<Arc<crate::audio::io::AudioIO>> {
        self.processor.lock().audio_inputs().to_vec()
    }

    fn audio_outputs(&self) -> Vec<Arc<crate::audio::io::AudioIO>> {
        self.processor.lock().audio_outputs().to_vec()
    }
}

impl crate::connectable::MidiPorts for ClapInstance {
    fn midi_inputs(&self) -> Vec<Arc<UnsafeMutex<Box<crate::midi::io::MIDIIO>>>> {
        self.processor.lock().midi_input_ports().to_vec()
    }

    fn midi_outputs(&self) -> Vec<Arc<UnsafeMutex<Box<crate::midi::io::MIDIIO>>>> {
        self.processor.lock().midi_output_ports().to_vec()
    }
}

impl crate::connectable::AudioPorts for Vst3Instance {
    fn audio_inputs(&self) -> Vec<Arc<crate::audio::io::AudioIO>> {
        self.processor.lock().audio_inputs().to_vec()
    }

    fn audio_outputs(&self) -> Vec<Arc<crate::audio::io::AudioIO>> {
        self.processor.lock().audio_outputs().to_vec()
    }
}

impl crate::connectable::MidiPorts for Vst3Instance {
    fn midi_inputs(&self) -> Vec<Arc<UnsafeMutex<Box<crate::midi::io::MIDIIO>>>> {
        self.processor.lock().midi_input_ports().to_vec()
    }

    fn midi_outputs(&self) -> Vec<Arc<UnsafeMutex<Box<crate::midi::io::MIDIIO>>>> {
        self.processor.lock().midi_output_ports().to_vec()
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
impl crate::connectable::AudioPorts for Lv2Instance {
    fn audio_inputs(&self) -> Vec<Arc<crate::audio::io::AudioIO>> {
        self.processor.lock().audio_inputs().to_vec()
    }

    fn audio_outputs(&self) -> Vec<Arc<crate::audio::io::AudioIO>> {
        self.processor.lock().audio_outputs().to_vec()
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
impl crate::connectable::MidiPorts for Lv2Instance {
    fn midi_inputs(&self) -> Vec<Arc<UnsafeMutex<Box<crate::midi::io::MIDIIO>>>> {
        self.processor.lock().midi_input_ports().to_vec()
    }

    fn midi_outputs(&self) -> Vec<Arc<UnsafeMutex<Box<crate::midi::io::MIDIIO>>>> {
        self.processor.lock().midi_output_ports().to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::{AudioClipBuffer, HwMidiOutEvent, Track};
    use crate::audio::clip::AudioClip;
    use crate::audio::io::AudioIO;
    use crate::mutex::UnsafeMutex;
    use crate::{kind::Kind, message::PluginGraphNode};
    use std::sync::Arc;

    #[test]
    fn default_audio_passthrough_uses_minimum_port_count() {
        let track = Track::new("t".to_string(), 1, 2, 0, 0, 64, 48_000.0);

        assert_eq!(track.audio.ins.len(), 1);
        assert_eq!(track.audio.outs.len(), 2);
        assert!(
            track.audio.outs[0]
                .connections
                .lock()
                .iter()
                .any(|conn| Arc::ptr_eq(conn, &track.audio.ins[0]))
        );
        assert!(
            track.audio.outs[1]
                .connections
                .lock()
                .iter()
                .any(|conn| Arc::ptr_eq(conn, &track.audio.ins[0]))
        );
    }

    #[test]
    fn default_midi_passthrough_uses_minimum_port_count() {
        let track = Track::new("t".to_string(), 0, 0, 1, 2, 64, 48_000.0);

        assert_eq!(track.midi.ins.len(), 1);
        assert_eq!(track.midi.outs.len(), 2);
        assert!(
            track.midi.ins[0]
                .lock()
                .connections
                .iter()
                .any(|conn| Arc::ptr_eq(conn, &track.midi.outs[0]))
        );
        assert!(
            track.midi.ins[0]
                .lock()
                .connections
                .iter()
                .all(|conn| !Arc::ptr_eq(conn, &track.midi.outs[1]))
        );
    }

    #[test]
    fn plugin_graph_includes_default_track_midi_passthrough() {
        let track = Track::new("t".to_string(), 0, 0, 1, 2, 64, 48_000.0);
        let connections = track.plugin_graph_connections();

        assert!(connections.iter().any(|c| {
            c.kind == Kind::MIDI
                && c.from_node == PluginGraphNode::TrackInput
                && c.from_port == 0
                && c.to_node == PluginGraphNode::TrackOutput
                && c.to_port == 0
        }));
        assert!(connections.iter().all(|c| {
            !(c.kind == Kind::MIDI
                && c.from_node == PluginGraphNode::TrackInput
                && c.from_port == 0
                && c.to_node == PluginGraphNode::TrackOutput
                && c.to_port == 1)
        }));
    }

    #[test]
    fn track_input_passthrough_respects_input_monitor() {
        let mut track = Track::new("t".to_string(), 1, 1, 0, 0, 8, 48_000.0);
        let source = Arc::new(AudioIO::new(8));
        source.buffer.lock()[0] = 0.5;
        source.buffer.lock()[1] = -0.25;
        AudioIO::connect(&source, &track.audio.ins[0]);

        track.input_monitor = vec![false];
        track.process();
        let out = track.audio.outs[0].buffer.lock().to_vec();
        assert_eq!(out[0], 0.0);
        assert_eq!(out[1], 0.0);

        track.input_monitor = vec![true];
        track.process();
        let out = track.audio.outs[0].buffer.lock().to_vec();
        assert_eq!(out[0], 0.5);
        assert_eq!(out[1], -0.25);
    }

    #[test]
    fn clip_playback_audible_with_input_monitor_off() {
        let mut track = Track::new("t".to_string(), 1, 1, 0, 0, 8, 48_000.0);
        track.input_monitor = vec![false];
        track.disk_monitor = vec![true];
        let mut clip = AudioClip::new("clip".to_string(), 0, 4);
        clip.fade_enabled = false;
        track.audio.clips.push(clip);
        track.audio_clip_cache.insert(
            "clip".to_string(),
            Arc::new(AudioClipBuffer {
                channels: 1,
                samples: vec![0.8, 0.0, 0.0, 0.0],
            }),
        );

        track.process();
        let out = track.audio.outs[0].buffer.lock().to_vec();
        assert_eq!(out[0], 0.8);
    }

    #[test]
    fn record_tap_captures_live_input_with_disk_monitor_on_and_input_monitor_off() {
        let mut track = Track::new("t".to_string(), 1, 1, 0, 0, 8, 48_000.0);
        track.input_monitor = vec![false];
        track.disk_monitor = vec![true];
        track.armed = true;
        track.record_tap_enabled = true;
        let source = Arc::new(AudioIO::new(8));
        source.buffer.lock()[0] = 0.5;
        source.buffer.lock()[1] = -0.25;
        AudioIO::connect(&source, &track.audio.ins[0]);

        track.process();

        assert_eq!(track.record_tap_outs[0][0], 0.5);
        assert_eq!(track.record_tap_outs[0][1], -0.25);
    }

    #[test]
    fn record_tap_falls_back_to_direct_input_when_no_internal_route_exists() {
        let mut track = Track::new("t".to_string(), 1, 1, 0, 0, 8, 48_000.0);
        track.input_monitor = vec![false];
        track.disk_monitor = vec![true];
        track.armed = true;
        track.record_tap_enabled = true;
        track.clear_default_passthrough();
        let source = Arc::new(AudioIO::new(8));
        source.buffer.lock()[0] = 0.25;
        source.buffer.lock()[1] = -0.5;
        AudioIO::connect(&source, &track.audio.ins[0]);

        track.process();

        assert_eq!(track.record_tap_outs[0][0], 0.25);
        assert_eq!(track.record_tap_outs[0][1], -0.5);
    }

    #[test]
    fn clip_playback_respects_clip_playback_enabled_flag() {
        let mut track = Track::new("t".to_string(), 1, 1, 0, 0, 8, 48_000.0);
        track.input_monitor = vec![false];
        track.disk_monitor = vec![true];
        track.clip_playback_enabled = false;
        let mut clip = AudioClip::new("clip".to_string(), 0, 4);
        clip.fade_enabled = false;
        track.audio.clips.push(clip);
        track.audio_clip_cache.insert(
            "clip".to_string(),
            Arc::new(AudioClipBuffer {
                channels: 1,
                samples: vec![0.8, 0.0, 0.0, 0.0],
            }),
        );

        track.process();
        let out = track.audio.outs[0].buffer.lock().to_vec();
        assert_eq!(out[0], 0.0);

        track.clip_playback_enabled = true;
        track.process();
        let out = track.audio.outs[0].buffer.lock().to_vec();
        assert_eq!(out[0], 0.8);
    }

    #[test]
    fn disconnecting_one_stereo_internal_channel_mutes_only_that_channel() {
        let mut track = Track::new("t".to_string(), 2, 2, 0, 0, 8, 48_000.0);
        let left = Arc::new(AudioIO::new(8));
        let right = Arc::new(AudioIO::new(8));
        left.buffer.lock()[0] = 0.25;
        right.buffer.lock()[0] = 0.75;
        AudioIO::connect(&left, &track.audio.ins[0]);
        AudioIO::connect(&right, &track.audio.ins[1]);
        track.input_monitor = vec![true; 2];
        track.disk_monitor = vec![false; 2];

        track.process();
        let out_l = track.audio.outs[0].buffer.lock().to_vec();
        let out_r = track.audio.outs[1].buffer.lock().to_vec();
        assert_eq!(out_l[0], 0.25);
        assert_eq!(out_r[0], 0.75);

        track
            .disconnect_plugin_audio(
                PluginGraphNode::TrackInput,
                1,
                PluginGraphNode::TrackOutput,
                1,
            )
            .unwrap();
        track.process();
        let out_l = track.audio.outs[0].buffer.lock().to_vec();
        let out_r = track.audio.outs[1].buffer.lock().to_vec();
        assert_eq!(out_l[0], 0.25);
        assert_eq!(out_r[0], 0.0);
    }

    #[test]
    fn direct_clip_graph_passthrough_is_audible_with_input_monitor_off() {
        let graph = serde_json::json!({
            "plugins": [],
            "connections": [
                {
                    "from_node": {"type":"track_input"},
                    "from_port": 0,
                    "to_node": {"type":"track_output"},
                    "to_port": 0,
                    "kind": "audio"
                }
            ]
        });
        let outputs = Track::process_direct_clip_graph(&graph, &[vec![0.5, -0.25]], 2);
        assert_eq!(outputs, vec![vec![0.5, -0.25]]);
    }

    #[test]
    fn direct_clip_graph_accepts_legacy_string_track_nodes() {
        let graph = serde_json::json!({
            "plugins": [],
            "connections": [
                {
                    "from_node": "TrackInput",
                    "from_port": 0,
                    "to_node": "TrackOutput",
                    "to_port": 0,
                    "kind": "Audio"
                }
            ]
        });
        let outputs = Track::process_direct_clip_graph(&graph, &[vec![0.5, -0.25]], 2);
        assert_eq!(outputs, vec![vec![0.5, -0.25]]);
    }

    #[test]
    fn direct_clip_graph_empty_connections_produces_silence() {
        let graph = serde_json::json!({
            "plugins": [],
            "connections": []
        });
        let outputs = Track::process_direct_clip_graph(&graph, &[vec![0.5, -0.25]], 2);
        assert_eq!(outputs, vec![vec![0.0, 0.0]]);
    }

    #[test]
    fn direct_clip_graph_respects_connection_port_fields_for_stereo() {
        let graph = serde_json::json!({
            "plugins": [],
            "connections": [
                {
                    "from_node": {"type":"track_input"},
                    "from_port": 0,
                    "to_node": {"type":"track_output"},
                    "to_port": 0,
                    "kind": "audio"
                },
                {
                    "from_node": {"type":"track_input"},
                    "from_port": 1,
                    "to_node": {"type":"track_output"},
                    "to_port": 1,
                    "kind": "audio"
                }
            ]
        });
        let outputs =
            Track::process_direct_clip_graph(&graph, &[vec![0.25, 0.0], vec![0.75, 0.0]], 2);
        assert_eq!(outputs, vec![vec![0.25, 0.0], vec![0.75, 0.0]]);
    }

    #[test]
    fn direct_clip_graph_ignores_non_audio_and_non_track_io_connections() {
        let graph = serde_json::json!({
            "plugins": [],
            "connections": [
                {
                    "from_node": {"type":"track_input"},
                    "from_port": 0,
                    "to_node": {"type":"track_output"},
                    "to_port": 0,
                    "kind": "midi"
                },
                {
                    "from_node": {"type":"plugin", "plugin_index": 0},
                    "from_port": 0,
                    "to_node": {"type":"track_output"},
                    "to_port": 0,
                    "kind": "audio"
                }
            ]
        });
        let outputs = Track::process_direct_clip_graph(&graph, &[vec![0.5, -0.25]], 2);
        assert_eq!(outputs, vec![vec![0.0, 0.0]]);
    }

    #[test]
    fn clip_graph_uses_plugin_runtime_only_when_plugins_are_present() {
        let no_plugins = serde_json::json!({
            "plugins": [],
            "connections": []
        });
        let with_plugin = serde_json::json!({
            "plugins": [
                {"format":"LV2","uri":"http://example.test/plugin"}
            ],
            "connections": []
        });

        assert!(!Track::clip_graph_uses_plugin_runtime(&no_plugins));
        assert!(Track::clip_graph_uses_plugin_runtime(&with_plugin));
    }

    #[test]
    fn clip_plugin_runtime_key_changes_when_graph_changes() {
        let mut clip = crate::audio::clip::AudioClip::new("clip.wav".to_string(), 0, 128);
        clip.plugin_graph_json = Some(serde_json::json!({
            "plugins": [],
            "connections": [
                {
                    "from_node": {"type":"track_input"},
                    "from_port": 0,
                    "to_node": {"type":"track_output"},
                    "to_port": 0,
                    "kind": "audio"
                }
            ]
        }));
        let key_before = Track::clip_plugin_runtime_key(&clip, 2, 2);

        clip.plugin_graph_json = Some(serde_json::json!({
            "plugins": [],
            "connections": []
        }));
        let key_after = Track::clip_plugin_runtime_key(&clip, 2, 2);

        assert_ne!(key_before, key_after);
    }

    #[test]
    fn clip_plugin_runtime_key_changes_when_channel_shape_changes() {
        let mut clip = crate::audio::clip::AudioClip::new("clip.wav".to_string(), 0, 128);
        clip.plugin_graph_json = Some(serde_json::json!({
            "plugins": [],
            "connections": []
        }));

        let stereo_key = Track::clip_plugin_runtime_key(&clip, 2, 2);
        let mono_key = Track::clip_plugin_runtime_key(&clip, 1, 1);

        assert_ne!(stereo_key, mono_key);
    }

    #[test]
    fn transport_timing_and_loop_config_clamp_invalid_values() {
        let mut track = Track::new("t".to_string(), 1, 1, 0, 0, 8, 48_000.0);

        track.set_transport_timing(0.0, 0, 0);
        assert_eq!(track.tempo_bpm, 1.0);
        assert_eq!(track.tsig_num, 1);
        assert_eq!(track.tsig_denom, 1);

        track.set_loop_config(true, Some((128, 256)));
        assert!(track.loop_enabled);
        assert_eq!(track.loop_range_samples, Some((128, 256)));
    }

    #[test]
    fn cycle_segments_wrap_across_loop_boundary() {
        let mut track = Track::new("t".to_string(), 1, 1, 0, 0, 8, 48_000.0);
        track.transport_sample = 14;
        track.loop_enabled = true;
        track.loop_range_samples = Some((10, 16));

        let segments = track.cycle_segments(6);
        assert_eq!(segments, vec![(14, 16, 0), (10, 14, 2)]);
    }

    #[test]
    fn offline_bounce_restores_transport_and_monitor_state() {
        let mut track = Track::new("t".to_string(), 1, 1, 0, 0, 8, 48_000.0);
        track.transport_sample = 123;
        track.disk_monitor = vec![false];
        track.input_monitor = vec![true];
        track.clip_playback_enabled = false;
        track.output_enabled = false;
        track.loop_enabled = true;
        track.loop_range_samples = Some((32, 64));
        track.armed = true;
        track.pending_hw_midi_out_events.push(HwMidiOutEvent {
            port: 0,
            event: crate::midi::io::MidiEvent::new(0, vec![0x90, 60, 100]),
        });

        let (channels, rendered) = track.offline_bounce_interleaved(0, 4);
        assert_eq!(channels, 1);
        assert_eq!(rendered.len(), 4);

        assert_eq!(track.transport_sample, 123);
        assert!(!track.disk_monitor.first().copied().unwrap_or(false));
        assert!(track.input_monitor.first().copied().unwrap_or(false));
        assert!(!track.clip_playback_enabled);
        assert!(!track.output_enabled);
        assert!(track.loop_enabled);
        assert_eq!(track.loop_range_samples, Some((32, 64)));
        assert!(track.armed);
        assert_eq!(track.pending_hw_midi_out_events.len(), 1);
    }

    #[test]
    fn midi_only_track_clip_playback_generates_hw_midi_events() {
        let mut track = Track::new("t".to_string(), 0, 0, 1, 1, 8, 48_000.0);
        track.disk_monitor = vec![true];
        track.clip_playback_enabled = true;
        track.midi.clips.push(crate::midi::clip::MIDIClip::new(
            "clip.mid".to_string(),
            0,
            8,
        ));
        track.midi_clip_cache.insert(
            "clip.mid".to_string(),
            Arc::new(vec![(0, vec![0x90, 60, 100])]),
        );

        track.process();

        assert_eq!(track.pending_hw_midi_out_events.len(), 2);
        assert_eq!(track.pending_hw_midi_out_events[0].port, 0);
        assert_eq!(
            track.pending_hw_midi_out_events[0].event,
            crate::midi::io::MidiEvent::new(0, vec![0x90, 60, 100])
        );
        assert_eq!(
            track.pending_hw_midi_out_events[1].event,
            crate::midi::io::MidiEvent::new(7, vec![0x80, 60, 64])
        );
    }

    #[test]
    fn midi_clip_emits_note_off_at_exact_clip_end() {
        let mut track = Track::new("t".to_string(), 0, 0, 1, 1, 8, 48_000.0);
        track.disk_monitor = vec![true];
        track.clip_playback_enabled = true;
        track.midi.clips.push(crate::midi::clip::MIDIClip::new(
            "clip.mid".to_string(),
            0,
            8,
        ));
        track.midi_clip_cache.insert(
            "clip.mid".to_string(),
            Arc::new(vec![(0, vec![0x90, 60, 100]), (8, vec![0x80, 60, 64])]),
        );

        track.process();

        assert_eq!(track.pending_hw_midi_out_events.len(), 2);
        assert_eq!(
            track.pending_hw_midi_out_events[0].event,
            crate::midi::io::MidiEvent::new(0, vec![0x90, 60, 100])
        );
        assert_eq!(
            track.pending_hw_midi_out_events[1].event,
            crate::midi::io::MidiEvent::new(7, vec![0x80, 60, 64])
        );
    }

    #[test]
    fn midi_clip_emits_note_off_at_exact_loop_end() {
        let mut track = Track::new("t".to_string(), 0, 0, 1, 1, 8, 48_000.0);
        track.disk_monitor = vec![true];
        track.clip_playback_enabled = true;
        track.loop_enabled = true;
        track.loop_range_samples = Some((0, 8));
        track.midi.clips.push(crate::midi::clip::MIDIClip::new(
            "clip.mid".to_string(),
            0,
            8,
        ));
        track.midi_clip_cache.insert(
            "clip.mid".to_string(),
            Arc::new(vec![(0, vec![0x90, 60, 100]), (8, vec![0x80, 60, 64])]),
        );

        track.process();

        assert_eq!(track.pending_hw_midi_out_events.len(), 2);
        assert_eq!(
            track.pending_hw_midi_out_events[0].event,
            crate::midi::io::MidiEvent::new(0, vec![0x90, 60, 100])
        );
        assert_eq!(
            track.pending_hw_midi_out_events[1].event,
            crate::midi::io::MidiEvent::new(7, vec![0x80, 60, 64])
        );
    }

    #[test]
    fn midi_clip_orders_loop_boundary_note_off_before_next_note_on() {
        let mut track = Track::new("t".to_string(), 0, 0, 1, 1, 4, 48_000.0);
        track.disk_monitor = vec![true];
        track.clip_playback_enabled = true;
        track.transport_sample = 6;
        track.loop_enabled = true;
        track.loop_range_samples = Some((0, 8));
        track.midi.clips.push(crate::midi::clip::MIDIClip::new(
            "clip.mid".to_string(),
            0,
            8,
        ));
        track.midi_clip_cache.insert(
            "clip.mid".to_string(),
            Arc::new(vec![(0, vec![0x90, 60, 100]), (8, vec![0x80, 60, 64])]),
        );

        track.process();

        assert_eq!(track.pending_hw_midi_out_events.len(), 2);
        assert_eq!(
            track.pending_hw_midi_out_events[0].event,
            crate::midi::io::MidiEvent::new(2, vec![0x80, 60, 64])
        );
        assert_eq!(
            track.pending_hw_midi_out_events[1].event,
            crate::midi::io::MidiEvent::new(2, vec![0x90, 60, 100])
        );
    }

    #[test]
    fn midi_clip_sends_note_off_at_clip_end_when_source_note_ends_later() {
        let mut track = Track::new("t".to_string(), 0, 0, 1, 1, 8, 48_000.0);
        track.disk_monitor = vec![true];
        track.clip_playback_enabled = true;
        track.midi.clips.push(crate::midi::clip::MIDIClip::new(
            "clip.mid".to_string(),
            0,
            8,
        ));
        track.midi_clip_cache.insert(
            "clip.mid".to_string(),
            Arc::new(vec![(0, vec![0x90, 60, 100]), (12, vec![0x80, 60, 64])]),
        );

        track.process();

        assert_eq!(track.pending_hw_midi_out_events.len(), 2);
        assert_eq!(
            track.pending_hw_midi_out_events[0].event,
            crate::midi::io::MidiEvent::new(0, vec![0x90, 60, 100])
        );
        assert_eq!(
            track.pending_hw_midi_out_events[1].event,
            crate::midi::io::MidiEvent::new(7, vec![0x80, 60, 64])
        );
    }

    #[test]
    fn midi_clip_orders_synthetic_loop_note_off_before_next_note_on() {
        let mut track = Track::new("t".to_string(), 0, 0, 1, 1, 4, 48_000.0);
        track.disk_monitor = vec![true];
        track.clip_playback_enabled = true;
        track.transport_sample = 6;
        track.loop_enabled = true;
        track.loop_range_samples = Some((0, 8));
        track.midi.clips.push(crate::midi::clip::MIDIClip::new(
            "clip.mid".to_string(),
            0,
            8,
        ));
        track.midi_clip_cache.insert(
            "clip.mid".to_string(),
            Arc::new(vec![(0, vec![0x90, 60, 100]), (12, vec![0x80, 60, 64])]),
        );

        track.process();

        assert_eq!(track.pending_hw_midi_out_events.len(), 2);
        assert_eq!(
            track.pending_hw_midi_out_events[0].event,
            crate::midi::io::MidiEvent::new(2, vec![0x80, 60, 64])
        );
        assert_eq!(
            track.pending_hw_midi_out_events[1].event,
            crate::midi::io::MidiEvent::new(2, vec![0x90, 60, 100])
        );
    }

    #[test]
    fn midi_lane_channel_filters_monitored_input() {
        let mut track = Track::new("t".to_string(), 0, 0, 1, 1, 8, 48_000.0);
        track.midi_input_monitor = vec![true];
        track.set_midi_lane_channel(0, Some(1));
        track.push_hw_midi_events_to_port(
            0,
            &[
                crate::midi::io::MidiEvent::new(0, vec![0x90, 60, 100]),
                crate::midi::io::MidiEvent::new(1, vec![0x91, 61, 101]),
                crate::midi::io::MidiEvent::new(2, vec![0xF8]),
            ],
        );

        let events = track.collect_track_input_midi_events();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].len(), 2);
        assert_eq!(
            events[0][0],
            crate::midi::io::MidiEvent::new(1, vec![0x91, 61, 101])
        );
        assert_eq!(events[0][1], crate::midi::io::MidiEvent::new(2, vec![0xF8]));
        assert_eq!(track.record_tap_midi_in.len(), 3);
    }

    #[test]
    fn midi_lane_channel_omni_does_not_filter_input() {
        let mut track = Track::new("t".to_string(), 0, 0, 1, 1, 8, 48_000.0);
        track.midi_input_monitor = vec![true];
        track.set_midi_lane_channel(0, None);
        track.push_hw_midi_events_to_port(
            0,
            &[
                crate::midi::io::MidiEvent::new(0, vec![0x90, 60, 100]),
                crate::midi::io::MidiEvent::new(1, vec![0x91, 61, 101]),
            ],
        );

        let events = track.collect_track_input_midi_events();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].len(), 2);
    }

    #[test]
    fn grouped_audio_playback_sums_child_buffers() {
        let mut track = Track::new("t".to_string(), 1, 1, 0, 0, 8, 48_000.0);
        track.input_monitor = vec![false];
        track.disk_monitor = vec![true];

        let mut active_child = AudioClip::new("active".to_string(), 0, 4);
        active_child.fade_enabled = false;
        let mut muted_child = AudioClip::new("muted".to_string(), 0, 4);
        muted_child.fade_enabled = false;
        muted_child.muted = true;

        let mut group = AudioClip::new("group".to_string(), 0, 4);
        group.fade_enabled = false;
        group.grouped_clips = vec![active_child, muted_child];
        track.audio.clips.push(group);
        track.audio_clip_cache.insert(
            "active".to_string(),
            Arc::new(AudioClipBuffer {
                channels: 1,
                samples: vec![0.6, 0.0, 0.0, 0.0],
            }),
        );
        track.audio_clip_cache.insert(
            "muted".to_string(),
            Arc::new(AudioClipBuffer {
                channels: 1,
                samples: vec![0.9, 0.0, 0.0, 0.0],
            }),
        );

        track.process();

        let out = track.audio.outs[0].buffer.lock().to_vec();
        assert_eq!(out[0], 1.5);
    }

    #[test]
    fn direct_clip_graph_ignores_malformed_track_nodes() {
        let graph = serde_json::json!({
            "plugins": [],
            "connections": [
                {
                    "from_node": {"type":"track_input"},
                    "from_port": 0,
                    "to_node": {"type":"unknown"},
                    "to_port": 0,
                    "kind": "audio"
                }
            ]
        });
        let outputs = Track::process_direct_clip_graph(&graph, &[vec![0.5, -0.25]], 2);
        assert_eq!(outputs, vec![vec![0.0, 0.0]]);
    }

    #[test]
    fn connectable_connections_reports_default_audio_passthrough() {
        let track = Track::new("t".to_string(), 1, 1, 0, 0, 8, 48_000.0);
        let connections = track.connectable_connections();

        assert!(connections.iter().any(|c| {
            c.kind == Kind::Audio
                && c.from == crate::connectable::ConnectableRef::TrackInput
                && c.from_port == 0
                && c.to == crate::connectable::ConnectableRef::TrackOutput
                && c.to_port == 0
        }));
    }

    #[test]
    fn connectable_connections_reports_child_to_folder_output() {
        let mut folder = Track::new_folder("Folder".to_string(), 1, 1, 0, 0, 8, 48_000.0);
        let child = Arc::new(UnsafeMutex::new(Box::new(Track::new(
            "Child".to_string(),
            1,
            1,
            0,
            0,
            8,
            48_000.0,
        ))));
        {
            let child_lock = child.lock();
            AudioIO::connect(&child_lock.audio.outs[0], &folder.audio.outs[0]);
        }
        folder.child_tracks.push(child);

        let connections = folder.connectable_connections();
        assert!(connections.iter().any(|c| {
            c.kind == Kind::Audio
                && c.from == crate::connectable::ConnectableRef::ChildTrack("Child".to_string())
                && c.from_port == 0
                && c.to == crate::connectable::ConnectableRef::TrackOutput
                && c.to_port == 0
        }));
    }

    #[test]
    fn connect_audio_connectable_links_child_output_to_track_input() {
        let mut track = Track::new("Parent".to_string(), 1, 1, 0, 0, 8, 48_000.0);
        track.clear_default_passthrough();
        let child = Arc::new(UnsafeMutex::new(Box::new(Track::new(
            "Child".to_string(),
            1,
            1,
            0,
            0,
            8,
            48_000.0,
        ))));
        track.child_tracks.push(child);

        track
            .connect_audio_connectable(
                crate::connectable::ConnectableRef::ChildTrack("Child".to_string()),
                0,
                crate::connectable::ConnectableRef::TrackInput,
                0,
            )
            .unwrap();

        let child_out = track.child_tracks[0].lock().audio.outs[0].clone();
        assert!(
            track.audio.ins[0]
                .connections
                .lock()
                .iter()
                .any(|c| Arc::ptr_eq(c, &child_out))
        );
    }

    #[test]
    fn connect_midi_connectable_links_child_output_to_track_input() {
        let mut track = Track::new("Parent".to_string(), 0, 0, 1, 1, 8, 48_000.0);
        track.clear_default_passthrough();
        let child = Arc::new(UnsafeMutex::new(Box::new(Track::new(
            "Child".to_string(),
            0,
            0,
            1,
            1,
            8,
            48_000.0,
        ))));
        track.child_tracks.push(child);

        track
            .connect_midi_connectable(
                crate::connectable::ConnectableRef::ChildTrack("Child".to_string()),
                0,
                crate::connectable::ConnectableRef::TrackInput,
                0,
            )
            .unwrap();

        let child_out = track.child_tracks[0].lock().midi.outs[0].clone();
        assert!(
            track.midi.ins[0]
                .lock()
                .sources
                .iter()
                .any(|s| Arc::ptr_eq(s, &child_out))
        );
    }

    #[test]
    fn connect_audio_connectable_rejects_track_input_as_source() {
        let mut track = Track::new("t".to_string(), 1, 1, 0, 0, 8, 48_000.0);
        let err = track
            .connect_audio_connectable(
                crate::connectable::ConnectableRef::TrackInput,
                0,
                crate::connectable::ConnectableRef::TrackOutput,
                0,
            )
            .unwrap_err();
        assert!(err.contains("cannot be used as an audio source"));
    }

    #[test]
    fn folder_track_cannot_become_master() {
        let mut track = Track::new_folder("folder".to_string(), 2, 2, 0, 0, 64, 48_000.0);
        assert!(!track.is_master);

        track.toggle_master();
        assert!(!track.is_master);

        track.set_master(true);
        assert!(!track.is_master);
    }

    #[test]
    fn master_track_can_be_unmastered_even_when_folder() {
        let mut track = Track::new_folder("folder".to_string(), 2, 2, 0, 0, 64, 48_000.0);
        track.is_master = true;

        track.toggle_master();
        assert!(!track.is_master);

        track.set_master(true);
        assert!(!track.is_master);
    }

    #[test]
    fn normal_track_can_be_toggled_master() {
        let mut track = Track::new("t".to_string(), 2, 2, 0, 0, 64, 48_000.0);
        assert!(!track.is_master);

        track.toggle_master();
        assert!(track.is_master);

        track.toggle_master();
        assert!(!track.is_master);
    }
}
