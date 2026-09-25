use crate::clap::{ClapParameterInfo, ClapPluginInfo};
pub use crate::connectable::{ConnectableConnection, ConnectableRef};
#[cfg(unix)]
use crate::lv2::Lv2PluginInfo;
use crate::midi::io::MidiEvent;
use crate::state::TrackHandle;
use crate::vst3::Vst3PluginInfo;
use crate::{kind::Kind, modulator::Modulator};
use std::net::SocketAddr;
use std::sync::{Arc, atomic::AtomicBool};
use tokio::sync::mpsc::Sender;

#[derive(Clone, Debug, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TrackColor {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MpeExpressionPoint {
    pub sample_offset: usize,
    /// Pitch bend uses 0..=16383 (8192 center); pressure and timbre use 0..=127.
    pub value: u16,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MpeExpressionCurve {
    pub points: Vec<MpeExpressionPoint>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MpeNoteExpression {
    pub pitch_bend: MpeExpressionCurve,
    pub pressure: MpeExpressionCurve,
    pub timbre: MpeExpressionCurve,
}

#[derive(Clone, Debug)]
pub struct MidiNoteData {
    pub start_sample: usize,
    pub length_samples: usize,
    pub pitch: u8,
    pub velocity: u8,
    pub channel: u8,
    pub mpe: MpeNoteExpression,
}

#[derive(Clone, Debug)]
pub struct MidiControllerData {
    pub sample: usize,
    pub controller: u8,
    pub value: u8,
    pub channel: u8,
}

#[derive(Debug, Clone)]
pub struct MidiRawEventData {
    pub sample: usize,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct JackPortInfo {
    pub name: String,
    pub kind: Kind,
    pub is_input: bool,
    pub is_output: bool,
    pub is_physical: bool,
    pub is_maolan: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct JackConnectionInfo {
    pub source: String,
    pub destination: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct JackGraphInfo {
    pub ports: Vec<JackPortInfo>,
    pub connections: Vec<JackConnectionInfo>,
}

#[derive(Clone, Debug)]
pub struct HwMidiEvent {
    pub device: String,
    pub event: MidiEvent,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct OfflineAutomationPoint {
    pub sample: usize,
    pub value: f32,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum OfflineAutomationTarget {
    Volume,
    Balance,
    MidiCc {
        channel: u8,
        cc: u8,
    },
    #[cfg(unix)]
    Lv2Parameter {
        instance_id: usize,
        index: u32,
        min: f32,
        max: f32,
    },
    Vst3Parameter {
        instance_id: usize,
        param_id: u32,
    },
    ClapParameter {
        instance_id: usize,
        param_id: u32,
        min: f64,
        max: f64,
    },
    MixOsc {
        addr: String,
        path: String,
    },
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct OfflineAutomationLane {
    pub target: OfflineAutomationTarget,
    #[serde(default)]
    pub visible: bool,
    pub points: Vec<OfflineAutomationPoint>,
}

impl OfflineAutomationLane {
    /// Linearly interpolates the lane value at `sample`, clamped to [0, 1].
    /// Returns `None` when the lane has no points.
    pub fn value_at(&self, sample: usize) -> Option<f32> {
        let points = &self.points;
        if points.is_empty() {
            return None;
        }
        if sample <= points[0].sample {
            return Some(points[0].value.clamp(0.0, 1.0));
        }
        let last = points.len().saturating_sub(1);
        if sample >= points[last].sample {
            return Some(points[last].value.clamp(0.0, 1.0));
        }
        for segment in points.windows(2) {
            let left = &segment[0];
            let right = &segment[1];
            if sample < left.sample || sample > right.sample {
                continue;
            }
            let span = right.sample.saturating_sub(left.sample).max(1) as f32;
            let t = (sample.saturating_sub(left.sample) as f32 / span).clamp(0.0, 1.0);
            return Some((left.value + (right.value - left.value) * t).clamp(0.0, 1.0));
        }
        None
    }
}

#[derive(Clone, Debug)]
pub struct OfflineBounceWork {
    pub state: Arc<crate::state::StateSnapshot>,
    pub track_name: String,
    pub output_path: String,
    pub start_sample: usize,
    pub length_samples: usize,
    pub tempo_bpm: f64,
    pub tsig_num: u16,
    pub tsig_denom: u16,
    pub automation_lanes: Vec<OfflineAutomationLane>,
    pub cancel: Arc<AtomicBool>,
    pub apply_fader: bool,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct PitchCorrectionPointData {
    pub start_sample: usize,
    pub length_samples: usize,
    pub detected_midi_pitch: f32,
    pub target_midi_pitch: f32,
    pub clarity: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TempoPoint {
    pub sample: usize,
    pub bpm: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TimeSignaturePoint {
    pub sample: usize,
    pub numerator: u16,
    pub denominator: u16,
}

/// Which pitch detector produced the clip's correction points. Only carried
/// through clip data so the GUI's detector choice survives engine
/// round-trips; the engine itself does not run detection.
#[derive(
    Clone, Copy, Debug, Default, Hash, PartialEq, Eq, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum PitchCorrectionDetector {
    #[default]
    Classic,
    Neural,
}

/// How pitch-corrected audio is produced for a clip. `Shift` pitch-shifts
/// the original recording (live in the engine or offline preview via
/// timestretch); `Resynth` re-synthesizes the clip offline through a neural
/// vocoder and always renders a preview.
#[derive(
    Clone, Copy, Debug, Default, Hash, PartialEq, Eq, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum PitchCorrectionMode {
    #[default]
    Shift,
    Resynth,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct AudioClipData {
    #[serde(default)]
    pub id: String,
    pub name: String,
    pub start: usize,
    pub length: usize,
    pub offset: usize,
    pub input_channel: usize,
    pub muted: bool,
    #[serde(default)]
    pub reversed: bool,
    #[serde(default)]
    pub gain_db: f32,
    pub peaks_file: Option<String>,
    pub fade_enabled: bool,
    pub fade_in_samples: usize,
    pub fade_out_samples: usize,
    pub preview_name: Option<String>,
    pub source_name: Option<String>,
    pub source_offset: Option<usize>,
    pub source_length: Option<usize>,
    pub pitch_correction_points: Vec<PitchCorrectionPointData>,
    pub pitch_correction_frame_likeness: Option<f32>,
    pub pitch_correction_inertia_ms: Option<u16>,
    pub pitch_correction_formant_compensation: Option<bool>,
    #[serde(default)]
    pub pitch_correction_detector: PitchCorrectionDetector,
    #[serde(default)]
    pub pitch_correction_mode: PitchCorrectionMode,
    pub plugin_graph_json: Option<serde_json::Value>,
    pub grouped_clips: Vec<AudioClipData>,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct MidiClipData {
    #[serde(default)]
    pub id: String,
    pub name: String,
    pub start: usize,
    pub length: usize,
    pub offset: usize,
    pub input_channel: usize,
    pub muted: bool,
    #[serde(default)]
    pub reversed: bool,
    pub grouped_clips: Vec<MidiClipData>,
}

/// Generates a new unique clip identifier.
pub fn generate_clip_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[derive(Clone, Debug)]
pub struct ClipMoveFrom {
    pub track_name: String,
    pub clip_index: usize,
}

#[derive(Clone, Debug)]
pub struct ClipMoveTo {
    pub track_name: String,
    pub sample_offset: usize,
    pub input_channel: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PluginGraphNode {
    TrackInput,
    TrackOutput,
    ClapPluginInstance(usize),
    Vst3PluginInstance(usize),
    #[cfg(unix)]
    Lv2PluginInstance(usize),
}

#[derive(Clone, Debug, PartialEq)]
pub struct PluginGraphPlugin {
    pub node: PluginGraphNode,
    pub instance_id: usize,
    pub format: String,
    pub uri: String,
    pub plugin_id: String,
    pub name: String,
    pub main_audio_inputs: usize,
    pub main_audio_outputs: usize,
    pub audio_inputs: usize,
    pub audio_outputs: usize,
    pub midi_inputs: usize,
    pub midi_outputs: usize,
    pub state: Option<serde_json::Value>,
    pub bypassed: bool,
}

#[cfg(unix)]
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Lv2StatePortValue {
    pub index: u32,
    pub value: f32,
}

#[cfg(unix)]
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Lv2StateProperty {
    pub key_uri: String,
    pub type_uri: String,
    pub flags: u32,
    pub value: Vec<u8>,
}

#[cfg(unix)]
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Lv2PluginState {
    pub port_values: Vec<Lv2StatePortValue>,
    pub properties: Vec<Lv2StateProperty>,
}

#[cfg(unix)]
#[derive(Clone, Debug, PartialEq)]
pub struct Lv2ControlPortInfo {
    pub index: u32,
    pub name: String,
    pub min: f32,
    pub max: f32,
    pub value: f32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginGraphConnection {
    pub from_node: PluginGraphNode,
    pub from_port: usize,
    pub to_node: PluginGraphNode,
    pub to_port: usize,
    pub kind: Kind,
}

pub type PluginGraphSnapshot = (Vec<PluginGraphPlugin>, Vec<PluginGraphConnection>);

#[derive(Clone, Debug, Copy, PartialEq, Eq, Hash)]
pub enum PluginKind {
    Clap,
    Vst3,
    #[cfg(unix)]
    Lv2,
}

#[derive(Clone, Debug)]
pub enum ProcessTask {
    Track(TrackHandle),
    FolderInput(TrackHandle),
    FolderOutput(TrackHandle),
    Plugin {
        track: TrackHandle,
        kind: PluginKind,
        index: usize,
    },
}

impl PartialEq for ProcessTask {
    fn eq(&self, other: &Self) -> bool {
        use ProcessTask::*;
        match (self, other) {
            (Track(a), Track(b))
            | (FolderInput(a), FolderInput(b))
            | (FolderOutput(a), FolderOutput(b)) => Arc::ptr_eq(a, b),
            (
                Plugin {
                    track: a,
                    kind: ka,
                    index: ia,
                },
                Plugin {
                    track: b,
                    kind: kb,
                    index: ib,
                },
            ) => Arc::ptr_eq(a, b) && ka == kb && ia == ib,
            _ => false,
        }
    }
}

impl Eq for ProcessTask {}

impl std::hash::Hash for ProcessTask {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        use ProcessTask::*;
        match self {
            Track(t) | FolderInput(t) | FolderOutput(t) => {
                Arc::as_ptr(t).hash(state);
            }
            Plugin { track, kind, index } => {
                Arc::as_ptr(track).hash(state);
                kind.hash(state);
                index.hash(state);
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Vst3GraphNode {
    TrackInput,
    TrackOutput,
    PluginInstance(usize),
}

#[derive(Clone, Debug)]
pub struct Vst3GraphPlugin {
    pub instance_id: usize,
    pub name: String,
    pub path: String,
    pub audio_inputs: usize,
    pub audio_outputs: usize,
    pub parameters: Vec<crate::vst3::port::ParameterInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Vst3GraphConnection {
    pub from_node: Vst3GraphNode,
    pub from_port: usize,
    pub to_node: Vst3GraphNode,
    pub to_port: usize,
    pub kind: Kind,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MidiLearnBinding {
    pub device: Option<String>,
    pub channel: u8,
    pub cc: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TrackMidiLearnTarget {
    Volume,
    Balance,
    Mute,
    Solo,
    Arm,
    InputMonitor,
    DiskMonitor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum GlobalMidiLearnTarget {
    PlayPause,
    Stop,
    RecordToggle,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SessionMidiLearnTarget {
    Slot {
        track_name: String,
        scene_index: usize,
    },
    Scene(usize),
    StopTrack(String),
    StopAll,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionSlotState {
    Stopped,
    Queued,
    Playing,
    Stopping,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchQuantization {
    None,
    Beat,
    Bar,
    TwoBars,
    FourBars,
    EightBars,
    Eighth,
    Sixteenth,
    ThirtySecond,
    SixtyFourth,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TrackAutomationMode {
    #[default]
    Read,
    Touch,
    Latch,
    Write,
}

impl TrackAutomationMode {
    pub fn as_u8(self) -> u8 {
        match self {
            Self::Read => 0,
            Self::Touch => 1,
            Self::Latch => 2,
            Self::Write => 3,
        }
    }

    pub fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Touch,
            2 => Self::Latch,
            3 => Self::Write,
            _ => Self::Read,
        }
    }
}

#[derive(Clone, Debug)]
pub enum SessionAction {
    LaunchClip {
        track_name: String,
        scene_index: usize,
        clip_id: String,
        launch_quantization: LaunchQuantization,
        loop_enabled: bool,
        loop_start_samples: usize,
        loop_end_samples: usize,
    },
    StopClip {
        track_name: String,
        scene_index: usize,
        launch_quantization: LaunchQuantization,
    },
    LaunchScene {
        scene_index: usize,
        launch_quantization: LaunchQuantization,
    },
    StopScene {
        scene_index: usize,
        launch_quantization: LaunchQuantization,
    },
    /// Queue a scene to launch when the longest currently playing clip
    /// finishes its current pass. When the current scene has no clips, it
    /// behaves as a clip whose length is `launch_quantization`.
    /// Queueing a different scene replaces the queue. Playing clips stop
    /// when the queued scene launches.
    QueueScene {
        scene_index: usize,
        launch_quantization: LaunchQuantization,
    },
    StopAllClips,
}

/// Engine state reports and unsolicited events. Formerly `Action` variants
/// the engine echoed back wrapped in `Ok(...)`; now delivered as
/// `Message::Event`.
///
/// `TransportPosition` here is the engine's position *report*; the seek
/// *command* remains `Action::TransportPosition`.
#[derive(Clone, Debug)]
pub enum Event {
    TransportPositionAt {
        sample: usize,
        after_frames: usize,
    },
    HistoryState {
        dirty: bool,
    },
    Log {
        source: String,
        message: String,
    },
    TrackClapStateDirty {
        track_name: String,
        instance_id: usize,
    },
    ClipClapStateDirty {
        track_name: String,
        clip_idx: usize,
        instance_id: usize,
    },
    SessionMidiLearnTriggered {
        target: SessionMidiLearnTarget,
    },
    SessionRuntimeReport {
        track_name: String,
        scene_index: usize,
        state: SessionSlotState,
        play_position_samples: usize,
        elapsed_samples: usize,
    },
    StepRecordMidiNote {
        device: String,
        channel: u8,
        pitch: u8,
        velocity: u8,
    },
    HWInfo {
        channels: usize,
        rate: usize,
        input: bool,
    },
    TrackClapStateSnapshot {
        track_name: String,
        instance_id: usize,
        plugin_id: String,
        state: Box<crate::clap::ClapPluginState>,
    },
    ClipClapStateSnapshot {
        track_name: String,
        clip_idx: usize,
        instance_id: usize,
        plugin_id: String,
        state: Box<crate::clap::ClapPluginState>,
    },
    TrackVst3StateSnapshot {
        track_name: String,
        instance_id: usize,
        state: Box<crate::vst3::state::Vst3PluginState>,
    },
    ClipVst3StateSnapshot {
        track_name: String,
        clip_idx: usize,
        instance_id: usize,
        state: Box<crate::vst3::state::Vst3PluginState>,
    },
    #[cfg(unix)]
    TrackLv2StateSnapshot {
        track_name: String,
        instance_id: usize,
        state: Vec<u8>,
    },
    #[cfg(unix)]
    ClipLv2StateSnapshot {
        track_name: String,
        clip_idx: usize,
        instance_id: usize,
        state: Vec<u8>,
    },
    TrackSnapshotAllClapStatesDone {
        track_name: String,
    },
    TrackClapResourceFiles {
        track_name: String,
        instance_id: usize,
        files: Vec<(u32, String)>,
    },
    ClipClapResourceFiles {
        track_name: String,
        clip_idx: usize,
        instance_id: usize,
        files: Vec<(u32, String)>,
    },
    TransportPosition(usize),
    /// Per-track meter report emitted from the throttled meter publish
    /// path. The bulk meter flow is the shared-memory
    /// `meter_snapshot_producer` triple buffer plus
    /// `QueryReply::MeterSnapshot`; this event keeps the message-channel
    /// path honest for clients that only consume `Event`s.
    TrackMeters {
        track_name: String,
        output_db: Vec<f32>,
    },
    /// Engine automation/modulator level echo. The *command* twin (GUI
    /// fader, OSC `/track/automation_level`) remains
    /// `Action::TrackAutomationLevel`.
    TrackAutomationLevel {
        track_name: String,
        level: f32,
    },
    /// Engine automation/modulator balance echo. The *command* twin (GUI
    /// fader, OSC `/track/automation_balance`) remains
    /// `Action::TrackAutomationBalance`.
    TrackAutomationBalance {
        track_name: String,
        balance: f32,
    },
    /// Result of an offline (freeze/export) bounce job. The inner `Action`
    /// is the finished or canceled `TrackOfflineBounce` command (those stay
    /// `Action` variants since the GUI also sends them).
    OfflineBounceFinished(Box<Result<Action, String>>),
}

/// Answers to `Request*`/`Get*` queries. Formerly `Action` variants echoed
/// in `Ok(...)`; now delivered as `Message::QueryReply`. The queries
/// themselves remain `Action` variants.
#[derive(Clone, Debug)]
pub enum QueryReply {
    TrackList(Vec<String>),
    TransportState {
        sample: usize,
        tempo_bpm: f64,
        playing: bool,
        paused: bool,
        tsig_num: u16,
        tsig_denom: u16,
    },
    MeterSnapshot {
        hw_out_db: Arc<Vec<f32>>,
        track_meters: Arc<Vec<(String, Vec<f32>)>>,
    },
    ClapPlugins(Vec<ClapPluginInfo>),
    ClapPluginsUnavailable {
        error: String,
    },
    Vst3Plugins(Vec<Vst3PluginInfo>),
    Vst3PluginsUnavailable {
        error: String,
    },
    #[cfg(unix)]
    Lv2Plugins(Vec<Lv2PluginInfo>),
    #[cfg(unix)]
    Lv2PluginsUnavailable {
        error: String,
    },
    TrackPluginGraph {
        track_name: String,
        plugins: Vec<PluginGraphPlugin>,
        connections: Vec<PluginGraphConnection>,
        connectable_connections: Vec<ConnectableConnection>,
    },
    TrackVst3Graph {
        track_name: String,
        plugins: Vec<Vst3GraphPlugin>,
        connections: Vec<Vst3GraphConnection>,
    },
    TrackClapParameters {
        track_name: String,
        instance_id: usize,
        parameters: Vec<ClapParameterInfo>,
    },
    ClipClapParameters {
        track_name: String,
        clip_idx: usize,
        instance_id: usize,
        parameters: Vec<ClapParameterInfo>,
    },
    TrackVst3Parameters {
        track_name: String,
        instance_id: usize,
        parameters: Vec<crate::vst3::port::ParameterInfo>,
    },
    ClipVst3Parameters {
        track_name: String,
        clip_idx: usize,
        instance_id: usize,
        parameters: Vec<crate::vst3::port::ParameterInfo>,
    },
    #[cfg(unix)]
    TrackLv2PluginControls {
        track_name: String,
        instance_id: usize,
        controls: Vec<Lv2ControlPortInfo>,
        instance_access_handle: Option<usize>,
    },
    #[cfg(unix)]
    ClipLv2PluginControls {
        track_name: String,
        clip_idx: usize,
        instance_id: usize,
        controls: Vec<Lv2ControlPortInfo>,
        instance_access_handle: Option<usize>,
    },
    TrackClapNoteNames {
        track_name: String,
        note_names: std::collections::HashMap<u8, String>,
    },
    #[cfg(unix)]
    TrackLv2Midnam {
        track_name: String,
        note_names: std::collections::HashMap<u8, String>,
    },
    JackGraph(JackGraphInfo),
    SessionDiagnosticsReport {
        track_count: usize,
        frozen_track_count: usize,
        audio_clip_count: usize,
        midi_clip_count: usize,
        #[cfg(unix)]
        lv2_instance_count: usize,
        vst3_instance_count: usize,
        clap_instance_count: usize,
        pending_requests: usize,
        workers_total: usize,
        workers_ready: usize,
        pending_hw_midi_events: usize,
        playing: bool,
        transport_running: bool,
        transport_sample: usize,
        tempo_bpm: f64,
        sample_rate_hz: usize,
        cycle_samples: usize,
    },
    MidiLearnMappingsReport {
        lines: Vec<String>,
    },
}

#[derive(Clone, Debug)]
pub enum Action {
    /// Seek command (jump the playhead). The engine's position *reports*
    /// are `Event::TransportPosition` / `Event::TransportPositionAt`.
    TransportPosition(usize),
    /// Seek with a sub-cycle anchor (OSC command; the report twin lives in
    /// `Event`).
    TransportPositionAt {
        sample: usize,
        after_frames: usize,
    },
    /// Step-recording note input (OSC/hardware command; engine reports use
    /// `Event::StepRecordMidiNote`).
    StepRecordMidiNote {
        device: String,
        channel: u8,
        pitch: u8,
        velocity: u8,
    },
    Quit,
    Log {
        source: String,
        message: String,
    },
    Play,
    Pause,
    Stop,
    SessionPlay,
    JumpToEnd,
    SetLoopEnabled(bool),
    SetLoopRange(Option<(usize, usize)>),
    SetPunchEnabled(bool),
    SetPunchRange(Option<(usize, usize)>),
    SetMetronomeEnabled(bool),
    SetTempo(f64),
    SetTimeSignature {
        numerator: u16,
        denominator: u16,
    },
    SetTempoMap {
        tempo_points: Vec<TempoPoint>,
        time_signature_points: Vec<TimeSignaturePoint>,
    },
    SetOscEnabled(bool),
    SetClipPlaybackEnabled(bool),
    SetSessionClipPlaybackEnabled(bool),
    SetRecordEnabled(bool),
    SetModulators(Vec<Modulator>),
    SetTrackAutomationLanes {
        track_name: String,
        lanes: serde_json::Value,
        mode: TrackAutomationMode,
    },
    TrackAutomationToggleLane {
        track_name: String,
        target: OfflineAutomationTarget,
    },
    TrackAutomationInsertPoint {
        track_name: String,
        target: OfflineAutomationTarget,
        sample: usize,
        value: f32,
    },
    TrackAutomationDeletePoint {
        track_name: String,
        target: OfflineAutomationTarget,
        sample: usize,
    },
    TrackAutomationSetMode {
        track_name: String,
        mode: TrackAutomationMode,
    },
    SetSessionPath(String),
    BeginHistoryGroup,
    EndHistoryGroup,
    ApplyGroupedActions(Vec<Action>),
    ClearHistory,
    BeginSessionRestore,
    EndSessionRestore,
    AddTrack {
        name: String,
        audio_ins: usize,
        midi_ins: usize,
        audio_outs: usize,
        midi_outs: usize,
        folder: bool,
        mixosc_addr: Option<String>,
    },
    TrackAddAudioInput(String),
    TrackAddAudioOutput(String),
    TrackRemoveAudioInput(String),
    TrackRemoveAudioOutput(String),
    AddClip {
        clip_id: String,
        name: String,
        track_name: String,
        start: usize,
        length: usize,
        offset: usize,
        input_channel: usize,
        muted: bool,
        reversed: bool,
        gain_db: f32,
        peaks_file: Option<String>,
        kind: Kind,
        fade_enabled: bool,
        fade_in_samples: usize,
        fade_out_samples: usize,
        source_name: Option<String>,
        source_offset: Option<usize>,
        source_length: Option<usize>,
        preview_name: Option<String>,
        pitch_correction_points: Vec<PitchCorrectionPointData>,
        pitch_correction_frame_likeness: Option<f32>,
        pitch_correction_inertia_ms: Option<u16>,
        pitch_correction_formant_compensation: Option<bool>,
        pitch_correction_detector: PitchCorrectionDetector,
        pitch_correction_mode: PitchCorrectionMode,
        plugin_graph_json: Option<serde_json::Value>,
    },
    AddGroupedClip {
        track_name: String,
        kind: Kind,
        audio_clip: Option<AudioClipData>,
        midi_clip: Option<MidiClipData>,
    },
    RemoveClip {
        track_name: String,
        kind: Kind,
        clip_indices: Vec<usize>,
    },
    MoveClipToUnused {
        track_name: String,
        kind: Kind,
        clip_indices: Vec<usize>,
    },
    DeleteUnusedClips {
        clip_ids: Vec<String>,
    },
    SetUnusedClips {
        audio: Vec<AudioClipData>,
        midi: Vec<MidiClipData>,
    },
    SetClipFade {
        track_name: String,
        clip_index: usize,
        kind: Kind,
        fade_enabled: bool,
        fade_in_samples: usize,
        fade_out_samples: usize,
    },
    SetClipBounds {
        track_name: String,
        clip_index: usize,
        kind: Kind,
        start: usize,
        length: usize,
        offset: usize,
    },
    SyncClipBounds {
        track_name: String,
        clip_index: usize,
        kind: Kind,
        start: usize,
        length: usize,
        offset: usize,
    },
    SetClipMuted {
        track_name: String,
        clip_index: usize,
        kind: Kind,
        muted: bool,
    },
    SetClipReversed {
        track_name: String,
        clip_index: usize,
        kind: Kind,
        reversed: bool,
    },
    SetClipGainDb {
        track_name: String,
        clip_index: usize,
        kind: Kind,
        gain_db: f32,
    },
    SetClipPluginGraphJson {
        track_name: String,
        clip_index: usize,
        plugin_graph_json: Option<serde_json::Value>,
    },
    SetClipPitchCorrection {
        track_name: String,
        clip_index: usize,
        preview_name: Option<String>,
        source_name: Option<String>,
        source_offset: Option<usize>,
        source_length: Option<usize>,
        pitch_correction_points: Vec<PitchCorrectionPointData>,
        pitch_correction_frame_likeness: Option<f32>,
        pitch_correction_inertia_ms: Option<u16>,
        pitch_correction_formant_compensation: Option<bool>,
    },
    RenameClip {
        track_name: String,
        kind: Kind,
        clip_index: usize,
        new_name: String,
    },
    SetClipIdentity {
        track_name: String,
        kind: Kind,
        clip_index: usize,
        new_id: String,
        new_name: String,
    },
    SetClipSourceName {
        track_name: String,
        kind: Kind,
        clip_index: usize,
        name: String,
    },
    RenameTrack {
        old_name: String,
        new_name: String,
    },
    RemoveTrack(String),
    TrackLevel(String, f32),
    TrackBalance(String, f32),
    /// Automation level command (GUI fader / OSC). The engine's automation
    /// *echoes* are `Event::TrackAutomationLevel`.
    TrackAutomationLevel(String, f32),
    /// Automation balance command (GUI fader / OSC). The engine's
    /// automation *echoes* are `Event::TrackAutomationBalance`.
    TrackAutomationBalance(String, f32),
    TrackMidiCc {
        track_name: String,
        channel: u8,
        cc: u8,
        value: u8,
    },
    RequestMeterSnapshot,
    RequestTrackList,
    RequestTransportState,
    TrackToggleArm(String),
    TrackToggleMute(String),
    TrackTogglePhase(String),
    TrackToggleSolo(String),
    TrackToggleMaster(String),
    TrackToggleInputMonitor {
        track_name: String,
        lane: usize,
    },
    TrackToggleDiskMonitor {
        track_name: String,
        lane: usize,
    },
    TrackToggleMidiInputMonitor {
        track_name: String,
        lane: usize,
    },
    TrackToggleMidiDiskMonitor {
        track_name: String,
        lane: usize,
    },
    TrackSetColor {
        track_name: String,
        color: Option<TrackColor>,
    },
    TrackArmMidiLearn {
        track_name: String,
        target: TrackMidiLearnTarget,
    },
    GlobalArmMidiLearn {
        target: GlobalMidiLearnTarget,
    },
    SessionArmMidiLearn {
        target: SessionMidiLearnTarget,
    },
    TrackSetMidiLearnBinding {
        track_name: String,
        target: TrackMidiLearnTarget,
        binding: Option<MidiLearnBinding>,
    },
    SetGlobalMidiLearnBinding {
        target: GlobalMidiLearnTarget,
        binding: Option<MidiLearnBinding>,
    },
    SetSessionMidiLearnBinding {
        target: SessionMidiLearnTarget,
        binding: Option<MidiLearnBinding>,
    },
    TrackSetFolder {
        track_name: String,
        is_folder: bool,
    },
    TrackSetParent {
        track_name: String,
        parent_name: Option<String>,
    },
    TrackToggleFolder {
        track_name: String,
    },
    TrackSetMidiLaneChannel {
        track_name: String,
        lane: usize,
        channel: Option<u8>,
    },
    TrackSetMpeZone {
        track_name: String,
        manager_channel: u8,
        member_count: u8,
    },
    TrackSetMpePitchBendSensitivity {
        track_name: String,
        channel: u8,
        semitones: u8,
    },
    TrackSetFrozen {
        track_name: String,
        frozen: bool,
    },
    TrackSetSessionSlot {
        track_name: String,
        scene_index: usize,
        clip_id: Option<String>,
    },
    TrackSetSessionSlotPlayEnabled {
        track_name: String,
        scene_index: usize,
        enabled: bool,
    },
    TrackSetSessionSlotStopEnabled {
        track_name: String,
        scene_index: usize,
        enabled: bool,
    },
    TrackOfflineBounce {
        track_name: String,
        output_path: String,
        start_sample: usize,
        length_samples: usize,
        automation_lanes: Vec<OfflineAutomationLane>,
        apply_fader: bool,
    },
    TrackOfflineBounceCancel {
        track_name: String,
    },
    TrackOfflineBounceCancelAll,
    TrackOfflineBounceCanceled {
        track_name: String,
    },
    TrackOfflineBounceProgress {
        track_name: String,
        progress: f32,
        operation: Option<String>,
    },
    PianoKey {
        track_name: String,
        note: u8,
        velocity: u8,
        on: bool,
    },
    ModifyMidiNotes {
        track_name: String,
        clip_index: usize,
        note_indices: Vec<usize>,
        new_notes: Vec<MidiNoteData>,
        old_notes: Vec<MidiNoteData>,
    },
    ModifyMidiControllers {
        track_name: String,
        clip_index: usize,
        controller_indices: Vec<usize>,
        new_controllers: Vec<MidiControllerData>,
        old_controllers: Vec<MidiControllerData>,
    },
    DeleteMidiControllers {
        track_name: String,
        clip_index: usize,
        controller_indices: Vec<usize>,
        deleted_controllers: Vec<(usize, MidiControllerData)>,
    },
    InsertMidiControllers {
        track_name: String,
        clip_index: usize,
        controllers: Vec<(usize, MidiControllerData)>,
    },
    DeleteMidiNotes {
        track_name: String,
        clip_index: usize,
        note_indices: Vec<usize>,
        deleted_notes: Vec<(usize, MidiNoteData)>,
    },
    InsertMidiNotes {
        track_name: String,
        clip_index: usize,
        notes: Vec<(usize, MidiNoteData)>,
    },
    SetStepRecording(bool),
    SetMidiSysExEvents {
        track_name: String,
        clip_index: usize,
        new_sysex_events: Vec<MidiRawEventData>,
        old_sysex_events: Vec<MidiRawEventData>,
    },
    TrackClearDefaultPassthrough {
        track_name: String,
    },
    TrackClearPlugins {
        track_name: String,
    },
    #[cfg(unix)]
    TrackSetLv2PluginState {
        track_name: String,
        instance_id: usize,
        state: Vec<u8>,
    },
    #[cfg(unix)]
    ClipSetLv2PluginState {
        track_name: String,
        clip_idx: usize,
        instance_id: usize,
        state: Vec<u8>,
    },
    #[cfg(unix)]
    TrackLv2SnapshotState {
        track_name: String,
        instance_id: usize,
    },
    #[cfg(unix)]
    ClipLv2SnapshotState {
        track_name: String,
        clip_idx: usize,
        instance_id: usize,
    },
    #[cfg(unix)]
    TrackGetLv2PluginControls {
        track_name: String,
        instance_id: usize,
    },
    #[cfg(unix)]
    ClipGetLv2PluginControls {
        track_name: String,
        clip_idx: usize,
        instance_id: usize,
    },
    #[cfg(unix)]
    TrackGetLv2Midnam {
        track_name: String,
    },
    TrackGetClapNoteNames {
        track_name: String,
    },
    #[cfg(unix)]
    TrackSetLv2ControlValue {
        track_name: String,
        instance_id: usize,
        index: u32,
        value: f32,
    },
    #[cfg(unix)]
    ClipSetLv2ControlValue {
        track_name: String,
        clip_idx: usize,
        instance_id: usize,
        index: u32,
        value: f32,
    },
    TrackGetPluginGraph {
        track_name: String,
        include_state: bool,
    },
    TrackConnectPluginAudio {
        track_name: String,
        from_node: PluginGraphNode,
        from_port: usize,
        to_node: PluginGraphNode,
        to_port: usize,
    },
    TrackConnectPluginMidi {
        track_name: String,
        from_node: PluginGraphNode,
        from_port: usize,
        to_node: PluginGraphNode,
        to_port: usize,
    },
    TrackDisconnectPluginAudio {
        track_name: String,
        from_node: PluginGraphNode,
        from_port: usize,
        to_node: PluginGraphNode,
        to_port: usize,
    },
    TrackDisconnectPluginMidi {
        track_name: String,
        from_node: PluginGraphNode,
        from_port: usize,
        to_node: PluginGraphNode,
        to_port: usize,
    },
    TrackConnectAudio {
        track_name: String,
        from: ConnectableRef,
        from_port: usize,
        to: ConnectableRef,
        to_port: usize,
    },
    TrackDisconnectAudio {
        track_name: String,
        from: ConnectableRef,
        from_port: usize,
        to: ConnectableRef,
        to_port: usize,
    },
    TrackConnectMidi {
        track_name: String,
        from: ConnectableRef,
        from_port: usize,
        to: ConnectableRef,
        to_port: usize,
    },
    TrackDisconnectMidi {
        track_name: String,
        from: ConnectableRef,
        from_port: usize,
        to: ConnectableRef,
        to_port: usize,
    },
    #[cfg(unix)]
    ListLv2Plugins,
    ListVst3Plugins,
    ListClapPlugins,
    ListClapPluginsWithCapabilities,
    TrackSetClapParameter {
        track_name: String,
        instance_id: usize,
        param_id: u32,
        value: f64,
    },
    ClipSetClapParameter {
        track_name: String,
        clip_idx: usize,
        instance_id: usize,
        param_id: u32,
        value: f64,
    },
    ClipGetClapParameters {
        track_name: String,
        clip_idx: usize,
        instance_id: usize,
    },
    TrackSetClapParameterAt {
        track_name: String,
        instance_id: usize,
        param_id: u32,
        value: f64,
        frame: u32,
    },
    TrackBeginClapParameterEdit {
        track_name: String,
        instance_id: usize,
        param_id: u32,
        frame: u32,
    },
    TrackEndClapParameterEdit {
        track_name: String,
        instance_id: usize,
        param_id: u32,
        frame: u32,
    },
    TrackGetClapParameters {
        track_name: String,
        instance_id: usize,
    },
    TrackClapSnapshotState {
        track_name: String,
        instance_id: usize,
    },
    ClipClapSnapshotState {
        track_name: String,
        clip_idx: usize,
        instance_id: usize,
    },
    TrackClapRestoreState {
        track_name: String,
        instance_id: usize,
        state: crate::clap::ClapPluginState,
    },
    ClipClapRestoreState {
        track_name: String,
        clip_idx: usize,
        instance_id: usize,
        state: crate::clap::ClapPluginState,
    },
    TrackSnapshotAllClapStates {
        track_name: String,
    },
    TrackLoadClapPlugin {
        track_name: String,
        plugin_id: String,
        instance_id: Option<usize>,
    },
    TrackUnloadClapPlugin {
        track_name: String,
        plugin_id: String,
    },
    TrackUnloadClapPluginInstance {
        track_name: String,
        instance_id: usize,
    },
    TrackShowClapGui {
        track_name: String,
        instance_id: usize,
    },
    ClipShowClapGui {
        track_name: String,
        clip_idx: usize,
        instance_id: usize,
    },
    TrackLoadVst3Plugin {
        track_name: String,
        plugin_id: String,
        instance_id: Option<usize>,
    },
    TrackUnloadVst3Plugin {
        track_name: String,
        plugin_id: String,
    },
    TrackUnloadVst3PluginInstance {
        track_name: String,
        instance_id: usize,
    },
    TrackShowVst3Gui {
        track_name: String,
        instance_id: usize,
    },
    ClipShowVst3Gui {
        track_name: String,
        clip_idx: usize,
        instance_id: usize,
    },
    #[cfg(unix)]
    TrackLoadLv2Plugin {
        track_name: String,
        plugin_uri: String,
        instance_id: Option<usize>,
    },
    #[cfg(unix)]
    TrackUnloadLv2Plugin {
        track_name: String,
        plugin_uri: String,
    },
    #[cfg(unix)]
    TrackUnloadLv2PluginInstance {
        track_name: String,
        instance_id: usize,
    },
    TrackShowLv2Gui {
        track_name: String,
        instance_id: usize,
    },
    ClipShowLv2Gui {
        track_name: String,
        clip_idx: usize,
        instance_id: usize,
    },
    TrackSetPluginResourceDir {
        track_name: String,
        instance_id: usize,
        format: String,
        directory: String,
        shared: bool,
    },
    TrackClapCollectResources {
        track_name: String,
        instance_id: usize,
    },
    ClipSetPluginResourceDir {
        track_name: String,
        clip_idx: usize,
        instance_id: usize,
        format: String,
        directory: String,
        shared: bool,
    },
    ClipClapCollectResources {
        track_name: String,
        clip_idx: usize,
        instance_id: usize,
    },
    TrackGetVst3Graph {
        track_name: String,
    },
    TrackSetVst3Parameter {
        track_name: String,
        instance_id: usize,
        param_id: u32,
        value: f32,
    },
    ClipSetVst3Parameter {
        track_name: String,
        clip_idx: usize,
        instance_id: usize,
        param_id: u32,
        value: f32,
    },
    TrackSetPluginBypassed {
        track_name: String,
        instance_id: usize,
        format: String,
        bypassed: bool,
    },
    TrackGetVst3Parameters {
        track_name: String,
        instance_id: usize,
    },
    ClipGetVst3Parameters {
        track_name: String,
        clip_idx: usize,
        instance_id: usize,
    },
    TrackVst3SnapshotState {
        track_name: String,
        instance_id: usize,
    },
    ClipVst3SnapshotState {
        track_name: String,
        clip_idx: usize,
        instance_id: usize,
    },
    TrackVst3RestoreState {
        track_name: String,
        instance_id: usize,
        state: crate::vst3::state::Vst3PluginState,
    },
    ClipVst3RestoreState {
        track_name: String,
        clip_idx: usize,
        instance_id: usize,
        state: crate::vst3::state::Vst3PluginState,
    },
    TrackConnectVst3Audio {
        track_name: String,
        from_node: Vst3GraphNode,
        from_port: usize,
        to_node: Vst3GraphNode,
        to_port: usize,
    },
    TrackDisconnectVst3Audio {
        track_name: String,
        from_node: Vst3GraphNode,
        from_port: usize,
        to_node: Vst3GraphNode,
        to_port: usize,
    },
    ClipMove {
        kind: Kind,
        from: ClipMoveFrom,
        to: ClipMoveTo,
        copy: bool,
    },
    Connect {
        from_track: String,
        from_port: usize,
        to_track: String,
        to_port: usize,
        kind: Kind,
    },
    Disconnect {
        from_track: String,
        from_port: usize,
        to_track: String,
        to_port: usize,
        kind: Kind,
    },
    OpenAudioDevice {
        device: String,
        input_device: Option<String>,
        sample_rate_hz: i32,
        bits: i32,
        exclusive: bool,
        period_frames: usize,
        nperiods: usize,
        sync_mode: bool,
        actual_period_frames: usize,
        input_channels: usize,
        output_channels: usize,
        bytes_per_frame: usize,
        /// Ring capacity multiplier for streaming clip playback, in periods
        /// per channel ring. 0 means the default (8); clamped to 2..=32.
        ring_buffer_multiplier: usize,
        /// Whether opening the audio device should also auto-open every
        /// discovered MIDI hardware device. Applications that never use
        /// MIDI (e.g. the audio player) should pass false so they don't
        /// grab MIDI nodes other programs may need.
        auto_open_midi_devices: bool,
    },
    JackAddAudioInputPort,
    JackRemoveAudioInputPort(usize),
    JackAddAudioOutputPort,
    JackRemoveAudioOutputPort(usize),
    JackGetGraph,
    JackConnect {
        source: String,
        destination: String,
    },
    JackDisconnect {
        source: String,
        destination: String,
    },
    OpenMidiInputDevice(String),
    OpenMidiOutputDevice(String),
    RequestSessionDiagnostics,
    RequestMidiLearnMappingsReport,
    ClearAllMidiLearnBindings,
    MarkHistorySavePoint,
    Undo,
    Redo,
    Session(SessionAction),
    Panic,
}

#[derive(Clone, Debug)]
pub enum Message {
    Ready(usize),
    TracksFinished,

    /// Dispatch a render-plan node to a worker (Phase 2, see `LOCKLESS.md`).
    NodeJob(crate::executor::NodeJob),
    /// A worker completed a plan node. Stale `epoch`s are dropped.
    NodeDone {
        worker_id: usize,
        epoch: u64,
        node: u32,
        output_linear: Vec<f32>,
        parameter_updates: Vec<Action>,
        latency_changed: bool,
    },
    ProcessOfflineBounce(OfflineBounceWork),
    Channel(Sender<Self>),
    /// Unsolicited state report / event broadcast to clients.
    Event(Event),
    /// Answer to a `Request*`/`Get*` query, delivered to clients.
    QueryReply(QueryReply),

    Request(Action),
    OscRequest {
        action: Action,
        reply_to: SocketAddr,
    },
    Response(Result<Action, String>),
    HWMidiEvents(Vec<HwMidiEvent>),
    HWMidiOutEvents(Vec<HwMidiEvent>),
    ClearHWMidiOutEvents,
    StartAudioPreview {
        samples: Arc<Vec<f32>>,
        channels: usize,
        start_sample: usize,
    },
    StopAudioPreview,
    HWSetPlaying(bool),
    HWSetOutputGainBalance {
        gain: f32,
        balance: f32,
    },
    HWOpenMidiInputDevice(String),
    HWOpenMidiOutputDevice(String),
    HWCloseMidiDevices,
    HWFinished,
    OfflineBounceFinished {
        result: Result<Action, String>,
    },
}

#[cfg(test)]
mod tests {
    use super::{AudioClipData, MidiClipData, PitchCorrectionPointData};
    use serde_json::json;

    #[test]
    fn audio_clip_data_serde_round_trips_nested_groups() {
        let clip = AudioClipData {
            id: String::new(),
            name: "group.wav".to_string(),
            start: 12,
            length: 96,
            offset: 3,
            input_channel: 1,
            muted: true,
            reversed: false,
            gain_db: 0.0,
            peaks_file: Some("peaks/group.json".to_string()),
            fade_enabled: false,
            fade_in_samples: 10,
            fade_out_samples: 20,
            preview_name: Some("preview.wav".to_string()),
            source_name: Some("source.wav".to_string()),
            source_offset: Some(4),
            source_length: Some(88),
            pitch_correction_points: vec![PitchCorrectionPointData {
                start_sample: 7,
                length_samples: 11,
                detected_midi_pitch: 60.1,
                target_midi_pitch: 61.2,
                clarity: 0.8,
            }],
            pitch_correction_frame_likeness: Some(0.5),
            pitch_correction_inertia_ms: Some(123),
            pitch_correction_formant_compensation: Some(false),
            pitch_correction_detector: super::PitchCorrectionDetector::Neural,
            pitch_correction_mode: super::PitchCorrectionMode::Resynth,
            plugin_graph_json: Some(json!({"plugins":[],"connections":[{"kind":"Audio"}]})),
            grouped_clips: vec![AudioClipData {
                name: "child.wav".to_string(),
                start: 0,
                length: 48,
                ..AudioClipData::default()
            }],
        };

        let value = serde_json::to_value(&clip).expect("serialize");
        let restored: AudioClipData = serde_json::from_value(value).expect("deserialize");

        assert_eq!(restored.name, clip.name);
        assert_eq!(restored.preview_name, clip.preview_name);
        assert_eq!(restored.source_name, clip.source_name);
        assert_eq!(restored.plugin_graph_json, clip.plugin_graph_json);
        assert_eq!(restored.grouped_clips.len(), 1);
        assert_eq!(restored.grouped_clips[0].name, "child.wav");
        assert_eq!(restored.pitch_correction_points[0].target_midi_pitch, 61.2);
    }

    #[test]
    fn midi_clip_data_serde_round_trips_nested_groups() {
        let clip = MidiClipData {
            id: String::new(),
            name: "group.mid".to_string(),
            start: 5,
            length: 64,
            offset: 2,
            input_channel: 3,
            muted: true,
            reversed: false,
            grouped_clips: vec![MidiClipData {
                name: "child.mid".to_string(),
                start: 0,
                length: 32,
                ..MidiClipData::default()
            }],
        };

        let value = serde_json::to_value(&clip).expect("serialize");
        let restored: MidiClipData = serde_json::from_value(value).expect("deserialize");

        assert_eq!(restored.name, clip.name);
        assert_eq!(restored.grouped_clips.len(), 1);
        assert_eq!(restored.grouped_clips[0].name, "child.mid");
    }

    #[test]
    fn pitch_correction_point_data_serde_round_trips() {
        let point = PitchCorrectionPointData {
            start_sample: 10,
            length_samples: 20,
            detected_midi_pitch: 57.5,
            target_midi_pitch: 58.0,
            clarity: 0.9,
        };

        let value = serde_json::to_value(&point).expect("serialize");
        let restored: PitchCorrectionPointData =
            serde_json::from_value(value).expect("deserialize");

        assert_eq!(restored.start_sample, 10);
        assert_eq!(restored.length_samples, 20);
        assert_eq!(restored.detected_midi_pitch, 57.5);
        assert_eq!(restored.target_midi_pitch, 58.0);
        assert_eq!(restored.clarity, 0.9);
    }

    #[test]
    fn audio_clip_data_deserializes_with_omitted_optional_fields() {
        let restored: AudioClipData = serde_json::from_value(json!({
            "name": "clip.wav",
            "start": 1,
            "length": 2,
            "offset": 3,
            "input_channel": 0,
            "muted": false,
            "fade_enabled": true,
            "fade_in_samples": 240,
            "fade_out_samples": 240,
            "pitch_correction_points": [],
            "grouped_clips": []
        }))
        .expect("deserialize");

        assert_eq!(restored.name, "clip.wav");
        assert!(restored.peaks_file.is_none());
        assert!(restored.preview_name.is_none());
        assert!(restored.source_name.is_none());
        assert!(restored.source_offset.is_none());
        assert!(restored.source_length.is_none());
        assert!(restored.pitch_correction_points.is_empty());
        assert!(restored.plugin_graph_json.is_none());
    }
}
