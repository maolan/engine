//! Inline render on the hardware cycle thread — Phase 2 of the latency
//! rearchitecture (see `ARCHITECTURE.md`).
//!
//! When the engine runs in RT-inline mode (an audio device is open, the
//! backend supports the mid-cycle render hook, and `MAOLAN_RT_INLINE` is not
//! `0`), the hardware cycle thread coordinates the per-cycle render plan
//! between the capture fill and the playback drain:
//!
//! 1. The dispatcher sends `TracksFinished` — the "Go" signal — after
//!    advancing the transport and preparing task tracks.
//! 2. The backend cycle reads capture into the plan arena, then calls
//!    [`InlineRender::render_cycle`].
//! 3. `render_cycle` drains hardware MIDI input into routed track ports,
//!    rendezvous with the node workers by default, or executes nodes serially
//!    when `MAOLAN_RT_PARALLEL=0` (skipping the already-filled `Op::HwInput`),
//!    mixes any audio preview into the
//!    hardware-output arena, and publishes the per-cycle outcome (node
//!    results, the plan that ran, forwarded MIDI-in events).
//! 4. The backend cycle drains the arena to the playback device and the hw
//!    worker reports `HWFinished`; the dispatcher picks the outcome up in its
//!    `HWFinished` handler (meters, parameter echoes, MIDI thru/learn may lag
//!    one cycle).
//!
//! All state crossing thread boundaries lives in this context behind short
//! mutexes or `arc_swap` snapshots. The mutexes are uncontended by
//! construction: the dispatcher only touches the outcome mailbox between
//! `HWFinished` and the next Go, and the MIDI source between cycles.

use crate::hw::traits::HwMidiHub;
use crate::message::HwMidiEvent;
use crate::render_plan::{NodeId, Op, PlanSlot, RenderPlan, SharedPlan};
use crate::state::TrackHandle;
use crate::workers::worker::{NodeJobResult, Worker};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

impl std::fmt::Debug for InlineRender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InlineRender")
            .field(
                "render_requested",
                &self.render_requested.load(Ordering::Relaxed),
            )
            .finish_non_exhaustive()
    }
}

/// Worker id stamped on inline node results; inline results bypass the
/// worker-pool routing, so this only appears in logs.
pub(crate) const INLINE_WORKER_ID: usize = usize::MAX;

/// Non-blocking hardware-MIDI source the RT cycle thread may drain mid-cycle.
/// Implemented for every `HwMidiHub`; shared with the hw worker behind a
/// mutex that is only ever contended across cycle boundaries.
pub trait MidiInSource {
    fn read_midi_events_into(&mut self, out: &mut Vec<HwMidiEvent>);
}

impl<T: HwMidiHub> MidiInSource for T {
    fn read_midi_events_into(&mut self, out: &mut Vec<HwMidiEvent>) {
        HwMidiHub::read_events_into(self, out);
    }
}

/// One hardware-MIDI-input route resolved to a track handle, as the RT
/// thread needs it. Rebuilt by the dispatcher whenever it prepares a Go.
pub struct MidiInRoute {
    pub device: String,
    pub track: TrackHandle,
    pub port: usize,
}

/// Audio preview state shared between the dispatcher (start/stop) and the
/// cycle thread (per-cycle mix into the hardware-output arena). The cursor is
/// atomic so the RT thread can advance it without `&mut`.
pub(crate) struct SharedAudioPreview {
    pub samples: Arc<Vec<f32>>,
    pub channels: usize,
    pub total_frames: usize,
    pub cursor: AtomicU64,
}

/// Everything the dispatcher needs back from one inline cycle. Published by
/// `render_cycle` on the cycle thread, consumed by the `HWFinished` handler.
pub(crate) struct InlineCycleOutcome {
    /// The plan that was rendered; `None` when the cycle replayed the arena
    /// without rendering (transport paused mid-stream, offline bounce
    /// suspension, or a stale render that was silenced).
    pub plan: Option<SharedPlan>,
    /// Per-node execution results in node order (meters, parameter echoes).
    pub results: Vec<NodeJobResult>,
    /// Hardware MIDI-in events drained this cycle. On the render path they
    /// were already pushed to the routed track ports; the dispatcher still
    /// needs them for thru-routing, MIDI-learn and step-record.
    pub midi_in_events: Vec<HwMidiEvent>,
    /// The transport position this cycle rendered for (the Go tag).
    pub transport_rendered: i64,
    /// Back-pressure decision (Phase 4): the render was stale — the device
    /// had moved at least one full period past the tag — so the cycle was
    /// silenced instead of writing stale content. `skipped_frames` is the
    /// whole-period gap the transport must skip to resync.
    pub stale: bool,
    pub skipped_frames: u64,
}

struct OutcomeMailbox {
    ready: Option<InlineCycleOutcome>,
}

/// Shared RT-inline render context. Owned by the `Engine`, cloned into the
/// audio driver (which invokes [`InlineRender::render_cycle`] mid-cycle) and
/// into the hw worker (which installs the MIDI source).
pub struct InlineRender {
    plan_slot: Arc<PlanSlot>,
    /// Pinned at Go: capture, node jobs, and playback must use one arena
    /// even if the background builder publishes a replacement mid-cycle.
    cycle_plan: PlanSlot,
    /// Set by the dispatcher with each Go; consumed (swapped out) by the
    /// cycle thread when the render hook runs. Cycles never overlap, so a
    /// plain flag is sufficient.
    render_requested: AtomicBool,
    mailbox: Mutex<OutcomeMailbox>,
    /// Installed once by the hw worker at spawn; read (briefly locked) by
    /// the cycle thread to drain MIDI input mid-cycle.
    midi_source: Mutex<Option<Arc<Mutex<dyn MidiInSource + Send>>>>,
    /// Hardware-MIDI-input routes resolved to track handles; rebuilt by the
    /// dispatcher before each Go.
    midi_routes: arc_swap::ArcSwap<Vec<MidiInRoute>>,
    /// Audio preview shared with the cycle thread.
    preview: arc_swap::ArcSwap<Option<Arc<SharedAudioPreview>>>,
    /// Transport position the current Go renders for (Phase 4 tag).
    go_tag: AtomicI64,
    /// Set with the Go after a transport (re)start: the cycle thread
    /// re-anchors the capture-frame ↔ transport mapping instead of checking
    /// skew.
    reanchor: AtomicBool,
    anchor_capture: AtomicI64,
    anchor_transport: AtomicI64,
    anchor_valid: AtomicBool,
    /// Set by `render_cycle` when a stale render was silenced; consumed by
    /// the driver, which writes silence instead of draining the arena.
    stale_silence: AtomicBool,
    parallel_enabled: AtomicBool,
    sample_rate: AtomicU64,
    parallel: Mutex<crate::parallel_render::ParallelRender>,
}

impl InlineRender {
    pub fn new(plan_slot: Arc<PlanSlot>) -> Arc<Self> {
        Arc::new(Self {
            cycle_plan: PlanSlot::from(plan_slot.load_full()),
            plan_slot,
            render_requested: AtomicBool::new(false),
            mailbox: Mutex::new(OutcomeMailbox { ready: None }),
            midi_source: Mutex::new(None),
            midi_routes: arc_swap::ArcSwap::from_pointee(Vec::new()),
            preview: arc_swap::ArcSwap::from_pointee(None),
            go_tag: AtomicI64::new(0),
            reanchor: AtomicBool::new(false),
            anchor_capture: AtomicI64::new(0),
            anchor_transport: AtomicI64::new(0),
            anchor_valid: AtomicBool::new(false),
            stale_silence: AtomicBool::new(false),
            parallel_enabled: AtomicBool::new(false),
            sample_rate: AtomicU64::new(48_000),
            parallel: Mutex::new(crate::parallel_render::ParallelRender::new()),
        })
    }

    pub(crate) fn add_parallel_worker(
        &self,
        mailbox: crate::parallel_render::WorkerMailbox,
        thread: std::thread::Thread,
    ) {
        self.parallel
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .add_worker(mailbox, thread);
    }

    /// Configured at device open, before any Go can be issued.
    pub(crate) fn configure_parallel(&self, enabled: bool, sample_rate: u64) {
        self.sample_rate
            .store(sample_rate.max(1), Ordering::Release);
        self.parallel_enabled.store(enabled, Ordering::Release);
    }

    /// The hardware worker calls this AFTER the driver's playback drain,
    /// even on driver failure, and BEFORE publishing HWFinished. Silence is
    /// already committed on deadline miss; this barrier prevents late jobs
    /// racing the next capture, recording tap, plan reuse, or offline work.
    pub(crate) fn finish_pending_render(&self) {
        let skipped = self
            .parallel
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .finish_pending();
        if let Some(skipped) = skipped {
            if let Some(outcome) = self.lock_mailbox().ready.as_mut() {
                outcome.skipped_frames = outcome.skipped_frames.max(skipped);
            }
            self.invalidate_anchor();
        }
    }

    /// RT thread was configured: the hw worker installs its MIDI hub share.
    pub fn set_midi_source(&self, source: &Arc<Mutex<dyn MidiInSource + Send>>) {
        *self.lock_midi_source() = Some(source.clone());
    }

    /// Dispatcher (Go path): whether the upcoming cycle should execute the
    /// render plan inline.
    pub fn request_render(&self, render: bool) {
        self.cycle_plan.store(self.plan_slot.load_full());
        self.render_requested.store(render, Ordering::Release);
    }

    pub(crate) fn cycle_plan(&self) -> SharedPlan {
        self.cycle_plan.load_full()
    }

    /// Dispatcher (Go path): publish the resolved MIDI-in routes for the
    /// upcoming render.
    pub fn publish_midi_routes(&self, routes: Vec<MidiInRoute>) {
        self.midi_routes.store(Arc::new(routes));
    }

    pub fn publish_preview(&self, samples: Arc<Vec<f32>>, channels: usize, start_sample: usize) {
        let channels = channels.max(1);
        let total_frames = samples.len() / channels;
        self.preview
            .store(Arc::new(Some(Arc::new(SharedAudioPreview {
                samples,
                channels,
                total_frames,
                cursor: AtomicU64::new(start_sample as u64),
            }))));
    }

    pub fn clear_preview(&self) {
        self.preview.store(Arc::new(None));
    }

    /// Whether a preview is active; the Go/rearm conditions treat it like
    /// playback.
    pub fn preview_active(&self) -> bool {
        self.preview.load_full().is_some()
    }

    /// Dispatcher (`HWFinished` handler): take the outcome of the cycle that
    /// just finished, if it ran inline.
    pub(crate) fn take_outcome(&self) -> Option<InlineCycleOutcome> {
        self.lock_mailbox().ready.take()
    }

    /// Dispatcher (Go path, Phase 4): tag the cycle with the transport
    /// position it renders for. `reanchor` re-establishes the
    /// capture-frame ↔ transport mapping at the cycle thread (transport
    /// (re)start, or after a stale cycle was skipped).
    pub fn publish_go(&self, transport_sample: i64, reanchor: bool) {
        self.go_tag.store(transport_sample, Ordering::Release);
        self.reanchor.store(reanchor, Ordering::Release);
    }

    /// Driver: true when `render_cycle` decided the render was stale and the
    /// playback drain must write silence instead of the arena. Consumed once
    /// per cycle.
    pub fn take_stale_silence(&self) -> bool {
        self.stale_silence.swap(false, Ordering::Acquire)
    }

    /// Dispatcher: drop the capture-frame anchor so the next render cycle
    /// re-establishes it (after a stale skip the mapping is undefined).
    pub fn invalidate_anchor(&self) {
        self.anchor_valid.store(false, Ordering::Release);
    }

    /// Backend deadline check after render/copy. The output missed its safe
    /// playback window, so report the gap through the same Phase 4 mailbox
    /// as a pre-render stale cycle. Called before HWFinished is published.
    #[cfg(any(target_os = "freebsd", test))]
    pub(crate) fn discard_completed_cycle(&self, skipped_frames: u64) {
        if let Some(outcome) = self.lock_mailbox().ready.as_mut() {
            outcome.stale = true;
            outcome.skipped_frames = outcome.skipped_frames.max(skipped_frames);
            outcome.plan = None;
            outcome.results.clear();
        }
        self.invalidate_anchor();
    }

    fn lock_mailbox(&self) -> std::sync::MutexGuard<'_, OutcomeMailbox> {
        self.mailbox.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_midi_source(
        &self,
    ) -> std::sync::MutexGuard<'_, Option<Arc<Mutex<dyn MidiInSource + Send>>>> {
        self.midi_source.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Execute one inline render cycle on the hardware cycle thread. Called
    /// by the backend after the capture fill and before the playback drain.
    ///
    /// `actual_capture` is the driver's capture-frame counter sampled right
    /// after the capture read (`None` when the backend reports none). It
    /// drives the Phase 4 staleness check: if the device has run at least a
    /// full period past the tagged transport position, this render is stale
    /// — it is discarded, the playback drain writes silence, and the outcome
    /// carries the gap the dispatcher must skip to resync.
    ///
    /// RT notes: no logging and no panic paths here; the mutexes are
    /// uncontended by construction (see the module docs) and poison-safe via
    /// `into_inner`.
    pub fn render_cycle(&self, frames: u32, actual_capture: Option<i64>) {
        let render = self.render_requested.swap(false, Ordering::Acquire);
        let tag = self.go_tag.load(Ordering::Acquire);

        // Phase 4: capture-frame ↔ transport mapping. Established on the
        // first cycle after a (re)start (or when re-anchored after a stale
        // skip); every later cycle verifies the tagged transport still
        // matches where the device actually is.
        let mut skipped = 0_u64;
        let mut stale = false;
        if let Some(actual) = actual_capture {
            if self.reanchor.swap(false, Ordering::Acquire)
                || !self.anchor_valid.load(Ordering::Acquire)
            {
                self.anchor_capture.store(actual, Ordering::Relaxed);
                self.anchor_transport.store(tag, Ordering::Relaxed);
                self.anchor_valid.store(true, Ordering::Release);
            } else {
                let anchor_capture = self.anchor_capture.load(Ordering::Relaxed);
                let anchor_transport = self.anchor_transport.load(Ordering::Relaxed);
                if let Some(gap) =
                    stale_skip((anchor_capture, anchor_transport), tag, actual, frames)
                {
                    stale = true;
                    skipped = gap;
                    self.stale_silence.store(true, Ordering::Release);
                    self.anchor_valid.store(false, Ordering::Release);
                }
            }
        }

        // Hardware MIDI-in: drained right after the capture read. On the
        // render path events land in the routed track ports before any task
        // runs, so this cycle's render sees them. A stale cycle must not push:
        // the events belong to the next (resynced) render, delivered via the
        // dispatcher buffer.
        let mut midi_events: Vec<HwMidiEvent> = Vec::with_capacity(64);
        let source = self.lock_midi_source().clone();
        if let Some(source) = source
            && let Ok(mut source) = source.lock()
        {
            source.read_midi_events_into(&mut midi_events);
        }
        if midi_events.len() > 1 && frames > 1 {
            spread_event_frames(&mut midi_events, frames);
        }
        if render && !stale && !midi_events.is_empty() {
            let routes = self.midi_routes.load();
            for hw_event in &midi_events {
                for route in routes.iter().filter(|r| r.device == hw_event.device) {
                    route.track.lock().push_hw_midi_events_to_port(
                        route.port,
                        std::slice::from_ref(&hw_event.event),
                    );
                }
            }
        }

        let mut outcome = InlineCycleOutcome {
            plan: None,
            results: Vec::new(),
            midi_in_events: midi_events,
            transport_rendered: tag,
            stale,
            skipped_frames: skipped,
        };

        if !stale && (render || self.preview_active()) {
            let plan = self.cycle_plan();
            if render {
                crate::cycle_trace::mark(crate::cycle_trace::TracePoint::RenderDispatched);
                if self.parallel_enabled.load(Ordering::Acquire) {
                    let period = std::time::Duration::from_secs_f64(
                        f64::from(frames) / self.sample_rate.load(Ordering::Acquire).max(1) as f64,
                    );
                    let results = self
                        .parallel
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .render(plan.clone(), period);
                    if let Some(results) = results {
                        outcome.results = results;
                    } else {
                        outcome.stale = true;
                        outcome.skipped_frames = u64::from(frames);
                        self.stale_silence.store(true, Ordering::Release);
                        self.invalidate_anchor();
                    }
                } else {
                    outcome
                        .results
                        .reserve(plan.nodes.len().saturating_sub(plan.hw_in_map.len()));
                    for node in 0..plan.nodes.len() as NodeId {
                        if matches!(&plan.nodes[node as usize], Op::HwInput { .. }) {
                            continue;
                        }
                        outcome.results.push(Worker::process_node_job_result(
                            INLINE_WORKER_ID,
                            crate::executor::NodeJob {
                                epoch: 0,
                                plan: plan.clone(),
                                node,
                            },
                        ));
                    }
                }
                crate::cycle_trace::mark(crate::cycle_trace::TracePoint::RenderEnd);
            }
            // Audio preview overwrites the hardware-output arena even when
            // the transport is paused (no render), so preview-only cycles
            // still mix it here.
            if !outcome.stale {
                mix_preview_into_hw_outs(self, &plan, frames as usize);
                outcome.plan = Some(plan);
            }
        }

        self.lock_mailbox().ready = Some(outcome);
    }
}

/// Phase 4 staleness check: given the capture-frame ↔ transport anchor and
/// the current Go tag, compare the capture frame the driver just read
/// against where the tag says it should be. Returns the whole-period gap to
/// skip when the device has run at least one full period ahead — the render
/// for `tag` would land a period late and must be silenced instead.
fn stale_skip(anchor: (i64, i64), tag: i64, actual: i64, frames: u32) -> Option<u64> {
    if frames == 0 {
        return None;
    }
    let expected = anchor.0 + (tag - anchor.1);
    let skew = actual - expected;
    let frames = i64::from(frames);
    if skew >= frames {
        Some((skew - skew % frames) as u64)
    } else {
        None
    }
}

/// Spread events without trustworthy timestamps across the cycle, matching
/// `spread_hw_event_frames` in `workers/hw_worker.rs` (the raw MIDI reader
/// stamps everything with frame 0).
fn spread_event_frames(events: &mut [HwMidiEvent], frames: u32) {
    let n = events.len() as u32;
    for (idx, event) in events.iter_mut().enumerate() {
        event.event.frame = ((idx as u64 * (frames - 1) as u64) / n as u64) as u32;
    }
}

/// Mix (actually: overwrite, matching the historic behavior) the hardware
/// outputs with the audio preview for `cycle_samples` frames, advancing the
/// shared cursor. Runs on whichever thread just produced the arena content
/// (the cycle thread inline, the dispatcher on the pool path).
pub(crate) fn mix_preview_into_hw_outs(
    inline: &InlineRender,
    plan: &RenderPlan,
    cycle_samples: usize,
) {
    if cycle_samples == 0 {
        return;
    }
    let snapshot = inline.preview.load_full();
    let Some(preview) = snapshot.as_ref() else {
        return;
    };
    let channels = preview.channels.max(1);
    let total_frames = preview.total_frames;
    let mut cursor = preview.cursor.load(Ordering::Relaxed) as usize;
    if cursor >= total_frames {
        inline.clear_preview();
        return;
    }

    for &(buffer, channel) in &plan.hw_out_map {
        // Safety: the caller runs after every producer of the output arena
        // completed for this cycle and before the hardware backend drains
        // it (inline: this is the single cycle thread; pool: the dispatcher
        // between render completion and TracksFinished).
        let dst = unsafe { &mut *plan.buffer_ptr(buffer) };
        let frames = cycle_samples.min(dst.len());
        dst[..frames].fill(0.0);
        let source_channel = channel.min(channels - 1);
        for (frame, out) in dst.iter_mut().take(frames).enumerate() {
            let source_frame = cursor + frame;
            if source_frame >= total_frames {
                break;
            }
            let sample_index = source_frame * channels + source_channel;
            *out = preview.samples.get(sample_index).copied().unwrap_or(0.0);
        }
    }

    cursor = cursor.saturating_add(cycle_samples);
    preview.cursor.store(cursor as u64, Ordering::Relaxed);
    if cursor >= total_frames {
        inline.clear_preview();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::HwMidiEvent;
    use crate::render_plan::{DelayLine, RenderPlan};
    use crate::track::Track;
    use std::cell::UnsafeCell;
    use std::collections::HashMap;

    struct TestSlotGuard {
        collector: Option<basedrop::Collector>,
        slot: Option<Arc<PlanSlot>>,
    }

    impl TestSlotGuard {
        fn new(plan: RenderPlan) -> Self {
            let collector = basedrop::Collector::new();
            let owned = basedrop::Owned::new(&collector.handle(), plan);
            Self {
                collector: Some(collector),
                slot: Some(Arc::new(PlanSlot::from_pointee(owned))),
            }
        }

        fn slot(&self) -> Arc<PlanSlot> {
            self.slot.as_ref().expect("test slot").clone()
        }
    }

    impl Drop for TestSlotGuard {
        fn drop(&mut self) {
            self.slot.take();
            let Some(mut collector) = self.collector.take() else {
                return;
            };
            collector.collect();
            let _ = collector.try_cleanup();
        }
    }

    /// One hw-input buffer (0), one zeroed buffer (1), one sum of both (2).
    fn sum_plan() -> RenderPlan {
        RenderPlan {
            buffer_size: 8,
            buffers: (0..3).map(|_| UnsafeCell::new(vec![0.0; 8])).collect(),
            buffer_latencies: (0..3)
                .map(|_| std::sync::atomic::AtomicUsize::new(0))
                .collect(),
            nodes: vec![
                Op::HwInput {
                    channel: 0,
                    output: 0,
                },
                Op::Zero { output: 1 },
                Op::Sum {
                    inputs: vec![0, 1],
                    delays: vec![
                        UnsafeCell::new(DelayLine::new()),
                        UnsafeCell::new(DelayLine::new()),
                    ],
                    output: 2,
                },
            ],
            indegree: vec![0, 0, 0],
            dependents: vec![vec![], vec![], vec![]],
            sources: vec![0, 1, 2],
            hw_in_map: vec![(0, 0)],
            hw_out_map: vec![(2, 0)],
            port_map: HashMap::new(),
            midi_edges: vec![],
            forced: vec![],
        }
    }

    struct FakeMidiSource {
        events: Vec<HwMidiEvent>,
    }

    impl MidiInSource for FakeMidiSource {
        fn read_midi_events_into(&mut self, out: &mut Vec<HwMidiEvent>) {
            out.extend(self.events.iter().cloned());
        }
    }

    #[test]
    fn go_pins_one_plan_across_capture_render_and_playback() {
        let guard = TestSlotGuard::new(sum_plan());
        let slot = guard.slot();
        let ctx = InlineRender::new(slot.clone());
        ctx.request_render(true);
        let captured_plan = ctx.cycle_plan();
        // Safety: capture fill before rendering, on the only test thread.
        unsafe {
            (&mut *captured_plan.buffer_ptr(0)).fill(0.75);
        }
        let replacement = Arc::new(basedrop::Owned::new(
            &guard.collector.as_ref().unwrap().handle(),
            sum_plan(),
        ));
        slot.store(replacement.clone());
        ctx.render_cycle(8, None);
        let outcome = ctx.take_outcome().unwrap();
        assert!(Arc::ptr_eq(outcome.plan.as_ref().unwrap(), &captured_plan));
        assert!(Arc::ptr_eq(&ctx.cycle_plan(), &captured_plan));
        // Safety: render completed; playback uses the same capture arena.
        let playback_plan = ctx.cycle_plan();
        assert_eq!(unsafe { playback_plan.buffer(2) }, &[0.75; 8]);
        ctx.request_render(true);
        assert!(Arc::ptr_eq(&ctx.cycle_plan(), &replacement));
    }

    #[test]
    fn parallel_failure_requests_silence_and_suppresses_preview_until_retired() {
        let guard = TestSlotGuard::new(sum_plan());
        let ctx = InlineRender::new(guard.slot());
        ctx.configure_parallel(true, 48_000);
        ctx.publish_preview(Arc::new(vec![0.5; 16]), 1, 0);
        ctx.publish_go(0, true);
        ctx.request_render(true);
        ctx.render_cycle(8, Some(1000));
        assert!(ctx.take_stale_silence());
        // This is the hardware-worker barrier, after submitting silence.
        ctx.finish_pending_render();
        let outcome = ctx.take_outcome().unwrap();
        assert!(outcome.stale);
        assert!(outcome.skipped_frames >= 8);
        assert!(outcome.plan.is_none());
        assert!(outcome.results.is_empty());
        assert_eq!(
            ctx.preview
                .load_full()
                .as_ref()
                .as_ref()
                .unwrap()
                .cursor
                .load(Ordering::Relaxed),
            0
        );
        ctx.configure_parallel(false, 48_000);
        ctx.publish_go(16, false);
        ctx.request_render(true);
        ctx.render_cycle(8, Some(1016));
        assert!(!ctx.take_outcome().unwrap().stale);
    }

    #[test]
    fn render_cycle_executes_non_hwinput_nodes_and_publishes_outcome() {
        let guard = TestSlotGuard::new(sum_plan());
        let slot = guard.slot();
        let ctx = InlineRender::new(slot.clone());

        // Simulate the driver capture fill of the hw-input arena buffer.
        let plan = slot.load_full();
        let capture = [0.25_f32; 8];
        // Safety: test thread, no cycle running.
        unsafe { (&mut *plan.buffer_ptr(0)).copy_from_slice(&capture) };
        drop(plan);

        ctx.request_render(true);
        ctx.render_cycle(8, None);

        let outcome = ctx.take_outcome().expect("outcome");
        let plan = outcome.plan.expect("plan recorded");
        assert_eq!(outcome.results.len(), 2, "Zero + Sum; HwInput skipped");
        assert_eq!(outcome.results[0].node, 1);
        assert_eq!(outcome.results[1].node, 2);
        // Safety: test thread, render completed.
        let sum = unsafe { plan.buffer(2) };
        assert_eq!(sum, &capture, "sum of capture + silence");
    }

    #[test]
    fn render_cycle_skipped_publishes_empty_outcome() {
        let guard = TestSlotGuard::new(sum_plan());
        let ctx = InlineRender::new(guard.slot());
        ctx.request_render(false);
        ctx.render_cycle(8, None);
        let outcome = ctx.take_outcome().expect("outcome");
        assert!(outcome.plan.is_none(), "no render ran");
        assert!(outcome.results.is_empty());
    }

    #[test]
    fn render_cycle_delivers_midi_to_routed_track_ports() {
        let guard = TestSlotGuard::new(sum_plan());
        let ctx = InlineRender::new(guard.slot());
        let source: Arc<Mutex<dyn MidiInSource + Send>> = Arc::new(Mutex::new(FakeMidiSource {
            events: vec![HwMidiEvent {
                device: "kbd".to_string(),
                event: crate::midi::io::MidiEvent::new(0, vec![0x90, 60, 100]),
            }],
        }));
        ctx.set_midi_source(&source);

        let track = Arc::new(Track::new("t".to_string(), 0, 0, 1, 0, 8, 48_000.0));
        ctx.publish_midi_routes(vec![MidiInRoute {
            device: "kbd".to_string(),
            track: track.clone(),
            port: 0,
        }]);
        ctx.request_render(true);
        ctx.render_cycle(8, None);

        // Safety: test thread, no cycle is running; the render completed.
        let track_lock = track.lock();
        let buffered = unsafe { track_lock.midi.ins[0].buffer() };
        assert_eq!(buffered.len(), 1, "event delivered to the track port");
        assert_eq!(buffered[0].data, vec![0x90, 60, 100]);
        // Events are also forwarded to the dispatcher for thru/learn.
        let outcome = ctx.take_outcome().expect("outcome");
        assert_eq!(outcome.midi_in_events.len(), 1);
    }

    #[test]
    fn preview_mixes_into_hw_out_arena_even_without_render() {
        let guard = TestSlotGuard::new(sum_plan());
        let ctx = InlineRender::new(guard.slot());
        let samples: Vec<f32> = (0..16).map(|i| i as f32 / 16.0).collect();
        ctx.publish_preview(Arc::new(samples), 1, 0);
        ctx.request_render(false);
        ctx.render_cycle(8, None);

        let outcome = ctx.take_outcome().expect("outcome");
        let plan = outcome.plan.expect("preview mix recorded the plan");
        // Safety: test thread, render_cycle completed.
        let out = unsafe { plan.buffer(2) };
        let expected: Vec<f32> = (0..8).map(|i| i as f32 / 16.0).collect();
        assert_eq!(out, &expected[..], "preview overwrote the hw-out buffer");
        // Cursor advanced by one cycle; the preview is 16 frames, so half
        // remains and the preview must still be active.
        assert!(ctx.preview_active(), "half the preview remains");
    }

    #[test]
    fn midi_port_buffers_are_cleared_between_cycles() {
        // MIDIIO port buffers are drained by the track task each cycle; a
        // second render with no new input must not see the first cycle's
        // events again. Exercises that render_cycle pushes exactly what the
        // source drained.
        let guard = TestSlotGuard::new(sum_plan());
        let ctx = InlineRender::new(guard.slot());
        let track = Arc::new(Track::new("t".to_string(), 0, 0, 1, 0, 8, 48_000.0));
        ctx.publish_midi_routes(vec![MidiInRoute {
            device: "kbd".to_string(),
            track: track.clone(),
            port: 0,
        }]);

        ctx.request_render(false);
        ctx.render_cycle(8, None);
        let outcome = ctx.take_outcome().expect("outcome");
        assert!(outcome.midi_in_events.is_empty());

        // A source delivering one event on the second cycle only.
        let source: Arc<Mutex<dyn MidiInSource + Send>> = Arc::new(Mutex::new(FakeMidiSource {
            events: vec![HwMidiEvent {
                device: "kbd".to_string(),
                event: crate::midi::io::MidiEvent::new(0, vec![0xB0, 7, 99]),
            }],
        }));
        ctx.set_midi_source(&source);
        ctx.request_render(true);
        ctx.render_cycle(8, None);
        // Safety: test thread, no cycle is running; the render completed.
        let track_lock = track.lock();
        let buffered = unsafe { track_lock.midi.ins[0].buffer() };
        assert_eq!(buffered.len(), 1, "exactly the new event");
        assert_eq!(buffered[0].data, vec![0xB0, 7, 99]);
    }

    #[test]
    fn outcome_mailbox_replaces_stale_outcome() {
        let guard = TestSlotGuard::new(sum_plan());
        let ctx = InlineRender::new(guard.slot());
        ctx.request_render(false);
        ctx.render_cycle(8, None);
        ctx.render_cycle(8, None);
        // Two cycles without a dispatcher drain: the second replaces the
        // first instead of leaking or queueing.
        assert!(ctx.take_outcome().is_some());
        assert!(ctx.take_outcome().is_none());
    }

    #[test]
    fn stale_skip_math() {
        // Anchor: capture frame 1000 corresponded to transport 0.
        let anchor = (1000_i64, 0_i64);
        // Device exactly on tag: no skip, even at exactly one period late
        // minus one frame.
        assert_eq!(stale_skip(anchor, 8, 1008, 8), None);
        assert_eq!(stale_skip(anchor, 16, 1015, 8), None);
        // A full period behind: skip the whole-period part of the gap.
        assert_eq!(stale_skip(anchor, 8, 1016, 8), Some(8));
        // skew 29 frames → skip the whole 24, leaving the sub-period part.
        assert_eq!(stale_skip(anchor, 8, 1037, 8), Some(24));
        // The device cannot be behind the tag (it paces); ignore negatives.
        assert_eq!(stale_skip(anchor, 16, 1000, 8), None);
        assert_eq!(
            stale_skip(anchor, 8, 1000, 0),
            None,
            "zero frames: no check"
        );
    }

    #[test]
    fn completed_render_can_be_discarded_when_backend_deadline_is_missed() {
        let guard = TestSlotGuard::new(sum_plan());
        let ctx = InlineRender::new(guard.slot());
        ctx.publish_go(0, true);
        ctx.request_render(true);
        ctx.render_cycle(8, Some(1000));
        ctx.discard_completed_cycle(16);
        let outcome = ctx.take_outcome().expect("completed cycle");
        assert!(outcome.stale);
        assert_eq!(outcome.skipped_frames, 16);
        assert!(outcome.plan.is_none());
        assert!(outcome.results.is_empty());
        ctx.publish_go(24, false);
        ctx.request_render(true);
        ctx.render_cycle(8, Some(1024));
        assert!(!ctx.take_outcome().unwrap().stale);
    }

    #[test]
    fn stale_render_is_silenced_and_reports_the_gap() {
        let guard = TestSlotGuard::new(sum_plan());
        let ctx = InlineRender::new(guard.slot());

        // Cycle 1: (re)start anchors the mapping (capture 1000 ↔ tag 0).
        ctx.publish_go(0, true);
        ctx.request_render(true);
        ctx.render_cycle(8, Some(1000));
        let outcome = ctx.take_outcome().expect("outcome");
        assert!(!outcome.stale, "anchoring cycle renders");
        assert!(ctx.take_outcome().is_none());

        // Cycle 2: device on tag — renders.
        ctx.publish_go(8, false);
        ctx.request_render(true);
        ctx.render_cycle(8, Some(1008));
        assert!(!ctx.take_outcome().expect("outcome").stale);

        // Cycle 3: the device has run two periods ahead of the tag — the
        // render is stale: no execution, silence requested, gap reported.
        ctx.publish_go(16, false);
        ctx.request_render(true);
        ctx.render_cycle(8, Some(1032));
        let outcome = ctx.take_outcome().expect("outcome");
        assert!(outcome.stale);
        assert_eq!(outcome.skipped_frames, 16);
        assert!(outcome.plan.is_none(), "stale render discarded");
        assert!(outcome.results.is_empty());
        assert!(
            ctx.take_stale_silence(),
            "driver must silence the playback drain"
        );
        assert!(!ctx.take_stale_silence(), "silence flag consumed once");

        // Cycle 4: anchor was invalidated, so this cycle re-anchors and
        // renders normally again.
        ctx.publish_go(32, false);
        ctx.request_render(true);
        ctx.render_cycle(8, Some(1032));
        let outcome = ctx.take_outcome().expect("outcome");
        assert!(!outcome.stale, "re-anchored cycle renders");
    }
}
