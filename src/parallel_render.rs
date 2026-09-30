//! Deadline-bounded DAG rendezvous on the existing node-worker threads.
//!
//! Each worker has a separate SPSC mailbox for inline jobs. The dispatcher
//! continues to own the legacy mailboxes; inline completions can never be
//! mistaken for legacy executor completions. Only one job per worker is in
//! flight. A timeout cancels undispatched work, but does NOT force-complete
//! running writers or clear their buffers. The hardware drain writes silence
//! and `finish_pending` retires the writers before HWFinished / the next Go.

use crate::executor::NodeJob;
use crate::render_plan::{NodeId, Op, SharedPlan};
use crate::workers::worker::{NodeJobResult, Worker};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

struct InlineJob {
    job: NodeJob,
    cancelled: Arc<AtomicBool>,
    deadline: Instant,
    waiter: Thread,
}

pub(crate) struct WorkerMailbox {
    jobs: rtrb::Producer<InlineJob>,
    results: rtrb::Consumer<Option<NodeJobResult>>,
}

pub(crate) struct WorkerEndpoint {
    jobs: rtrb::Consumer<InlineJob>,
    results: rtrb::Producer<Option<NodeJobResult>>,
}

pub(crate) fn worker_mailbox() -> (WorkerMailbox, WorkerEndpoint) {
    let (jobs_tx, jobs_rx) = rtrb::RingBuffer::new(1);
    let (results_tx, results_rx) = rtrb::RingBuffer::new(1);
    (
        WorkerMailbox {
            jobs: jobs_tx,
            results: results_rx,
        },
        WorkerEndpoint {
            jobs: jobs_rx,
            results: results_tx,
        },
    )
}

impl WorkerEndpoint {
    /// Called by the existing node-worker loop before checking legacy work.
    pub(crate) fn process_one(&mut self, worker_id: usize) -> bool {
        self.process_with(|job| Worker::process_node_job_result(worker_id, job))
    }

    fn process_with(&mut self, process: impl FnOnce(NodeJob) -> NodeJobResult) -> bool {
        let Ok(job) = self.jobs.pop() else {
            return false;
        };
        let result = if job.cancelled.load(Ordering::Acquire) || Instant::now() >= job.deadline {
            None
        } else {
            // A panicking task must still acknowledge ownership release, or
            // teardown would wait forever. No dependent is run on failure.
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| process(job.job))).ok()
        };
        // One outstanding job per worker means the completion slot is empty.
        // If the receiver has gone away, dropping the result is also safe:
        // all node writes have finished before we publish/retire it.
        let _ = self.results.push(result);
        job.waiter.unpark();
        true
    }
}

struct WorkerSlot {
    mailbox: WorkerMailbox,
    thread: Thread,
    busy: Option<NodeId>,
}

pub(crate) struct ParallelRender {
    workers: Vec<WorkerSlot>,
    remaining: Vec<u32>,
    ready: VecDeque<NodeId>,
    cancelled: Arc<AtomicBool>,
    active_plan: Option<SharedPlan>,
    epoch: u64,
    started: Instant,
    period: Duration,
    missed: bool,
}

impl ParallelRender {
    pub(crate) fn new() -> Self {
        Self {
            workers: Vec::new(),
            remaining: Vec::new(),
            ready: VecDeque::new(),
            cancelled: Arc::new(AtomicBool::new(false)),
            active_plan: None,
            epoch: 0,
            started: Instant::now(),
            period: Duration::from_millis(1),
            missed: false,
        }
    }

    pub(crate) fn add_worker(&mut self, mailbox: WorkerMailbox, thread: Thread) {
        self.workers.push(WorkerSlot {
            mailbox,
            thread,
            busy: None,
        });
    }

    /// `None` requests silence. Running jobs still own their buffers until
    /// `finish_pending`, which must run after the playback drain and before
    /// publishing HWFinished, including driver-error and shutdown paths.
    pub(crate) fn render(
        &mut self,
        plan: SharedPlan,
        period: Duration,
    ) -> Option<Vec<NodeJobResult>> {
        // Never reuse an arena while a previous cycle still owns it.
        if self.active_plan.is_some() {
            return None;
        }
        self.started = Instant::now();
        self.period = period.max(Duration::from_nanos(1));
        let deadline = self.started + self.period;
        self.epoch = self.epoch.wrapping_add(1);
        self.cancelled.store(false, Ordering::Release);
        self.active_plan = Some(plan.clone());
        self.remaining.clone_from(&plan.indegree);
        self.ready.clear();
        // Feedback plans have backward edges. Preserve the serial inline
        // order for the entire plan on a worker, never concurrent feedback
        // reads/writes (the timeout executor's forced completions are unsafe
        // here because running nodes still own the arena).
        let serial = !plan.forced.is_empty();
        if serial {
            if !plan.nodes.is_empty() {
                self.ready.push_back(0);
            }
        } else {
            self.ready.extend(plan.sources.iter().copied());
        }
        let mut completed = 0;
        let mut results = Vec::with_capacity(plan.nodes.len());
        let waiter = thread::current();
        loop {
            if Instant::now() >= deadline {
                break;
            }
            for index in 0..self.workers.len() {
                if self.workers[index].busy.is_none() {
                    continue;
                }
                match self.workers[index].mailbox.results.pop() {
                    Ok(Some(result)) if result.epoch == self.epoch => {
                        let node = self.workers[index].busy.take().expect("in-flight node");
                        if result.node != node {
                            return self.fail();
                        }
                        completed += 1;
                        self.complete(&plan, node, serial);
                        results.push(result);
                    }
                    Ok(_) => {
                        self.workers[index].busy = None;
                        return self.fail();
                    }
                    Err(_) if self.workers[index].mailbox.results.is_abandoned() => {
                        self.workers[index].busy = None;
                        return self.fail();
                    }
                    Err(_) => {}
                }
            }
            if completed == plan.nodes.len() {
                if Instant::now() >= deadline {
                    break;
                }
                self.active_plan = None;
                return Some(results);
            }
            // Hardware input nodes are already complete at capture fill.
            // All actual processing, including Zero/Sum, stays on workers.
            while let Some(&node) = self.ready.front() {
                if Instant::now() >= deadline {
                    return self.fail();
                }
                if matches!(plan.nodes[node as usize], Op::HwInput { .. }) {
                    self.ready.pop_front();
                    if let Op::HwInput { output, .. } = plan.nodes[node as usize] {
                        plan.set_buffer_latency(output, 0);
                    }
                    completed += 1;
                    self.complete(&plan, node, serial);
                    continue;
                }
                let Some(worker) = self.workers.iter_mut().find(|worker| worker.busy.is_none())
                else {
                    break;
                };
                if worker.mailbox.jobs.is_abandoned() {
                    return self.fail();
                }
                self.ready.pop_front();
                let job = InlineJob {
                    job: NodeJob {
                        epoch: self.epoch,
                        plan: plan.clone(),
                        node,
                    },
                    cancelled: self.cancelled.clone(),
                    deadline,
                    waiter: waiter.clone(),
                };
                if worker.mailbox.jobs.push(job).is_err() {
                    return self.fail();
                }
                worker.busy = Some(node);
                worker.thread.unpark();
            }
            if completed == plan.nodes.len() {
                continue;
            }
            if self.workers.iter().all(|worker| worker.busy.is_none()) {
                // An unsatisfied/corrupt DAG or an unavailable pool must
                // fail closed instead of spinning or doing RT track work.
                break;
            }
            // Blocking lets lower-priority node workers run on a single CPU.
            // Unpark tokens cover completions between polling and parking.
            thread::park_timeout(deadline.saturating_duration_since(Instant::now()));
        }
        self.fail()
    }

    fn complete(&mut self, plan: &SharedPlan, node: NodeId, serial: bool) {
        if serial {
            if (node as usize + 1) < plan.nodes.len() {
                self.ready.push_back(node + 1);
            }
        } else {
            for &dependent in &plan.dependents[node as usize] {
                self.remaining[dependent as usize] -= 1;
                if self.remaining[dependent as usize] == 0 {
                    self.ready.push_back(dependent);
                }
            }
        }
    }

    fn fail(&mut self) -> Option<Vec<NodeJobResult>> {
        self.cancelled.store(true, Ordering::Release);
        self.ready.clear();
        self.missed = true;
        None
    }

    /// After silence has been submitted to the driver, retire late work
    /// before capture, recording taps, offline work, or plan reuse can run.
    /// Never silence an in-flight node's arena from another thread.
    pub(crate) fn finish_pending(&mut self) -> Option<u64> {
        if !self.missed {
            return None;
        }
        loop {
            let mut pending = false;
            for worker in &mut self.workers {
                if worker.busy.is_some() {
                    if worker.mailbox.results.pop().is_ok() || worker.mailbox.results.is_abandoned()
                    {
                        worker.busy = None;
                    } else {
                        pending = true;
                    }
                }
            }
            if !pending {
                break;
            }
            thread::park_timeout(Duration::from_millis(1));
        }
        let frames = self.active_plan.take().map_or(0, |plan| {
            for op in &plan.nodes {
                if let Op::IoDelayGenerator { node, .. } = op {
                    node.restart_after_cancelled_cycle();
                }
            }
            plan.buffer_size as u64
        });
        let periods = (self.started.elapsed().as_nanos() / self.period.as_nanos()).max(1);
        self.missed = false;
        Some(frames.saturating_mul(periods.min(u128::from(u64::MAX)) as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render_plan::{DelayLine, RenderPlan};
    use std::cell::UnsafeCell;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Condvar, Mutex};

    struct Pool {
        render: ParallelRender,
        quit: Arc<AtomicBool>,
        threads: Vec<std::thread::JoinHandle<()>>,
    }

    impl Pool {
        fn new(
            count: usize,
            process: impl Fn(NodeJob) -> NodeJobResult + Send + Sync + 'static,
        ) -> Self {
            let process = Arc::new(process);
            let quit = Arc::new(AtomicBool::new(false));
            let mut render = ParallelRender::new();
            let mut threads = Vec::new();
            for _ in 0..count {
                let (mailbox, mut endpoint) = worker_mailbox();
                let process = process.clone();
                let quit = quit.clone();
                let handle = thread::spawn(move || {
                    loop {
                        if endpoint.process_with(|job| process(job)) {
                            continue;
                        }
                        if quit.load(Ordering::Acquire) {
                            break;
                        }
                        thread::park();
                    }
                });
                render.add_worker(mailbox, handle.thread().clone());
                threads.push(handle);
            }
            Self {
                render,
                quit,
                threads,
            }
        }
    }

    impl Drop for Pool {
        fn drop(&mut self) {
            self.render.finish_pending();
            self.quit.store(true, Ordering::Release);
            for handle in &self.threads {
                handle.thread().unpark();
            }
            for handle in self.threads.drain(..) {
                handle.join().unwrap();
            }
        }
    }

    fn plan() -> RenderPlan {
        RenderPlan {
            buffer_size: 8,
            buffers: (0..5).map(|_| UnsafeCell::new(vec![0.25; 8])).collect(),
            buffer_latencies: (0..5).map(|_| AtomicUsize::new(0)).collect(),
            nodes: vec![
                Op::HwInput {
                    channel: 0,
                    output: 0,
                },
                Op::Zero { output: 1 },
                Op::Sum {
                    inputs: vec![0, 1],
                    delays: (0..2).map(|_| UnsafeCell::new(DelayLine::new())).collect(),
                    output: 2,
                },
                Op::Zero { output: 3 },
                Op::Sum {
                    inputs: vec![2, 3],
                    delays: (0..2).map(|_| UnsafeCell::new(DelayLine::new())).collect(),
                    output: 4,
                },
            ],
            indegree: vec![0, 0, 2, 0, 2],
            dependents: vec![vec![2], vec![2], vec![4], vec![4], vec![]],
            sources: vec![0, 1, 3],
            hw_in_map: vec![(0, 0)],
            hw_out_map: vec![(4, 0)],
            port_map: HashMap::new(),
            midi_edges: vec![],
            forced: vec![],
        }
    }

    fn shared(collector: &basedrop::Collector, plan: RenderPlan) -> SharedPlan {
        Arc::new(basedrop::Owned::new(&collector.handle(), plan))
    }

    #[test]
    fn compiled_monitored_tracks_and_folder_match_serial_output() {
        use crate::audio::io::AudioIO;
        use crate::state::State;
        use crate::track::Track;

        let collector = basedrop::Collector::new();
        let input = Arc::new(AudioIO::new(64));
        let output = Arc::new(AudioIO::new(64));
        let folder = Arc::new(Track::new("folder".into(), 0, 1, 0, 0, 64, 48_000.0));
        folder.lock().is_folder = true;
        AudioIO::connect(&folder.lock().audio.outs[0], &output);
        let mut state = State::default();
        state.tracks.insert("folder".into(), folder.clone());
        for name in ["left", "right"] {
            let child = Arc::new(Track::new(name.into(), 1, 1, 0, 0, 64, 48_000.0));
            child.lock().set_input_monitor(vec![true]);
            child.lock().parent_track = Some("folder".into());
            AudioIO::connect(&input, &child.lock().audio.ins[0]);
            AudioIO::connect(&child.lock().audio.outs[0], &folder.lock().audio.outs[0]);
            folder.lock().child_tracks.push(child.clone());
            state.tracks.insert(name.into(), child);
        }
        let compiled = RenderPlan::compile(&state.snapshot(), &[input], &[output], 64);
        compiled.verify().unwrap();
        let plan = shared(&collector, compiled);
        let input_buffer = plan.hw_in_map[0].1;
        let output_buffer = plan.hw_out_map[0].0;
        // Safety: capture fill before any job has been dispatched.
        unsafe {
            (&mut *plan.buffer_ptr(input_buffer)).fill(0.25);
        }
        for node in 0..plan.nodes.len() as NodeId {
            Worker::process_node_job_result(
                0,
                NodeJob {
                    epoch: 0,
                    plan: plan.clone(),
                    node,
                },
            );
        }
        // Safety: the serial cycle has finished all writers.
        let serial = unsafe { plan.buffer(output_buffer) }.to_vec();
        assert_eq!(serial, vec![0.5; 64]);
        let mut pool = Pool::new(2, |job| Worker::process_node_job_result(0, job));
        assert!(
            pool.render
                .render(plan.clone(), Duration::from_secs(5))
                .is_some()
        );
        // Safety: parallel cycle acknowledged every writer before returning.
        assert_eq!(unsafe { plan.buffer(output_buffer) }, &serial);
    }

    #[test]
    fn independent_nodes_overlap_and_fan_in_waits_for_both_producers() {
        let collector = basedrop::Collector::new();
        let plan = shared(&collector, plan());
        let entered = Arc::new(AtomicUsize::new(0));
        let overlapping = Arc::new(AtomicUsize::new(0));
        let mut pool = Pool::new(2, {
            let entered = entered.clone();
            let overlapping = overlapping.clone();
            move |job| {
                if matches!(job.node, 1 | 3) {
                    entered.fetch_add(1, Ordering::AcqRel);
                    let limit = Instant::now() + Duration::from_secs(1);
                    while entered.load(Ordering::Acquire) < 2 && Instant::now() < limit {
                        thread::sleep(Duration::from_micros(50));
                    }
                    if entered.load(Ordering::Acquire) == 2 {
                        overlapping.fetch_add(1, Ordering::AcqRel);
                    }
                }
                Worker::process_node_job_result(0, job)
            }
        });
        let results = pool
            .render
            .render(plan.clone(), Duration::from_secs(5))
            .unwrap();
        assert_eq!(overlapping.load(Ordering::Acquire), 2);
        let mut nodes: Vec<_> = results.iter().map(|result| result.node).collect();
        nodes.sort_unstable();
        assert_eq!(nodes, vec![1, 2, 3, 4]);
        // Safety: all workers acknowledged completion before render returned.
        assert_eq!(unsafe { plan.buffer(4) }, &[0.25; 8]);
        assert!(pool.render.finish_pending().is_none());
    }

    #[test]
    fn timeout_returns_before_writer_finishes_and_next_plan_waits_for_retirement() {
        let collector = basedrop::Collector::new();
        let plan = shared(&collector, plan());
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let started = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let mut pool = Pool::new(1, {
            let gate = gate.clone();
            let started = started.clone();
            let finished = finished.clone();
            move |job| {
                started.store(true, Ordering::Release);
                let (lock, condition) = &*gate;
                let guard = lock.lock().unwrap();
                drop(condition.wait_while(guard, |released| !*released).unwrap());
                let result = Worker::process_node_job_result(0, job);
                finished.store(true, Ordering::Release);
                result
            }
        });
        // A generous scheduling window ensures the job actually starts; the
        // gate makes it impossible to complete until render has returned.
        let result = pool.render.render(plan.clone(), Duration::from_millis(100));
        let saw_started = started.load(Ordering::Acquire);
        let saw_finished = finished.load(Ordering::Acquire);
        let refused_reuse = pool
            .render
            .render(plan.clone(), Duration::from_secs(1))
            .is_none();
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        let skipped = pool.render.finish_pending().unwrap();
        assert!(result.is_none());
        assert!(saw_started);
        assert!(!saw_finished, "deadline must not wait for a running writer");
        assert!(refused_reuse);
        assert!(skipped >= 8);
        assert!(finished.load(Ordering::Acquire));
        let replacement = shared(&collector, self::plan());
        let results = pool
            .render
            .render(replacement.clone(), Duration::from_secs(5))
            .unwrap();
        assert_eq!(
            results.len(),
            4,
            "late results must not leak into a new plan"
        );
        // Safety: replacement cycle completed and no old jobs remain.
        assert_eq!(unsafe { replacement.buffer(4) }, &[0.25; 8]);
    }

    #[test]
    fn panicking_worker_fails_closed_and_can_process_the_next_cycle() {
        let collector = basedrop::Collector::new();
        let plan = shared(&collector, plan());
        let first = Arc::new(AtomicBool::new(true));
        let mut pool = Pool::new(1, move |job| {
            assert!(
                !first.swap(false, Ordering::AcqRel),
                "simulated node failure"
            );
            Worker::process_node_job_result(0, job)
        });
        assert!(
            pool.render
                .render(plan.clone(), Duration::from_secs(5))
                .is_none()
        );
        assert!(pool.render.finish_pending().is_some());
        assert!(pool.render.render(plan, Duration::from_secs(5)).is_some());
    }

    #[test]
    fn feedback_plans_run_serially_in_plan_order_on_workers() {
        let collector = basedrop::Collector::new();
        let mut graph = plan();
        graph.forced = vec![1, 2, 3, 4];
        graph.dependents[4].push(1);
        graph.indegree[1] += 1;
        graph.sources = vec![0, 3];
        let plan = shared(&collector, graph);
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut pool = Pool::new(2, {
            let order = order.clone();
            move |job| {
                order.lock().unwrap().push(job.node);
                Worker::process_node_job_result(0, job)
            }
        });
        assert!(pool.render.render(plan, Duration::from_secs(5)).is_some());
        assert_eq!(*order.lock().unwrap(), vec![1, 2, 3, 4]);
    }

    #[test]
    fn missing_workers_fail_closed_without_executing_nodes() {
        let collector = basedrop::Collector::new();
        let plan = shared(&collector, plan());
        let mut render = ParallelRender::new();
        assert!(render.render(plan, Duration::from_secs(1)).is_none());
        assert!(render.finish_pending().is_some());
    }
}
