//! Parallel offline rendering of a dependency-closed selection of plan nodes.
use crate::executor::NodeJob;
use crate::render_plan::{NodeId, RenderPlan, SharedPlan};
use crate::workers::worker::Worker;
use rayon::prelude::*;

pub(crate) struct OfflineRender {
    levels: Vec<Vec<Vec<NodeId>>>,
}

impl OfflineRender {
    pub(crate) fn new(plan: &RenderPlan, selected: &[NodeId]) -> Self {
        Self {
            levels: chain_levels(&plan.dependents, !plan.forced.is_empty(), selected),
        }
    }

    pub(crate) fn render(&self, worker_id: usize, plan: &SharedPlan) {
        let process = |chain: &Vec<NodeId>| {
            for &node in chain {
                Worker::process_node_job_result(
                    worker_id,
                    NodeJob {
                        epoch: 0,
                        plan: plan.clone(),
                        node,
                    },
                );
            }
        };
        for level in &self.levels {
            // Single-chain levels avoid pool dispatch. Parallel iteration joins
            // every writer before consumers or the next block access the arena.
            if level.len() == 1 {
                process(&level[0]);
            } else {
                level.par_iter().for_each(process);
            }
        }
    }
}

/// Collapse linear dependencies into one job so a track's routing and DSP
/// can stay on one worker instead of returning to the pool after every node.
/// Keep forks and joins as scheduling boundaries: merging across either
/// would delay independent work or bypass a dependency.
fn chain_levels(
    dependents: &[Vec<NodeId>],
    feedback: bool,
    selected: &[NodeId],
) -> Vec<Vec<Vec<NodeId>>> {
    if selected.is_empty() {
        return Vec::new();
    }
    if feedback {
        return vec![vec![selected.to_vec()]];
    }
    let mut included = vec![false; dependents.len()];
    for &node in selected {
        included[node as usize] = true;
    }
    let mut parents = vec![Vec::new(); dependents.len()];
    let mut child_count = vec![0; dependents.len()];
    for &node in selected {
        for &child in &dependents[node as usize] {
            if included[child as usize] {
                parents[child as usize].push(node);
                child_count[node as usize] += 1;
            }
        }
    }
    let mut group_for = vec![0; dependents.len()];
    let mut chains: Vec<Vec<NodeId>> = Vec::new();
    for &node in selected {
        let predecessors = &parents[node as usize];
        let group = if predecessors.len() == 1 && child_count[predecessors[0] as usize] == 1 {
            group_for[predecessors[0] as usize]
        } else {
            chains.push(Vec::new());
            chains.len() - 1
        };
        group_for[node as usize] = group;
        chains[group].push(node);
    }
    let mut edges = vec![Vec::new(); chains.len()];
    for &node in selected {
        let from = group_for[node as usize];
        for &child in &dependents[node as usize] {
            if included[child as usize] {
                let to = group_for[child as usize];
                if from != to {
                    edges[from].push(to as NodeId);
                }
            }
        }
    }
    let groups: Vec<_> = (0..chains.len() as NodeId).collect();
    levels(&edges, false, &groups)
        .into_iter()
        .map(|level| {
            level
                .into_iter()
                .map(|group| std::mem::take(&mut chains[group as usize]))
                .collect()
        })
        .collect()
}

fn levels(dependents: &[Vec<NodeId>], feedback: bool, selected: &[NodeId]) -> Vec<Vec<NodeId>> {
    // Feedback buffers rely on the plan's serial read/write order.
    if feedback {
        return selected.iter().map(|&node| vec![node]).collect();
    }
    let mut depth = vec![0; dependents.len()];
    let mut levels: Vec<Vec<NodeId>> = Vec::new();
    // Selection is in topological order and includes every prerequisite.
    // Thus nodes at equal depth cannot share a producer/consumer buffer.
    for &node in selected {
        let level = depth[node as usize];
        levels.resize_with(levels.len().max(level + 1), Vec::new);
        levels[level].push(node);
        for &next in &dependents[node as usize] {
            depth[next as usize] = depth[next as usize].max(level + 1);
        }
    }
    levels
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalesces_track_chains_but_preserves_folder_join() {
        let edges = vec![vec![1], vec![4], vec![3], vec![4], vec![5], vec![]];
        assert_eq!(
            chain_levels(&edges, false, &[0, 1, 2, 3, 4, 5]),
            vec![vec![vec![0, 1], vec![2, 3]], vec![vec![4, 5]]]
        );
    }

    #[test]
    fn sidechain_fork_is_not_merged_into_either_consumer() {
        let edges = vec![vec![1, 2], vec![3], vec![3], vec![]];
        assert_eq!(
            chain_levels(&edges, false, &[0, 1, 2, 3]),
            vec![vec![vec![0]], vec![vec![1], vec![2]], vec![vec![3]]]
        );
        // Unselected downstream tracks need not create scheduling boundaries.
        assert_eq!(chain_levels(&edges, false, &[0, 1]), vec![vec![vec![0, 1]]]);
        assert_eq!(
            chain_levels(&edges, true, &[0, 1, 2, 3]),
            vec![vec![vec![0, 1, 2, 3]]]
        );
        assert!(chain_levels(&edges, false, &[]).is_empty());
    }

    #[test]
    fn coalescing_preserves_every_dependency_in_small_dags() {
        // Exhaust all forward-edge graphs on five nodes, including isolated
        // nodes, unequal branch lengths, forks, joins and transitive edges.
        for mask in 0..1024 {
            let mut edges = vec![Vec::new(); 5];
            let mut bit = 0;
            for (from, children) in edges.iter_mut().enumerate() {
                for to in from + 1..5 {
                    if mask & (1 << bit) != 0 {
                        children.push(to as NodeId);
                    }
                    bit += 1;
                }
            }
            let schedule = chain_levels(&edges, false, &[0, 1, 2, 3, 4]);
            let mut positions = [None; 5];
            for (level, chains) in schedule.iter().enumerate() {
                for (chain, nodes) in chains.iter().enumerate() {
                    for (index, &node) in nodes.iter().enumerate() {
                        assert!(
                            positions[node as usize]
                                .replace((level, chain, index))
                                .is_none()
                        );
                    }
                }
            }
            assert!(positions.iter().all(Option::is_some));
            for (from, children) in edges.iter().enumerate() {
                let (level, chain, index) = positions[from].unwrap();
                for &to in children {
                    let (next_level, next_chain, next_index) = positions[to as usize].unwrap();
                    assert!(
                        level < next_level
                            || (level == next_level && chain == next_chain && index < next_index)
                    );
                }
            }
        }
    }

    #[test]
    #[ignore = "manual scheduling benchmark; run with --ignored --nocapture"]
    fn benchmark_offline_chain_scheduling() {
        use crate::render_plan::Op;
        use std::cell::UnsafeCell;
        use std::collections::HashMap;
        use std::sync::{Arc, atomic::AtomicUsize};
        use std::time::Instant;

        // Eight independent ten-node routing chains, then a common folder.
        // Isolate scheduling cost; this is not a full plugin/session benchmark.
        let count = 81;
        let mut edges = vec![Vec::new(); count];
        for (node, children) in edges.iter_mut().enumerate().take(80) {
            children.push(if node % 10 == 9 { 80 } else { node + 1 } as NodeId);
        }
        let mut indegree = vec![0; count];
        for children in &edges {
            for &child in children {
                indegree[child as usize] += 1;
            }
        }
        let selected: Vec<_> = (0..count as NodeId).collect();
        let old = OfflineRender {
            levels: levels(&edges, false, &selected)
                .into_iter()
                .map(|level| level.into_iter().map(|node| vec![node]).collect())
                .collect(),
        };
        let collector = basedrop::Collector::new();
        let plan = Arc::new(basedrop::Owned::new(
            &collector.handle(),
            RenderPlan {
                buffer_size: 1024,
                buffers: (0..count)
                    .map(|_| UnsafeCell::new(vec![0.25; 1024]))
                    .collect(),
                buffer_latencies: (0..count).map(|_| AtomicUsize::new(0)).collect(),
                nodes: selected.iter().map(|&output| Op::Zero { output }).collect(),
                indegree,
                dependents: edges,
                sources: (0..80).step_by(10).collect(),
                hw_in_map: vec![],
                hw_out_map: vec![],
                port_map: HashMap::new(),
                midi_edges: vec![],
                forced: vec![],
            },
        ));
        let new = OfflineRender::new(&plan, &selected);
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(8)
            .build()
            .unwrap();
        for render in [&old, &new] {
            pool.install(|| render.render(0, &plan));
        }
        for _ in 0..3 {
            for (label, render) in [("per-node", &old), ("coalesced", &new)] {
                let start = Instant::now();
                for _ in 0..2000 {
                    pool.install(|| render.render(0, &plan));
                }
                eprintln!("{label}: {:?} (2000 blocks)", start.elapsed());
            }
        }
    }

    #[test]
    fn independent_plugins_run_together_before_folder_mix() {
        // Two track -> plugin chains feeding a folder, plus an unrelated track.
        let edges = vec![vec![1], vec![4], vec![3], vec![4], vec![], vec![]];
        assert_eq!(
            levels(&edges, false, &[0, 1, 2, 3, 4]),
            vec![vec![0, 2], vec![1, 3], vec![4]]
        );
    }

    #[test]
    fn sidechain_dependency_delays_consumer() {
        let edges = vec![vec![1], vec![3], vec![3], vec![4], vec![]];
        assert_eq!(
            levels(&edges, false, &[0, 1, 2, 3, 4]),
            vec![vec![0, 2], vec![1], vec![3], vec![4]]
        );
    }

    #[test]
    fn feedback_keeps_serial_order_and_empty_selection_stays_empty() {
        let edges = vec![vec![1], vec![0]];
        assert_eq!(levels(&edges, true, &[0, 1]), vec![vec![0], vec![1]]);
        assert!(levels(&edges, false, &[]).is_empty());
    }
}
