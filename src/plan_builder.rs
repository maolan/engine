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
use crate::render_plan::{PlanSlot, RenderPlan};
use crate::state::StateSlot;
use arc_swap::ArcSwap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

/// Hardware ports and block size the builder compiles into each plan. The
/// engine replaces this snapshot whenever a driver opens/closes or the
/// buffer size changes; the builder only reads it. Published RCU-style
/// (`ArcSwap`) — no blocking primitive on either side.
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
    rebuild_requests: std::sync::mpsc::Sender<tokio::sync::oneshot::Sender<()>>,
}

impl PlanBuilder {
    /// Spawn the builder thread and mark it dirty so an initial plan is
    /// published right away. `collector` moves to the builder thread, which
    /// becomes the only place plan memory is actually freed.
    pub fn spawn(
        state: Arc<StateSlot>,
        hw: Arc<ArcSwap<HwPorts>>,
        slot: Arc<PlanSlot>,
        collector: basedrop::Collector,
    ) -> Self {
        let dirty = Arc::new(AtomicBool::new(true));
        let quit = Arc::new(AtomicBool::new(false));
        let (rebuild_requests, rebuild_rx) = std::sync::mpsc::channel();
        let join = std::thread::spawn({
            let dirty = dirty.clone();
            let quit = quit.clone();
            move || builder_loop(state, hw, slot, collector, dirty, quit, rebuild_rx)
        });
        Self {
            dirty,
            quit,
            thread: join.thread().clone(),
            join: Some(join),
            rebuild_requests,
        }
    }

    /// Wait for a plan compiled after this request to be published. Session
    /// restore uses this barrier before announcing that playback is ready.
    pub async fn rebuild(&self) -> Result<(), String> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.rebuild_requests
            .send(tx)
            .map_err(|_| "Render plan builder stopped".to_string())?;
        self.thread.unpark();
        rx.await
            .map_err(|_| "Render plan builder stopped".to_string())
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
        if let Some(join) = self.join.take()
            && let Err(error) = join.join()
        {
            tracing::error!("Plan builder thread panicked during shutdown: {error:?}");
        }
    }
}

fn builder_loop(
    state: Arc<StateSlot>,
    hw: Arc<ArcSwap<HwPorts>>,
    slot: Arc<PlanSlot>,
    mut collector: basedrop::Collector,
    dirty: Arc<AtomicBool>,
    quit: Arc<AtomicBool>,
    rebuild_requests: std::sync::mpsc::Receiver<tokio::sync::oneshot::Sender<()>>,
) {
    loop {
        // The timeout bounds shutdown latency and guards against a missed
        // unpark; builds themselves are triggered by the dirty flag.
        std::thread::park_timeout(Duration::from_millis(10));
        if quit.load(Ordering::Acquire) {
            drop(slot);
            collector.collect();
            if collector.try_cleanup().is_err() {
                tracing::warn!("Render plan collector still has live handles during shutdown");
            }
            break;
        }
        let waiters: Vec<_> = rebuild_requests.try_iter().collect();
        if !dirty.swap(false, Ordering::AcqRel) && waiters.is_empty() {
            continue;
        }
        let HwPorts {
            ins,
            outs,
            buffer_size,
        } = hw.load_full().as_ref().clone();
        let state = state.load_full();
        let plan = RenderPlan::compile(&state, &ins, &outs, buffer_size.max(1));
        slot.store(Arc::new(basedrop::Owned::new(&collector.handle(), plan)));
        for waiter in waiters {
            let _ = waiter.send(());
        }
        // Free whatever retired plans have been fully released since the
        // last build (the swapped-out one included, once workers let go).
        collector.collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rebuild_acknowledges_a_plan_with_the_latest_ports() {
        let snapshot = crate::state::State::default().lock().snapshot();
        let state = Arc::new(StateSlot::from_pointee(snapshot.clone()));
        let hw = Arc::new(ArcSwap::from_pointee(HwPorts {
            buffer_size: 32,
            ..Default::default()
        }));
        let collector = basedrop::Collector::new();
        let slot = Arc::new(PlanSlot::from_pointee(basedrop::Owned::new(
            &collector.handle(),
            RenderPlan::compile(&snapshot, &[], &[], 32),
        )));
        let builder = PlanBuilder::spawn(state, hw.clone(), slot.clone(), collector);
        for buffer_size in [64, 128, 256] {
            hw.store(Arc::new(HwPorts {
                buffer_size,
                ..Default::default()
            }));
            tokio::time::timeout(Duration::from_secs(5), builder.rebuild())
                .await
                .expect("builder timeout")
                .expect("rebuild");
            assert_eq!(slot.load().buffer_size, buffer_size);
        }
        drop(slot);
        drop(builder);
    }
}
