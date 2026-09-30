//! Env-gated cycle-timeline instrumentation (Phase 1 of the latency
//! rearchitecture; see ARCHITECTURE.md).
//!
//! When `MAOLAN_CYCLE_TRACE` is present in the environment (any value except
//! `0`), each audio cycle records nanosecond timestamps at fixed pipeline
//! points into a preallocated ring of per-cycle slots. Every 100th cycle the
//! dispatcher logs a single-line summary of the segment durations, and fixed
//! histogram rings are dumped when the transport stops.
//!
//! All entry points are a relaxed atomic flag load plus an early return when
//! disabled; enabled hot paths only perform fixed-index atomic stores into
//! preallocated storage (no allocation, no locking).
//!
//! Stamp attribution: all points are stored under the sequence number that
//! `CURRENT_SEQ` holds at store time. `begin_cycle` runs on the hw cycle
//! thread when the Go signal (`TracksFinished`) arrives, so a slot collects
//! the tail of the previous pipeline phase (render + Go) followed by the hw
//! cycle that the Go started. Concretely, slot N holds: hw cycle N−1's
//! `HwFinishedSent`, the render for cycle N (`RenderDispatched`,
//! `RenderEnd`, plugin waits), `GoSent`/`GoReceived` for cycle N, and, after
//! cycle N runs, `HwFinishedReceived` stays in slot N−1 while
//! `HwCycleStart`..`HwCycleEnd` land in slot N. `log_summary` and the
//! histogram account for this cross-slot layout.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Instant;

/// Number of per-cycle slots in the timeline ring.
const RING_SLOTS: usize = 1024;
/// Number of samples in each histogram ring (overwrite-oldest).
const HISTOGRAM_SLOTS: usize = 4096;
/// Emit a summary every Nth cycle sequence.
const SUMMARY_INTERVAL: u64 = 100;

/// Fixed pipeline measurement points. Order is part of the layout; do not
/// reorder. Appending is safe.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(usize)]
pub(crate) enum TracePoint {
    HwCycleStart = 0,
    CaptureReadDone,
    RenderDispatched,
    RenderEnd,
    PlaybackWriteDone,
    HwCycleEnd,
    HwFinishedSent,
    HwFinishedReceived,
    TransportAdvanced,
    GoSent,
    GoReceived,
    /// Direct-mmap path only: the period was copied into the playback ring
    /// and the post-copy deadline check passed.
    PlaybackMapDone,
}

const N_POINTS: usize = TracePoint::PlaybackMapDone as usize + 1;
const _: () = assert!(N_POINTS == 12);

/// Frame-delta metrics stored alongside the timestamps. These are not wall
/// clock segments but ring/pointer distances measured by the cycle, and
/// they form the measured latency budget: how stale the capture was when
/// read, and how far ahead of the play pointer the playback was written.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(usize)]
pub(crate) enum FrameMetric {
    /// Capture frames produced beyond the end of the period just read:
    /// how old the data was when the engine consumed it.
    CaptureOvershootFrames = 0,
    /// Playback frames between the write position and the play pointer at
    /// write time: how far ahead of the DAC the freshly rendered period sits.
    PlaybackLeadFrames,
}

const N_FRAME_METRICS: usize = FrameMetric::PlaybackLeadFrames as usize + 1;

/// One per-cycle slot: one stamp per point plus the plugin-wait accumulator
/// and the frame-delta metrics.
struct CycleSlot {
    points: [AtomicU64; N_POINTS],
    plugin_wait_ns: AtomicU64,
    frame_metrics: [AtomicI64; N_FRAME_METRICS],
}

impl CycleSlot {
    fn new() -> Self {
        Self {
            points: std::array::from_fn(|_| AtomicU64::new(0)),
            plugin_wait_ns: AtomicU64::new(0),
            frame_metrics: std::array::from_fn(|_| AtomicI64::new(0)),
        }
    }

    fn clear(&self) {
        for point in &self.points {
            point.store(0, Ordering::Relaxed);
        }
        self.plugin_wait_ns.store(0, Ordering::Relaxed);
        for metric in &self.frame_metrics {
            metric.store(0, Ordering::Relaxed);
        }
    }

    fn point(&self, point: TracePoint) -> u64 {
        self.points[point as usize].load(Ordering::Relaxed)
    }

    fn frame_metric(&self, metric: FrameMetric) -> i64 {
        self.frame_metrics[metric as usize].load(Ordering::Relaxed)
    }
}

struct Histogram {
    samples: Vec<AtomicU64>,
    next: AtomicU64,
}

impl Histogram {
    fn new() -> Self {
        Self {
            samples: (0..HISTOGRAM_SLOTS).map(|_| AtomicU64::new(0)).collect(),
            next: AtomicU64::new(0),
        }
    }

    fn push(&self, value_ns: u64) {
        if value_ns == 0 {
            return;
        }
        let index = (self.next.fetch_add(1, Ordering::Relaxed) as usize) % HISTOGRAM_SLOTS;
        self.samples[index].store(value_ns, Ordering::Relaxed);
    }
}

struct TraceStorage {
    origin: Instant,
    slots: Vec<CycleSlot>,
    hist_hw_cycle: Histogram,
    hist_render: Histogram,
    hist_plugin_wait: Histogram,
}

impl TraceStorage {
    fn new() -> Self {
        Self {
            origin: Instant::now(),
            slots: (0..RING_SLOTS).map(|_| CycleSlot::new()).collect(),
            hist_hw_cycle: Histogram::new(),
            hist_render: Histogram::new(),
            hist_plugin_wait: Histogram::new(),
        }
    }
}

static ENABLED: OnceLock<bool> = OnceLock::new();
static STORAGE: OnceLock<TraceStorage> = OnceLock::new();
static CURRENT_SEQ: AtomicU64 = AtomicU64::new(0);
static SUMMARY_DUE: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
static FORCE: AtomicU64 = AtomicU64::new(0);

fn env_flag() -> bool {
    match std::env::var("MAOLAN_CYCLE_TRACE") {
        Ok(value) => value != "0",
        Err(_) => false,
    }
}

/// True when cycle tracing is active. Cached after the first call.
pub(crate) fn enabled() -> bool {
    #[cfg(test)]
    {
        let forced = FORCE.load(Ordering::Relaxed);
        if forced != 0 {
            return forced == 2;
        }
    }
    *ENABLED.get_or_init(env_flag)
}

#[cfg(test)]
pub(crate) fn force(on: bool) {
    FORCE.store(if on { 2 } else { 1 }, Ordering::SeqCst);
    if on {
        let _ = STORAGE.get_or_init(TraceStorage::new);
    }
}

fn slot_index(seq: u64) -> usize {
    (seq % RING_SLOTS as u64) as usize
}

/// Start a new cycle on the hw cycle thread: bump the global sequence, clear
/// the new sequence's slot, and flag a summary when the sequence hits the
/// interval. Returns the new sequence, or 0 when tracing is disabled.
pub(crate) fn begin_cycle() -> u64 {
    if !enabled() {
        return 0;
    }
    let storage = STORAGE.get_or_init(TraceStorage::new);
    let seq = CURRENT_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    storage.slots[slot_index(seq)].clear();
    if seq.is_multiple_of(SUMMARY_INTERVAL) {
        SUMMARY_DUE.store(seq, Ordering::Relaxed);
    }
    seq
}

/// Store the current time (ns since origin) for `point` under the current
/// sequence. On `HwCycleEnd` also pushes histogram samples: the hw-cycle
/// duration from the current slot, render and plugin-wait from the previous
/// slot (where their pipeline phase was attributed).
pub(crate) fn mark(point: TracePoint) {
    if !enabled() {
        return;
    }
    let Some(storage) = STORAGE.get() else {
        return;
    };
    let now = storage.origin.elapsed().as_nanos() as u64;
    let seq = CURRENT_SEQ.load(Ordering::Relaxed);
    let slot = &storage.slots[slot_index(seq)];
    slot.points[point as usize].store(now, Ordering::Relaxed);
    if point == TracePoint::HwCycleEnd {
        push_histogram_samples(storage, seq);
    }
}

fn push_histogram_samples(storage: &TraceStorage, seq: u64) {
    let slot = &storage.slots[slot_index(seq)];
    let hw_start = slot.point(TracePoint::HwCycleStart);
    if hw_start != 0 {
        storage
            .hist_hw_cycle
            .push(slot.point(TracePoint::HwCycleEnd).saturating_sub(hw_start));
    }
    if seq >= 1 {
        let prev = &storage.slots[slot_index(seq - 1)];
        let render_start = prev.point(TracePoint::RenderDispatched);
        if render_start != 0 {
            storage.hist_render.push(
                prev.point(TracePoint::RenderEnd)
                    .saturating_sub(render_start),
            );
        }
        storage
            .hist_plugin_wait
            .push(prev.plugin_wait_ns.load(Ordering::Relaxed));
    }
}

/// RAII guard accumulating the enclosing scope's duration into the current
/// cycle's plugin-wait total. Free (no storage access) when tracing is
/// disabled.
pub(crate) struct PluginWaitGuard {
    start: Option<Instant>,
    seq: u64,
}

pub(crate) fn plugin_wait() -> PluginWaitGuard {
    if !enabled() {
        return PluginWaitGuard {
            start: None,
            seq: 0,
        };
    }
    if STORAGE.get().is_none() {
        return PluginWaitGuard {
            start: None,
            seq: 0,
        };
    }
    PluginWaitGuard {
        start: Some(Instant::now()),
        seq: CURRENT_SEQ.load(Ordering::Relaxed),
    }
}

impl Drop for PluginWaitGuard {
    fn drop(&mut self) {
        let Some(start) = self.start else {
            return;
        };
        let Some(storage) = STORAGE.get() else {
            return;
        };
        let elapsed = start.elapsed().as_nanos() as u64;
        let slot = &storage.slots[slot_index(self.seq)];
        slot.plugin_wait_ns.fetch_add(elapsed, Ordering::Relaxed);
    }
}

/// Store a frame-delta metric for the current sequence. Called from the
/// cycle thread; a relaxed atomic store into preallocated storage.
pub(crate) fn set_frame_metric(metric: FrameMetric, frames: i64) {
    if !enabled() {
        return;
    }
    let Some(storage) = STORAGE.get() else {
        return;
    };
    let seq = CURRENT_SEQ.load(Ordering::Relaxed);
    storage.slots[slot_index(seq)].frame_metrics[metric as usize].store(frames, Ordering::Relaxed);
}

/// Dispatcher side: return and clear the sequence whose summary is due, if
/// any.
pub(crate) fn take_summary_due() -> Option<u64> {
    if !enabled() {
        return None;
    }
    let due = SUMMARY_DUE.swap(0, Ordering::Relaxed);
    (due != 0).then_some(due)
}

fn segment_ms(start_ns: u64, end_ns: u64) -> String {
    if start_ns == 0 || end_ns == 0 || end_ns < start_ns {
        "-".to_string()
    } else {
        format!("{:.2}ms", (end_ns - start_ns) as f64 / 1_000_000.0)
    }
}

/// Log one summary line for the cycle timeline ending at `due_seq` (the value
/// from `take_summary_due`). The render/Go segments live in the previous
/// slot; fall back to the current slot when the previous one has no render
/// stamps (e.g. first cycles, or the unit test's single-slot layout).
pub(crate) fn log_summary(due_seq: u64) {
    if !enabled() || due_seq < 2 {
        return;
    }
    let Some(storage) = STORAGE.get() else {
        return;
    };
    let cur = &storage.slots[slot_index(due_seq)];
    let prev = &storage.slots[slot_index(due_seq - 1)];
    let prev_has_render = prev.point(TracePoint::RenderDispatched) != 0;
    let render_slot = if prev_has_render { prev } else { cur };

    let hw_cycle = segment_ms(
        cur.point(TracePoint::HwCycleStart),
        cur.point(TracePoint::HwCycleEnd),
    );
    let capture_wait = segment_ms(
        cur.point(TracePoint::HwCycleStart),
        cur.point(TracePoint::CaptureReadDone),
    );
    let render = segment_ms(
        render_slot.point(TracePoint::RenderDispatched),
        render_slot.point(TracePoint::RenderEnd),
    );
    let pre_go = segment_ms(
        render_slot.point(TracePoint::RenderEnd),
        render_slot.point(TracePoint::GoSent),
    );
    let go_to_hw_end = segment_ms(
        render_slot.point(TracePoint::GoSent),
        cur.point(TracePoint::HwCycleEnd),
    );
    let hwfin_handoff = segment_ms(
        render_slot.point(TracePoint::HwFinishedSent),
        render_slot.point(TracePoint::HwFinishedReceived),
    );
    let dispatch = segment_ms(
        render_slot.point(TracePoint::HwFinishedReceived),
        render_slot.point(TracePoint::RenderDispatched),
    );
    let plugin_wait = if prev_has_render || cur.plugin_wait_ns.load(Ordering::Relaxed) != 0 {
        format!(
            "{:.2}ms",
            render_slot.plugin_wait_ns.load(Ordering::Relaxed) as f64 / 1_000_000.0
        )
    } else {
        "-".to_string()
    };
    let capture_overshoot = cur.frame_metric(FrameMetric::CaptureOvershootFrames);
    let playback_lead = cur.frame_metric(FrameMetric::PlaybackLeadFrames);

    tracing::info!(
        seq = due_seq,
        hw_cycle = %hw_cycle,
        capture_wait = %capture_wait,
        render = %render,
        pre_go = %pre_go,
        go_to_hw_end = %go_to_hw_end,
        hwfin_handoff = %hwfin_handoff,
        dispatch = %dispatch,
        plugin_wait = %plugin_wait,
        capture_overshoot = capture_overshoot,
        playback_lead = playback_lead,
        "cycle_trace summary"
    );
}

fn dump_histogram_ring(metric: &str, ring: &Histogram) {
    let mut samples: Vec<u64> = ring
        .samples
        .iter()
        .map(|sample| sample.swap(0, Ordering::Relaxed))
        .filter(|&sample| sample != 0)
        .collect();
    if samples.is_empty() {
        return;
    }
    samples.sort_unstable();
    let n = samples.len();
    let ms = |value_ns: u64| format!("{:.2}ms", value_ns as f64 / 1_000_000.0);
    tracing::info!(
        metric,
        samples = n,
        min = %ms(samples[0]),
        p50 = %ms(samples[n / 2]),
        p95 = %ms(samples[(n * 95) / 100]),
        max = %ms(samples[n - 1]),
        "cycle_trace histogram"
    );
}

/// Log min/p50/p95/max for each histogram ring and reset the rings.
pub(crate) fn dump_histogram() {
    if !enabled() {
        return;
    }
    let Some(storage) = STORAGE.get() else {
        return;
    };
    dump_histogram_ring("hw_cycle", &storage.hist_hw_cycle);
    dump_histogram_ring("render", &storage.hist_render);
    dump_histogram_ring("plugin_wait", &storage.hist_plugin_wait);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycle_trace_smoke() {
        force(true);
        // Slot 1: tail of the previous phase — render for cycle 2, Go for
        // cycle 2 — followed by nothing else; the hw cycle itself stamps
        // slot 2 once begun.
        let seq1 = begin_cycle();
        assert!(seq1 > 0);
        mark(TracePoint::HwFinishedSent);
        mark(TracePoint::HwFinishedReceived);
        mark(TracePoint::RenderDispatched);
        mark(TracePoint::RenderEnd);
        mark(TracePoint::GoSent);
        mark(TracePoint::GoReceived);
        {
            let _guard = plugin_wait();
            std::thread::sleep(std::time::Duration::from_micros(200));
        }

        let seq2 = begin_cycle();
        assert_eq!(seq2, seq1 + 1);
        mark(TracePoint::HwCycleStart);
        mark(TracePoint::CaptureReadDone);
        mark(TracePoint::PlaybackWriteDone);
        mark(TracePoint::HwCycleEnd);
        mark(TracePoint::HwFinishedSent);
        mark(TracePoint::HwFinishedReceived);
        mark(TracePoint::TransportAdvanced);

        log_summary(seq2);
        dump_histogram();

        let storage = STORAGE.get().expect("storage allocated by force(true)");
        let slot2 = &storage.slots[slot_index(seq2)];
        let start = slot2.point(TracePoint::HwCycleStart);
        let end = slot2.point(TracePoint::HwCycleEnd);
        assert!(start != 0, "HwCycleStart stamped");
        assert!(end >= start, "HwCycleEnd monotonic after HwCycleStart");
        let slot1 = &storage.slots[slot_index(seq1)];
        assert!(
            slot1.plugin_wait_ns.load(Ordering::Relaxed) > 0,
            "plugin wait accumulated into the cycle slot"
        );

        force(false);
        assert!(!enabled());
        assert_eq!(begin_cycle(), 0);
        assert_eq!(take_summary_due(), None);
        drop(plugin_wait());
        mark(TracePoint::HwCycleEnd);
        dump_histogram();
    }
}
