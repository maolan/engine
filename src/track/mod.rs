use super::{audio::track::AudioTrack, midi::track::MIDITrack};
#[cfg(target_os = "macos")]
use crate::clap::ClapMidiOutputEvent;
use crate::message::{PluginGraphConnection, PluginGraphNode};
#[cfg(unix)]
use crate::rubberband::LivePitchShifter;

use crate::kind::Kind;
use crate::{
    audio::io::AudioIO,
    midi::io::{MIDIIO, MidiEvent},
};
use arc_swap::{ArcSwap, ArcSwapOption};
use std::{
    cell::UnsafeCell,
    collections::{HashMap, HashSet},
    ops::{Deref, DerefMut},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicUsize, Ordering},
    },
};

mod clip_render;
mod instances;
mod plugins;
mod process;
mod session;
mod track_routing;
#[cfg(all(unix, not(target_os = "macos")))]
pub use instances::Lv2Instance;
pub use instances::{ClapInstance, Vst3Instance};

type MidiClipEvents = Arc<Vec<(usize, Vec<u8>)>>;

/// A single slot in the live/session grid.
#[derive(Debug, Clone)]
pub struct SessionSlot {
    /// Identifier of the clip assigned to this slot.
    pub clip_id: String,
    /// Whether this slot takes part in scene launches.
    pub play_enabled: bool,
}

impl SessionSlot {
    pub fn new(clip_id: String) -> Self {
        Self {
            clip_id,
            play_enabled: true,
        }
    }
}

pub(crate) struct TrackIoCounts {
    audio_ins: usize,
    audio_outs: usize,
    midi_ins: usize,
    midi_outs: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct AudioClipBuffer {
    channels: usize,
    samples: Vec<f32>,
}

#[cfg(unix)]
#[derive(Debug)]
pub(crate) struct ClipPitchShifter {
    shifter: LivePitchShifter,
}

#[derive(Debug)]
pub(crate) struct ClipPluginRuntime {
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
                .and_then(|instance| instance.processor.audio_outputs().get(port).cloned())
                .ok_or_else(|| format!("Invalid clip CLAP output port: {id}:{port}")),
            PluginGraphNode::Vst3PluginInstance(id) => self
                .vst3_plugins
                .iter()
                .find(|instance| instance.id == *id)
                .and_then(|instance| instance.processor.audio_outputs().get(port).cloned())
                .ok_or_else(|| format!("Invalid clip VST3 output port: {id}:{port}")),
            #[cfg(all(unix, not(target_os = "macos")))]
            PluginGraphNode::Lv2PluginInstance(id) => self
                .lv2_plugins
                .iter()
                .find(|instance| instance.id == *id)
                .and_then(|instance| instance.processor.audio_outputs().get(port).cloned())
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
                .and_then(|instance| instance.processor.audio_inputs().get(port).cloned())
                .ok_or_else(|| format!("Invalid clip CLAP input port: {id}:{port}")),
            PluginGraphNode::Vst3PluginInstance(id) => self
                .vst3_plugins
                .iter()
                .find(|instance| instance.id == *id)
                .and_then(|instance| instance.processor.audio_inputs().get(port).cloned())
                .ok_or_else(|| format!("Invalid clip VST3 input port: {id}:{port}")),
            #[cfg(all(unix, not(target_os = "macos")))]
            PluginGraphNode::Lv2PluginInstance(id) => self
                .lv2_plugins
                .iter()
                .find(|instance| instance.id == *id)
                .and_then(|instance| instance.processor.audio_inputs().get(port).cloned())
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
            let mut buffer = source.buffer.lock();
            let len = buffer.len().min(request_len);
            buffer.fill(0.0);
            buffer[..len].copy_from_slice(&samples[..len]);
            source.finished.store(true, Ordering::Release);
        }
        for source in self.input_sources.iter().skip(input_blocks.len()) {
            source.buffer.lock().fill(0.0);
            source.finished.store(true, Ordering::Release);
        }

        self.process_plugins_in_graph_order(request_len, &[], &mut HashMap::new());

        let mut outputs = Vec::with_capacity(self.outputs.len());
        for output in &self.outputs {
            if output.ready() {
                output.process();
            } else {
                output.buffer.lock().fill(0.0);
                output.finished.store(true, Ordering::Release);
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
                let processor = self.clap_plugins[idx].processor.clone();
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
                let processor = self.vst3_plugins[idx].processor.clone();
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
                let processor = self.lv2_plugins[idx].processor.clone();
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

    fn plugin_midi_inputs_ready(ports: &[Arc<MIDIIO>]) -> bool {
        ports.iter().all(|port| port.ready())
    }

    fn prepare_plugin_midi_inputs(ports: &[Arc<MIDIIO>]) -> Vec<Vec<MidiEvent>> {
        ports
            .iter()
            .map(|port| {
                // Safety: plan single-writer invariant — this task is the sole
                // writer of its own ports this cycle; sources it reads were
                // produced by earlier plan nodes (LOCKLESS.md Phase 3).
                unsafe { port.process() };
                // Safety: as above — this task just produced the buffer.
                unsafe { port.buffer() }.to_vec()
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct HwMidiOutEvent {
    pub port: usize,
    pub event: MidiEvent,
}

#[derive(Debug, Clone)]
pub struct PendingSessionLaunch {
    pub scene_index: usize,
    pub clip_id: String,
    pub kind: Kind,
    pub launch_at_sample: usize,
    pub loop_enabled: bool,
    pub loop_start_samples: usize,
    pub loop_end_samples: usize,
}

#[derive(Debug, Clone)]
pub struct PlayingSessionClip {
    pub scene_index: usize,
    pub clip_id: String,
    pub kind: Kind,
    pub play_position_samples: usize,
    pub elapsed_samples: usize,
    pub loop_enabled: bool,
    pub loop_start_samples: usize,
    pub loop_end_samples: usize,
    pub stop_at_sample: Option<usize>,
    pub active_midi_notes: HashSet<(u8, u8)>,
}

/// Per-cycle (real-time) state of a [`Track`].
#[derive(Debug)]
pub struct TrackRt {
    pub echoed_parameter_updates: Vec<crate::message::Action>,
    pub pending_hw_midi_out_events: Vec<HwMidiOutEvent>,
    pub pending_modulator_midi_events: Vec<MidiEvent>,
    pub pending_automation_midi_events: Vec<MidiEvent>,
    last_render_block_silent: bool,
    pub process_epoch: usize,
    pub transport_sample: usize,
    pub loop_enabled: bool,
    pub loop_range_samples: Option<(usize, usize)>,
    pub tempo_bpm: f64,
    pub tsig_num: u16,
    pub tsig_denom: u16,
    pub clip_playback_enabled: bool,
    session_clip_playback_enabled: bool,
    output_meter_linear_cache: Vec<f32>,
    meter_peak_hold_linear: Vec<f32>,
    pub record_tap_outs: Vec<Vec<f32>>,
    pub record_tap_midi_in: Vec<MidiEvent>,
    record_tap_enabled: bool,
    audio_clip_cache: HashMap<String, Arc<AudioClipBuffer>>,
    clip_plugin_tracks: HashMap<String, ClipPluginRuntime>,
    #[cfg(unix)]
    pub(crate) clip_pitch_shifters: HashMap<String, ClipPitchShifter>,
    midi_clip_cache: HashMap<String, MidiClipEvents>,
    internal_output_routes_cache: Vec<Vec<Arc<AudioIO>>>,
    audio_route_cache_dirty: bool,
    midi_input_to_out_routes_cache: Vec<Vec<usize>>,
    midi_out_external_targets_cache: Vec<Vec<Arc<MIDIIO>>>,
    midi_route_cache_dirty: bool,
    pub pending_session_launches: Vec<PendingSessionLaunch>,
    pub playing_session_clips: Vec<PlayingSessionClip>,
    pending_session_midi_note_offs: Vec<MidiEvent>,
    pub session_slots: HashMap<usize, SessionSlot>,
    folder_input_midi_events: Vec<Vec<MidiEvent>>,
    folder_plugin_midi_node_events: HashMap<(PluginGraphNode, usize), Vec<MidiEvent>>,
    folder_processed_midi_plugins: HashSet<PluginGraphNode>,
    folder_clip_playback_active: bool,
    folder_record_tap_input_snapshots: Vec<Vec<f32>>,
}

impl TrackRt {
    fn new(audio_outs: usize, buffer_size: usize) -> Self {
        Self {
            echoed_parameter_updates: Vec::new(),
            pending_hw_midi_out_events: vec![],
            pending_modulator_midi_events: vec![],
            pending_automation_midi_events: vec![],
            last_render_block_silent: true,
            process_epoch: 0,
            transport_sample: 0,
            loop_enabled: false,
            loop_range_samples: None,
            tempo_bpm: 120.0,
            tsig_num: 4,
            tsig_denom: 4,
            clip_playback_enabled: true,
            session_clip_playback_enabled: false,
            output_meter_linear_cache: vec![0.0; audio_outs],
            meter_peak_hold_linear: vec![0.0; audio_outs],
            record_tap_outs: vec![vec![0.0; buffer_size]; audio_outs],
            record_tap_midi_in: vec![],
            record_tap_enabled: false,
            audio_clip_cache: HashMap::new(),
            clip_plugin_tracks: HashMap::new(),
            #[cfg(unix)]
            clip_pitch_shifters: HashMap::new(),
            midi_clip_cache: HashMap::new(),
            internal_output_routes_cache: Vec::new(),
            audio_route_cache_dirty: true,
            midi_input_to_out_routes_cache: Vec::new(),
            midi_out_external_targets_cache: Vec::new(),
            midi_route_cache_dirty: true,
            pending_session_launches: Vec::new(),
            playing_session_clips: Vec::new(),
            pending_session_midi_note_offs: Vec::new(),
            session_slots: HashMap::new(),
            folder_input_midi_events: Vec::new(),
            folder_plugin_midi_node_events: HashMap::new(),
            folder_processed_midi_plugins: HashSet::new(),
            folder_clip_playback_active: false,
            folder_record_tap_input_snapshots: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub struct TrackRtCell(UnsafeCell<TrackRt>);

impl TrackRtCell {
    fn new(rt: TrackRt) -> Self {
        Self(UnsafeCell::new(rt))
    }

    /// Mutable RT access is valid only in writer windows guaranteed by the
    /// render plan: dispatcher cycle setup, one track/folder task body, or
    /// the offline bounce worker while live plan cycles are suspended.
    #[allow(clippy::mut_from_ref)]
    pub(crate) fn with_rt<R>(&self, f: impl FnOnce(&mut TrackRt) -> R) -> R {
        f(unsafe { &mut *self.0.get() })
    }

    /// Read-only RT access is for plan windows where no writer is active,
    /// primarily the folder plugin fan-out between FolderInput and
    /// FolderOutput.
    pub(crate) fn rt_read(&self) -> &TrackRt {
        unsafe { &*self.0.get() }
    }
}

impl Deref for TrackRtCell {
    type Target = TrackRt;

    fn deref(&self) -> &Self::Target {
        self.rt_read()
    }
}

impl DerefMut for TrackRtCell {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.get_mut()
    }
}

#[derive(Debug)]
pub struct Track {
    pub rt: TrackRtCell,
    pub name: String,
    // Atomic scalars (5b-iv-1): single-location control values written by the
    // dispatcher and read by the RT plan. Relaxed ordering is sufficient —
    // cross-thread happens-before comes from the plan-dispatch channel and
    // cycle barriers (see LOCKLESS.md).
    pub level: AtomicU32,
    pub balance: AtomicU32,
    pub armed: AtomicBool,
    pub muted: AtomicBool,
    pub phase_inverted: AtomicBool,
    pub soloed: AtomicBool,
    pub is_master: AtomicBool,
    input_monitor: ArcSwap<Vec<bool>>,
    disk_monitor: ArcSwap<Vec<bool>>,
    midi_input_monitor: ArcSwap<Vec<bool>>,
    midi_disk_monitor: ArcSwap<Vec<bool>>,
    pub color: Option<crate::message::TrackColor>,
    pub midi_learn_volume: Option<crate::message::MidiLearnBinding>,
    pub midi_learn_balance: Option<crate::message::MidiLearnBinding>,
    pub midi_learn_mute: Option<crate::message::MidiLearnBinding>,
    pub midi_learn_solo: Option<crate::message::MidiLearnBinding>,
    pub midi_learn_arm: Option<crate::message::MidiLearnBinding>,
    pub midi_learn_input_monitor: Option<crate::message::MidiLearnBinding>,
    pub midi_learn_disk_monitor: Option<crate::message::MidiLearnBinding>,
    pub is_folder: bool,
    pub folder_open: AtomicBool,
    pub parent_track: Option<String>,
    pub child_tracks: Vec<Arc<Track>>,
    pub automation_lanes: serde_json::Value,
    pub automation_mode: AtomicU8,
    pub frozen: AtomicBool,
    midi_lane_channels: ArcSwap<Vec<Option<u8>>>,
    primary_audio_ins: usize,
    primary_audio_outs: usize,
    pub audio: AudioTrack,
    pub midi: MIDITrack,
    pub clap_plugins: Vec<ClapInstance>,
    pub vst3_plugins: Vec<Vst3Instance>,
    #[cfg(all(unix, not(target_os = "macos")))]
    pub lv2_plugins: Vec<Lv2Instance>,
    pub plugin_midi_connections: Vec<PluginGraphConnection>,

    pub next_clap_instance_id: AtomicUsize,
    pub next_vst3_instance_id: AtomicUsize,
    #[cfg(all(unix, not(target_os = "macos")))]
    pub next_lv2_instance_id: AtomicUsize,
    pub next_plugin_instance_id: AtomicUsize,
    pub sample_rate: f64,
    process_block_size: AtomicUsize,
    force_realtime_domain: bool,
    shared_realtime_mixed: bool,
    pub output_enabled: AtomicBool,
    pub metronome_enabled: AtomicBool,
    pub session_base_dir: Option<PathBuf>,
    metronome_source: ArcSwapOption<AudioIO>,
}

pub struct TrackGuard {
    ptr: *mut Track,
}

unsafe impl Send for TrackGuard {}

impl Deref for TrackGuard {
    type Target = Track;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.ptr }
    }
}

impl DerefMut for TrackGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.ptr }
    }
}

impl Track {
    pub fn lock(&self) -> TrackGuard {
        TrackGuard {
            ptr: std::ptr::from_ref(self).cast_mut(),
        }
    }
}

unsafe impl Sync for Track {}
unsafe impl Send for Track {}

impl crate::connectable::AudioPorts for Track {
    fn audio_inputs(&self) -> Vec<Arc<crate::audio::io::AudioIO>> {
        self.audio.ins.clone()
    }

    fn audio_outputs(&self) -> Vec<Arc<crate::audio::io::AudioIO>> {
        self.audio.outs.clone()
    }
}

impl crate::connectable::MidiPorts for Track {
    fn midi_inputs(&self) -> Vec<Arc<crate::midi::io::MIDIIO>> {
        self.midi.ins.clone()
    }

    fn midi_outputs(&self) -> Vec<Arc<crate::midi::io::MIDIIO>> {
        self.midi.outs.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::{AudioClipBuffer, HwMidiOutEvent, Track};
    use crate::audio::clip::AudioClip;
    use crate::audio::io::AudioIO;
    use crate::{kind::Kind, message::PluginGraphNode};
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

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
                .connections()
                .iter()
                .any(|conn| Arc::ptr_eq(conn, &track.midi.outs[0]))
        );
        assert!(
            track.midi.ins[0]
                .connections()
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

        track.set_input_monitor(vec![false]);
        track.process();
        let out = track.audio.outs[0].buffer.lock().to_vec();
        assert_eq!(out[0], 0.0);
        assert_eq!(out[1], 0.0);

        track.set_input_monitor(vec![true]);
        track.process();
        let out = track.audio.outs[0].buffer.lock().to_vec();
        assert_eq!(out[0], 0.5);
        assert_eq!(out[1], -0.25);
    }

    #[test]
    fn clip_playback_audible_with_input_monitor_off() {
        let mut track = Track::new("t".to_string(), 1, 1, 0, 0, 8, 48_000.0);
        track.set_input_monitor(vec![false]);
        track.set_disk_monitor(vec![true]);
        let mut clip = AudioClip::new("clip".to_string(), 0, 4);
        clip.fade_enabled = false;
        track.audio.push_clip(clip);
        track.rt.audio_clip_cache.insert(
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
        track.set_input_monitor(vec![false]);
        track.set_disk_monitor(vec![true]);
        track.armed.store(true, Ordering::Relaxed);
        track.rt.record_tap_enabled = true;
        let source = Arc::new(AudioIO::new(8));
        source.buffer.lock()[0] = 0.5;
        source.buffer.lock()[1] = -0.25;
        AudioIO::connect(&source, &track.audio.ins[0]);

        track.process();

        assert_eq!(track.rt.record_tap_outs[0][0], 0.5);
        assert_eq!(track.rt.record_tap_outs[0][1], -0.25);
    }

    #[test]
    fn record_tap_falls_back_to_direct_input_when_no_internal_route_exists() {
        let mut track = Track::new("t".to_string(), 1, 1, 0, 0, 8, 48_000.0);
        track.set_input_monitor(vec![false]);
        track.set_disk_monitor(vec![true]);
        track.armed.store(true, Ordering::Relaxed);
        track.rt.record_tap_enabled = true;
        track.clear_default_passthrough();
        let source = Arc::new(AudioIO::new(8));
        source.buffer.lock()[0] = 0.25;
        source.buffer.lock()[1] = -0.5;
        AudioIO::connect(&source, &track.audio.ins[0]);

        track.process();

        assert_eq!(track.rt.record_tap_outs[0][0], 0.25);
        assert_eq!(track.rt.record_tap_outs[0][1], -0.5);
    }

    #[test]
    fn clip_playback_respects_clip_playback_enabled_flag() {
        let mut track = Track::new("t".to_string(), 1, 1, 0, 0, 8, 48_000.0);
        track.set_input_monitor(vec![false]);
        track.set_disk_monitor(vec![true]);
        track.rt.clip_playback_enabled = false;
        let mut clip = AudioClip::new("clip".to_string(), 0, 4);
        clip.fade_enabled = false;
        track.audio.push_clip(clip);
        track.rt.audio_clip_cache.insert(
            "clip".to_string(),
            Arc::new(AudioClipBuffer {
                channels: 1,
                samples: vec![0.8, 0.0, 0.0, 0.0],
            }),
        );

        track.process();
        let out = track.audio.outs[0].buffer.lock().to_vec();
        assert_eq!(out[0], 0.0);

        track.rt.clip_playback_enabled = true;
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
        track.set_input_monitor(vec![true; 2]);
        track.set_disk_monitor(vec![false; 2]);

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
        assert_eq!(track.rt.tempo_bpm, 1.0);
        assert_eq!(track.rt.tsig_num, 1);
        assert_eq!(track.rt.tsig_denom, 1);

        track.set_loop_config(true, Some((128, 256)));
        assert!(track.rt.loop_enabled);
        assert_eq!(track.rt.loop_range_samples, Some((128, 256)));
    }

    #[test]
    fn cycle_segments_wrap_across_loop_boundary() {
        let mut track = Track::new("t".to_string(), 1, 1, 0, 0, 8, 48_000.0);
        track.rt.transport_sample = 14;
        track.rt.loop_enabled = true;
        track.rt.loop_range_samples = Some((10, 16));

        let segments = track.cycle_segments(6);
        assert_eq!(segments, vec![(14, 16, 0), (10, 14, 2)]);
    }

    #[test]
    fn offline_bounce_restores_transport_and_monitor_state() {
        let mut track = Track::new("t".to_string(), 1, 1, 0, 0, 8, 48_000.0);
        track.rt.transport_sample = 123;
        track.set_disk_monitor(vec![false]);
        track.set_input_monitor(vec![true]);
        track.rt.clip_playback_enabled = false;
        track.set_output_enabled(false);
        track.rt.loop_enabled = true;
        track.rt.loop_range_samples = Some((32, 64));
        track.armed.store(true, Ordering::Relaxed);
        track.rt.pending_hw_midi_out_events.push(HwMidiOutEvent {
            port: 0,
            event: crate::midi::io::MidiEvent::new(0, vec![0x90, 60, 100]),
        });

        let (channels, rendered) = track.offline_bounce_interleaved(0, 4);
        assert_eq!(channels, 1);
        assert_eq!(rendered.len(), 4);

        assert_eq!(track.rt.transport_sample, 123);
        assert!(!track.disk_monitor().first().copied().unwrap_or(false));
        assert!(track.input_monitor().first().copied().unwrap_or(false));
        assert!(!track.rt.clip_playback_enabled);
        assert!(!track.output_enabled());
        assert!(track.rt.loop_enabled);
        assert_eq!(track.rt.loop_range_samples, Some((32, 64)));
        assert!(track.armed());
        assert_eq!(track.rt.pending_hw_midi_out_events.len(), 1);
    }

    #[test]
    fn midi_only_track_clip_playback_generates_hw_midi_events() {
        let mut track = Track::new("t".to_string(), 0, 0, 1, 1, 8, 48_000.0);
        track.set_disk_monitor(vec![true]);
        track.rt.clip_playback_enabled = true;
        track.midi.push_clip(crate::midi::clip::MIDIClip::new(
            "clip.mid".to_string(),
            0,
            8,
        ));
        track.rt.midi_clip_cache.insert(
            "clip.mid".to_string(),
            Arc::new(vec![(0, vec![0x90, 60, 100])]),
        );

        track.process();

        assert_eq!(track.rt.pending_hw_midi_out_events.len(), 2);
        assert_eq!(track.rt.pending_hw_midi_out_events[0].port, 0);
        assert_eq!(
            track.rt.pending_hw_midi_out_events[0].event,
            crate::midi::io::MidiEvent::new(0, vec![0x90, 60, 100])
        );
        assert_eq!(
            track.rt.pending_hw_midi_out_events[1].event,
            crate::midi::io::MidiEvent::new(7, vec![0x80, 60, 64])
        );
    }

    #[test]
    fn midi_clip_emits_note_off_at_exact_clip_end() {
        let mut track = Track::new("t".to_string(), 0, 0, 1, 1, 8, 48_000.0);
        track.set_disk_monitor(vec![true]);
        track.rt.clip_playback_enabled = true;
        track.midi.push_clip(crate::midi::clip::MIDIClip::new(
            "clip.mid".to_string(),
            0,
            8,
        ));
        track.rt.midi_clip_cache.insert(
            "clip.mid".to_string(),
            Arc::new(vec![(0, vec![0x90, 60, 100]), (8, vec![0x80, 60, 64])]),
        );

        track.process();

        assert_eq!(track.rt.pending_hw_midi_out_events.len(), 2);
        assert_eq!(
            track.rt.pending_hw_midi_out_events[0].event,
            crate::midi::io::MidiEvent::new(0, vec![0x90, 60, 100])
        );
        assert_eq!(
            track.rt.pending_hw_midi_out_events[1].event,
            crate::midi::io::MidiEvent::new(7, vec![0x80, 60, 64])
        );
    }

    #[test]
    fn midi_clip_emits_note_off_at_exact_loop_end() {
        let mut track = Track::new("t".to_string(), 0, 0, 1, 1, 8, 48_000.0);
        track.set_disk_monitor(vec![true]);
        track.rt.clip_playback_enabled = true;
        track.rt.loop_enabled = true;
        track.rt.loop_range_samples = Some((0, 8));
        track.midi.push_clip(crate::midi::clip::MIDIClip::new(
            "clip.mid".to_string(),
            0,
            8,
        ));
        track.rt.midi_clip_cache.insert(
            "clip.mid".to_string(),
            Arc::new(vec![(0, vec![0x90, 60, 100]), (8, vec![0x80, 60, 64])]),
        );

        track.process();

        assert_eq!(track.rt.pending_hw_midi_out_events.len(), 2);
        assert_eq!(
            track.rt.pending_hw_midi_out_events[0].event,
            crate::midi::io::MidiEvent::new(0, vec![0x90, 60, 100])
        );
        assert_eq!(
            track.rt.pending_hw_midi_out_events[1].event,
            crate::midi::io::MidiEvent::new(7, vec![0x80, 60, 64])
        );
    }

    #[test]
    fn midi_clip_orders_loop_boundary_note_off_before_next_note_on() {
        let mut track = Track::new("t".to_string(), 0, 0, 1, 1, 4, 48_000.0);
        track.set_disk_monitor(vec![true]);
        track.rt.clip_playback_enabled = true;
        track.rt.transport_sample = 6;
        track.rt.loop_enabled = true;
        track.rt.loop_range_samples = Some((0, 8));
        track.midi.push_clip(crate::midi::clip::MIDIClip::new(
            "clip.mid".to_string(),
            0,
            8,
        ));
        track.rt.midi_clip_cache.insert(
            "clip.mid".to_string(),
            Arc::new(vec![(0, vec![0x90, 60, 100]), (8, vec![0x80, 60, 64])]),
        );

        track.process();

        assert_eq!(track.rt.pending_hw_midi_out_events.len(), 2);
        assert_eq!(
            track.rt.pending_hw_midi_out_events[0].event,
            crate::midi::io::MidiEvent::new(2, vec![0x80, 60, 64])
        );
        assert_eq!(
            track.rt.pending_hw_midi_out_events[1].event,
            crate::midi::io::MidiEvent::new(2, vec![0x90, 60, 100])
        );
    }

    #[test]
    fn midi_clip_sends_note_off_at_clip_end_when_source_note_ends_later() {
        let mut track = Track::new("t".to_string(), 0, 0, 1, 1, 8, 48_000.0);
        track.set_disk_monitor(vec![true]);
        track.rt.clip_playback_enabled = true;
        track.midi.push_clip(crate::midi::clip::MIDIClip::new(
            "clip.mid".to_string(),
            0,
            8,
        ));
        track.rt.midi_clip_cache.insert(
            "clip.mid".to_string(),
            Arc::new(vec![(0, vec![0x90, 60, 100]), (12, vec![0x80, 60, 64])]),
        );

        track.process();

        assert_eq!(track.rt.pending_hw_midi_out_events.len(), 2);
        assert_eq!(
            track.rt.pending_hw_midi_out_events[0].event,
            crate::midi::io::MidiEvent::new(0, vec![0x90, 60, 100])
        );
        assert_eq!(
            track.rt.pending_hw_midi_out_events[1].event,
            crate::midi::io::MidiEvent::new(7, vec![0x80, 60, 64])
        );
    }

    #[test]
    fn midi_clip_orders_synthetic_loop_note_off_before_next_note_on() {
        let mut track = Track::new("t".to_string(), 0, 0, 1, 1, 4, 48_000.0);
        track.set_disk_monitor(vec![true]);
        track.rt.clip_playback_enabled = true;
        track.rt.transport_sample = 6;
        track.rt.loop_enabled = true;
        track.rt.loop_range_samples = Some((0, 8));
        track.midi.push_clip(crate::midi::clip::MIDIClip::new(
            "clip.mid".to_string(),
            0,
            8,
        ));
        track.rt.midi_clip_cache.insert(
            "clip.mid".to_string(),
            Arc::new(vec![(0, vec![0x90, 60, 100]), (12, vec![0x80, 60, 64])]),
        );

        track.process();

        assert_eq!(track.rt.pending_hw_midi_out_events.len(), 2);
        assert_eq!(
            track.rt.pending_hw_midi_out_events[0].event,
            crate::midi::io::MidiEvent::new(2, vec![0x80, 60, 64])
        );
        assert_eq!(
            track.rt.pending_hw_midi_out_events[1].event,
            crate::midi::io::MidiEvent::new(2, vec![0x90, 60, 100])
        );
    }

    #[test]
    fn midi_lane_channel_filters_monitored_input() {
        let mut track = Track::new("t".to_string(), 0, 0, 1, 1, 8, 48_000.0);
        track.set_midi_input_monitor(vec![true]);
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
        assert_eq!(track.rt.record_tap_midi_in.len(), 3);
    }

    #[test]
    fn midi_lane_channel_omni_does_not_filter_input() {
        let mut track = Track::new("t".to_string(), 0, 0, 1, 1, 8, 48_000.0);
        track.set_midi_input_monitor(vec![true]);
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
        track.set_input_monitor(vec![false]);
        track.set_disk_monitor(vec![true]);

        let mut active_child = AudioClip::new("active".to_string(), 0, 4);
        active_child.fade_enabled = false;
        let mut muted_child = AudioClip::new("muted".to_string(), 0, 4);
        muted_child.fade_enabled = false;
        muted_child.muted = true;

        let mut group = AudioClip::new("group".to_string(), 0, 4);
        group.fade_enabled = false;
        group.grouped_clips = vec![active_child, muted_child];
        track.audio.push_clip(group);
        track.rt.audio_clip_cache.insert(
            "active".to_string(),
            Arc::new(AudioClipBuffer {
                channels: 1,
                samples: vec![0.6, 0.0, 0.0, 0.0],
            }),
        );
        track.rt.audio_clip_cache.insert(
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
        let child = Arc::new(Track::new("Child".to_string(), 1, 1, 0, 0, 8, 48_000.0));
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
        let child = Arc::new(Track::new("Child".to_string(), 1, 1, 0, 0, 8, 48_000.0));
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
        let child = Arc::new(Track::new("Child".to_string(), 0, 0, 1, 1, 8, 48_000.0));
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
                .sources()
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
        let track = Track::new_folder("folder".to_string(), 2, 2, 0, 0, 64, 48_000.0);
        assert!(!track.is_master());

        track.toggle_master();
        assert!(!track.is_master());

        track.set_master(true);
        assert!(!track.is_master());
    }

    #[test]
    fn master_track_can_be_unmastered_even_when_folder() {
        let track = Track::new_folder("folder".to_string(), 2, 2, 0, 0, 64, 48_000.0);
        track.is_master.store(true, Ordering::Relaxed);

        track.toggle_master();
        assert!(!track.is_master());

        track.set_master(true);
        assert!(!track.is_master());
    }

    #[test]
    fn normal_track_can_be_toggled_master() {
        let track = Track::new("t".to_string(), 2, 2, 0, 0, 64, 48_000.0);
        assert!(!track.is_master());

        track.toggle_master();
        assert!(track.is_master());

        track.toggle_master();
        assert!(!track.is_master());
    }

    #[test]
    fn quantize_sample_to_boundary_beat() {
        let sample_rate = 48_000.0;
        let bpm = 120.0;
        // One quarter note at 120 BPM, 48 kHz = 24000 samples.
        assert_eq!(
            Track::quantize_sample_to_boundary(
                100,
                crate::message::LaunchQuantization::Beat,
                bpm,
                4,
                4,
                sample_rate
            ),
            24_000
        );
    }

    #[test]
    fn quantize_sample_to_boundary_bar() {
        let sample_rate = 48_000.0;
        let bpm = 120.0;
        // One bar (4 beats) = 96000 samples.
        assert_eq!(
            Track::quantize_sample_to_boundary(
                100,
                crate::message::LaunchQuantization::Bar,
                bpm,
                4,
                4,
                sample_rate
            ),
            96_000
        );
    }

    #[test]
    fn quantize_sample_to_boundary_two_bars() {
        let sample_rate = 48_000.0;
        let bpm = 120.0;
        // Two bars = 192000 samples.
        assert_eq!(
            Track::quantize_sample_to_boundary(
                100,
                crate::message::LaunchQuantization::TwoBars,
                bpm,
                4,
                4,
                sample_rate
            ),
            192_000
        );
    }

    #[test]
    fn quantize_sample_to_boundary_none_returns_input() {
        let sample_rate = 48_000.0;
        let bpm = 120.0;
        assert_eq!(
            Track::quantize_sample_to_boundary(
                12345,
                crate::message::LaunchQuantization::None,
                bpm,
                4,
                4,
                sample_rate
            ),
            12345
        );
    }

    #[test]
    fn quantize_sample_to_boundary_uses_min_bpm_when_zero() {
        // Zero BPM is clamped to 1.0 inside the function, so one beat at 48 kHz
        // becomes 2_880_000 samples and sample 100 rounds up to that boundary.
        assert_eq!(
            Track::quantize_sample_to_boundary(
                100,
                crate::message::LaunchQuantization::Beat,
                0.0,
                4,
                4,
                48_000.0
            ),
            2_880_000
        );
    }

    #[test]
    fn session_audio_launch_plays_referenced_clip() {
        let mut track = Track::new("t".to_string(), 1, 1, 0, 0, 8, 48_000.0);
        track.set_input_monitor(vec![false]);
        track.set_disk_monitor(vec![true]);
        track.rt.clip_playback_enabled = true;

        // Place the arrangement clip far after the current transport so only the
        // session playback path contributes audio.
        let mut clip = AudioClip::new("session_clip".to_string(), 1000, 1004);
        clip.id = "clip-1".to_string();
        clip.fade_enabled = false;
        track.audio.push_clip(clip);

        track.rt.audio_clip_cache.insert(
            "session_clip".to_string(),
            Arc::new(AudioClipBuffer {
                channels: 1,
                samples: vec![0.8, 0.0, 0.0, 0.0],
            }),
        );

        track.schedule_session_launch(super::PendingSessionLaunch {
            scene_index: 0,
            clip_id: "clip-1".to_string(),
            kind: Kind::Audio,
            launch_at_sample: 0,
            loop_enabled: false,
            loop_start_samples: 0,
            loop_end_samples: 0,
        });

        track.process();

        let out = track.audio.outs[0].buffer.lock().to_vec();
        assert_eq!(out[0], 0.8);
    }

    #[test]
    fn session_audio_stop_at_boundary_halts_playback() {
        let mut track = Track::new("t".to_string(), 1, 1, 0, 0, 8, 48_000.0);
        track.set_input_monitor(vec![false]);
        track.set_disk_monitor(vec![true]);
        track.rt.clip_playback_enabled = true;

        let mut clip = AudioClip::new("session_clip".to_string(), 1000, 1016);
        clip.id = "clip-1".to_string();
        clip.fade_enabled = false;
        track.audio.push_clip(clip);

        track.rt.audio_clip_cache.insert(
            "session_clip".to_string(),
            Arc::new(AudioClipBuffer {
                channels: 1,
                samples: vec![0.5; 16],
            }),
        );

        track.schedule_session_launch(super::PendingSessionLaunch {
            scene_index: 0,
            clip_id: "clip-1".to_string(),
            kind: Kind::Audio,
            launch_at_sample: 0,
            loop_enabled: false,
            loop_start_samples: 0,
            loop_end_samples: 0,
        });

        track.process();
        assert_eq!(track.rt.playing_session_clips.len(), 1);
        assert_eq!(track.rt.playing_session_clips[0].play_position_samples, 8);

        track.rt.transport_sample = 8;
        track.schedule_session_stop(0, 8);
        track.process();

        assert!(track.rt.playing_session_clips.is_empty());
    }

    #[test]
    fn session_audio_loop_repeats_clip_content() {
        let mut track = Track::new("t".to_string(), 1, 1, 0, 0, 8, 48_000.0);
        track.set_input_monitor(vec![false]);
        track.set_disk_monitor(vec![true]);
        track.rt.clip_playback_enabled = true;

        let mut clip = AudioClip::new("session_clip".to_string(), 1000, 1004);
        clip.id = "clip-1".to_string();
        clip.fade_enabled = false;
        track.audio.push_clip(clip);

        track.rt.audio_clip_cache.insert(
            "session_clip".to_string(),
            Arc::new(AudioClipBuffer {
                channels: 1,
                samples: vec![0.1, 0.2, 0.3, 0.4],
            }),
        );

        track.schedule_session_launch(super::PendingSessionLaunch {
            scene_index: 0,
            clip_id: "clip-1".to_string(),
            kind: Kind::Audio,
            launch_at_sample: 0,
            loop_enabled: true,
            loop_start_samples: 0,
            loop_end_samples: 4,
        });

        track.process();

        let out = track.audio.outs[0].buffer.lock().to_vec();
        assert_eq!(out[0], 0.1);
        assert_eq!(out[1], 0.2);
        assert_eq!(out[2], 0.3);
        assert_eq!(out[3], 0.4);
        assert_eq!(out[4], 0.1);
        assert_eq!(out[5], 0.2);
        assert_eq!(out[6], 0.3);
        assert_eq!(out[7], 0.4);
    }

    #[test]
    fn session_audio_loop_with_zero_end_loops_full_clip() {
        let mut track = Track::new("t".to_string(), 1, 1, 0, 0, 8, 48_000.0);
        track.set_input_monitor(vec![false]);
        track.set_disk_monitor(vec![true]);
        track.rt.clip_playback_enabled = true;

        let mut clip = AudioClip::new("session_clip".to_string(), 1000, 1004);
        clip.id = "clip-1".to_string();
        clip.fade_enabled = false;
        track.audio.push_clip(clip);

        track.rt.audio_clip_cache.insert(
            "session_clip".to_string(),
            Arc::new(AudioClipBuffer {
                channels: 1,
                samples: vec![0.1, 0.2, 0.3, 0.4],
            }),
        );

        track.schedule_session_launch(super::PendingSessionLaunch {
            scene_index: 0,
            clip_id: "clip-1".to_string(),
            kind: Kind::Audio,
            launch_at_sample: 0,
            loop_enabled: true,
            loop_start_samples: 0,
            loop_end_samples: 0,
        });

        track.process();

        let out = track.audio.outs[0].buffer.lock().to_vec();
        assert_eq!(out[0], 0.1);
        assert_eq!(out[1], 0.2);
        assert_eq!(out[2], 0.3);
        assert_eq!(out[3], 0.4);
        assert_eq!(out[4], 0.1);
        assert_eq!(out[5], 0.2);
        assert_eq!(out[6], 0.3);
        assert_eq!(out[7], 0.4);
    }

    #[test]
    fn session_multiple_clips_mix_together() {
        let mut track = Track::new("t".to_string(), 1, 1, 0, 0, 8, 48_000.0);
        track.set_input_monitor(vec![false]);
        track.set_disk_monitor(vec![true]);
        track.rt.clip_playback_enabled = true;

        let mut clip1 = AudioClip::new("clip1".to_string(), 1000, 1004);
        clip1.id = "id1".to_string();
        clip1.fade_enabled = false;
        track.audio.push_clip(clip1);

        let mut clip2 = AudioClip::new("clip2".to_string(), 1000, 1004);
        clip2.id = "id2".to_string();
        clip2.fade_enabled = false;
        track.audio.push_clip(clip2);

        track.rt.audio_clip_cache.insert(
            "clip1".to_string(),
            Arc::new(AudioClipBuffer {
                channels: 1,
                samples: vec![0.5, 0.0, 0.0, 0.0],
            }),
        );
        track.rt.audio_clip_cache.insert(
            "clip2".to_string(),
            Arc::new(AudioClipBuffer {
                channels: 1,
                samples: vec![0.3, 0.0, 0.0, 0.0],
            }),
        );

        track.schedule_session_launch(super::PendingSessionLaunch {
            scene_index: 0,
            clip_id: "id1".to_string(),
            kind: Kind::Audio,
            launch_at_sample: 0,
            loop_enabled: false,
            loop_start_samples: 0,
            loop_end_samples: 0,
        });
        track.schedule_session_launch(super::PendingSessionLaunch {
            scene_index: 1,
            clip_id: "id2".to_string(),
            kind: Kind::Audio,
            launch_at_sample: 0,
            loop_enabled: false,
            loop_start_samples: 0,
            loop_end_samples: 0,
        });

        track.process();

        let out = track.audio.outs[0].buffer.lock().to_vec();
        assert_eq!(out[0], 0.8);
    }

    #[test]
    fn session_midi_launch_plays_referenced_clip() {
        let mut track = Track::new("t".to_string(), 0, 0, 1, 1, 8, 48_000.0);
        track.set_disk_monitor(vec![true]);
        track.rt.clip_playback_enabled = true;

        let mut clip = crate::midi::clip::MIDIClip::new("session_clip".to_string(), 0, 8);
        clip.id = "clip-1".to_string();
        track.midi.push_clip(clip);

        track.rt.midi_clip_cache.insert(
            "session_clip".to_string(),
            Arc::new(vec![(0, vec![0x90, 60, 100])]),
        );

        track.schedule_session_launch(super::PendingSessionLaunch {
            scene_index: 0,
            clip_id: "clip-1".to_string(),
            kind: Kind::MIDI,
            launch_at_sample: 0,
            loop_enabled: false,
            loop_start_samples: 0,
            loop_end_samples: 0,
        });

        track.process();

        assert!(
            track
                .rt
                .pending_hw_midi_out_events
                .iter()
                .any(|e| e.event.data == vec![0x90, 60, 100])
        );
    }

    #[test]
    fn session_midi_stop_emits_note_off_for_active_notes() {
        let mut track = Track::new("t".to_string(), 0, 0, 1, 1, 8, 48_000.0);
        track.set_disk_monitor(vec![true]);
        track.rt.clip_playback_enabled = true;

        let mut clip = crate::midi::clip::MIDIClip::new("session_clip".to_string(), 0, 16);
        clip.id = "clip-1".to_string();
        track.midi.push_clip(clip);

        track.rt.midi_clip_cache.insert(
            "session_clip".to_string(),
            Arc::new(vec![(0, vec![0x90, 60, 100])]),
        );

        track.schedule_session_launch(super::PendingSessionLaunch {
            scene_index: 0,
            clip_id: "clip-1".to_string(),
            kind: Kind::MIDI,
            launch_at_sample: 0,
            loop_enabled: false,
            loop_start_samples: 0,
            loop_end_samples: 0,
        });

        track.process();
        assert!(
            track
                .rt
                .pending_hw_midi_out_events
                .iter()
                .any(|e| e.event.data == vec![0x90, 60, 100])
        );

        track.rt.transport_sample = 8;
        track.schedule_session_stop(0, 8);
        track.process();

        assert!(
            track
                .rt
                .pending_hw_midi_out_events
                .iter()
                .any(|e| e.event.data == vec![0x80, 60, 64])
        );
    }

    #[test]
    fn session_launch_with_missing_clip_is_ignored() {
        let mut track = Track::new("t".to_string(), 1, 1, 0, 0, 8, 48_000.0);
        track.set_input_monitor(vec![false]);
        track.set_disk_monitor(vec![true]);
        track.rt.clip_playback_enabled = true;

        track.schedule_session_launch(super::PendingSessionLaunch {
            scene_index: 0,
            clip_id: "missing-id".to_string(),
            kind: Kind::Audio,
            launch_at_sample: 0,
            loop_enabled: false,
            loop_start_samples: 0,
            loop_end_samples: 0,
        });

        track.process();

        assert!(track.rt.playing_session_clips.is_empty());
        let out = track.audio.outs[0].buffer.lock().to_vec();
        assert_eq!(out[0], 0.0);
    }

    #[test]
    fn session_launch_same_scene_replaces_playing_clip() {
        let mut track = Track::new("t".to_string(), 1, 1, 0, 0, 8, 48_000.0);
        track.set_input_monitor(vec![false]);
        track.set_disk_monitor(vec![true]);
        track.rt.clip_playback_enabled = true;

        let mut clip1 = AudioClip::new("clip1".to_string(), 1000, 1008);
        clip1.id = "id1".to_string();
        clip1.fade_enabled = false;
        track.audio.push_clip(clip1);

        let mut clip2 = AudioClip::new("clip2".to_string(), 1000, 1008);
        clip2.id = "id2".to_string();
        clip2.fade_enabled = false;
        track.audio.push_clip(clip2);

        track.rt.audio_clip_cache.insert(
            "clip1".to_string(),
            Arc::new(AudioClipBuffer {
                channels: 1,
                samples: vec![0.1; 8],
            }),
        );
        track.rt.audio_clip_cache.insert(
            "clip2".to_string(),
            Arc::new(AudioClipBuffer {
                channels: 1,
                samples: vec![0.7; 8],
            }),
        );

        track
            .rt
            .playing_session_clips
            .push(super::PlayingSessionClip {
                scene_index: 0,
                clip_id: "id1".to_string(),
                kind: Kind::Audio,
                play_position_samples: 4,
                elapsed_samples: 4,
                loop_enabled: false,
                loop_start_samples: 0,
                loop_end_samples: 0,
                stop_at_sample: None,
                active_midi_notes: std::collections::HashSet::new(),
            });

        track.schedule_session_launch(super::PendingSessionLaunch {
            scene_index: 0,
            clip_id: "id2".to_string(),
            kind: Kind::Audio,
            launch_at_sample: 0,
            loop_enabled: false,
            loop_start_samples: 0,
            loop_end_samples: 0,
        });

        track.process();

        assert_eq!(track.rt.playing_session_clips.len(), 1);
        assert_eq!(track.rt.playing_session_clips[0].clip_id, "id2");
        let out = track.audio.outs[0].buffer.lock().to_vec();
        assert_eq!(out[0], 0.7);
    }

    #[test]
    fn folder_output_passes_child_session_clip_audio() {
        let mut child = Track::new("child".to_string(), 1, 1, 0, 0, 8, 48_000.0);
        child.set_input_monitor(vec![false]);
        child.set_disk_monitor(vec![true]);
        child.rt.clip_playback_enabled = false;
        child.rt.session_clip_playback_enabled = true;

        let mut clip = AudioClip::new("session_clip".to_string(), 1000, 1004);
        clip.id = "clip-1".to_string();
        clip.fade_enabled = false;
        child.audio.push_clip(clip);
        child.rt.audio_clip_cache.insert(
            "session_clip".to_string(),
            Arc::new(AudioClipBuffer {
                channels: 1,
                samples: vec![0.1, 0.2, 0.3, 0.4],
            }),
        );
        child.schedule_session_launch(super::PendingSessionLaunch {
            scene_index: 0,
            clip_id: "clip-1".to_string(),
            kind: Kind::Audio,
            launch_at_sample: 0,
            loop_enabled: true,
            loop_start_samples: 0,
            loop_end_samples: 0,
        });

        child.process();
        assert_eq!(child.audio.outs[0].buffer.lock()[0], 0.1);

        let mut folder = Track::new_folder("folder".to_string(), 1, 1, 0, 0, 8, 48_000.0);
        folder.rt.clip_playback_enabled = false;
        folder.rt.session_clip_playback_enabled = true;
        let child_arc = Arc::new(child);
        {
            let child_lock = child_arc.lock();
            AudioIO::connect(&child_lock.audio.outs[0], &folder.audio.outs[0]);
        }
        folder.child_tracks.push(child_arc);
        folder.process();

        let folder_out = folder.audio.outs[0].buffer.lock().to_vec();
        assert!(
            folder_out.iter().any(|&s| s > 0.0),
            "folder output should carry child session clip audio, got {:?}",
            folder_out
        );
    }
}
