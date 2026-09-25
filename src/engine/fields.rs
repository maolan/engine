//! Owned field groups of [`Engine`](super::Engine).
//!
//! Phase 3: the former flat field list lives here as pub sub-structs with
//! `Default`-style constructors that preserve the exact initialization
//! values previously written inline in `Engine::new_with_snapshots`.
//! Cross-cutting fields (channels, workers, executor, history, OSC) stay
//! flat on `Engine`.
use super::*;
use std::collections::{HashMap, HashSet, VecDeque};
#[cfg(feature = "mixosc")]
use std::net::UdpSocket;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct MeterDecay {
    pub started_at: Instant,
    pub hw_out_linear: Vec<f32>,
    pub track_linear: Vec<(String, Vec<f32>)>,
}

/// Transport position, loop/punch ranges, tempo map, and per-dispatch
/// transport bookkeeping.
pub struct TransportFields {
    pub transport_sample: usize,
    pub transport_running: bool,
    pub playing: bool,
    pub notified_loop_wrap_sample: Option<usize>,
    /// Lock-free transport-position snapshot shared with all tracks. The
    /// dispatcher mirrors `transport_sample`/`session_transport_sample` into
    /// it on every task dispatch (see `Engine::prepare_task_track` and
    /// [`crate::track::TransportSampleSnapshot`]); a per-cycle advance thus
    /// reaches tracks without a generation bump or track lock.
    pub transport_sample_snapshot: Arc<crate::track::TransportSampleSnapshot>,
    /// Generation counter for the per-dispatch transport-state push performed
    /// by `prepare_task_track`. Bumped at every mutation of any value pushed
    /// there, so tracks whose `last_prepare_generation` equals this value can
    /// skip the push and the track lock for the rest of the generation. The
    /// transport sample itself is excluded: it reaches tracks through the
    /// mirrored `transport_sample_snapshot` without a bump.
    pub prepare_generation: u64,
    pub transport_panic_flush_pending: bool,
    pub transport_restart_pending: bool,
    pub awaiting_hwfinished: bool,
    pub handling_hwfinished: bool,
    pub loop_enabled: bool,
    pub loop_range_samples: Option<(usize, usize)>,
    pub metronome_enabled: bool,
    pub tempo_bpm: f64,
    pub tsig_num: u16,
    pub tsig_denom: u16,
    pub tempo_points: Vec<crate::message::TempoPoint>,
    pub time_signature_points: Vec<crate::message::TimeSignaturePoint>,
    pub punch_enabled: bool,
    pub punch_range_samples: Option<(usize, usize)>,
    pub clip_playback_enabled: bool,
    pub session_clip_playback_enabled: bool,
    pub session_transport_sample: usize,
    pub hw_input_latency_frames: usize,
    pub hw_output_latency_frames: usize,
    pub transport_snapshot_producer:
        crate::triple_buffer::TripleBufferProducer<crate::meter::TransportSnapshot>,
}

impl TransportFields {
    pub(crate) fn new(
        transport_snapshot_producer: crate::triple_buffer::TripleBufferProducer<
            crate::meter::TransportSnapshot,
        >,
    ) -> Self {
        Self {
            transport_sample: 0,
            transport_running: false,
            playing: false,
            notified_loop_wrap_sample: None,
            transport_sample_snapshot: Arc::new(crate::track::TransportSampleSnapshot::new()),
            prepare_generation: 1,
            transport_panic_flush_pending: false,
            transport_restart_pending: false,
            awaiting_hwfinished: false,
            handling_hwfinished: false,
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
            clip_playback_enabled: true,
            session_clip_playback_enabled: false,
            session_transport_sample: 0,
            hw_input_latency_frames: 0,
            hw_output_latency_frames: 0,
            transport_snapshot_producer,
        }
    }
}

/// Master-output level/mute plus meter publish/decay state and the meter
/// triple-buffer producer.
pub struct MeterFields {
    pub hw_out_level_db: f32,
    pub hw_out_balance: f32,
    pub hw_out_muted: bool,
    pub last_hw_out_meter_publish: Option<Instant>,
    #[cfg(unix)]
    pub last_hw_out_meter_linear: Vec<f32>,
    pub hw_out_peak_hold_linear: Vec<f32>,
    #[cfg(unix)]
    pub hw_out_meter_publish_phase: bool,
    pub last_track_meter_publish: Option<Instant>,
    pub last_meter_snapshot_publish: Option<Instant>,
    pub track_meter_linear_by_track: HashMap<String, Vec<f32>>,
    pub meter_decay_after_stop: Option<MeterDecay>,
    pub meter_snapshot_producer:
        crate::triple_buffer::TripleBufferProducer<crate::meter::MeterSnapshot>,
    pub latest_hw_out_meter_db: Arc<Vec<f32>>,
    pub latest_track_meter_snapshot: Arc<Vec<(String, Vec<f32>)>>,
}

impl MeterFields {
    pub(crate) const METER_PUBLISH_INTERVAL: Duration = Duration::from_millis(50);
    pub(crate) const METER_DECAY_AFTER_STOP: Duration = Duration::from_secs(1);
    #[cfg(unix)]
    pub(crate) const HW_OUT_METER_LINEAR_EPSILON: f32 = 0.0025;

    pub(crate) fn new(
        meter_snapshot_producer: crate::triple_buffer::TripleBufferProducer<
            crate::meter::MeterSnapshot,
        >,
    ) -> Self {
        Self {
            hw_out_level_db: 0.0,
            hw_out_balance: 0.0,
            hw_out_muted: false,
            last_hw_out_meter_publish: None,
            #[cfg(unix)]
            last_hw_out_meter_linear: vec![],
            hw_out_peak_hold_linear: vec![],
            #[cfg(unix)]
            hw_out_meter_publish_phase: false,
            last_track_meter_publish: None,
            last_meter_snapshot_publish: None,
            track_meter_linear_by_track: HashMap::new(),
            meter_decay_after_stop: None,
            meter_snapshot_producer,
            latest_hw_out_meter_db: Arc::new(Vec::new()),
            latest_track_meter_snapshot: Arc::new(Vec::new()),
        }
    }
}

/// Session view runtime: scene queue/current-scene state, completed-clip
/// pass tracking, session directory, and the session runtime snapshot
/// producer.
pub struct SessionFields {
    pub session_dir: Option<PathBuf>,
    /// Scene queued via [`crate::message::SessionAction::QueueScene`] as
    /// (scene_index, launch_at_sample); `None` when no scene is queued.
    pub session_scene_queue: Option<(usize, usize)>,
    pub session_scene_queue_length_samples: usize,
    /// Scene whose launch most recently fired; reported in the session
    /// runtime snapshot so clients can highlight it as the current scene.
    pub session_current_scene: Option<usize>,
    pub session_current_scene_previous_scene: Option<usize>,
    pub session_current_scene_start_sample: usize,
    pub session_current_scene_length_samples: usize,
    pub session_completed_clip_passes: Vec<crate::meter::SessionCompletedClipPass>,
    pub session_reported_clip_passes: HashSet<(String, usize, String, usize, usize)>,
    pub session_runtime_snapshot_producer:
        crate::triple_buffer::TripleBufferProducer<crate::meter::SessionRuntimeSnapshot>,
    pub last_session_report_publish: Option<Instant>,
}

impl SessionFields {
    pub(crate) const SESSION_RUNTIME_REPORT_INTERVAL: Duration = Duration::from_millis(50);

    pub(crate) fn new(
        session_runtime_snapshot_producer: crate::triple_buffer::TripleBufferProducer<
            crate::meter::SessionRuntimeSnapshot,
        >,
    ) -> Self {
        Self {
            session_dir: None,
            session_scene_queue: None,
            session_scene_queue_length_samples: 0,
            session_current_scene: None,
            session_current_scene_previous_scene: None,
            session_current_scene_start_sample: 0,
            session_current_scene_length_samples: 0,
            session_completed_clip_passes: Vec::new(),
            session_reported_clip_passes: HashSet::new(),
            session_runtime_snapshot_producer,
            last_session_report_publish: None,
        }
    }
}

/// Pending and bound MIDI-learn state (track/global/session) plus the CC
/// learn gate.
#[derive(Default)]
pub struct MidiLearnFields {
    pub pending_midi_learn: Option<(String, crate::message::TrackMidiLearnTarget, Option<String>)>,
    pub pending_global_midi_learn: Option<crate::message::GlobalMidiLearnTarget>,
    pub pending_session_midi_learn: Option<crate::message::SessionMidiLearnTarget>,
    pub global_midi_learn_play_pause: Option<crate::message::MidiLearnBinding>,
    pub global_midi_learn_stop: Option<crate::message::MidiLearnBinding>,
    pub global_midi_learn_record_toggle: Option<crate::message::MidiLearnBinding>,
    pub session_midi_learn_slots: HashMap<(String, usize), crate::message::MidiLearnBinding>,
    pub session_midi_learn_scenes: HashMap<usize, crate::message::MidiLearnBinding>,
    pub session_midi_learn_stop_track: HashMap<String, crate::message::MidiLearnBinding>,
    pub session_midi_learn_stop_all: Option<crate::message::MidiLearnBinding>,
    pub midi_cc_gate: HashMap<(String, u8, u8), bool>,
}

/// Hardware MIDI event queues/routes and active-note tracking.
#[derive(Default)]
pub struct HwMidiFields {
    pub pending_hw_midi_events: Vec<MidiEvent>,
    pub pending_hw_midi_events_by_device: HashMap<String, Vec<MidiEvent>>,
    pub pending_hw_midi_out_events: Vec<MidiEvent>,
    pub pending_hw_midi_out_events_by_device: Vec<HwMidiEvent>,
    pub active_hw_notes_by_track: HashMap<String, HashSet<(String, u8, u8)>>,
    pub active_hw_notes_cycle_start: HashMap<String, HashSet<(String, u8, u8)>>,
    pub midi_hw_in_routes: Vec<MidiHwInRoute>,
    pub midi_hw_out_routes: Vec<MidiHwOutRoute>,
    pub midi_hw_thru_routes: Vec<MidiHwThruRoute>,
}

/// In-flight and completed audio/MIDI recordings plus record toggles.
#[derive(Default)]
pub struct RecordingFields {
    pub record_enabled: bool,
    pub step_recording_enabled: bool,
    pub audio_recordings: HashMap<String, RecordingSession>,
    pub midi_recordings: HashMap<String, MidiRecordingSession>,
    pub completed_audio_recordings: Vec<(String, RecordingSession)>,
    pub completed_midi_recordings: Vec<(String, MidiRecordingSession)>,
}

/// Modulators, their cached values, and the MixOSC automation socket/state.
#[derive(Default)]
pub struct AutomationFields {
    pub modulators: Vec<crate::modulator::Modulator>,
    pub modulator_values: Option<Arc<HashMap<usize, f32>>>,
    #[cfg(feature = "mixosc")]
    pub mixosc_last_values: HashMap<(String, String), f32>,
    #[cfg(feature = "mixosc")]
    pub mixosc_socket: Option<UdpSocket>,
}

/// Dispatcher bookkeeping: worker availability, queued requests, and
/// offline-bounce jobs.
#[derive(Default)]
pub struct DispatchFields {
    pub ready_workers: Vec<usize>,
    pub pending_requests: VecDeque<Action>,
    pub offline_bounce_jobs: HashMap<String, OfflineBounceJob>,
    /// Bounce jobs registered while a plan cycle was still in flight; the
    /// work is handed to the reserved worker when the cycle completes
    /// (`on_all_tracks_finished`), so the bounce never races RT workers.
    pub pending_bounce_starts: Vec<(usize, crate::message::OfflineBounceWork)>,
    /// Worker index → track name for in-flight bounce jobs; the worker's
    /// terminal `Ready(id)` removes the job even on error/cancel paths
    /// whose `OfflineBounceFinished` payload carries no track name.
    pub bounce_worker_tracks: HashMap<usize, String>,
}
