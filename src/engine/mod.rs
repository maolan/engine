use std::{
    collections::{HashMap, VecDeque},
    net::{SocketAddr, UdpSocket},
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
    time::Instant,
};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::task::JoinHandle;

mod hardware;
mod midi;
mod plugins;
mod runtime;
mod topology;
mod transport_record_bounce;

type HwDeviceInfo = (usize, usize, usize, ((usize, usize), (usize, usize)));

pub fn parse_automation_lanes(
    value: &serde_json::Value,
) -> Vec<crate::message::OfflineAutomationLane> {
    serde_json::from_value(value.clone()).unwrap_or_else(|_| {
        if let Some(array) = value.as_array() {
            array
                .iter()
                .filter_map(|item| serde_json::from_value(item.clone()).ok())
                .collect()
        } else {
            vec![]
        }
    })
}

#[cfg(target_os = "linux")]
use crate::hw::alsa::{HwDriver, HwOptions, MidiHub};
#[cfg(target_os = "macos")]
use crate::hw::coreaudio::{HwDriver, HwOptions, MidiHub};
#[cfg(unix)]
use crate::hw::jack::JackRuntime;
#[cfg(target_os = "windows")]
use crate::hw::options::HwOptions;
#[cfg(target_os = "freebsd")]
use crate::hw::oss::{HwDriver, MidiHub};
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
    kind::Kind,
    message::{Action, HwMidiEvent, Message, MidiControllerData, MidiNoteData},
    midi::io::MidiEvent,
    osc::OscServer,
    state::{State, StateSlot},
    workers::worker::NodeJobResult,
};

struct RtProducer<T>(rtrb::Producer<T>);

// Safety: each producer is owned only by the engine task and is never shared
// with the consumer thread. The `Sync` impl only permits the engine future to
// remain `Send` across `.await`; no concurrent access is introduced.
unsafe impl<T: Send> Send for RtProducer<T> {}
unsafe impl<T: Send> Sync for RtProducer<T> {}

impl<T> RtProducer<T> {
    fn push(&mut self, value: T) -> Result<(), rtrb::PushError<T>> {
        self.0.push(value)
    }
}

struct RtConsumer<T>(rtrb::Consumer<T>);

// Safety: each consumer is owned only by the engine task and is never shared
// with the producer thread. See `RtProducer` for the `Sync` rationale.
unsafe impl<T: Send> Send for RtConsumer<T> {}
unsafe impl<T: Send> Sync for RtConsumer<T> {}

impl<T> RtConsumer<T> {
    fn pop(&mut self) -> Result<T, rtrb::PopError> {
        self.0.pop()
    }
}

struct WorkerData {
    tx: Sender<Message>,
    handle: Option<JoinHandle<()>>,
    node_job_tx: Option<RtProducer<crate::executor::NodeJob>>,
    node_result_rx: Option<RtConsumer<NodeJobResult>>,
    node_thread: Option<std::thread::Thread>,
    node_quit: Option<Arc<AtomicBool>>,
}

impl WorkerData {
    pub fn new(tx: Sender<Message>, handle: JoinHandle<()>) -> Self {
        Self {
            tx,
            handle: Some(handle),
            node_job_tx: None,
            node_result_rx: None,
            node_thread: None,
            node_quit: None,
        }
    }

    pub fn with_node_mailbox(
        tx: Sender<Message>,
        handle: JoinHandle<()>,
        node_job_tx: rtrb::Producer<crate::executor::NodeJob>,
        node_result_rx: rtrb::Consumer<NodeJobResult>,
        node_thread: std::thread::Thread,
        node_quit: Arc<AtomicBool>,
    ) -> Self {
        Self {
            tx,
            handle: Some(handle),
            node_job_tx: Some(RtProducer(node_job_tx)),
            node_result_rx: Some(RtConsumer(node_result_rx)),
            node_thread: Some(node_thread),
            node_quit: Some(node_quit),
        }
    }
}

impl Drop for WorkerData {
    fn drop(&mut self) {
        if let Some(quit) = &self.node_quit {
            quit.store(true, std::sync::atomic::Ordering::Release);
        }
        if let Some(thread) = &self.node_thread {
            thread.unpark();
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HwDriverInfo {
    pub cycle_samples: usize,
    pub sample_rate: i32,
    pub input_channels: usize,
    pub output_channels: usize,
    pub sample_bits: i32,
    pub frame_size_bytes: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct RecordingSession {
    start_sample: usize,
    samples: Vec<f32>,
    channels: usize,
    file_name: String,

    stripe_peaks: Vec<Vec<[f32; 2]>>,

    current_stripe_frames: usize,
}

const RECORDING_STRIPE_FRAMES: usize = 256;

#[derive(Debug, Clone)]
pub(crate) struct MidiRecordingSession {
    start_sample: usize,
    events: Vec<(u64, Vec<u8>)>,
    file_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct MidiHwInRoute {
    device: String,
    to_track: String,
    to_port: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct MidiHwOutRoute {
    from_track: String,
    from_port: usize,
    device: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct MidiHwThruRoute {
    from_device: String,
    to_device: String,
}

struct OfflineBounceJob {
    cancel: Arc<AtomicBool>,
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JackTransportPlaySync {
    Start,
    Stop,
}

#[derive(Clone, Copy)]
#[cfg(unix)]
pub(crate) struct AudioOpenRequest<'a> {
    device: &'a str,
    input_device: Option<&'a str>,
    sample_rate_hz: i32,
    bits: i32,
    exclusive: bool,
    period_frames: usize,
    nperiods: usize,
    sync_mode: bool,
}

pub(crate) struct ClipAddRequest<'a> {
    clip_id: &'a str,
    name: &'a str,
    track_name: &'a str,
    start: usize,
    length: usize,
    offset: usize,
    input_channel: usize,
    muted: bool,
    peaks_file: Option<String>,
    kind: Kind,
    fade_enabled: bool,
    fade_in_samples: usize,
    fade_out_samples: usize,
    source_name: Option<String>,
    source_offset: Option<usize>,
    source_length: Option<usize>,
    preview_name: Option<String>,
    pitch_correction_points: Vec<crate::message::PitchCorrectionPointData>,
    pitch_correction_frame_likeness: Option<f32>,
    pitch_correction_inertia_ms: Option<u16>,
    pitch_correction_formant_compensation: Option<bool>,
    plugin_graph_json: Option<serde_json::Value>,
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct JackTransportSyncDecision {
    play_sync: Option<JackTransportPlaySync>,
    position_sync: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MidiLearnSlot {
    Track(String, crate::message::TrackMidiLearnTarget),
    Global(crate::message::GlobalMidiLearnTarget),
    Session(crate::message::SessionMidiLearnTarget),
}

pub struct Engine {
    clients: Vec<Sender<Message>>,
    rx: Receiver<Message>,
    state: Arc<State>,
    state_snapshot: Arc<StateSlot>,
    tx: Sender<Message>,
    workers: Vec<WorkerData>,
    hw_driver: Option<HwDriver>,
    hw_driver_info: Option<HwDriverInfo>,
    hw_input_ports: Vec<Arc<crate::audio::io::AudioIO>>,
    hw_output_ports: Vec<Arc<crate::audio::io::AudioIO>>,
    #[cfg(unix)]
    jack_runtime: Option<JackRuntime>,
    midi_hub: Option<MidiHub>,
    hw_worker: Option<WorkerData>,
    osc_server: Option<OscServer>,
    osc_reply_socket: Option<UdpSocket>,
    osc_reply_target: Option<SocketAddr>,
    pending_hw_midi_events: Vec<MidiEvent>,
    pending_hw_midi_events_by_device: HashMap<String, Vec<MidiEvent>>,
    pending_hw_midi_out_events: Vec<MidiEvent>,
    pending_hw_midi_out_events_by_device: Vec<HwMidiEvent>,
    active_hw_notes_by_track: HashMap<String, std::collections::HashSet<(String, u8, u8)>>,
    active_hw_notes_cycle_start: HashMap<String, std::collections::HashSet<(String, u8, u8)>>,
    midi_hw_in_routes: Vec<MidiHwInRoute>,
    midi_hw_out_routes: Vec<MidiHwOutRoute>,
    midi_hw_thru_routes: Vec<MidiHwThruRoute>,
    ready_workers: Vec<usize>,
    pending_requests: VecDeque<Action>,
    awaiting_hwfinished: bool,
    handling_hwfinished: bool,
    transport_panic_flush_pending: bool,
    transport_restart_pending: bool,
    notified_loop_wrap_sample: Option<usize>,
    transport_sample: usize,

    hw_input_latency_frames: usize,

    hw_output_latency_frames: usize,
    loop_enabled: bool,
    loop_range_samples: Option<(usize, usize)>,
    metronome_enabled: bool,
    tempo_bpm: f64,
    tsig_num: u16,
    tsig_denom: u16,
    tempo_points: Vec<crate::message::TempoPoint>,
    time_signature_points: Vec<crate::message::TimeSignaturePoint>,
    punch_enabled: bool,
    punch_range_samples: Option<(usize, usize)>,
    audio_recordings: std::collections::HashMap<String, RecordingSession>,
    midi_recordings: std::collections::HashMap<String, MidiRecordingSession>,
    completed_audio_recordings: Vec<(String, RecordingSession)>,
    completed_midi_recordings: Vec<(String, MidiRecordingSession)>,
    playing: bool,
    transport_running: bool,
    clip_playback_enabled: bool,
    session_clip_playback_enabled: bool,
    session_transport_sample: usize,
    record_enabled: bool,
    step_recording_enabled: bool,
    session_dir: Option<PathBuf>,
    hw_out_level_db: f32,
    hw_out_balance: f32,
    hw_out_muted: bool,
    last_hw_out_meter_publish: Option<Instant>,
    #[cfg(any(target_os = "freebsd", target_os = "linux", target_os = "openbsd"))]
    last_hw_out_meter_linear: Vec<f32>,
    hw_out_peak_hold_linear: Vec<f32>,
    #[cfg(any(target_os = "freebsd", target_os = "linux", target_os = "openbsd"))]
    hw_out_meter_publish_phase: bool,
    last_track_meter_publish: Option<Instant>,
    last_meter_snapshot_publish: Option<Instant>,
    last_session_report_publish: Option<Instant>,
    track_meter_linear_by_track: HashMap<String, Vec<f32>>,
    meter_snapshot_producer:
        crate::triple_buffer::TripleBufferProducer<crate::meter::MeterSnapshot>,
    transport_snapshot_producer:
        crate::triple_buffer::TripleBufferProducer<crate::meter::TransportSnapshot>,
    session_runtime_snapshot_producer:
        crate::triple_buffer::TripleBufferProducer<crate::meter::SessionRuntimeSnapshot>,
    /// Phase 2 render-plan machinery (see `LOCKLESS.md`): the executor
    /// drives per-cycle node dispatch, the builder thread recompiles and
    /// publishes plans, `pending_node_jobs` buffers jobs when no worker is
    /// free, and `hw_ports` is the snapshot the builder compiles against.
    executor: crate::executor::CycleExecutor,
    plan_slot: Arc<crate::render_plan::PlanSlot>,
    hw_ports: Arc<arc_swap::ArcSwap<crate::plan_builder::HwPorts>>,
    pending_node_jobs: VecDeque<crate::executor::NodeJob>,
    plan_builder: crate::plan_builder::PlanBuilder,
    latest_hw_out_meter_db: Arc<Vec<f32>>,
    latest_track_meter_snapshot: Arc<Vec<(String, Vec<f32>)>>,
    history: History,
    history_group: Option<UndoEntry>,
    history_suspended: bool,
    offline_bounce_jobs: HashMap<String, OfflineBounceJob>,
    /// Bounce jobs registered while a plan cycle was still in flight; the
    /// work is handed to the reserved worker when the cycle completes
    /// (`on_all_tracks_finished`), so the bounce never races RT workers.
    pending_bounce_starts: Vec<(usize, crate::message::OfflineBounceWork)>,
    /// Worker index → track name for in-flight bounce jobs; the worker's
    /// terminal `Ready(id)` removes the job even on error/cancel paths
    /// whose `OfflineBounceFinished` payload carries no track name.
    bounce_worker_tracks: HashMap<usize, String>,
    pending_midi_learn: Option<(String, crate::message::TrackMidiLearnTarget, Option<String>)>,
    pending_global_midi_learn: Option<crate::message::GlobalMidiLearnTarget>,
    pending_session_midi_learn: Option<crate::message::SessionMidiLearnTarget>,
    global_midi_learn_play_pause: Option<crate::message::MidiLearnBinding>,
    global_midi_learn_stop: Option<crate::message::MidiLearnBinding>,
    global_midi_learn_record_toggle: Option<crate::message::MidiLearnBinding>,
    session_midi_learn_slots: HashMap<(String, usize), crate::message::MidiLearnBinding>,
    session_midi_learn_scenes: HashMap<usize, crate::message::MidiLearnBinding>,
    session_midi_learn_stop_track: HashMap<String, crate::message::MidiLearnBinding>,
    session_midi_learn_stop_all: Option<crate::message::MidiLearnBinding>,
    midi_cc_gate: HashMap<(String, u8, u8), bool>,
    modulators: Vec<crate::modulator::Modulator>,
    modulator_values: Option<Arc<std::collections::HashMap<usize, f32>>>,
}

type MidiEditParseResult = (
    Vec<MidiNoteData>,
    Vec<MidiControllerData>,
    Vec<(u64, Vec<u8>)>,
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::clip::AudioClip;
    use crate::message::PluginKind;
    use crate::track::Track;
    use std::path::Path;
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use tokio::sync::mpsc::channel;
    use tokio::time::{Duration as TokioDuration, timeout};

    #[test]
    #[cfg(unix)]
    fn jack_transport_sync_decision_starts_and_syncs_position_on_external_play() {
        let decision = Engine::jack_transport_sync_decision(false, 128, true, 256, 64);

        assert_eq!(decision.play_sync, Some(JackTransportPlaySync::Start));
        assert_eq!(decision.position_sync, Some(256));
    }

    #[test]
    #[cfg(unix)]
    fn jack_transport_sync_decision_stops_and_syncs_position_on_external_stop() {
        let decision = Engine::jack_transport_sync_decision(true, 512, false, 96, 64);

        assert_eq!(decision.play_sync, Some(JackTransportPlaySync::Stop));
        assert_eq!(decision.position_sync, Some(96));
    }

    #[test]
    #[cfg(unix)]
    fn jack_transport_sync_decision_ignores_small_rolling_drift() {
        let decision = Engine::jack_transport_sync_decision(true, 1024, true, 1040, 64);

        assert_eq!(decision.play_sync, None);
        assert_eq!(decision.position_sync, None);
    }

    #[test]
    #[cfg(unix)]
    fn jack_transport_sync_decision_syncs_large_rolling_jump() {
        let decision = Engine::jack_transport_sync_decision(true, 1024, true, 1200, 64);

        assert_eq!(decision.play_sync, None);
        assert_eq!(decision.position_sync, Some(1200));
    }

    #[test]
    #[cfg(unix)]
    fn jack_transport_sync_decision_syncs_locate_while_stopped() {
        let decision = Engine::jack_transport_sync_decision(false, 400, false, 900, 64);

        assert_eq!(decision.play_sync, None);
        assert_eq!(decision.position_sync, Some(900));
    }

    fn make_engine_with_client() -> (Engine, tokio::sync::mpsc::Receiver<Message>) {
        let (engine_tx, engine_rx) = channel(16);
        let mut engine = Engine::new(engine_rx, engine_tx);
        let (client_tx, client_rx) = channel(16);
        engine.clients.push(client_tx);
        (engine, client_rx)
    }

    fn insert_track(engine: &mut Engine, track: Track) {
        engine
            .state
            .lock()
            .tracks
            .insert(track.name.clone(), Arc::new(track));
        engine.publish_state_snapshot();
        engine.plan_builder.mark_dirty();
    }

    fn insert_track_for_modulator_test(engine: &mut Engine, track: Track) {
        engine
            .state
            .lock()
            .tracks
            .insert(track.name.clone(), Arc::new(track));
        engine.publish_state_snapshot();
    }

    fn osc_packet(address: &str) -> Vec<u8> {
        fn push_padded_osc_string(packet: &mut Vec<u8>, value: &str) {
            packet.extend_from_slice(value.as_bytes());
            packet.push(0);
            while !packet.len().is_multiple_of(4) {
                packet.push(0);
            }
        }

        let mut packet = Vec::new();
        push_padded_osc_string(&mut packet, address);
        push_padded_osc_string(&mut packet, ",");
        packet
    }

    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "Tokio runtime uses kqueue, which Miri does not support on FreeBSD"
    )]
    #[tokio::test]
    async fn set_osc_enabled_starts_and_stops_server() {
        let (mut engine, _client_rx) = make_engine_with_client();

        engine
            .set_osc_enabled_with(true, |tx| OscServer::start_on_addr(tx, "127.0.0.1:0"))
            .expect("start osc server on ephemeral port");
        assert!(engine.osc_server.is_some());

        engine
            .set_osc_enabled_with(false, OscServer::start)
            .expect("stop osc server");
        assert!(engine.osc_server.is_none());
    }

    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "Tokio runtime uses kqueue, which Miri does not support on FreeBSD"
    )]
    #[tokio::test]
    async fn set_tempo_map_is_recorded_and_undone() {
        let (mut engine, _client_rx) = make_engine_with_client();
        let original_tempo_points = engine.tempo_points.clone();
        let original_time_signature_points = engine.time_signature_points.clone();

        let new_tempo_points = vec![crate::message::TempoPoint {
            sample: 0,
            bpm: 140.0,
        }];
        let new_time_signature_points = vec![crate::message::TimeSignaturePoint {
            sample: 0,
            numerator: 3,
            denominator: 4,
        }];

        engine
            .handle_request(Action::SetTempoMap {
                tempo_points: new_tempo_points.clone(),
                time_signature_points: new_time_signature_points.clone(),
            })
            .await;

        assert_eq!(engine.tempo_points, new_tempo_points);
        assert_eq!(engine.time_signature_points, new_time_signature_points);
        assert_eq!(engine.tempo_bpm, 140.0);
        assert_eq!(engine.tsig_num, 3);
        assert_eq!(engine.tsig_denom, 4);
        assert!(engine.history.is_dirty());

        engine.handle_request(Action::Undo).await;

        assert_eq!(engine.tempo_points, original_tempo_points);
        assert_eq!(engine.time_signature_points, original_time_signature_points);
        assert_eq!(engine.tempo_bpm, 120.0);
        assert_eq!(engine.tsig_num, 4);
        assert_eq!(engine.tsig_denom, 4);
    }

    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "Tokio runtime uses kqueue, which Miri does not support on FreeBSD"
    )]
    #[tokio::test]
    async fn osc_server_forwards_transport_packets_to_engine_channel() {
        let (tx, mut rx) = channel(4);
        let mut server =
            OscServer::start_on_addr(tx, "127.0.0.1:0").expect("start osc test server");
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind sender socket");
        let packet = osc_packet("/transport/play");
        socket
            .send_to(&packet, server.listen_addr())
            .expect("send osc packet");

        let message = timeout(TokioDuration::from_secs(1), rx.recv())
            .await
            .expect("packet delivery timeout")
            .expect("osc message");
        match message {
            Message::OscRequest {
                action: Action::Play,
                ..
            } => {}
            other => panic!("unexpected osc message: {other:?}"),
        }

        server.stop();
    }

    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "Tokio runtime uses kqueue, which Miri does not support on FreeBSD"
    )]
    #[tokio::test]
    async fn track_offline_bounce_rejects_zero_length_requests() {
        let (mut engine, mut client_rx) = make_engine_with_client();
        insert_track(
            &mut engine,
            Track::new("track".to_string(), 1, 1, 0, 0, 64, 48_000.0),
        );

        engine
            .handle_request(Action::TrackOfflineBounce {
                track_name: "track".to_string(),
                output_path: "/tmp/out.wav".to_string(),
                start_sample: 0,
                length_samples: 0,
                automation_lanes: vec![],
                apply_fader: false,
            })
            .await;

        match client_rx.recv().await.expect("response") {
            Message::Response(Err(err)) => {
                assert!(err.contains("has no renderable content for offline bounce"));
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "Tokio runtime uses kqueue, which Miri does not support on FreeBSD"
    )]
    #[tokio::test]
    async fn track_offline_bounce_rejects_when_same_track_is_active() {
        let (mut engine, mut client_rx) = make_engine_with_client();
        engine.offline_bounce_jobs.insert(
            "other".to_string(),
            OfflineBounceJob {
                cancel: Arc::new(AtomicBool::new(false)),
            },
        );

        engine
            .handle_request(Action::TrackOfflineBounce {
                track_name: "other".to_string(),
                output_path: "/tmp/out.wav".to_string(),
                start_sample: 0,
                length_samples: 128,
                automation_lanes: vec![],
                apply_fader: false,
            })
            .await;

        match client_rx.recv().await.expect("response") {
            Message::Response(Err(err)) => {
                assert!(err.contains("already in progress"));
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "Tokio runtime uses kqueue, which Miri does not support on FreeBSD"
    )]
    #[tokio::test]
    async fn track_offline_bounce_allows_different_track_concurrently() {
        let (mut engine, _client_rx) = make_engine_with_client();
        insert_track(
            &mut engine,
            Track::new("track".to_string(), 1, 1, 0, 0, 64, 48_000.0),
        );
        engine.offline_bounce_jobs.insert(
            "other".to_string(),
            OfflineBounceJob {
                cancel: Arc::new(AtomicBool::new(false)),
            },
        );

        engine
            .handle_request(Action::TrackOfflineBounce {
                track_name: "track".to_string(),
                output_path: "/tmp/out.wav".to_string(),
                start_sample: 0,
                length_samples: 128,
                automation_lanes: vec![],
                apply_fader: false,
            })
            .await;

        assert!(engine.offline_bounce_jobs.contains_key("other"));
        assert_eq!(engine.pending_requests.len(), 1);
    }

    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "Tokio runtime uses kqueue, which Miri does not support on FreeBSD"
    )]
    #[tokio::test]
    async fn reject_if_track_frozen_sends_error_and_blocks_operation() {
        let (mut engine, mut client_rx) = make_engine_with_client();
        let track = Track::new("track".to_string(), 1, 1, 0, 0, 64, 48_000.0);
        track.set_frozen(true);
        insert_track(&mut engine, track);

        let rejected = engine
            .reject_if_track_frozen("track", "arming/disarming")
            .await;

        assert!(rejected);
        match client_rx.recv().await.expect("response") {
            Message::Response(Err(err)) => {
                assert_eq!(err, "Track 'track' is frozen; arming/disarming is blocked");
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "Tokio runtime uses kqueue, which Miri does not support on FreeBSD"
    )]
    #[tokio::test]
    async fn dispatcher_track_mutations_publish_state_snapshot() {
        let (mut engine, _client_rx) = make_engine_with_client();
        insert_track(
            &mut engine,
            Track::new("snap".to_string(), 1, 1, 0, 0, 64, 48_000.0),
        );
        engine.publish_state_snapshot();

        let snapshot = engine.state_snapshot.load_full();
        assert!(snapshot.tracks.contains_key("snap"));

        engine
            .handle_request(Action::TrackToggleArm("snap".to_string()))
            .await;

        let snapshot = engine.state_snapshot.load_full();
        assert!(snapshot.tracks.get("snap").unwrap().lock().armed());

        engine
            .handle_request(Action::RemoveTrack("snap".to_string()))
            .await;

        let snapshot = engine.state_snapshot.load_full();
        assert!(!snapshot.tracks.contains_key("snap"));
    }

    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "Tokio runtime uses kqueue, which Miri does not support on FreeBSD"
    )]
    #[tokio::test]
    async fn undo_restores_original_clip_bounds_after_stretch_style_group() {
        let (mut engine, _client_rx) = make_engine_with_client();
        let track = Track::new("track".to_string(), 1, 1, 0, 0, 64, 48_000.0);
        let mut clip = AudioClip::new("audio/original.wav".to_string(), 100, 220);
        clip.offset = 12;
        clip.fade_in_samples = 20;
        clip.fade_out_samples = 30;
        track.audio.push_clip(clip);
        insert_track(&mut engine, track);

        engine.handle_request(Action::BeginHistoryGroup).await;
        engine
            .handle_request(Action::SetClipBounds {
                track_name: "track".to_string(),
                clip_index: 0,
                kind: Kind::Audio,
                start: 120,
                length: 180,
                offset: 0,
            })
            .await;
        engine
            .handle_request(Action::SetClipSourceName {
                track_name: "track".to_string(),
                clip_index: 0,
                kind: Kind::Audio,
                name: "audio/stretched.wav".to_string(),
            })
            .await;
        engine
            .handle_request(Action::SetClipFade {
                track_name: "track".to_string(),
                clip_index: 0,
                kind: Kind::Audio,
                fade_enabled: true,
                fade_in_samples: 12,
                fade_out_samples: 12,
            })
            .await;
        engine.handle_request(Action::EndHistoryGroup).await;

        engine.handle_request(Action::Undo).await;

        let state = engine.state.lock();
        let track = state.tracks.get("track").expect("track exists").lock();
        let clips = track.audio.clips();
        let clip = clips.first().expect("clip exists");
        assert_eq!(clip.name, "audio/original.wav");
        assert_eq!(clip.start, 100);
        assert_eq!(clip.end, 220);
        assert_eq!(clip.end.saturating_sub(clip.start), 120);
        assert_eq!(clip.offset, 12);
    }

    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "Tokio runtime uses kqueue, which Miri does not support on FreeBSD"
    )]
    #[tokio::test]
    async fn track_offline_bounce_queues_when_no_worker_is_ready() {
        let (mut engine, _client_rx) = make_engine_with_client();
        insert_track(
            &mut engine,
            Track::new("track".to_string(), 1, 1, 0, 0, 64, 48_000.0),
        );

        engine
            .handle_request(Action::TrackOfflineBounce {
                track_name: "track".to_string(),
                output_path: "/tmp/out.wav".to_string(),
                start_sample: 0,
                length_samples: 128,
                automation_lanes: vec![],
                apply_fader: false,
            })
            .await;

        assert!(engine.offline_bounce_jobs.is_empty());
        assert_eq!(engine.pending_requests.len(), 1);
        assert!(matches!(
            engine.pending_requests.front(),
            Some(Action::TrackOfflineBounce { track_name, length_samples, .. })
                if track_name == "track" && *length_samples == 128
        ));
    }

    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "Tokio runtime uses kqueue, which Miri does not support on FreeBSD"
    )]
    #[tokio::test]
    async fn track_offline_bounce_returns_missing_track_error() {
        let (mut engine, mut client_rx) = make_engine_with_client();

        engine
            .handle_request(Action::TrackOfflineBounce {
                track_name: "missing".to_string(),
                output_path: "/tmp/out.wav".to_string(),
                start_sample: 0,
                length_samples: 128,
                automation_lanes: vec![],
                apply_fader: false,
            })
            .await;

        match client_rx.recv().await.expect("response") {
            Message::Response(Err(err)) => {
                assert_eq!(err, "Track not found: missing");
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "Tokio runtime uses kqueue, which Miri does not support on FreeBSD"
    )]
    #[tokio::test]
    async fn track_offline_bounce_clears_job_when_worker_send_fails() {
        let (mut engine, mut client_rx) = make_engine_with_client();
        insert_track(
            &mut engine,
            Track::new("track".to_string(), 1, 1, 0, 0, 64, 48_000.0),
        );
        let (worker_tx, worker_rx) = channel(1);
        drop(worker_rx);
        engine
            .workers
            .push(WorkerData::new(worker_tx, tokio::spawn(async {})));
        engine.ready_workers.push(0);

        engine
            .handle_request(Action::TrackOfflineBounce {
                track_name: "track".to_string(),
                output_path: "/tmp/out.wav".to_string(),
                start_sample: 0,
                length_samples: 128,
                automation_lanes: vec![],
                apply_fader: false,
            })
            .await;

        assert!(engine.offline_bounce_jobs.is_empty());
        match client_rx.recv().await.expect("response") {
            Message::Response(Err(err)) => {
                assert!(err.contains("Failed to schedule offline bounce"));
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "Tokio runtime uses kqueue, which Miri does not support on FreeBSD"
    )]
    #[tokio::test]
    async fn play_stop_play_keeps_clip_output_audible() {
        use crate::audio::clip::AudioClip;
        use crate::audio_codec::write_wav_f32;

        let (engine_tx, engine_rx) = channel(16);
        let mut engine = Engine::new(engine_rx, engine_tx);
        let state = engine.state();
        let (client_tx, mut client_rx) = channel(16);
        engine.clients.push(client_tx);
        engine.init().await;

        let tmp_dir = std::env::temp_dir().join("maolan_play_stop_play_test");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let wav_path = tmp_dir.join("tone.wav");
        let sample_rate = 48_000u32;
        let clip_samples = sample_rate as usize;
        let mut samples = Vec::with_capacity(clip_samples);
        for i in 0..clip_samples {
            let phase = i as f32 / sample_rate as f32 * 2.0 * std::f32::consts::PI * 440.0;
            samples.push(phase.sin() * 0.5);
        }
        write_wav_f32(&wav_path, &samples, 1, sample_rate).expect("write wav");

        let mut track = Track::new("track".to_string(), 1, 1, 0, 0, 1024, sample_rate as f64);
        let mut clip = AudioClip::new(wav_path.to_string_lossy().to_string(), 0, clip_samples);
        clip.fade_enabled = false;
        track.audio.push_clip(clip);
        track.session_base_dir = Some(tmp_dir.clone());
        insert_track(&mut engine, track);

        let tx = engine.tx.clone();
        let work_handle = tokio::spawn(async move {
            engine.work().await;
        });

        // Wait for worker tasks to start up and send Ready messages.
        tokio::time::sleep(TokioDuration::from_millis(100)).await;

        async fn drain_responses(
            client_rx: &mut tokio::sync::mpsc::Receiver<Message>,
            count: usize,
        ) {
            for _ in 0..count {
                let _ = tokio::time::timeout(TokioDuration::from_secs(2), client_rx.recv()).await;
            }
        }

        async fn wait_for_audible_track(
            client_rx: &mut tokio::sync::mpsc::Receiver<Message>,
            state: &State,
        ) -> Option<f32> {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                {
                    let state = state.lock();
                    let peak = state
                        .tracks
                        .get("track")
                        .map(|t| {
                            t.lock()
                                .output_meter_linear()
                                .into_iter()
                                .fold(0.0_f32, f32::max)
                        })
                        .unwrap_or(0.0);
                    if peak > 0.001 {
                        return Some(peak);
                    }
                }
                let _ =
                    tokio::time::timeout(TokioDuration::from_millis(10), client_rx.recv()).await;
                tokio::time::sleep(TokioDuration::from_millis(10)).await;
            }
            None
        }

        tx.send(Message::Request(Action::SetClipPlaybackEnabled(true)))
            .await
            .unwrap();
        tx.send(Message::Request(Action::Play)).await.unwrap();
        let first_peak = wait_for_audible_track(&mut client_rx, &state)
            .await
            .unwrap_or(0.0);
        assert!(
            first_peak > 0.001,
            "expected audible output on first play, got {first_peak}"
        );

        tx.send(Message::Request(Action::SetClipPlaybackEnabled(true)))
            .await
            .unwrap();
        tx.send(Message::Request(Action::Stop)).await.unwrap();
        drain_responses(&mut client_rx, 2).await;

        tx.send(Message::Request(Action::SetClipPlaybackEnabled(true)))
            .await
            .unwrap();
        tx.send(Message::Request(Action::Play)).await.unwrap();
        let second_peak = wait_for_audible_track(&mut client_rx, &state)
            .await
            .unwrap_or(0.0);
        assert!(
            second_peak > 0.001,
            "expected audible output on second play after stop, got {second_peak}"
        );

        let _ = tx.send(Message::Request(Action::Quit)).await;
        tokio::time::sleep(TokioDuration::from_millis(200)).await;
        work_handle.abort();
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn modulator_sets_track_volume() {
        let (mut engine, _client_rx) = make_engine_with_client();
        let track = Track::new("vol-track".to_string(), 0, 2, 0, 0, 128, 48_000.0);
        insert_track_for_modulator_test(&mut engine, track);

        engine.modulators = vec![crate::modulator::Modulator {
            id: 1,
            name: "LFO".to_string(),
            shape: crate::modulator::ModulatorShape::Sine,
            rate: crate::modulator::ModulatorRate::Hz(1.0),
            phase: 0.0,
            enabled: true,
            targets: vec![crate::modulator::ModulatorTarget::TrackVolume {
                track_name: "vol-track".to_string(),
                min: -90.0,
                max: 20.0,
            }],
        }];

        // At sample 12000 (1/4 period at 48kHz/1Hz), sine value maps to 1.0 -> max 20 dB.
        let echoes = engine.apply_modulators(12_000);
        let state_guard = engine.state.lock();
        let track = state_guard.tracks["vol-track"].lock();
        assert!(
            (track.level() - 20.0).abs() < 0.01,
            "expected 20 dB, got {}",
            track.level()
        );
        assert!(
            echoes
                .iter()
                .any(|a| matches!(a, Action::TrackAutomationLevel(name, _) if name == "vol-track"))
        );
    }

    #[test]
    fn modulator_sets_track_balance() {
        let (mut engine, _client_rx) = make_engine_with_client();
        let track = Track::new("pan-track".to_string(), 0, 2, 0, 0, 128, 48_000.0);
        insert_track_for_modulator_test(&mut engine, track);

        engine.modulators = vec![crate::modulator::Modulator {
            id: 1,
            name: "LFO".to_string(),
            shape: crate::modulator::ModulatorShape::Sine,
            rate: crate::modulator::ModulatorRate::Hz(1.0),
            phase: 0.0,
            enabled: true,
            targets: vec![crate::modulator::ModulatorTarget::TrackBalance {
                track_name: "pan-track".to_string(),
                min: -1.0,
                max: 1.0,
            }],
        }];

        // At sample 12000 (1/4 period), sine value maps to 1.0 -> max balance 1.0.
        let echoes = engine.apply_modulators(12_000);
        let state_guard = engine.state.lock();
        let track = state_guard.tracks["pan-track"].lock();
        assert!(
            (track.balance() - 1.0).abs() < 0.01,
            "expected balance 1.0, got {}",
            track.balance()
        );
        assert!(
            echoes.iter().any(
                |a| matches!(a, Action::TrackAutomationBalance(name, _) if name == "pan-track")
            )
        );
    }

    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "Tokio runtime uses kqueue, which Miri does not support on FreeBSD"
    )]
    #[tokio::test]
    async fn track_set_parent_wires_folder_input_to_child_input_and_child_output_to_folder_output()
    {
        let (mut engine, mut client_rx) = make_engine_with_client();
        let folder = Track::new_folder("folder".to_string(), 2, 2, 0, 0, 64, 48_000.0);
        let child = Track::new("child".to_string(), 2, 2, 0, 0, 64, 48_000.0);
        insert_track(&mut engine, folder);
        insert_track(&mut engine, child);

        engine
            .handle_request_inner(
                Action::TrackSetParent {
                    track_name: "child".to_string(),
                    parent_name: Some("folder".to_string()),
                },
                false,
            )
            .await;

        // Drain client messages so the channel does not block later drops.
        while let Ok(Some(_)) =
            tokio::time::timeout(TokioDuration::from_millis(10), client_rx.recv()).await
        {}

        let state = engine.state.lock();
        let folder = state.tracks.get("folder").unwrap().lock();
        let child = state.tracks.get("child").unwrap().lock();

        assert!(folder.child_tracks.iter().any(|c| c.lock().name == "child"));
        assert_eq!(child.parent_track.as_deref(), Some("folder"));

        // Folder input -> child input.
        for (i, (parent_in, child_in)) in folder.audio.ins.iter().zip(&child.audio.ins).enumerate()
        {
            assert!(
                child_in
                    .connections()
                    .iter()
                    .any(|c| Arc::ptr_eq(c, parent_in)),
                "folder input {i} is not routed to child input {i}"
            );
            assert!(
                !parent_in
                    .connections()
                    .iter()
                    .any(|c| Arc::ptr_eq(c, child_in)),
                "folder input {i} should not read from child input {i}"
            );
        }

        // Child output -> folder output.
        for (i, (child_out, parent_out)) in
            child.audio.outs.iter().zip(&folder.audio.outs).enumerate()
        {
            assert!(
                parent_out
                    .connections()
                    .iter()
                    .any(|c| Arc::ptr_eq(c, child_out)),
                "child output {i} is not routed to folder output {i}"
            );
        }

        // Child passthrough is restored so audio can flow through.
        for (i, child_out) in child.audio.outs.iter().enumerate() {
            assert!(
                child_out.connections().iter().any(|c| {
                    child
                        .audio
                        .ins
                        .get(i)
                        .is_some_and(|inp| Arc::ptr_eq(c, inp))
                }),
                "child output {i} is not connected to child input {i}"
            );
        }
    }

    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "Tokio runtime uses kqueue, which Miri does not support on FreeBSD"
    )]
    #[tokio::test]
    async fn track_set_parent_to_none_restores_root_passthrough() {
        let (mut engine, mut client_rx) = make_engine_with_client();
        let folder = Track::new_folder("folder".to_string(), 2, 2, 0, 0, 64, 48_000.0);
        let child = Track::new("child".to_string(), 2, 2, 0, 0, 64, 48_000.0);
        insert_track(&mut engine, folder);
        insert_track(&mut engine, child);

        engine
            .handle_request_inner(
                Action::TrackSetParent {
                    track_name: "child".to_string(),
                    parent_name: Some("folder".to_string()),
                },
                false,
            )
            .await;
        engine
            .handle_request_inner(
                Action::TrackSetParent {
                    track_name: "child".to_string(),
                    parent_name: None,
                },
                false,
            )
            .await;

        while let Ok(Some(_)) =
            tokio::time::timeout(TokioDuration::from_millis(10), client_rx.recv()).await
        {}

        let state = engine.state.lock();
        let folder = state.tracks.get("folder").unwrap().lock();
        let child = state.tracks.get("child").unwrap().lock();

        assert!(folder.child_tracks.is_empty());
        assert!(child.parent_track.is_none());

        for (i, child_out) in child.audio.outs.iter().enumerate() {
            assert!(
                child_out.connections().iter().any(|c| {
                    child
                        .audio
                        .ins
                        .get(i)
                        .is_some_and(|inp| Arc::ptr_eq(c, inp))
                }),
                "child output {i} should be connected to child input {i} after moving to root"
            );
        }
    }

    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "Tokio runtime uses kqueue, which Miri does not support on FreeBSD"
    )]
    #[tokio::test]
    async fn track_set_parent_wires_folder_midi_to_child_midi() {
        let (mut engine, mut client_rx) = make_engine_with_client();
        let folder = Track::new_folder("folder".to_string(), 0, 0, 1, 1, 64, 48_000.0);
        let child = Track::new("child".to_string(), 0, 0, 1, 1, 64, 48_000.0);
        insert_track(&mut engine, folder);
        insert_track(&mut engine, child);

        engine
            .handle_request_inner(
                Action::TrackSetParent {
                    track_name: "child".to_string(),
                    parent_name: Some("folder".to_string()),
                },
                false,
            )
            .await;

        while let Ok(Some(_)) =
            tokio::time::timeout(TokioDuration::from_millis(10), client_rx.recv()).await
        {}

        let state = engine.state.lock();
        let folder = state.tracks.get("folder").unwrap().lock();
        let child = state.tracks.get("child").unwrap().lock();

        let folder_midi_in = &folder.midi.ins[0];
        let child_midi_in = &child.midi.ins[0];
        assert!(
            child_midi_in
                .connections()
                .iter()
                .any(|c| Arc::ptr_eq(c, folder_midi_in)),
            "folder MIDI input should be routed to child MIDI input"
        );

        let child_midi_out = &child.midi.outs[0];
        let folder_midi_out = &folder.midi.outs[0];
        assert!(
            child_midi_out
                .connections()
                .iter()
                .any(|c| Arc::ptr_eq(c, folder_midi_out)),
            "child MIDI output should be routed to folder MIDI output"
        );
    }

    fn plan_task_node(
        plan: &crate::render_plan::RenderPlan,
        name: &str,
        want: fn(&crate::message::ProcessTask) -> bool,
    ) -> usize {
        use crate::message::ProcessTask;
        use crate::render_plan::Op;
        plan.nodes
            .iter()
            .enumerate()
            .find_map(|(i, op)| match op {
                Op::Task { task, .. } => {
                    let track = match task {
                        ProcessTask::Track(t)
                        | ProcessTask::FolderInput(t)
                        | ProcessTask::FolderOutput(t) => t,
                        ProcessTask::Plugin { track, .. } => track,
                    };
                    if track.lock().name == name && want(task) {
                        Some(i)
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .expect("task node not found")
    }

    fn plan_reachable(plan: &crate::render_plan::RenderPlan, from: usize, to: usize) -> bool {
        let mut seen = std::collections::HashSet::new();
        let mut queue = std::collections::VecDeque::from([from as u32]);
        while let Some(n) = queue.pop_front() {
            for &d in &plan.dependents[n as usize] {
                if d as usize == to {
                    return true;
                }
                if seen.insert(d) {
                    queue.push_back(d);
                }
            }
        }
        false
    }

    #[test]
    fn nested_folder_expands_in_render_plan() {
        use crate::message::ProcessTask;

        let state = crate::state::State::default();
        let outer = Arc::new(Track::new_folder(
            "outer".to_string(),
            2,
            2,
            0,
            0,
            64,
            48_000.0,
        ));
        let inner = Arc::new(Track::new_folder(
            "inner".to_string(),
            2,
            2,
            0,
            0,
            64,
            48_000.0,
        ));
        let leaf = Arc::new(Track::new("leaf".to_string(), 2, 2, 0, 0, 64, 48_000.0));
        outer.lock().child_tracks.push(inner.clone());
        inner.lock().child_tracks.push(leaf.clone());
        inner.lock().parent_track = Some("outer".to_string());
        leaf.lock().parent_track = Some("inner".to_string());
        {
            let mut state = state.lock();
            state.tracks.insert("outer".to_string(), outer);
            state.tracks.insert("inner".to_string(), inner);
            state.tracks.insert("leaf".to_string(), leaf);
        }

        let plan = crate::render_plan::RenderPlan::compile(&state.snapshot(), &[], &[], 64);
        plan.verify().expect("plan invariants");

        let is_fi = |t: &ProcessTask| matches!(t, ProcessTask::FolderInput(_));
        let is_fo = |t: &ProcessTask| matches!(t, ProcessTask::FolderOutput(_));
        let is_track = |t: &ProcessTask| matches!(t, ProcessTask::Track(_));
        let in_outer = plan_task_node(&plan, "outer", is_fi);
        let in_inner = plan_task_node(&plan, "inner", is_fi);
        let track_leaf = plan_task_node(&plan, "leaf", is_track);
        let out_inner = plan_task_node(&plan, "inner", is_fo);
        let out_outer = plan_task_node(&plan, "outer", is_fo);

        assert!(
            in_outer < in_inner
                && in_inner < track_leaf
                && track_leaf < out_inner
                && out_inner < out_outer,
            "nested folder tasks should expand in topological order"
        );
        for (a, b) in [
            (in_outer, in_inner),
            (in_inner, track_leaf),
            (track_leaf, out_inner),
            (out_inner, out_outer),
        ] {
            assert!(plan_reachable(&plan, a, b), "{a} should reach {b}");
        }
        assert!(plan.forced.is_empty(), "no feedback cycle");
    }

    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "uses filesystem metadata, which Miri isolation does not support on FreeBSD"
    )]
    #[test]
    fn child_to_plugin_to_folder_output_render_plan_has_no_cycle() {
        use crate::message::{ConnectableRef, ProcessTask};

        let plugin_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("daw")
            .join("plugin-host")
            .join("tests")
            .join("test_passthrough.clap");
        if !plugin_path.exists() {
            return;
        }
        if crate::plugins::ipc::find_plugin_host_binary().is_none() {
            return;
        }

        let (mut engine, _client_rx) = make_engine_with_client();
        let mut folder = Track::new_folder("folder".to_string(), 2, 2, 0, 0, 64, 48_000.0);
        let child = Track::new("child".to_string(), 2, 2, 0, 0, 64, 48_000.0);

        folder
            .load_clap_plugin(
                &format!("{}::com.maolan.test.passthrough", plugin_path.display()),
                None,
            )
            .expect("should load CLAP plugin on folder");
        folder.clap_plugins[0].processor.setup_audio_ports();
        let plugin_id = folder.clap_plugins[0].id;

        insert_track(&mut engine, folder);
        insert_track(&mut engine, child);

        {
            let state = engine.state.lock();
            let folder = state.tracks.get("folder").unwrap().clone();
            let child = state.tracks.get("child").unwrap().clone();
            folder.lock().child_tracks.push(child.clone());
            child.lock().parent_track = Some("folder".to_string());

            folder
                .lock()
                .connect_audio_connectable(
                    ConnectableRef::ChildTrack("child".to_string()),
                    0,
                    ConnectableRef::ClapPlugin(plugin_id),
                    0,
                )
                .expect("connect child L to plugin L");
            folder
                .lock()
                .connect_audio_connectable(
                    ConnectableRef::ChildTrack("child".to_string()),
                    1,
                    ConnectableRef::ClapPlugin(plugin_id),
                    1,
                )
                .expect("connect child R to plugin R");
            folder
                .lock()
                .connect_audio_connectable(
                    ConnectableRef::ClapPlugin(plugin_id),
                    0,
                    ConnectableRef::TrackOutput,
                    0,
                )
                .expect("connect plugin L to folder output L");
            folder
                .lock()
                .connect_audio_connectable(
                    ConnectableRef::ClapPlugin(plugin_id),
                    1,
                    ConnectableRef::TrackOutput,
                    1,
                )
                .expect("connect plugin R to folder output R");
        }

        let plan = {
            let state = engine.state.lock();
            crate::render_plan::RenderPlan::compile(&state.snapshot(), &[], &[], 64)
        };
        plan.verify().expect("plan invariants");

        let folder_in = plan_task_node(&plan, "folder", |t| {
            matches!(t, ProcessTask::FolderInput(_))
        });
        let child_task = plan_task_node(&plan, "child", |t| matches!(t, ProcessTask::Track(_)));
        let plugin = plan_task_node(&plan, "folder", |t| {
            matches!(
                t,
                ProcessTask::Plugin {
                    kind: PluginKind::Clap,
                    index: 0,
                    ..
                }
            )
        });
        let folder_out = plan_task_node(&plan, "folder", |t| {
            matches!(t, ProcessTask::FolderOutput(_))
        });

        assert!(
            plan_reachable(&plan, folder_in, child_task),
            "child task should depend on folder input"
        );
        assert!(
            plan_reachable(&plan, folder_in, plugin) && plan_reachable(&plan, child_task, plugin),
            "plugin task should depend on folder input and child"
        );
        assert!(
            plan_reachable(&plan, folder_in, folder_out)
                && plan_reachable(&plan, plugin, folder_out)
                && plan_reachable(&plan, child_task, folder_out),
            "folder output should depend on folder input, plugin, and child"
        );
        assert!(
            plan.forced.is_empty(),
            "render plan should not contain a cycle when a plugin reads from a child track"
        );
    }

    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "Tokio runtime uses kqueue, which Miri does not support on FreeBSD"
    )]
    #[tokio::test]
    async fn track_set_parent_wires_child_io_to_folder_even_after_addtrack() {
        let (mut engine, mut client_rx) = make_engine_with_client();
        let folder = Track::new_folder("folder".to_string(), 2, 2, 0, 0, 64, 48_000.0);
        let child = Track::new("child".to_string(), 2, 2, 0, 0, 64, 48_000.0);
        insert_track(&mut engine, folder);
        insert_track(&mut engine, child);

        engine
            .handle_request_inner(
                Action::TrackSetParent {
                    track_name: "child".to_string(),
                    parent_name: Some("folder".to_string()),
                },
                false,
            )
            .await;

        while let Ok(Some(_)) =
            tokio::time::timeout(TokioDuration::from_millis(10), client_rx.recv()).await
        {}

        let state = engine.state.lock();
        let folder = state.tracks.get("folder").unwrap().lock();
        let child = state.tracks.get("child").unwrap().lock();

        // Folder input -> child input.
        for (i, (parent_in, child_in)) in folder.audio.ins.iter().zip(&child.audio.ins).enumerate()
        {
            assert!(
                child_in
                    .connections()
                    .iter()
                    .any(|c| Arc::ptr_eq(c, parent_in)),
                "folder input {i} is not routed to child input {i}"
            );
        }

        // Child output -> folder output.
        for (i, (child_out, parent_out)) in
            child.audio.outs.iter().zip(&folder.audio.outs).enumerate()
        {
            assert!(
                parent_out
                    .connections()
                    .iter()
                    .any(|c| Arc::ptr_eq(c, child_out)),
                "child output {i} is not routed to folder output {i}"
            );
        }
    }

    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "Tokio runtime uses kqueue, which Miri does not support on FreeBSD"
    )]
    #[tokio::test]
    async fn folder_child_audio_passes_through() {
        let (mut engine, mut client_rx) = make_engine_with_client();
        let folder = Track::new_folder("folder".to_string(), 1, 1, 0, 0, 64, 48_000.0);
        let child = Track::new("child".to_string(), 1, 1, 0, 0, 64, 48_000.0);
        insert_track(&mut engine, folder);
        insert_track(&mut engine, child);

        engine
            .handle_request_inner(
                Action::TrackSetParent {
                    track_name: "child".to_string(),
                    parent_name: Some("folder".to_string()),
                },
                false,
            )
            .await;
        while let Ok(Some(_)) =
            tokio::time::timeout(TokioDuration::from_millis(10), client_rx.recv()).await
        {}

        {
            let state = engine.state.lock();
            let folder = state.tracks.get("folder").unwrap().clone();
            let child = state.tracks.get("child").unwrap().clone();

            folder.lock().set_input_monitor(vec![true]);
            child.lock().set_input_monitor(vec![true]);

            // Feed a signal into the folder input from a fake hardware source
            // and execute the compiled render plan once.
            let source = Arc::new(crate::audio::io::AudioIO::new(64));
            crate::audio::io::AudioIO::connect(&source, &folder.lock().audio.ins[0]);
            let plan =
                crate::render_plan::RenderPlan::compile(&state.snapshot(), &[source], &[], 64);
            plan.verify().expect("plan invariants");
            let (_, hw_buf) = plan.hw_in_map[0];
            // Safety: test thread, no node is running yet; the fake driver owns
            // the hardware-input buffer before the cycle starts.
            unsafe { (&mut *plan.buffer_ptr(hw_buf)).fill(0.75) };
            let collector = basedrop::Collector::new();
            let shared = Arc::new(basedrop::Owned::new(&collector.handle(), plan));
            for node in 0..shared.nodes.len() {
                crate::workers::worker::Worker::process_node_job_result(
                    0,
                    crate::executor::NodeJob {
                        epoch: 0,
                        plan: shared.clone(),
                        node: node as u32,
                    },
                );
            }

            let folder_lock = folder.lock();
            let output = folder_lock.last_audio_outputs()[0].clone();
            assert!(
                output.iter().any(|s| (*s - 0.75).abs() < 1e-5),
                "folder output should contain the child-processed folder input signal, got {:?}",
                output.iter().take(8).collect::<Vec<_>>()
            );
        }
    }

    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "Tokio runtime uses kqueue, which Miri does not support on FreeBSD"
    )]
    #[tokio::test]
    async fn remove_folder_track_deletes_descendants_recursively() {
        let (mut engine, mut client_rx) = make_engine_with_client();
        let folder = Track::new_folder("folder".to_string(), 1, 1, 0, 0, 64, 48_000.0);
        let child = Track::new_folder("child".to_string(), 1, 1, 0, 0, 64, 48_000.0);
        let grandchild = Track::new("grandchild".to_string(), 1, 1, 0, 0, 64, 48_000.0);
        insert_track(&mut engine, folder);
        insert_track(&mut engine, child);
        insert_track(&mut engine, grandchild);

        engine
            .handle_request(Action::TrackSetParent {
                track_name: "child".to_string(),
                parent_name: Some("folder".to_string()),
            })
            .await;
        engine
            .handle_request(Action::TrackSetParent {
                track_name: "grandchild".to_string(),
                parent_name: Some("child".to_string()),
            })
            .await;

        // Drain TrackSetParent notifications so we can inspect the removal notifications.
        while let Ok(Some(_)) =
            tokio::time::timeout(TokioDuration::from_millis(10), client_rx.recv()).await
        {}

        engine
            .handle_request(Action::RemoveTrack("folder".to_string()))
            .await;

        {
            let state = engine.state.lock();
            assert!(
                !state.tracks.contains_key("folder"),
                "folder should have been removed"
            );
            assert!(
                !state.tracks.contains_key("child"),
                "child should have been removed"
            );
            assert!(
                !state.tracks.contains_key("grandchild"),
                "grandchild should have been removed"
            );
        }

        let mut removed_names = Vec::new();
        for _ in 0..3 {
            let msg = tokio::time::timeout(TokioDuration::from_millis(100), client_rx.recv()).await;
            if let Ok(Some(Message::Response(Ok(Action::RemoveTrack(name))))) = msg {
                removed_names.push(name);
            }
        }
        assert_eq!(
            removed_names,
            vec!["grandchild", "child", "folder"],
            "descendants should be removed before the folder and clients notified"
        );
    }

    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "Tokio runtime uses kqueue, which Miri does not support on FreeBSD"
    )]
    #[tokio::test]
    async fn track_set_folder_rejects_master_track() {
        let (mut engine, mut client_rx) = make_engine_with_client();
        let track = Track::new("master".to_string(), 2, 2, 0, 0, 64, 48_000.0);
        track.is_master.store(true, Ordering::Relaxed);
        insert_track(&mut engine, track);

        engine
            .handle_request_inner(
                Action::TrackSetFolder {
                    track_name: "master".to_string(),
                    is_folder: true,
                },
                false,
            )
            .await;

        {
            let state = engine.state.lock();
            assert!(!state.tracks.get("master").unwrap().lock().is_folder);
        }

        let msg = tokio::time::timeout(TokioDuration::from_millis(100), client_rx.recv()).await;
        assert!(
            matches!(msg, Ok(Some(Message::Response(Err(_))))),
            "master track folder conversion should report an error"
        );
    }

    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "Tokio runtime uses kqueue, which Miri does not support on FreeBSD"
    )]
    #[tokio::test]
    async fn track_toggle_master_ignored_for_folder_track() {
        let (mut engine, mut client_rx) = make_engine_with_client();
        let folder = Track::new_folder("folder".to_string(), 2, 2, 0, 0, 64, 48_000.0);
        insert_track(&mut engine, folder);

        engine
            .handle_request_inner(Action::TrackToggleMaster("folder".to_string()), false)
            .await;

        {
            let state = engine.state.lock();
            assert!(!state.tracks.get("folder").unwrap().lock().is_master());
        }

        let msg = tokio::time::timeout(TokioDuration::from_millis(100), client_rx.recv()).await;
        assert!(
            matches!(
                msg,
                Ok(Some(Message::Response(Ok(Action::TrackToggleMaster(ref name)))))
                    if name == "folder"
            ),
            "folder track master toggle should still be echoed to clients: {msg:?}"
        );
    }
}
