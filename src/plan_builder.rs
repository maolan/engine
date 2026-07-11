//! Background render-plan builder — the RCU publish side of Phase 2
//! (see `LOCKLESS.md`, "Builder contract").
//!
//! Graph edits set `plan_dirty` via [`PlanBuilder::mark_dirty`]; ONE builder
//! thread wakes, takes a brief consistent read of the topology (tracks, ports,
//! connections, hardware bridges — never audio data), compiles a
//! [`RenderPlan`], and `ArcSwap::store`s it. Bursts coalesce to at most one
//! in-flight build; last-state-wins. The store IS the notification: the
//! dispatcher pulls the plan at cycle start, so the old plan always executes
//! to completion and swaps only at a cycle boundary.
//!
//! Retired plans are allocated through this thread's `basedrop::Collector`:
//! the final `Arc` drop may land on a worker thread (in-flight jobs hold
//! clones), but it only queues the memory — the actual free of the whole
//! buffer arena happens here, off every real-time path.

use crate::audio::io::AudioIO;
use crate::mutex::UnsafeMutex;
use crate::render_plan::{PlanSlot, RenderPlan};
use crate::state::State;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// Hardware ports and block size the builder compiles into each plan. The
/// engine replaces this snapshot whenever a driver opens/closes or the
/// buffer size changes; the builder only reads it. A plain `std` mutex on
/// the control side — never touched on the real-time path.
#[derive(Clone, Default)]
pub struct HwPorts {
    pub ins: Vec<Arc<AudioIO>>,
    pub outs: Vec<Arc<AudioIO>>,
    pub buffer_size: usize,
}

/// Handle to the builder thread. Dropping it stops the thread.
pub struct PlanBuilder {
    dirty: Arc<AtomicBool>,
    quit: Arc<AtomicBool>,
    thread: std::thread::Thread,
    join: Option<JoinHandle<()>>,
}

impl PlanBuilder {
    /// Spawn the builder thread and mark it dirty so an initial plan is
    /// published right away. `collector` moves to the builder thread, which
    /// becomes the only place plan memory is actually freed.
    pub fn spawn(
        state: Arc<UnsafeMutex<State>>,
        hw: Arc<Mutex<HwPorts>>,
        slot: Arc<PlanSlot>,
        collector: basedrop::Collector,
    ) -> Self {
        let dirty = Arc::new(AtomicBool::new(true));
        let quit = Arc::new(AtomicBool::new(false));
        let join = std::thread::spawn({
            let dirty = dirty.clone();
            let quit = quit.clone();
            move || builder_loop(state, hw, slot, collector, dirty, quit)
        });
        Self {
            dirty,
            quit,
            thread: join.thread().clone(),
            join: Some(join),
        }
    }

    /// Mark the topology dirty. Cheap and idempotent: repeated calls before
    /// the builder wakes coalesce into a single build.
    pub fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Release);
        self.thread.unpark();
    }
}

impl Drop for PlanBuilder {
    fn drop(&mut self) {
        self.quit.store(true, Ordering::Release);
        self.thread.unpark();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn builder_loop(
    state: Arc<UnsafeMutex<State>>,
    hw: Arc<Mutex<HwPorts>>,
    slot: Arc<PlanSlot>,
    mut collector: basedrop::Collector,
    dirty: Arc<AtomicBool>,
    quit: Arc<AtomicBool>,
) {
    loop {
        // The timeout bounds shutdown latency and guards against a missed
        // unpark; builds themselves are triggered by the dirty flag.
        std::thread::park_timeout(Duration::from_millis(10));
        if quit.load(Ordering::Acquire) {
            break;
        }
        if !dirty.swap(false, Ordering::AcqRel) {
            continue;
        }
        let HwPorts {
            ins,
            outs,
            buffer_size,
        } = hw.lock().expect("hw ports poisoned").clone();
        let plan = {
            let state = state.lock();
            RenderPlan::compile(state, &ins, &outs, buffer_size.max(1))
        };
        slot.store(Arc::new(basedrop::Owned::new(&collector.handle(), plan)));
        // Free whatever retired plans have been fully released since the
        // last build (the swapped-out one included, once workers let go).
        collector.collect();
    }
}
