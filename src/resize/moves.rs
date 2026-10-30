#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataMove {
    pub source_cluster: u32,
    pub target_cluster: u32,
    pub source_offset: u64,
    pub target_offset: u64,
    pub length: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoveStep {
    SaveToScratch {
        source_offset: u64,
        length: u32,
        slot: u32,
    },
    Copy {
        source_offset: u64,
        target_offset: u64,
        length: u32,
    },
    RestoreFromScratch {
        target_offset: u64,
        length: u32,
        slot: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveSchedule {
    pub steps: Vec<MoveStep>,
    pub scratch_bytes: u64,
}

#[derive(Clone)]
struct SchedulerNode {
    movement: DataMove,
    scratch: Option<u32>,
    source_released: bool,
    complete: bool,
}

/// Schedules arbitrary equal- or unequal-length byte moves without allowing a
/// destination write to destroy a source that has not yet been preserved.
pub fn schedule(moves: &[DataMove]) -> MoveSchedule {
    use std::collections::BTreeSet;

    let mut nodes: Vec<SchedulerNode> = moves
        .iter()
        .filter(|movement| movement.source_offset != movement.target_offset)
        .cloned()
        .map(|movement| SchedulerNode {
            movement,
            scratch: None,
            source_released: false,
            complete: false,
        })
        .collect();
    nodes.sort_by_key(|item| {
        (
            item.movement.source_offset,
            item.movement.target_offset,
            item.movement.source_cluster,
        )
    });
    if !nodes.windows(2).all(|pair| {
        range(pair[0].movement.source_offset, pair[0].movement.length).1
            <= pair[1].movement.source_offset
    }) {
        return schedule_overlapping_sources(moves);
    }

    // A move is ready once its destination no longer overlaps any source that
    // has not yet been copied or saved. Source ranges are sorted and disjoint,
    // so building this dependency graph is O((n + overlaps) log n), rather
    // than repeatedly scanning every pending move.
    let mut blockers = vec![0_usize; nodes.len()];
    let mut dependents = vec![Vec::<usize>::new(); nodes.len()];
    for index in 0..nodes.len() {
        let destination = range(
            nodes[index].movement.target_offset,
            nodes[index].movement.length,
        );
        let first = nodes.partition_point(|node| {
            range(node.movement.source_offset, node.movement.length).1 <= destination.0
        });
        for other in first..nodes.len() {
            let source = range(
                nodes[other].movement.source_offset,
                nodes[other].movement.length,
            );
            if source.0 >= destination.1 {
                break;
            }
            if other != index && overlaps(destination, source) {
                blockers[index] += 1;
                dependents[other].push(index);
            }
        }
    }

    let mut steps = Vec::new();
    let mut next_slot = 0_u32;
    let mut scratch_lengths = Vec::new();
    let mut ready = blockers
        .iter()
        .enumerate()
        .filter_map(|(index, &count)| (count == 0).then_some(index))
        .collect::<BTreeSet<_>>();
    let mut remaining = nodes.len();

    while remaining != 0 {
        if let Some(index) = ready.pop_first() {
            if nodes[index].complete {
                continue;
            }
            if let Some(slot) = nodes[index].scratch {
                steps.push(MoveStep::RestoreFromScratch {
                    target_offset: nodes[index].movement.target_offset,
                    length: nodes[index].movement.length,
                    slot,
                });
            } else {
                steps.push(MoveStep::Copy {
                    source_offset: nodes[index].movement.source_offset,
                    target_offset: nodes[index].movement.target_offset,
                    length: nodes[index].movement.length,
                });
            }
            nodes[index].complete = true;
            remaining -= 1;
            release_source(index, &mut nodes, &dependents, &mut blockers, &mut ready);
            continue;
        }

        // No ready node means the remaining dependency graph contains a
        // cycle. Preserve one source in scratch, release its dependency, and
        // continue topological scheduling.
        let index = nodes
            .iter()
            .position(|node| !node.complete && node.scratch.is_none())
            .expect("saved moves eventually become ready");
        let slot = next_slot;
        next_slot += 1;
        nodes[index].scratch = Some(slot);
        scratch_lengths.push(u64::from(nodes[index].movement.length));
        steps.push(MoveStep::SaveToScratch {
            source_offset: nodes[index].movement.source_offset,
            length: nodes[index].movement.length,
            slot,
        });
        release_source(index, &mut nodes, &dependents, &mut blockers, &mut ready);
    }
    MoveSchedule {
        steps,
        scratch_bytes: scratch_lengths.into_iter().sum(),
    }
}

fn schedule_overlapping_sources(moves: &[DataMove]) -> MoveSchedule {
    let mut pending = moves
        .iter()
        .filter(|movement| movement.source_offset != movement.target_offset)
        .cloned()
        .map(|movement| SchedulerNode {
            movement,
            scratch: None,
            source_released: false,
            complete: false,
        })
        .collect::<Vec<_>>();
    pending.sort_by_key(|item| {
        (
            item.movement.source_offset,
            item.movement.target_offset,
            item.movement.source_cluster,
        )
    });
    let mut steps = Vec::new();
    let mut next_slot = 0_u32;
    let mut scratch_lengths = Vec::new();
    while !pending.is_empty() {
        let ready = (0..pending.len()).find(|&index| {
            let destination = range(
                pending[index].movement.target_offset,
                pending[index].movement.length,
            );
            pending.iter().enumerate().all(|(other_index, other)| {
                index == other_index
                    || other.scratch.is_some()
                    || !overlaps(
                        destination,
                        range(other.movement.source_offset, other.movement.length),
                    )
            })
        });
        if let Some(index) = ready {
            let item = pending.remove(index);
            if let Some(slot) = item.scratch {
                steps.push(MoveStep::RestoreFromScratch {
                    target_offset: item.movement.target_offset,
                    length: item.movement.length,
                    slot,
                });
            } else {
                steps.push(MoveStep::Copy {
                    source_offset: item.movement.source_offset,
                    target_offset: item.movement.target_offset,
                    length: item.movement.length,
                });
            }
        } else {
            let index = pending
                .iter()
                .position(|item| item.scratch.is_none())
                .expect("saved moves eventually become ready");
            let slot = next_slot;
            next_slot += 1;
            pending[index].scratch = Some(slot);
            scratch_lengths.push(u64::from(pending[index].movement.length));
            steps.push(MoveStep::SaveToScratch {
                source_offset: pending[index].movement.source_offset,
                length: pending[index].movement.length,
                slot,
            });
        }
    }
    MoveSchedule {
        steps,
        scratch_bytes: scratch_lengths.into_iter().sum(),
    }
}

fn release_source(
    index: usize,
    nodes: &mut [SchedulerNode],
    dependents: &[Vec<usize>],
    blockers: &mut [usize],
    ready: &mut std::collections::BTreeSet<usize>,
) {
    if nodes[index].source_released {
        return;
    }
    nodes[index].source_released = true;
    for &dependent in &dependents[index] {
        blockers[dependent] -= 1;
        if blockers[dependent] == 0 {
            ready.insert(dependent);
        }
    }
}

fn range(start: u64, length: u32) -> (u64, u64) {
    (start, start.saturating_add(u64::from(length)))
}

fn overlaps(left: (u64, u64), right: (u64, u64)) -> bool {
    left.0 < right.1 && right.0 < left.1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn movement(source: u64, target: u64) -> DataMove {
        DataMove {
            source_cluster: source as u32,
            target_cluster: target as u32,
            source_offset: source,
            target_offset: target,
            length: 10,
        }
    }

    #[test]
    fn orders_overlap_chain_before_overwrite() {
        let result = schedule(&[movement(0, 10), movement(10, 20)]);
        assert_eq!(
            result.steps,
            vec![
                MoveStep::Copy {
                    source_offset: 10,
                    target_offset: 20,
                    length: 10
                },
                MoveStep::Copy {
                    source_offset: 0,
                    target_offset: 10,
                    length: 10
                }
            ]
        );
        assert_eq!(result.scratch_bytes, 0);
    }

    #[test]
    fn breaks_swap_cycle_with_scratch() {
        let result = schedule(&[movement(0, 10), movement(10, 0)]);
        assert_eq!(result.scratch_bytes, 10);
        assert!(matches!(result.steps[0], MoveStep::SaveToScratch { .. }));
        assert!(matches!(
            result.steps.last().unwrap(),
            MoveStep::RestoreFromScratch { .. }
        ));
    }
}
