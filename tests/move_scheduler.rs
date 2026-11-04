use std::collections::BTreeMap;

use fatresize::resize::{DataMove, MoveStep, schedule_moves};

mod support;
use support::Lcg;

const REGION: usize = 1024;

fn movement(source: usize, target: usize, length: usize) -> DataMove {
    DataMove {
        source_cluster: source as u32,
        target_cluster: target as u32,
        source_offset: source as u64,
        target_offset: target as u64,
        length: length as u32,
    }
}

fn original() -> Vec<u8> {
    let mut rng = Lcg::new(0x5eed);
    (0..REGION).map(|_| rng.next_u32() as u8).collect()
}

fn simulate(moves: &[DataMove]) -> (Vec<u8>, usize) {
    let original = original();
    let mut current = original.clone();
    let mut scratch = BTreeMap::<u32, Vec<u8>>::new();
    let schedule = schedule_moves(moves);
    for step in &schedule.steps {
        match *step {
            MoveStep::SaveToScratch {
                source_offset,
                length,
                slot,
            } => {
                let start = source_offset as usize;
                let end = start + length as usize;
                assert_eq!(
                    &current[start..end],
                    &original[start..end],
                    "scratch save read data that was already overwritten"
                );
                assert!(
                    scratch.insert(slot, current[start..end].to_vec()).is_none(),
                    "scratch slot {slot} reused before restore"
                );
            }
            MoveStep::Copy {
                source_offset,
                target_offset,
                length,
            } => {
                let start = source_offset as usize;
                let end = start + length as usize;
                assert_eq!(
                    &current[start..end],
                    &original[start..end],
                    "copy read data that was already overwritten"
                );
                let target = target_offset as usize;
                current[target..target + length as usize].copy_from_slice(&original[start..end]);
            }
            MoveStep::RestoreFromScratch {
                target_offset,
                length,
                slot,
            } => {
                let bytes = scratch.remove(&slot).expect("scratch slot was not saved");
                assert_eq!(bytes.len(), length as usize);
                let target = target_offset as usize;
                current[target..target + bytes.len()].copy_from_slice(&bytes);
            }
        }
    }
    assert!(scratch.is_empty(), "scratch slots were left behind");

    for movement in moves {
        if movement.source_offset == movement.target_offset {
            continue;
        }
        let source = movement.source_offset as usize;
        let target = movement.target_offset as usize;
        let length = movement.length as usize;
        assert_eq!(
            &current[target..target + length],
            &original[source..source + length],
            "move {source} -> {target} length {length} was not applied"
        );
    }
    (current, schedule.scratch_bytes as usize)
}

#[test]
fn handles_swap_cycle_and_scratch_restore() {
    let moves = vec![movement(0, 10, 10), movement(10, 0, 10)];
    let (_, scratch) = simulate(&moves);
    assert_eq!(scratch, 10);
}

#[test]
fn handles_overlapping_source_ranges_with_disjoint_targets() {
    let moves = vec![
        movement(0, 100, 20),
        movement(5, 200, 20),
        movement(10, 300, 20),
        movement(0, 400, 30),
    ];
    let (_, _) = simulate(&moves);
}

#[test]
fn handles_unequal_length_overlaps_and_shift_chains() {
    let moves = vec![
        movement(0, 10, 10),
        movement(5, 20, 20),
        movement(15, 0, 5),
        movement(30, 40, 7),
        movement(40, 50, 7),
    ];
    let (_, _) = simulate(&moves);
}

#[test]
fn randomized_disjoint_target_schedules_preserve_every_source() {
    for seed in 0..250_u64 {
        let mut rng = Lcg::new(seed);
        let count = rng.range(1, 16) as usize;
        let mut moves = Vec::new();
        let mut targets = Vec::<(usize, usize)>::new();
        let mut attempts = 0;
        while moves.len() < count && attempts < 1_000 {
            attempts += 1;
            let length = rng.range(1, 20) as usize;
            let source = rng.range(0, (REGION - length) as u32) as usize;
            let target = rng.range(0, (REGION - length) as u32) as usize;
            if source == target {
                continue;
            }
            let target_end = target + length;
            if targets
                .iter()
                .any(|&(start, end)| target < end && start < target_end)
            {
                continue;
            }
            targets.push((target, target_end));
            moves.push(movement(source, target, length));
        }
        assert!(!moves.is_empty());
        let (_, _) = simulate(&moves);
    }
}

#[test]
fn identity_moves_are_removed() {
    let moves = vec![movement(10, 10, 20), movement(30, 30, 5)];
    let schedule = schedule_moves(&moves);
    assert!(schedule.steps.is_empty());
    assert_eq!(schedule.scratch_bytes, 0);
}

#[test]
fn schedules_many_disjoint_moves_without_quadratic_behavior() {
    const COUNT: usize = 20_000;
    let region = COUNT * 2;
    let mut moves = Vec::with_capacity(COUNT);
    for index in 0..COUNT {
        moves.push(movement(index, region / 2 + index, 1));
    }
    let schedule = schedule_moves(&moves);
    assert_eq!(schedule.steps.len(), COUNT);
    assert_eq!(schedule.scratch_bytes, 0);
    for (step, index) in schedule.steps.iter().zip(0..) {
        assert!(matches!(
            step,
            MoveStep::Copy {
                source_offset,
                target_offset,
                length: 1
            } if *source_offset == index as u64
                && *target_offset == (region / 2 + index) as u64
        ));
    }
}
