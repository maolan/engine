//! Compiled, immutable audio render plan — Phase 2 of `LOCKLESS.md`.
//!
//! A `RenderPlan` is the flattened, topologically ordered form of the engine's
//! per-cycle task graph plus the `AudioIO` port network. It is built on the
//! control thread (see the builder contract in `LOCKLESS.md`), published with
//! `arc_swap`, and executed by the cycle executor with count-up dependency
//! counters. Nothing in it is behind a mutex: while a plan is alive it never
//! mutates.
//!
//! Key invariants (checked by [`RenderPlan::verify`] after every compile):
//!
//! - **Single-producer chains.** Every arena buffer has at least one writer,
//!   and multiple writers of the same buffer always form a dependency chain
//!   (e.g. an input `Sum` node writes a track input, then the track task adds
//!   clip audio in place). Two nodes that could run concurrently never write
//!   the same buffer.
//! - **Topological order.** Except for `forced` nodes (feedback loops, broken
//!   deliberately — the same situation today's `finished`-flag scheduler
//!   resolves with its force-progress fallback), every edge points from a
//!   lower node index to a higher one, so sequential execution is a plain
//!   iteration and the arena can be split safely at each output index.

use crate::audio::io::AudioIO;
use crate::connectable::{ConnectableConnection, ConnectableRef};
use crate::message::{PluginKind, ProcessTask};
use crate::mutex::UnsafeMutex;
use crate::state::State;
use crate::track::Track;
use std::cell::UnsafeCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

/// A plan shared across the dispatcher, workers and hardware drivers.
///
/// The `Owned` wrapper defers the actual free of the plan (the whole buffer
/// arena) to the builder thread's `basedrop::Collector`: the final `Arc` drop
/// may land on a worker thread (in-flight jobs hold clones), but it only
/// queues the memory for reclamation instead of freeing it inline.
pub type SharedPlan = Arc<basedrop::Owned<RenderPlan>>;
/// The atomically published plan slot. The dispatcher pulls with
/// `load_full()` at cycle start; the builder stores newly compiled plans.
pub type PlanSlot = arc_swap::ArcSwap<basedrop::Owned<RenderPlan>>;

/// Index into [`RenderPlan::buffers`].
pub type BufferId = u32;
/// Index into [`RenderPlan::nodes`].
pub type NodeId = u32;

/// What a plan node does when executed.
#[derive(Debug)]
pub enum Op {
    /// Fill `output` with silence — an unconnected consumer port
    /// (`AudioIO::process()` with zero connections fills its buffer today).
    Zero { output: BufferId },
    /// Sum all `inputs` into `output` — the compiled form of
    /// `AudioIO::process()` for a connected consumer port.
    Sum {
        inputs: Vec<BufferId>,
        output: BufferId,
    },
    /// Engine task: track / folder section / plugin processing. Keeps the
    /// `Arc<UnsafeMutex<Box<Track>>>` handle until Phase 5; `ins`/`outs` are
    /// the arena buffers this task reads (post-`Sum`) and produces.
    Task {
        task: ProcessTask,
        ins: Vec<BufferId>,
        outs: Vec<BufferId>,
    },
    /// Hardware input bridge: the driver thread writes `output` directly each
    /// block (JACK `copy_audio_inputs`, `fill_ports_from_interleaved_buffer`).
    /// A pure source — no plan node produces it.
    HwInput { channel: usize, output: BufferId },
}

/// A compiled, immutable render plan. Owns the whole buffer arena.
#[derive(Debug)]
pub struct RenderPlan {
    /// Samples per port buffer (the driver block size at compile time).
    pub buffer_size: usize,
    /// The arena: one buffer per `AudioIO` port, owned by the plan. Replaces
    /// `Arc<UnsafeMutex<Vec<f32>>>` on the real-time path.
    ///
    /// Interior mutability is required because workers execute disjoint nodes
    /// of the same plan concurrently. Soundness rests on the
    /// single-producer-chain invariant enforced by [`RenderPlan::verify`]:
    /// two nodes that may run concurrently never touch the same buffer, and
    /// every buffer access goes through the node currently executing on this
    /// thread. This is the one audited `unsafe` cell of the design.
    pub buffers: Vec<UnsafeCell<Vec<f32>>>,
    /// Nodes in topological order (producers before consumers).
    pub nodes: Vec<Op>,
    /// `indegree[i]` = number of nodes that must finish before `nodes[i]`.
    pub indegree: Vec<u32>,
    /// `dependents[i]` = nodes that may run once `nodes[i]` has finished.
    pub dependents: Vec<Vec<NodeId>>,
    /// Nodes with `indegree == 0` — the cycle seeds.
    pub sources: Vec<NodeId>,
    /// `(hw channel, buffer)` — the driver fills these before dispatch.
    pub hw_in_map: Vec<(usize, BufferId)>,
    /// The bridge port for each `hw_in_map` entry, same order. The driver
    /// writes the arena buffer (no `UnsafeMutex` on the RT path); the
    /// dispatcher copies arena → port buffer at cycle start so task bodies,
    /// which still read `port.buffer`, see this cycle's hardware input.
    pub hw_in_ports: Vec<Arc<AudioIO>>,
    /// `(buffer, hw channel)` — the driver drains these after the cycle.
    pub hw_out_map: Vec<(BufferId, usize)>,
    /// `Arc` pointer identity of each `AudioIO` port → its arena buffer.
    /// Transition aid so control code can resolve ports without re-walking.
    pub port_map: HashMap<usize, BufferId>,
    /// Nodes whose dependencies may never be satisfied (feedback loops).
    /// The executor force-completes them after the task timeout, mirroring
    /// today's `!progressed` fallback in the dynamic scheduler.
    pub forced: Vec<NodeId>,
}

// Safety: the only interior mutability is the buffer arena. Access to it goes
// through `buffer`/`buffer_ptr`, whose safety contract is the
// single-producer-chain invariant checked by `verify()`: concurrently running
// nodes never touch the same buffer, and each buffer access is performed by
// the thread executing the node that owns it this cycle. Everything else in
// the plan is plain immutable data (plus `Arc` handles that are already
// `Send + Sync`).
unsafe impl Sync for RenderPlan {}

impl RenderPlan {
    /// Mutable access to an arena buffer, as a raw pointer.
    ///
    /// Returns a pointer rather than a `&mut` because the aliasing discipline
    /// is dynamic (enforced by the plan's dependency graph, not the borrow
    /// checker) — this is the same shape as `UnsafeCell::get`.
    ///
    /// # Safety
    /// The caller must be executing (or have already completed) the unique
    /// node chain that writes buffer `id` in this cycle, per the plan's
    /// single-producer-chain invariant: no other concurrently running node
    /// may read or write the same buffer. The returned pointer is valid for
    /// the lifetime of the plan.
    pub unsafe fn buffer_ptr(&self, id: BufferId) -> *mut Vec<f32> {
        self.buffers[id as usize].get()
    }

    /// Read access to an arena buffer.
    ///
    /// # Safety
    /// Same discipline as [`RenderPlan::buffer_ptr`]: the buffer's producer
    /// chain must have completed, and no concurrent writer may exist.
    pub unsafe fn buffer(&self, id: BufferId) -> &[f32] {
        unsafe { &*self.buffers[id as usize].get() }
    }

    /// Number of arena buffers.
    pub fn buffer_count(&self) -> usize {
        self.buffers.len()
    }
    /// Compile the current topology (tracks, folders, plugins, port wiring,
    /// HW bridges) into an immutable plan. Runs on the control thread; may
    /// allocate freely.
    ///
    /// Track visit order is sorted by name so plans are deterministic — the
    /// legacy scheduler iterated `State.tracks` in HashMap order.
    pub fn compile(
        state: &State,
        hw_inputs: &[Arc<AudioIO>],
        hw_outputs: &[Arc<AudioIO>],
        buffer_size: usize,
    ) -> Self {
        let mut b = Builder::new(buffer_size);
        b.add_hw(hw_inputs, hw_outputs);

        let mut ordered: Vec<(String, Arc<UnsafeMutex<Box<Track>>>)> = state
            .tracks
            .iter()
            .map(|(name, track)| (name.clone(), track.clone()))
            .collect();
        ordered.sort_by(|a, b| a.0.cmp(&b.0));

        for (_name, track) in &ordered {
            if track.lock().parent_track.is_some() {
                continue;
            }
            b.append_track(track.clone(), None);
        }

        b.finish()
    }

    /// Verify the plan invariants (see the module docs). Called by `compile`
    /// (violations are logged) and by tests. Returns the first violation.
    pub fn verify(&self) -> Result<(), String> {
        let forced: HashSet<NodeId> = self.forced.iter().copied().collect();

        // Non-forced edges must point forward (topological order).
        for (from, dependents) in self.dependents.iter().enumerate() {
            for &to in dependents {
                if from as NodeId >= to
                    && !(forced.contains(&(from as NodeId)) && forced.contains(&to))
                {
                    return Err(format!("edge {from} -> {to} violates topological order"));
                }
            }
        }

        // Collect writers per buffer. `Track` and `FolderInput` tasks also
        // write their input buffers in place (clip audio is mixed into the
        // summed input today), so they count as chained writers of `ins`.
        let mut writers: HashMap<BufferId, Vec<NodeId>> = HashMap::new();
        for (idx, op) in self.nodes.iter().enumerate() {
            let idx = idx as NodeId;
            match op {
                Op::Zero { output } | Op::Sum { output, .. } | Op::HwInput { output, .. } => {
                    writers.entry(*output).or_default().push(idx);
                }
                Op::Task { task, ins, outs } => {
                    let writes_ins =
                        matches!(task, ProcessTask::Track(_) | ProcessTask::FolderInput(_));
                    for b in outs {
                        writers.entry(*b).or_default().push(idx);
                    }
                    if writes_ins {
                        for b in ins {
                            writers.entry(*b).or_default().push(idx);
                        }
                    }
                }
            }
        }

        for buffer in 0..self.buffers.len() as BufferId {
            let ws = writers.get(&buffer).cloned().unwrap_or_default();
            if ws.is_empty() {
                return Err(format!("buffer {buffer} has no writer"));
            }
            // Multiple writers must be chain-ordered: each consecutive pair
            // (in node order) must have a dependency path between them.
            let mut sorted = ws;
            sorted.sort_unstable();
            for pair in sorted.windows(2) {
                if !self.reachable(pair[0], pair[1]) {
                    return Err(format!(
                        "buffer {buffer} written by unordered nodes {} and {}",
                        pair[0], pair[1]
                    ));
                }
            }
        }
        Ok(())
    }

    /// Is there a dependency path from `from` to `to`?
    fn reachable(&self, from: NodeId, to: NodeId) -> bool {
        if from == to {
            return true;
        }
        let mut seen = HashSet::new();
        let mut queue = VecDeque::from([from]);
        seen.insert(from);
        while let Some(n) = queue.pop_front() {
            for &d in &self.dependents[n as usize] {
                if d == to {
                    return true;
                }
                if seen.insert(d) {
                    queue.push_back(d);
                }
            }
        }
        false
    }
}

/// Mutable compile-time state. Not part of the plan.
struct Builder {
    buffer_size: usize,
    buffers: Vec<UnsafeCell<Vec<f32>>>,
    port_map: HashMap<usize, BufferId>,
    nodes: Vec<Op>,
    edges: HashSet<(NodeId, NodeId)>,
    /// Buffers that need a `Sum`/`Zero` node (consumer ports).
    consumer_ports: Vec<Arc<AudioIO>>,
    /// Task nodes that read each consumer buffer.
    port_readers: HashMap<BufferId, Vec<NodeId>>,
    /// Task nodes that also write each consumer buffer in place.
    port_inplace_writers: HashMap<BufferId, Vec<NodeId>>,
    /// Producer node per buffer, filled as producer nodes are created.
    producer: HashMap<BufferId, NodeId>,
    hw_in_map: Vec<(usize, BufferId)>,
    hw_in_ports: Vec<Arc<AudioIO>>,
    hw_out_map: Vec<(BufferId, usize)>,
}

impl Builder {
    fn new(buffer_size: usize) -> Self {
        Self {
            buffer_size,
            buffers: Vec::new(),
            port_map: HashMap::new(),
            nodes: Vec::new(),
            edges: HashSet::new(),
            consumer_ports: Vec::new(),
            port_readers: HashMap::new(),
            port_inplace_writers: HashMap::new(),
            producer: HashMap::new(),
            hw_in_map: Vec::new(),
            hw_in_ports: Vec::new(),
            hw_out_map: Vec::new(),
        }
    }

    /// Arena buffer for a port, registering it (at silence) on first sight.
    fn buffer_for(&mut self, port: &Arc<AudioIO>) -> BufferId {
        let key = Arc::as_ptr(port) as usize;
        if let Some(&id) = self.port_map.get(&key) {
            return id;
        }
        let id = self.buffers.len() as BufferId;
        self.buffers
            .push(UnsafeCell::new(vec![0.0; self.buffer_size]));
        self.port_map.insert(key, id);
        id
    }

    fn push_node(&mut self, op: Op) -> NodeId {
        self.nodes.push(op);
        (self.nodes.len() - 1) as NodeId
    }

    fn add_hw(&mut self, hw_inputs: &[Arc<AudioIO>], hw_outputs: &[Arc<AudioIO>]) {
        for (channel, port) in hw_inputs.iter().enumerate() {
            let output = self.buffer_for(port);
            let node = self.push_node(Op::HwInput { channel, output });
            self.producer.insert(output, node);
            self.hw_in_map.push((channel, output));
            self.hw_in_ports.push(port.clone());
        }
        for (channel, port) in hw_outputs.iter().enumerate() {
            let buffer = self.buffer_for(port);
            self.consumer_ports.push(port.clone());
            self.hw_out_map.push((buffer, channel));
        }
    }

    /// Mirror of the legacy `append_track_tasks`: emits the task nodes for a
    /// track (folder sections, plugins, children) and returns the first and
    /// last node of the track's subgraph, for chaining by the caller.
    fn append_track(
        &mut self,
        track: Arc<UnsafeMutex<Box<Track>>>,
        predecessor: Option<NodeId>,
    ) -> (NodeId, NodeId) {
        let t = track.lock();
        let ins: Vec<BufferId> = t.audio.ins.iter().map(|p| self.buffer_for(p)).collect();
        let outs: Vec<BufferId> = t.audio.outs.iter().map(|p| self.buffer_for(p)).collect();
        for p in &t.audio.ins {
            self.consumer_ports.push(p.clone());
        }

        if t.is_folder {
            let folder_input = self.push_node(Op::Task {
                task: ProcessTask::FolderInput(track.clone()),
                ins: ins.clone(),
                outs: Vec::new(),
            });
            if let Some(pred) = predecessor {
                self.edges.insert((pred, folder_input));
            }
            self.register_task_ports(folder_input, &ins, true);

            let mut source_keys: HashMap<ConnectableRef, NodeId> = HashMap::new();
            let mut target_keys: HashMap<ConnectableRef, NodeId> = HashMap::new();
            source_keys.insert(ConnectableRef::TrackInput, folder_input);
            target_keys.insert(ConnectableRef::TrackInput, folder_input);

            let mut plugin_nodes: Vec<NodeId> = Vec::new();
            for idx in 0..t.clap_plugins.len() {
                let node = self.push_plugin(&track, t, PluginKind::Clap, idx, folder_input);
                let id = t.clap_plugins[idx].id;
                source_keys.insert(ConnectableRef::ClapPlugin(id), node);
                target_keys.insert(ConnectableRef::ClapPlugin(id), node);
                plugin_nodes.push(node);
            }
            for idx in 0..t.vst3_plugins.len() {
                let node = self.push_plugin(&track, t, PluginKind::Vst3, idx, folder_input);
                let id = t.vst3_plugins[idx].id;
                source_keys.insert(ConnectableRef::Vst3Plugin(id), node);
                target_keys.insert(ConnectableRef::Vst3Plugin(id), node);
                plugin_nodes.push(node);
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            for idx in 0..t.lv2_plugins.len() {
                let node = self.push_plugin(&track, t, PluginKind::Lv2, idx, folder_input);
                let id = t.lv2_plugins[idx].id;
                source_keys.insert(ConnectableRef::Lv2Plugin(id), node);
                target_keys.insert(ConnectableRef::Lv2Plugin(id), node);
                plugin_nodes.push(node);
            }

            let mut child_lasts: Vec<NodeId> = Vec::new();
            for child_track in &t.child_tracks {
                let (child_first, child_last) =
                    self.append_track(child_track.clone(), Some(folder_input));
                let child_name = child_track.lock().name.clone();
                source_keys.insert(ConnectableRef::ChildTrack(child_name.clone()), child_last);
                target_keys.insert(ConnectableRef::ChildTrack(child_name), child_first);
                child_lasts.push(child_last);
            }

            let folder_output = self.push_node(Op::Task {
                task: ProcessTask::FolderOutput(track.clone()),
                ins: Vec::new(),
                outs: outs.clone(),
            });
            self.edges.insert((folder_input, folder_output));
            for &p in &plugin_nodes {
                self.edges.insert((p, folder_output));
            }
            for &c in &child_lasts {
                self.edges.insert((c, folder_output));
            }
            for &out in &outs {
                self.producer.insert(out, folder_output);
            }

            // Cross-connectable edges within this folder's routing graph,
            // exactly as the legacy builder derived them.
            for conn in t.connectable_connections() {
                let ConnectableConnection { from, to, .. } = conn;
                let (Some(&source), Some(&target)) = (source_keys.get(&from), target_keys.get(&to))
                else {
                    continue;
                };
                if source != target {
                    self.edges.insert((source, target));
                }
            }

            (folder_input, folder_output)
        } else {
            let task = self.push_node(Op::Task {
                task: ProcessTask::Track(track.clone()),
                ins: ins.clone(),
                outs: outs.clone(),
            });
            if let Some(pred) = predecessor {
                self.edges.insert((pred, task));
            }
            self.register_task_ports(task, &ins, true);
            for &out in &outs {
                self.producer.insert(out, task);
            }
            (task, task)
        }
    }

    fn push_plugin(
        &mut self,
        track: &Arc<UnsafeMutex<Box<Track>>>,
        t: &Track,
        kind: PluginKind,
        index: usize,
        folder_input: NodeId,
    ) -> NodeId {
        let (input_ports, output_ports): (Vec<Arc<AudioIO>>, Vec<Arc<AudioIO>>) = match kind {
            PluginKind::Clap => {
                let proc = t.clap_plugins[index].processor.lock();
                (proc.audio_inputs().to_vec(), proc.audio_outputs().to_vec())
            }
            PluginKind::Vst3 => {
                let proc = t.vst3_plugins[index].processor.lock();
                (proc.audio_inputs().to_vec(), proc.audio_outputs().to_vec())
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            PluginKind::Lv2 => {
                let proc = t.lv2_plugins[index].processor.lock();
                (proc.audio_inputs().to_vec(), proc.audio_outputs().to_vec())
            }
        };
        for p in &input_ports {
            self.consumer_ports.push(p.clone());
        }
        let pins: Vec<BufferId> = input_ports.iter().map(|p| self.buffer_for(p)).collect();
        let pouts: Vec<BufferId> = output_ports.iter().map(|p| self.buffer_for(p)).collect();
        let node = self.push_node(Op::Task {
            task: ProcessTask::Plugin {
                track: track.clone(),
                kind,
                index,
            },
            ins: pins.clone(),
            outs: pouts.clone(),
        });
        self.edges.insert((folder_input, node));
        self.register_task_ports(node, &pins, false);
        for &out in &pouts {
            self.producer.insert(out, node);
        }
        node
    }

    /// Record which task reads (and optionally writes in place) each port.
    fn register_task_ports(&mut self, node: NodeId, ins: &[BufferId], in_place: bool) {
        for &b in ins {
            self.port_readers.entry(b).or_default().push(node);
            if in_place {
                self.port_inplace_writers.entry(b).or_default().push(node);
            }
        }
    }

    /// Create the `Sum`/`Zero` nodes for every consumer port, wire the edges,
    /// topologically sort, and freeze into a `RenderPlan`.
    fn finish(mut self) -> RenderPlan {
        for port in self.consumer_ports.clone() {
            let output = self.buffer_for(&port);
            let sources: Vec<BufferId> = {
                let conns = port.connections.lock();
                conns.iter().map(|p| self.buffer_for(p)).collect()
            };
            let node = if sources.is_empty() {
                self.push_node(Op::Zero { output })
            } else {
                let node = self.push_node(Op::Sum {
                    inputs: sources.clone(),
                    output,
                });
                for src in sources {
                    match self.producer.get(&src) {
                        Some(&prod) => {
                            self.edges.insert((prod, node));
                        }
                        None => {
                            tracing::warn!(
                                "render plan: connection source for buffer {src} has no producer; \
                                 treating as silent"
                            );
                        }
                    }
                }
                node
            };
            // Every task reading this port runs after its Sum/Zero node.
            if let Some(readers) = self.port_readers.get(&output).cloned() {
                for reader in readers {
                    self.edges.insert((node, reader));
                }
            }
        }

        let n = self.nodes.len();
        let (order, forced) = topo_sort(n, &self.edges);
        let mut remap = vec![0u32; n];
        for (new_idx, &old_idx) in order.iter().enumerate() {
            remap[old_idx as usize] = new_idx as NodeId;
        }

        let mut nodes = Vec::with_capacity(n);
        for &old_idx in &order {
            nodes.push(std::mem::replace(
                &mut self.nodes[old_idx as usize],
                Op::Zero { output: 0 },
            ));
        }

        let mut indegree = vec![0u32; n];
        let mut dependents: Vec<Vec<NodeId>> = vec![Vec::new(); n];
        for &(from, to) in &self.edges {
            let (from, to) = (remap[from as usize], remap[to as usize]);
            indegree[to as usize] += 1;
            dependents[from as usize].push(to);
        }
        let sources: Vec<NodeId> = (0..n as NodeId)
            .filter(|&i| indegree[i as usize] == 0)
            .collect();
        let forced: Vec<NodeId> = forced.iter().map(|&f| remap[f as usize]).collect();

        let plan = RenderPlan {
            buffer_size: self.buffer_size,
            buffers: self.buffers,
            nodes,
            indegree,
            dependents,
            sources,
            hw_in_map: self.hw_in_map,
            hw_in_ports: self.hw_in_ports,
            hw_out_map: self.hw_out_map,
            port_map: self.port_map,
            forced,
        };
        if let Err(e) = plan.verify() {
            tracing::error!("render plan invariant violation: {e}");
        }
        plan
    }
}

/// Kahn's algorithm. Returns the topological order of all nodes — nodes left
/// over after the algorithm (feedback loops) are appended at the end and also
/// returned separately as `forced`.
fn topo_sort(n: usize, edges: &HashSet<(NodeId, NodeId)>) -> (Vec<NodeId>, Vec<NodeId>) {
    let mut indegree = vec![0u32; n];
    let mut dependents: Vec<Vec<NodeId>> = vec![Vec::new(); n];
    for &(from, to) in edges {
        indegree[to as usize] += 1;
        dependents[from as usize].push(to);
    }
    let mut queue: VecDeque<NodeId> = (0..n as NodeId)
        .filter(|&i| indegree[i as usize] == 0)
        .collect();
    let mut order = Vec::with_capacity(n);
    while let Some(node) = queue.pop_front() {
        order.push(node);
        for &d in &dependents[node as usize] {
            indegree[d as usize] -= 1;
            if indegree[d as usize] == 0 {
                queue.push_back(d);
            }
        }
    }
    let placed: HashSet<NodeId> = order.iter().copied().collect();
    let forced: Vec<NodeId> = (0..n as NodeId).filter(|i| !placed.contains(i)).collect();
    order.extend(forced.iter().copied());
    (order, forced)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectable::connect_audio;

    fn make_track(name: &str, ins: usize, outs: usize) -> Arc<UnsafeMutex<Box<Track>>> {
        Arc::new(UnsafeMutex::new(Box::new(Track::new(
            name.to_string(),
            ins,
            outs,
            0,
            0,
            64,
            48_000.0,
        ))))
    }

    fn state_with(tracks: Vec<Arc<UnsafeMutex<Box<Track>>>>) -> State {
        let mut state = State::default();
        for t in tracks {
            state.tracks.insert(t.lock().name.clone(), t);
        }
        state
    }

    /// Connect `a`'s output `a_port` to `b`'s input `b_port`.
    fn connect(
        a: &Arc<UnsafeMutex<Box<Track>>>,
        a_port: usize,
        b: &Arc<UnsafeMutex<Box<Track>>>,
        b_port: usize,
    ) {
        let src = a.lock();
        let dst = b.lock();
        connect_audio(&**src, a_port, &**dst, b_port).expect("connect");
    }

    fn task_nodes(plan: &RenderPlan, name: &str) -> Vec<usize> {
        plan.nodes
            .iter()
            .enumerate()
            .filter_map(|(i, op)| match op {
                Op::Task { task, .. } => {
                    let track = match task {
                        ProcessTask::Track(t)
                        | ProcessTask::FolderInput(t)
                        | ProcessTask::FolderOutput(t) => t,
                        ProcessTask::Plugin { track, .. } => track,
                    };
                    if track.lock().name == name {
                        Some(i)
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .collect()
    }

    fn task_node(plan: &RenderPlan, name: &str, want: fn(&ProcessTask) -> bool) -> usize {
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

    fn sum_nodes(plan: &RenderPlan) -> Vec<(usize, Vec<BufferId>, BufferId)> {
        plan.nodes
            .iter()
            .enumerate()
            .filter_map(|(i, op)| match op {
                Op::Sum { inputs, output } => Some((i, inputs.clone(), *output)),
                _ => None,
            })
            .collect()
    }

    fn zero_count(plan: &RenderPlan) -> usize {
        plan.nodes
            .iter()
            .filter(|op| matches!(op, Op::Zero { .. }))
            .count()
    }

    fn is_track(t: &ProcessTask) -> bool {
        matches!(t, ProcessTask::Track(_))
    }
    fn is_folder_input(t: &ProcessTask) -> bool {
        matches!(t, ProcessTask::FolderInput(_))
    }
    fn is_folder_output(t: &ProcessTask) -> bool {
        matches!(t, ProcessTask::FolderOutput(_))
    }

    #[test]
    fn producer_chain_orders_zero_track_sum_track() {
        let a = make_track("a", 1, 1);
        let b = make_track("b", 1, 1);
        connect(&a, 0, &b, 0);
        let plan = RenderPlan::compile(&state_with(vec![a, b]), &[], &[], 64);
        plan.verify().expect("invariants");

        let sums = sum_nodes(&plan);
        assert_eq!(sums.len(), 1, "one connected input -> one Sum");
        assert_eq!(sums[0].1.len(), 1);

        let task_a = task_node(&plan, "a", is_track);
        let task_b = task_node(&plan, "b", is_track);
        let sum = sums[0].0;
        assert_eq!(zero_count(&plan), 1, "a's unconnected input -> Zero");
        let zero = plan
            .nodes
            .iter()
            .position(|op| matches!(op, Op::Zero { .. }))
            .expect("zero node");

        assert!(zero < task_a, "Zero before the task that reads it");
        assert!(task_a < sum, "producer before the Sum of its consumer");
        assert!(sum < task_b, "Sum before the consuming track");
        assert_eq!(plan.sources, vec![zero as NodeId]);
        assert_eq!(plan.indegree[task_b], 1);
        assert!(plan.forced.is_empty());
    }

    #[test]
    fn two_sources_insert_sum_with_two_inputs() {
        let a = make_track("a", 0, 1);
        let b = make_track("b", 0, 1);
        let c = make_track("c", 1, 1);
        connect(&a, 0, &c, 0);
        connect(&b, 0, &c, 0);
        let plan = RenderPlan::compile(&state_with(vec![a, b, c]), &[], &[], 64);
        plan.verify().expect("invariants");

        let sums = sum_nodes(&plan);
        assert_eq!(sums.len(), 1);
        assert_eq!(sums[0].1.len(), 2, "both sources summed");
        assert_eq!(plan.indegree[sums[0].0], 2);

        let task_a = task_node(&plan, "a", is_track);
        let task_b = task_node(&plan, "b", is_track);
        assert!(task_a < sums[0].0 && task_b < sums[0].0);
        // No Zero nodes: c's input is connected, a and b have no inputs.
        assert_eq!(zero_count(&plan), 0);
        assert_eq!(plan.sources.len(), 2, "two root tracks are sources");
    }

    #[test]
    fn folder_track_emits_input_child_output_chain() {
        let folder = make_track("folder", 1, 1);
        let child = make_track("child", 1, 1);
        folder.lock().is_folder = true;
        child.lock().parent_track = Some("folder".to_string());
        folder.lock().child_tracks.push(child.clone());
        let plan = RenderPlan::compile(&state_with(vec![folder, child]), &[], &[], 64);
        plan.verify().expect("invariants");

        let fi = task_node(&plan, "folder", is_folder_input);
        let fo = task_node(&plan, "folder", is_folder_output);
        let child_task = task_node(&plan, "child", is_track);
        assert!(fi < child_task, "folder input before child");
        assert!(child_task < fo, "child before folder output");
        assert!(plan.dependents[fi].contains(&(child_task as NodeId)));
        assert!(plan.dependents[child_task].contains(&(fo as NodeId)));
        // Only the folder shows up at the top level; the child is not a root.
        assert_eq!(task_nodes(&plan, "child").len(), 1);
    }

    #[test]
    fn feedback_cycle_is_broken_and_marked_forced() {
        let a = make_track("a", 1, 1);
        let b = make_track("b", 1, 1);
        connect(&a, 0, &b, 0);
        connect(&b, 0, &a, 0);
        let plan = RenderPlan::compile(&state_with(vec![a, b]), &[], &[], 64);

        // Two tasks + two Sums, all in the cycle: nothing is a source.
        assert_eq!(plan.nodes.len(), 4);
        assert!(plan.sources.is_empty());
        assert_eq!(plan.forced.len(), 4, "whole cycle marked forced");
        // verify() tolerates forced nodes (edges among them may point any way).
        plan.verify().expect("invariants tolerate forced cycle");
    }

    #[test]
    fn hw_bridges_become_source_and_sink_nodes() {
        let t = make_track("t", 1, 1);
        let hw_in = Arc::new(AudioIO::new(64));
        let hw_out = Arc::new(AudioIO::new(64));
        // Route: hw_in -> track input, track output -> hw_out.
        {
            let track = t.lock();
            AudioIO::connect(&hw_in, &track.audio.ins[0]);
            AudioIO::connect(&track.audio.outs[0], &hw_out);
        }
        let plan = RenderPlan::compile(
            &state_with(vec![t]),
            std::slice::from_ref(&hw_in),
            std::slice::from_ref(&hw_out),
            64,
        );
        plan.verify().expect("invariants");

        let hw_node = plan
            .nodes
            .iter()
            .position(|op| matches!(op, Op::HwInput { .. }))
            .expect("HwInput node");
        assert_eq!(plan.hw_in_map.len(), 1);
        assert_eq!(plan.hw_out_map.len(), 1);
        let (chan, buf) = plan.hw_in_map[0];
        assert_eq!(chan, 0);
        match &plan.nodes[hw_node] {
            Op::HwInput { output, .. } => assert_eq!(*output, buf),
            _ => unreachable!(),
        }
        // hw_in is a true source feeding the track input's Sum.
        assert!(plan.sources.contains(&(hw_node as NodeId)));
        let sums = sum_nodes(&plan);
        assert_eq!(sums.len(), 2, "track input sum + hw_out bridge sum");
        // hw_out bridge output buffer is mapped for draining.
        let (out_buf, out_chan) = plan.hw_out_map[0];
        assert_eq!(out_chan, 0);
        assert!(sums.iter().any(|(_, _, output)| *output == out_buf));
    }

    /// Build a plan by hand for `verify` negative tests.
    fn hand_plan(
        buffers: usize,
        nodes: Vec<Op>,
        indegree: Vec<u32>,
        dependents: Vec<Vec<NodeId>>,
        sources: Vec<NodeId>,
    ) -> RenderPlan {
        RenderPlan {
            buffer_size: 64,
            buffers: (0..buffers)
                .map(|_| UnsafeCell::new(vec![0.0; 64]))
                .collect(),
            nodes,
            indegree,
            dependents,
            sources,
            hw_in_map: vec![],
            hw_in_ports: vec![],
            hw_out_map: vec![],
            port_map: HashMap::new(),
            forced: vec![],
        }
    }

    #[test]
    fn verify_rejects_backward_edge() {
        let plan = hand_plan(
            2,
            vec![
                Op::Sum {
                    inputs: vec![1],
                    output: 0,
                },
                Op::HwInput {
                    channel: 0,
                    output: 1,
                },
            ],
            vec![1, 0],
            vec![vec![], vec![0]],
            vec![1],
        );
        // Node 1 (HwInput) sits after node 0 (Sum) but feeds it: fine topo-wise
        // (1 -> 0 is backward!) — this must be rejected.
        assert!(plan.verify().is_err());
    }

    #[test]
    fn verify_rejects_racing_writers() {
        // Two Sum nodes write the same buffer with no path between them.
        let plan = hand_plan(
            3,
            vec![
                Op::HwInput {
                    channel: 0,
                    output: 1,
                },
                Op::HwInput {
                    channel: 1,
                    output: 2,
                },
                Op::Sum {
                    inputs: vec![1],
                    output: 0,
                },
                Op::Sum {
                    inputs: vec![2],
                    output: 0,
                },
            ],
            vec![2, 0, 0, 0],
            vec![vec![], vec![2], vec![], vec![]],
            vec![0, 1],
        );
        let err = plan.verify().expect_err("racing writers must fail");
        assert!(err.contains("unordered nodes"));
    }

    #[test]
    fn buffers_are_sized_and_silent() {
        let t = make_track("t", 2, 1);
        let plan = RenderPlan::compile(&state_with(vec![t]), &[], &[], 256);
        assert_eq!(plan.buffer_size, 256);
        // 2 ins + 1 out = 3 port buffers.
        assert_eq!(plan.buffer_count(), 3);
        for i in 0..plan.buffer_count() as BufferId {
            // Safety: test thread, no node is executing.
            let buf = unsafe { plan.buffer(i) };
            assert_eq!(buf.len(), 256);
            assert!(buf.iter().all(|&s| s == 0.0));
        }
        // Two unconnected inputs -> two Zero nodes.
        assert_eq!(zero_count(&plan), 2);
        assert_eq!(plan.port_map.len(), 3);
    }
}
