//! Cycle executor for [`RenderPlan`] — Phase 2 of `LOCKLESS.md`.
//!
//! The executor replaces the legacy dynamic scheduler (`cycle_tasks`,
//! `cycle_task_deps`, `finished`-flag sweeps in `engine/runtime.rs`) with
//! count-up dependency counters over an immutable, atomically published plan:
//!
//! - `deps_completed[i]` is a plain (non-atomic) `u64` — only the dispatcher
//!   thread mutates it. A node is ready when its counter reaches
//!   `cycle * indegree[i]`; exactly one completion ever crosses the threshold.
//! - Per cycle: `cycle += 1`, dispatch everything in `plan.sources`. No
//!   counter reset, no memcpy, no scan, no allocation.
//! - Per plan swap (rare, user-driven): the dispatcher pulls the published
//!   plan at cycle start (`ArcSwap::load_full`), bumps `epoch`, and
//!   re-baselines `deps_completed[i] = cycle * indegree[i]`. The old plan
//!   always executes to completion; the swap takes effect at the next cycle
//!   boundary. Stale completions from the previous plan are dropped via
//!   `epoch`.
//!
//! Nodes whose dependencies can never be satisfied (`plan.forced`, feedback
//! loops) are dispatched once the task timeout has elapsed since cycle start,
//! mirroring the legacy `!progressed` fallback: they run with stale input
//! data, which is the standard feedback-loop behaviour.

use crate::message::{PluginKind, ProcessTask};
use crate::render_plan::{NodeId, Op, PlanSlot, SharedPlan};
use crate::state::TrackHandle;
#[cfg(test)]
use crate::track::Track;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// A unit of work handed to a worker: execute `node` of `plan`.
///
/// The worker holds the plan `Arc` for the duration of the job, so the arena
/// stays alive even if the plan is swapped out mid-cycle.
#[derive(Clone)]
pub struct NodeJob {
    pub epoch: u64,
    pub plan: SharedPlan,
    pub node: NodeId,
}

impl std::fmt::Debug for NodeJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeJob")
            .field("epoch", &self.epoch)
            .field("node", &self.node)
            .finish()
    }
}

/// Outcome of [`CycleExecutor::force_timeouts`] and
/// [`CycleExecutor::abandon_node`].
#[derive(Debug, Default)]
pub struct ForceOutcome {
    /// Newly dispatchable jobs (forced feedback nodes and dependents of
    /// force-completed nodes).
    pub jobs: Vec<NodeJob>,
    /// Nodes that were force-completed with silence (timed out or abandoned).
    pub silenced: Vec<NodeId>,
    /// All nodes are done — the cycle is complete.
    pub cycle_complete: bool,
}

/// The single-dispatcher cycle executor. Not `Sync` by design: all methods
/// run on the engine dispatcher task.
pub struct CycleExecutor {
    slot: Arc<PlanSlot>,
    plan: SharedPlan,
    /// Monotonic cycle counter; a node of the current cycle is ready when
    /// `deps_completed[i] == cycle * indegree[i]`.
    cycle: u64,
    /// Bumped on every plan swap; stale `NodeDone`s are dropped on mismatch.
    epoch: u64,
    deps_completed: Vec<u64>,
    /// Cycle number at which each node was dispatched (0 = never).
    dispatched: Vec<u64>,
    /// Cycle number at which each node completed (0 = never).
    completed: Vec<u64>,
    /// Dispatch instant per node, for timeout detection.
    started_at: Vec<Option<Instant>>,
    /// Nodes not yet completed in the current cycle.
    pending: usize,
    cycle_started_at: Instant,
    /// Forced (feedback) nodes were dispatched this cycle.
    forced_dispatched: bool,
}

impl CycleExecutor {
    pub fn new(slot: Arc<PlanSlot>) -> Self {
        let plan = slot.load_full();
        let now = Instant::now();
        let n = plan.nodes.len();
        Self {
            slot,
            plan,
            cycle: 0,
            epoch: 0,
            deps_completed: vec![0; n],
            dispatched: vec![0; n],
            completed: vec![0; n],
            started_at: vec![None; n],
            pending: 0,
            cycle_started_at: now,
            forced_dispatched: false,
        }
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn plan(&self) -> &SharedPlan {
        &self.plan
    }

    pub fn cycle_complete(&self) -> bool {
        self.pending == 0
    }

    /// Pull a newly published plan, if any. Only called at a cycle boundary,
    /// so the old plan always executes to completion before the swap.
    fn pull_plan(&mut self) {
        let new = self.slot.load_full();
        if Arc::ptr_eq(&new, &self.plan) {
            return;
        }
        self.plan = new;
        self.epoch = self.epoch.wrapping_add(1);
        let n = self.plan.nodes.len();
        // Re-baseline: after a fully completed cycle C, every counter equals
        // C * indegree[i]; match that state for the new plan.
        self.deps_completed.clear();
        self.deps_completed
            .extend(self.plan.indegree.iter().map(|&d| self.cycle * d as u64));
        self.dispatched.clear();
        self.dispatched.resize(n, 0);
        self.completed.clear();
        self.completed.resize(n, 0);
        self.started_at.clear();
        self.started_at.resize(n, None);
    }

    /// Start the next cycle and return the seed jobs (all source nodes).
    pub fn start_cycle(&mut self, now: Instant) -> Vec<NodeJob> {
        self.pull_plan();
        self.cycle += 1;
        self.pending = self.plan.nodes.len();
        self.cycle_started_at = now;
        self.forced_dispatched = false;
        let cycle = self.cycle;
        let sources = self.plan.sources.clone();
        sources
            .iter()
            .map(|&node| self.dispatch(node, cycle, now))
            .collect()
    }

    /// Record a worker completion. Stale-epoch completions are dropped.
    /// Returns newly dispatchable jobs and whether the cycle is complete.
    pub fn on_node_done(&mut self, epoch: u64, node: NodeId, now: Instant) -> (Vec<NodeJob>, bool) {
        if epoch != self.epoch {
            return (Vec::new(), self.cycle_complete());
        }
        if self.completed[node as usize] == self.cycle {
            // Duplicate completion (e.g. a force-completed node's worker
            // finally answered). Ignore: exactly-once is already satisfied.
            return (Vec::new(), self.cycle_complete());
        }
        let jobs = self.complete_node(node, now);
        let complete = self.cycle_complete();
        (jobs, complete)
    }

    /// Force-complete timed-out nodes (silence their outputs) and, once the
    /// task timeout has elapsed since cycle start, dispatch the forced
    /// feedback nodes. Call this on every dispatcher tick and completion.
    pub fn force_timeouts(&mut self, now: Instant, timeout: Duration) -> ForceOutcome {
        let mut outcome = ForceOutcome::default();
        if self.cycle_complete() {
            return outcome;
        }
        let cycle = self.cycle;
        let mut timed_out = Vec::new();
        for node in 0..self.plan.nodes.len() as NodeId {
            let idx = node as usize;
            if self.dispatched[idx] != cycle || self.completed[idx] == cycle {
                continue;
            }
            let Some(started) = self.started_at[idx] else {
                continue;
            };
            if now.duration_since(started) >= timeout {
                timed_out.push(node);
            }
        }
        for node in timed_out {
            self.silence_node(node);
            outcome.silenced.push(node);
            outcome.jobs.extend(self.complete_node(node, now));
        }
        if !self.forced_dispatched && now.duration_since(self.cycle_started_at) >= timeout {
            self.forced_dispatched = true;
            for &node in &self.plan.forced.clone() {
                if self.dispatched[node as usize] != cycle {
                    outcome.jobs.push(self.dispatch(node, cycle, now));
                }
            }
        }
        outcome.cycle_complete = self.cycle_complete();
        outcome
    }

    /// A dispatch failed (worker channel closed): complete the node with
    /// silence immediately so the cycle cannot stall.
    pub fn abandon_node(&mut self, node: NodeId, now: Instant) -> ForceOutcome {
        let mut outcome = ForceOutcome::default();
        if self.completed[node as usize] == self.cycle {
            outcome.cycle_complete = self.cycle_complete();
            return outcome;
        }
        self.silence_node(node);
        outcome.silenced.push(node);
        outcome.jobs = self.complete_node(node, now);
        outcome.cycle_complete = self.cycle_complete();
        outcome
    }

    /// Mark a node dispatched and build its job.
    fn dispatch(&mut self, node: NodeId, cycle: u64, now: Instant) -> NodeJob {
        let idx = node as usize;
        self.dispatched[idx] = cycle;
        self.started_at[idx] = Some(now);
        NodeJob {
            epoch: self.epoch,
            plan: self.plan.clone(),
            node,
        }
    }

    /// Mark a node complete and cascade counters into its dependents,
    /// dispatching each dependent whose threshold was just crossed.
    fn complete_node(&mut self, node: NodeId, now: Instant) -> Vec<NodeJob> {
        let idx = node as usize;
        self.completed[idx] = self.cycle;
        self.started_at[idx] = None;
        self.pending = self.pending.saturating_sub(1);
        let cycle = self.cycle;
        let mut jobs = Vec::new();
        let dependents = self.plan.dependents[idx].clone();
        for dep in dependents {
            let dep_idx = dep as usize;
            self.deps_completed[dep_idx] += 1;
            let threshold = cycle * self.plan.indegree[dep_idx] as u64;
            if self.dispatched[dep_idx] != cycle && self.deps_completed[dep_idx] == threshold {
                jobs.push(self.dispatch(dep, cycle, now));
            }
        }
        jobs
    }

    /// Write silence into the node's arena output buffers and, for task
    /// nodes, set the port finished flags so any remaining legacy readiness
    /// checks see a completed producer.
    fn silence_node(&mut self, node: NodeId) {
        let op = &self.plan.nodes[node as usize];
        let (outs, task) = match op {
            Op::Zero { output } => (vec![*output], None),
            Op::Sum { output, .. } => (vec![*output], None),
            Op::HwInput { output, .. } => (vec![*output], None),
            Op::Task { task, outs, .. } => (outs.clone(), Some(task)),
        };
        for buf in outs {
            // Safety: the executor force-completes the node's writer chain;
            // dependents have not been dispatched yet, so no concurrent
            // access to these buffers exists.
            unsafe { &mut *self.plan.buffer_ptr(buf) }.fill(0.0);
        }
        let Some(task) = task else {
            return;
        };
        let track = match task {
            ProcessTask::Track(t) | ProcessTask::FolderInput(t) | ProcessTask::FolderOutput(t) => {
                t.clone()
            }
            ProcessTask::Plugin { track, .. } => track.clone(),
        };
        silence_task_ports(&track, task);
    }
}

/// Mirror of the legacy `force_stalled_task_completions` state handling:
/// mark the task's output ports finished and clear track processing state.
fn silence_task_ports(track: &TrackHandle, task: &ProcessTask) {
    let t = track.lock();
    match task {
        ProcessTask::Track(_) | ProcessTask::FolderOutput(_) => t.audio.outs.clone(),
        ProcessTask::FolderInput(_) => Vec::new(),
        ProcessTask::Plugin { kind, index, .. } => match kind {
            PluginKind::Clap => t
                .clap_plugins
                .get(*index)
                .map(|p| p.processor.audio_outputs().to_vec())
                .unwrap_or_default(),
            PluginKind::Vst3 => t
                .vst3_plugins
                .get(*index)
                .map(|p| p.processor.audio_outputs().to_vec())
                .unwrap_or_default(),
            #[cfg(all(unix, not(target_os = "macos")))]
            PluginKind::Lv2 => t
                .lv2_plugins
                .get(*index)
                .map(|p| p.processor.audio_outputs().to_vec())
                .unwrap_or_default(),
        },
    }
    .iter()
    .for_each(|out| {
        out.finished.store(true, Ordering::Release);
    });
    t.audio.set_processing(false);
    t.audio.set_finished(true);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render_plan::RenderPlan;
    use std::cell::UnsafeCell;
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct TestPlanSlot {
        collector: Option<basedrop::Collector>,
        slot: Option<Arc<PlanSlot>>,
    }

    impl TestPlanSlot {
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

        fn store(&self, plan: RenderPlan) {
            let owned = basedrop::Owned::new(
                &self.collector.as_ref().expect("test collector").handle(),
                plan,
            );
            self.slot
                .as_ref()
                .expect("test slot")
                .store(Arc::new(owned));
        }
    }

    impl Drop for TestPlanSlot {
        fn drop(&mut self) {
            self.slot.take();
            let Some(mut collector) = self.collector.take() else {
                return;
            };
            collector.collect();
            let _ = collector.try_cleanup();
        }
    }

    fn slot_with(plan: RenderPlan) -> TestPlanSlot {
        TestPlanSlot::new(plan)
    }

    /// Chain plan: two source tasks -> sum -> sink task.
    /// Buffers: 0 = task A out, 1 = task B out, 2 = sink in, 3 = sink out.
    fn chain_plan(track: &TrackHandle) -> RenderPlan {
        let nodes = vec![
            Op::Task {
                task: ProcessTask::Track(track.clone()),
                ins: vec![],
                outs: vec![0],
            },
            Op::Task {
                task: ProcessTask::Track(track.clone()),
                ins: vec![],
                outs: vec![1],
            },
            Op::Sum {
                inputs: vec![0, 1],
                delays: vec![
                    UnsafeCell::new(crate::render_plan::DelayLine::new()),
                    UnsafeCell::new(crate::render_plan::DelayLine::new()),
                ],
                output: 2,
            },
            Op::Task {
                task: ProcessTask::Track(track.clone()),
                ins: vec![2],
                outs: vec![3],
            },
        ];
        RenderPlan {
            buffer_size: 8,
            buffers: (0..4).map(|_| UnsafeCell::new(vec![0.0; 8])).collect(),
            buffer_latencies: (0..4)
                .map(|_| std::sync::atomic::AtomicUsize::new(0))
                .collect(),
            nodes,
            indegree: vec![0, 0, 2, 1],
            dependents: vec![vec![2], vec![2], vec![3], vec![]],
            sources: vec![0, 1],
            hw_in_map: vec![],
            hw_out_map: vec![],
            port_map: HashMap::new(),
            midi_edges: vec![],
            forced: vec![],
        }
    }

    fn make_track(name: &str) -> TrackHandle {
        Arc::new(Track::new(name.to_string(), 1, 1, 0, 0, 8, 48_000.0))
    }

    #[test]
    fn counters_dispatch_in_dependency_order_exactly_once() {
        let track = make_track("t");
        let slot_guard = slot_with(chain_plan(&track));
        let slot = slot_guard.slot();
        let mut exec = CycleExecutor::new(slot);
        let now = Instant::now();

        let jobs = exec.start_cycle(now);
        assert_eq!(jobs.len(), 2, "two source tasks");
        let mut seen: Vec<NodeId> = jobs.iter().map(|j| j.node).collect();
        seen.sort_unstable();
        assert_eq!(seen, vec![0, 1]);

        // Complete source 0: the sum needs both, so nothing new.
        let (jobs, complete) = exec.on_node_done(exec.epoch(), 0, now);
        assert!(jobs.is_empty() && !complete);

        // Complete source 1: threshold for the sum is crossed exactly once.
        let (jobs, complete) = exec.on_node_done(exec.epoch(), 1, now);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].node, 2);
        assert!(!complete);
        // A duplicate completion of source 1 must not re-dispatch.
        let (jobs, _) = exec.on_node_done(exec.epoch(), 1, now);
        assert!(jobs.is_empty());

        let (jobs, complete) = exec.on_node_done(exec.epoch(), 2, now);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].node, 3);
        assert!(!complete);

        let (jobs, complete) = exec.on_node_done(exec.epoch(), 3, now);
        assert!(jobs.is_empty());
        assert!(complete, "cycle complete after the sink");

        // Next cycle re-dispatches sources without any counter reset.
        let jobs = exec.start_cycle(now);
        assert_eq!(jobs.len(), 2);
        let (jobs, _) = exec.on_node_done(exec.epoch(), 0, now);
        assert!(jobs.is_empty());
        let (jobs, _) = exec.on_node_done(exec.epoch(), 1, now);
        assert_eq!(jobs.len(), 1, "cycle 2 counters re-baselined correctly");
    }

    #[test]
    fn swap_during_cycle_keeps_old_plan_until_boundary() {
        let track = make_track("t");
        let slot_guard = slot_with(chain_plan(&track));
        let slot = slot_guard.slot();
        let mut exec = CycleExecutor::new(slot.clone());
        let now = Instant::now();

        let jobs = exec.start_cycle(now);
        assert_eq!(jobs.len(), 2);
        let epoch0 = exec.epoch();

        // Swap in a fresh plan mid-cycle.
        slot_guard.store(chain_plan(&track));

        // Old-epoch completions still count; the swap is invisible mid-cycle.
        let (jobs, _) = exec.on_node_done(epoch0, 0, now);
        assert!(jobs.is_empty());
        let (jobs, _) = exec.on_node_done(epoch0, 1, now);
        assert_eq!(jobs.len(), 1, "old plan still executes to completion");
        assert_eq!(exec.epoch(), epoch0, "no swap before the boundary");

        // A completion stamped with a *future* epoch is dropped.
        let (jobs, _) = exec.on_node_done(epoch0 + 1, 2, now);
        assert!(jobs.is_empty());

        let (jobs, _) = exec.on_node_done(epoch0, 2, now);
        assert_eq!(jobs.len(), 1);
        let (_, complete) = exec.on_node_done(epoch0, 3, now);
        assert!(complete);

        // The boundary pulls the new plan and re-baselines.
        let jobs = exec.start_cycle(now);
        assert_eq!(exec.epoch(), epoch0 + 1);
        assert_eq!(jobs.len(), 2);
        let (jobs, _) = exec.on_node_done(exec.epoch(), 0, now);
        assert!(jobs.is_empty());
        let (jobs, _) = exec.on_node_done(exec.epoch(), 1, now);
        assert_eq!(jobs.len(), 1, "new plan runs after the boundary");
    }

    /// The spec's swap test: a simulated parallel cycle (two worker threads)
    /// runs while the plan is swapped; every node must execute exactly once
    /// and the cycle must complete without tearing.
    #[test]
    fn swap_during_simulated_parallel_cycle_exactly_once() {
        let track = make_track("t");
        let slot_guard = slot_with(chain_plan(&track));
        let slot = slot_guard.slot();
        let exec = Mutex::new(CycleExecutor::new(slot.clone()));
        let now = Instant::now();

        let jobs = exec.lock().expect("lock").start_cycle(now);
        let epoch0 = exec.lock().expect("lock").epoch();
        // Swap immediately: the parallel cycle runs entirely on the old plan.
        slot_guard.store(chain_plan(&track));

        // A single shared queue behind a Mutex; two threads pull jobs and
        // report done through the executor, exactly as the tokio workers do.
        let queue: Arc<Mutex<std::collections::VecDeque<NodeJob>>> =
            Arc::new(Mutex::new(jobs.into_iter().collect()));
        let executed: Arc<Mutex<Vec<NodeId>>> = Arc::new(Mutex::new(Vec::new()));
        std::thread::scope(|s| {
            for _ in 0..2 {
                let queue = queue.clone();
                let executed = executed.clone();
                let exec = &exec;
                s.spawn(move || {
                    loop {
                        let job = queue.lock().expect("lock").pop_front();
                        let Some(job) = job else {
                            break;
                        };
                        // "Execute": write the node index into its first output
                        // buffer, like a real task writing audio.
                        if let Op::Task { outs, .. } = &job.plan.nodes[job.node as usize]
                            && let Some(&buf) = outs.first()
                        {
                            // Safety: this worker executes this node; the plan
                            // invariant guarantees no concurrent access.
                            let out = unsafe { &mut *job.plan.buffer_ptr(buf) };
                            out[0] = job.node as f32;
                        }
                        executed.lock().expect("lock").push(job.node);
                        std::thread::yield_now();
                        let (new_jobs, _) = exec
                            .lock()
                            .expect("lock")
                            .on_node_done(epoch0, job.node, now);
                        queue.lock().expect("lock").extend(new_jobs);
                    }
                });
            }
        });

        let mut counts = [0usize; 4];
        for node in executed.lock().expect("lock").iter() {
            counts[*node as usize] += 1;
        }
        assert_eq!(
            counts,
            [1, 1, 1, 1],
            "every node executed exactly once across two racing workers"
        );
        assert!(exec.lock().expect("lock").cycle_complete());
    }

    #[test]
    fn timeout_silences_node_outputs_and_completes_by_index() {
        let track = make_track("t");
        let slot_guard = slot_with(chain_plan(&track));
        let slot = slot_guard.slot();
        let mut exec = CycleExecutor::new(slot);
        let now = Instant::now();
        let timeout = Duration::from_millis(250);

        let jobs = exec.start_cycle(now);
        assert_eq!(jobs.len(), 2);
        // Dirty the output buffers of both source tasks via the plan.
        // Safety: test thread, nothing else is running.
        unsafe {
            (&mut *exec.plan().buffer_ptr(0)).fill(1.0);
            (&mut *exec.plan().buffer_ptr(1)).fill(2.0);
        }

        // Complete nothing; advance past the timeout.
        let later = now + timeout + Duration::from_millis(1);
        let outcome = exec.force_timeouts(later, timeout);
        assert_eq!(outcome.silenced, vec![0, 1], "both sources timed out");
        // Their outputs were silenced by index.
        unsafe {
            assert!(exec.plan().buffer(0).iter().all(|&s| s == 0.0));
            assert!(exec.plan().buffer(1).iter().all(|&s| s == 0.0));
        }
        // The sum became dispatchable (both dependencies force-completed).
        assert_eq!(outcome.jobs.len(), 1);
        assert_eq!(outcome.jobs[0].node, 2);
        assert!(!outcome.cycle_complete);

        // The track's legacy port flags were set so dependent bodies proceed.
        let t = track.lock();
        assert!(t.audio.finished());
        assert!(!t.audio.processing());
        for out in &t.audio.outs {
            assert!(out.finished.load(Ordering::Acquire));
        }
        // Finish the cycle; a second timeout pass is a no-op.
        let (jobs, _) = exec.on_node_done(exec.epoch(), 2, later);
        assert_eq!(jobs.len(), 1);
        let outcome = exec.force_timeouts(later, timeout);
        assert!(outcome.silenced.is_empty());
        let (_, complete) = exec.on_node_done(exec.epoch(), 3, later);
        assert!(complete);
    }

    #[test]
    fn abandon_node_completes_with_silence() {
        let track = make_track("t");
        let slot_guard = slot_with(chain_plan(&track));
        let slot = slot_guard.slot();
        let mut exec = CycleExecutor::new(slot);
        let now = Instant::now();

        let jobs = exec.start_cycle(now);
        assert_eq!(jobs.len(), 2);
        unsafe { (&mut *exec.plan().buffer_ptr(0)).fill(3.0) };

        let outcome = exec.abandon_node(0, now);
        assert_eq!(outcome.silenced, vec![0]);
        unsafe {
            assert!(exec.plan().buffer(0).iter().all(|&s| s == 0.0));
        }
        // Abandoning twice is a no-op.
        let outcome = exec.abandon_node(0, now);
        assert!(outcome.silenced.is_empty());
        assert!(outcome.jobs.is_empty());

        let (jobs, _) = exec.on_node_done(exec.epoch(), 1, now);
        assert_eq!(jobs.len(), 1, "abandon + done crosses the sum threshold");
    }

    #[test]
    fn forced_feedback_nodes_dispatch_after_timeout() {
        let track = make_track("t");
        let mut plan = chain_plan(&track);
        // Turn the sink into an undispatchable feedback node.
        plan.indegree[3] = 2;
        plan.forced = vec![3];
        let slot_guard = slot_with(plan);
        let slot = slot_guard.slot();
        let mut exec = CycleExecutor::new(slot);
        let now = Instant::now();
        let timeout = Duration::from_millis(250);

        let jobs = exec.start_cycle(now);
        assert_eq!(jobs.len(), 2);
        // Run the acyclic part; the sink can never reach its threshold.
        let (jobs, _) = exec.on_node_done(exec.epoch(), 0, now);
        assert!(jobs.is_empty());
        let (jobs, _) = exec.on_node_done(exec.epoch(), 1, now);
        assert_eq!(jobs.len(), 1);
        let (jobs, complete) = exec.on_node_done(exec.epoch(), 2, now);
        assert!(jobs.is_empty() && !complete);

        // Before the timeout: still stuck.
        let outcome = exec.force_timeouts(now + Duration::from_millis(10), timeout);
        assert!(outcome.jobs.is_empty());
        // After the timeout: the forced node is dispatched.
        let outcome = exec.force_timeouts(now + timeout + Duration::from_millis(1), timeout);
        assert_eq!(outcome.jobs.len(), 1);
        assert_eq!(outcome.jobs[0].node, 3);

        let (_, complete) = exec.on_node_done(exec.epoch(), 3, now);
        assert!(complete);
    }

    #[test]
    fn stale_epoch_completions_are_dropped() {
        let track = make_track("t");
        let slot_guard = slot_with(chain_plan(&track));
        let slot = slot_guard.slot();
        let mut exec = CycleExecutor::new(slot);
        let now = Instant::now();

        let jobs = exec.start_cycle(now);
        assert_eq!(jobs.len(), 2);
        let (jobs, _) = exec.on_node_done(exec.epoch() + 7, 0, now);
        assert!(jobs.is_empty());
        let (jobs, _) = exec.on_node_done(exec.epoch().wrapping_sub(1), 1, now);
        assert!(jobs.is_empty());
        // Real completions still work afterwards.
        let (jobs, _) = exec.on_node_done(exec.epoch(), 0, now);
        assert!(jobs.is_empty());
        let (jobs, _) = exec.on_node_done(exec.epoch(), 1, now);
        assert_eq!(jobs.len(), 1);
    }
}
