use std::io;

use fatresize::resize::{
    RecoveryError, RecoveryPhase, RecoveryRecord, ResizeError, SectorBackup, SourceGeometry,
    SourceIdentity, writer,
};
use fatresize::{FatType, inspect, read_metadata};

mod support;
use support::{FatImage, FatSpec, Mem, read_file, temp_path};

fn fat16_file() -> Mem {
    let mut image = FatImage::new(FatSpec::fat16(10_000));
    image.set_chain(&[2]);
    image.fill_cluster(2, 0x11);
    image.put_entry(None, 0, b"LOW     BIN", 0x20, 2, 300);
    image.into_memory()
}

fn fat16_high_file() -> Mem {
    let mut image = FatImage::new(FatSpec::fat16(10_000));
    image.set_chain(&[2]);
    image.fill_cluster(2, 0x11);
    image.put_entry(None, 0, b"LOW     BIN", 0x20, 2, 300);
    image.set_chain(&[9_000, 9_001]);
    image.fill_cluster(9_000, 0x22);
    image.fill_cluster(9_001, 0x23);
    image.put_entry(None, 1, b"HIGH    BIN", 0x20, 9_000, 700);
    image.into_memory()
}

fn fat32_high_file() -> Mem {
    let mut image = FatImage::new(FatSpec::fat32(70_000));
    image.set_chain(&[3]);
    image.fill_cluster(3, 0x33);
    image.put_entry(None, 0, b"LOW     BIN", 0x20, 3, 100);
    image.set_chain(&[68_000]);
    image.fill_cluster(68_000, 0x44);
    image.put_entry(None, 1, b"HIGH    BIN", 0x20, 68_000, 200);
    image.finalize();
    image.into_memory()
}

fn record() -> RecoveryRecord {
    RecoveryRecord {
        source: SourceIdentity {
            volume_id: 7,
            volume_bytes: 5_022 * 512,
            boot_checksum: 9,
            identity_checksum: 10,
        },
        geometry: SourceGeometry {
            fat_type: FatType::Fat16,
            bytes_per_sector: 512,
            sectors_per_cluster: 1,
            reserved_sectors: 1,
            fat_count: 2,
            sectors_per_fat: 10,
            first_data_sector: 22,
            cluster_count: 5_000,
            total_sectors: 5_022,
        },
        phase: RecoveryPhase::Planned,
        sectors: vec![
            SectorBackup::new(0, vec![3; 512]),
            SectorBackup::new(10, vec![4; 1_024]),
        ],
    }
}

#[test]
fn fat16_shrink_relocates_high_clusters_and_grows_back() {
    let volume = fat16_high_file();
    let before = read_metadata(&volume).unwrap();
    assert!(before.report.is_consistent(), "{:?}", before.report.issues);
    let high = read_file(&volume, "/HIGH.BIN");
    let low = read_file(&volume, "/LOW.BIN");

    let plan = fatresize::resize::plan(&volume, 5_000 * 512).unwrap();
    assert_eq!(plan.target.fat_type, FatType::Fat16);
    assert_eq!(plan.target.total_sectors, 5_000);
    assert_eq!(plan.source_total_bytes, 10_000 * 512);
    assert!(
        plan.mappings
            .iter()
            .any(|mapping| mapping.source == 9_000 && mapping.target != 9_000)
    );
    assert!(
        plan.directory_patches
            .iter()
            .any(|patch| patch.path == "/HIGH.BIN")
    );
    assert!(!plan.moves.steps.is_empty());
    assert!(plan.recovery.backed_up_sectors() > 0);
    assert_eq!(
        plan.operations.first(),
        Some(&fatresize::resize::Operation::VerifyPreconditions)
    );
    assert_eq!(
        plan.operations.last(),
        Some(&fatresize::resize::Operation::MarkRecoveryComplete)
    );

    let recovery = temp_path("plan-fat16-shrink");
    writer::execute(&volume, &plan, &recovery, |_| {}).unwrap();
    let after = inspect(&volume).unwrap();
    assert!(after.is_consistent(), "{:?}", after.issues);
    assert_eq!(after.bpb.total_sectors, 5_000);
    assert_eq!(read_file(&volume, "/HIGH.BIN"), high);
    assert_eq!(read_file(&volume, "/LOW.BIN"), low);

    let grow_plan = fatresize::resize::plan(&volume, 8_000 * 512).unwrap();
    assert_eq!(grow_plan.target.total_sectors, 8_000);
    let grow_recovery = temp_path("plan-fat16-grow");
    writer::execute(&volume, &grow_plan, &grow_recovery, |_| {}).unwrap();
    let grown = inspect(&volume).unwrap();
    assert!(grown.is_consistent(), "{:?}", grown.issues);
    assert_eq!(grown.bpb.total_sectors, 8_000);
    assert_eq!(read_file(&volume, "/HIGH.BIN"), high);
    assert_eq!(read_file(&volume, "/LOW.BIN"), low);

    std::fs::remove_file(recovery).unwrap();
    std::fs::remove_file(grow_recovery).unwrap();
}

#[test]
fn fat16_plan_moves_the_fixed_root_directory() {
    let volume = fat16_high_file();
    let before = read_metadata(&volume).unwrap();
    let plan = fatresize::resize::plan(&volume, 5_000 * 512).unwrap();
    let root_bytes = u64::from(before.report.layout.root_dir_sectors) * 512;
    assert!(
        plan.moves.steps.iter().any(|step| matches!(
            step,
            fatresize::resize::MoveStep::Copy { length, .. } if u64::from(*length) == root_bytes
        )),
        "expected a move for the FAT16 fixed root directory"
    );
}

#[test]
fn fat32_shrink_updates_root_fsinfo_and_content() {
    let volume = fat32_high_file();
    let before = read_metadata(&volume).unwrap();
    assert!(before.report.is_consistent(), "{:?}", before.report.issues);
    let high = read_file(&volume, "/HIGH.BIN");
    let low = read_file(&volume, "/LOW.BIN");

    let plan = fatresize::resize::plan(&volume, 67_000 * 512).unwrap();
    assert_eq!(plan.target.fat_type, FatType::Fat32);
    assert!(
        plan.mappings
            .iter()
            .any(|mapping| mapping.source == 68_000 && mapping.target != 68_000)
    );
    let recovery = temp_path("plan-fat32-shrink");
    writer::execute(&volume, &plan, &recovery, |_| {}).unwrap();

    let after = inspect(&volume).unwrap();
    assert!(after.is_consistent(), "{:?}", after.issues);
    assert_eq!(after.bpb.total_sectors, 67_000);
    assert_eq!(read_file(&volume, "/HIGH.BIN"), high);
    assert_eq!(read_file(&volume, "/LOW.BIN"), low);
    let info = after.fs_info.unwrap();
    assert_eq!(
        info.free_clusters,
        Some(after.layout.cluster_count - after.allocated_clusters)
    );
    assert!(info.next_free.is_some());

    std::fs::remove_file(recovery).unwrap();
}

#[test]
fn fat32_root_cluster_can_be_relocated_from_the_high_end() {
    let spec = FatSpec {
        root_cluster: 90_000,
        ..FatSpec::fat32(100_000)
    };
    let mut image = FatImage::new(spec);
    image.set_chain(&[3]);
    image.fill_cluster(3, 0x55);
    image.put_entry(None, 0, b"ROOTFILEBIN", 0x20, 3, 64);
    image.finalize();
    let volume = image.into_memory();
    let before = read_metadata(&volume).unwrap();
    assert!(before.report.is_consistent(), "{:?}", before.report.issues);
    assert_eq!(before.report.bpb.root_cluster, 90_000);

    let plan = fatresize::resize::plan(&volume, 70_000 * 512).unwrap();
    let (source_root, target_root) = plan.root_cluster.unwrap();
    assert_eq!(source_root, 90_000);
    assert_ne!(target_root, source_root);

    let recovery = temp_path("plan-fat32-root");
    writer::execute(&volume, &plan, &recovery, |_| {}).unwrap();
    let after = read_metadata(&volume).unwrap();
    assert!(after.report.is_consistent(), "{:?}", after.report.issues);
    assert_eq!(after.report.bpb.root_cluster, target_root);
    assert_eq!(
        read_file(&volume, "/ROOTFILE.BIN"),
        (0..64)
            .map(|index| 0x55_u8.wrapping_add((index % 251) as u8))
            .collect::<Vec<_>>()
    );

    std::fs::remove_file(recovery).unwrap();
}

#[test]
fn active_fat_copy_is_used_and_both_copies_are_rewritten() {
    let mut image = FatImage::new(FatSpec::fat32(70_000).with_active_fat(1));
    image.set_chain(&[3]);
    image.fill_cluster(3, 0x66);
    image.put_entry(None, 0, b"ACTIVE  BIN", 0x20, 3, 128);
    image.finalize();
    let bps = image.bps();
    let fat_bytes = image.spec.effective_fat_sectors() as usize * bps;
    let copy0 = image.fat_offset(0);
    let copy1 = image.fat_offset(1);
    // Damage only the inactive copy. The reader must use copy 1.
    image.bytes.copy_within(copy1..copy1 + fat_bytes, copy0);
    image.bytes[copy0 + 20 * 4..copy0 + 20 * 4 + 4].copy_from_slice(&0x0fff_ffff_u32.to_le_bytes());
    let volume = image.into_memory();
    let report = inspect(&volume).unwrap();
    assert!(report.is_consistent(), "{:?}", report.issues);
    let content = read_file(&volume, "/ACTIVE.BIN");

    let plan = fatresize::resize::plan(&volume, 68_000 * 512).unwrap();
    let recovery = temp_path("plan-active-fat");
    writer::execute(&volume, &plan, &recovery, |_| {}).unwrap();

    let after = read_metadata(&volume).unwrap();
    assert!(after.report.is_consistent(), "{:?}", after.report.issues);
    assert_eq!(read_file(&volume, "/ACTIVE.BIN"), content);
    let target_fat_bytes = plan.target.sectors_per_fat as usize * bps;
    let target_copy0 = usize::from(plan.target.reserved_sectors) * bps;
    let target_copy1 = target_copy0 + target_fat_bytes;
    assert_eq!(
        &volume.bytes.borrow()[target_copy0..target_copy0 + target_fat_bytes],
        &volume.bytes.borrow()[target_copy1..target_copy1 + target_fat_bytes]
    );

    std::fs::remove_file(recovery).unwrap();
}

#[test]
fn planning_rejects_unsafe_targets_and_inconsistent_sources() {
    let volume = fat16_file();
    assert!(matches!(
        fatresize::resize::plan(&volume, 10_000 * 512 + 1),
        Err(ResizeError::TargetUnaligned { .. })
    ));
    assert!(matches!(
        fatresize::resize::plan(&volume, 10_001 * 512),
        Err(ResizeError::TargetTooLarge)
    ));
    assert!(matches!(
        fatresize::resize::plan(&volume, 4_000 * 512),
        Err(ResizeError::Fat12Unsupported)
    ));
    let fat32 = fat32_high_file();
    assert!(matches!(
        fatresize::resize::plan(&fat32, 60_000 * 512),
        Err(ResizeError::FatTypeTransition { .. })
    ));

    let mut bad = FatImage::new(FatSpec::fat16(10_000));
    bad.set_chain(&[2]);
    bad.put_entry(None, 0, b"BAD     BIN", 0x20, 2, 1);
    bad.set_fat_entry(2, 0xfff7);
    assert!(matches!(
        fatresize::resize::plan(&bad.into_memory(), 5_000 * 512),
        Err(ResizeError::InconsistentSource(_))
    ));

    let mut insufficient = FatImage::new(FatSpec::fat16(10_000));
    let chain = (2..5_002).collect::<Vec<_>>();
    insufficient.set_chain(&chain);
    insufficient.put_entry(None, 0, b"FULL    BIN", 0x20, 2, 5_000 * 512);
    assert!(matches!(
        fatresize::resize::plan(&insufficient.into_memory(), 5_000 * 512),
        Err(ResizeError::InsufficientSpace { .. })
    ));
}

#[test]
fn same_size_plan_is_a_noop_and_json_is_deterministic() {
    let volume = fat16_high_file();
    let first = fatresize::resize::plan(&volume, 10_000 * 512).unwrap();
    let second = fatresize::resize::plan(&volume, 10_000 * 512).unwrap();
    assert_eq!(first.to_json(), second.to_json());
    assert_eq!(first.to_text(), second.to_text());
    assert!(first.moves.steps.is_empty());
    assert_eq!(first.normalized_bytes, 10_000 * 512);
    assert!(first.mappings.iter().all(|m| m.source == m.target));
    assert!(first.to_json().contains("\"dry_run\":true"));
    assert!(first.to_json().contains("\"fat_type\":\"FAT16\""));
    assert!(first.to_text().contains("Resize plan (unchanged, dry-run)"));

    let recovery = temp_path("plan-noop");
    writer::execute(&volume, &first, &recovery, |_| {}).unwrap();
    assert_eq!(inspect(&volume).unwrap().bpb.total_sectors, 10_000);
    std::fs::remove_file(recovery).unwrap();
}

#[test]
fn execute_refuses_a_truncated_target_before_writing() {
    let volume = fat16_high_file();
    let plan = fatresize::resize::plan(&volume, 8_000 * 512).unwrap();
    let recovery = temp_path("plan-short");
    let mut small = volume.snapshot();
    small.truncate(7_000 * 512);
    let small = Mem::new(small);
    let error = writer::execute(&small, &plan, &recovery, |_| {}).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    assert!(small.writes.borrow().is_empty());
    assert!(!recovery.exists());
}

#[test]
fn recovery_record_rejects_every_body_corruption_and_truncation() {
    let encoded = record().serialize();
    assert_eq!(RecoveryRecord::deserialize(&encoded).unwrap(), record());
    for end in 0..encoded.len() {
        assert!(
            RecoveryRecord::deserialize(&encoded[..end]).is_err(),
            "accepted truncation at {end}"
        );
    }
    const HEADER: usize = 8 + 2 + 16 * 2;
    for index in HEADER..encoded.len() {
        let mut corrupt = encoded.clone();
        corrupt[index] ^= 1;
        assert!(
            RecoveryRecord::deserialize(&corrupt).is_err(),
            "accepted corruption at byte {index}"
        );
    }
    let mut bad_magic = encoded.clone();
    bad_magic[0] ^= 1;
    assert_eq!(
        RecoveryRecord::deserialize(&bad_magic),
        Err(RecoveryError::BadMagic)
    );
    let mut bad_version = encoded.clone();
    bad_version[8] = 99;
    assert!(matches!(
        RecoveryRecord::deserialize(&bad_version),
        Err(RecoveryError::UnsupportedVersion(99))
    ));
}

#[test]
fn recovery_record_rejects_invalid_geometry_and_overlapping_backups() {
    let mut invalid = record();
    invalid.geometry.bytes_per_sector = 123;
    assert_eq!(
        RecoveryRecord::deserialize(&invalid.serialize()),
        Err(RecoveryError::InvalidValue("geometry"))
    );

    let mut invalid = record();
    invalid.geometry.total_sectors = 5_000;
    assert!(matches!(
        RecoveryRecord::deserialize(&invalid.serialize()),
        Err(RecoveryError::InvalidValue("geometry"))
    ));

    let mut invalid = record();
    invalid.source.volume_bytes += 1;
    assert!(matches!(
        RecoveryRecord::deserialize(&invalid.serialize()),
        Err(RecoveryError::InvalidValue("geometry"))
    ));

    let mut invalid = record();
    invalid.geometry.fat_type = FatType::Fat32;
    assert!(matches!(
        RecoveryRecord::deserialize(&invalid.serialize()),
        Err(RecoveryError::InvalidValue("geometry"))
    ));

    let mut invalid = record();
    invalid.geometry.sectors_per_cluster = 3;
    assert!(matches!(
        RecoveryRecord::deserialize(&invalid.serialize()),
        Err(RecoveryError::InvalidValue("geometry"))
    ));

    let mut invalid = record();
    invalid.geometry.first_data_sector = 5_022;
    assert!(matches!(
        RecoveryRecord::deserialize(&invalid.serialize()),
        Err(RecoveryError::InvalidValue("geometry"))
    ));

    let mut overlapping = record();
    overlapping.sectors = vec![
        SectorBackup::new(10, vec![4; 1_024]),
        SectorBackup::new(11, vec![5; 512]),
    ];
    assert!(matches!(
        RecoveryRecord::deserialize(&overlapping.serialize()),
        Err(RecoveryError::InvalidValue("sector backup"))
    ));

    let mut boot = record();
    boot.sectors = vec![SectorBackup::new(0, vec![1; 1_024])];
    assert!(matches!(
        RecoveryRecord::deserialize(&boot.serialize()),
        Err(RecoveryError::InvalidValue("sector backup"))
    ));

    let mut past_end = record();
    past_end.sectors = vec![SectorBackup::new(5_022, vec![1; 512])];
    assert!(matches!(
        RecoveryRecord::deserialize(&past_end.serialize()),
        Err(RecoveryError::InvalidValue("sector backup"))
    ));

    let mut empty = record();
    empty.sectors.clear();
    let decoded = RecoveryRecord::deserialize(&empty.serialize()).unwrap();
    assert!(decoded.sectors.is_empty());
    assert_eq!(decoded.backed_up_sectors(), 0);
}

#[test]
fn prepare_refuses_existing_or_mismatched_records() {
    let volume = fat16_file();
    let plan = fatresize::resize::plan(&volume, 5_000 * 512).unwrap();
    let path = temp_path("prepare-twice");
    writer::prepare(&volume, &plan, &path).unwrap();
    assert_eq!(
        writer::prepare(&volume, &plan, &path).unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );

    let mut mismatched = writer::load_record(&path).unwrap();
    mismatched.source.volume_id ^= 1;
    std::fs::write(&path, mismatched.serialize()).unwrap();
    let error = writer::execute(&volume, &plan, &path, |_| {}).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("existing recovery record does not match")
    );

    std::fs::remove_file(path).unwrap();
}

#[test]
fn backup_then_restore_is_byte_exact() {
    let volume = fat16_high_file();
    let original = volume.snapshot();
    let plan = fatresize::resize::plan(&volume, 10_000 * 512).unwrap();
    let path = temp_path("backup-restore");
    writer::prepare(&volume, &plan, &path).unwrap();
    writer::restore(&volume, &path, false).unwrap();
    assert_eq!(volume.snapshot(), original);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn completed_restore_requires_force_and_restores_exact_bytes() {
    let volume = fat16_high_file();
    let original = volume.snapshot();
    let plan = fatresize::resize::plan(&volume, 5_000 * 512).unwrap();
    let path = temp_path("completed-restore");
    writer::execute(&volume, &plan, &path, |_| {}).unwrap();
    let resized = volume.snapshot();
    assert_ne!(resized, original);

    assert_eq!(
        writer::restore(&volume, &path, false).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(volume.snapshot(), resized);
    writer::restore(&volume, &path, true).unwrap();
    assert_eq!(volume.snapshot(), original);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn every_fat32_shrink_write_failure_is_recoverable() {
    let volume = fat32_high_file();
    let original = volume.snapshot();
    let plan = fatresize::resize::plan(&volume, 67_000 * 512).unwrap();

    let baseline = Mem::new(original.clone());
    let baseline_recovery = temp_path("fat32-fail-baseline");
    writer::execute(&baseline, &plan, &baseline_recovery, |_| {}).unwrap();
    let write_count = baseline.writes.borrow().len();
    assert!(write_count > 0);
    std::fs::remove_file(baseline_recovery).unwrap();

    for fail_at in 1..=write_count {
        let failed = Mem::failing(original.clone(), fail_at);
        let recovery = temp_path(&format!("fat32-fail-{fail_at}"));
        let error = writer::execute(&failed, &plan, &recovery, |_| {}).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other, "write {fail_at}");

        let restored = Mem::new(failed.snapshot());
        writer::restore(&restored, &recovery, false).unwrap();
        assert_eq!(
            restored.snapshot(),
            original,
            "restore after injected write {fail_at}"
        );
        std::fs::remove_file(recovery).unwrap();
    }
}

#[test]
fn four_k_sector_volume_shrinks_grows_and_keeps_content() {
    let spec = FatSpec::fat16(8_000)
        .with_sector_size(4096)
        .with_device_slack(2_000);
    let mut image = FatImage::new(spec);
    image.set_chain(&[2]);
    image.fill_cluster(2, 0x77);
    image.put_entry(None, 0, b"SMALL   BIN", 0x20, 2, 128);
    image.set_chain(&[7_900]);
    image.fill_cluster(7_900, 0x88);
    image.put_entry(None, 1, b"FAR     BIN", 0x20, 7_900, 256);
    let volume = image.into_memory();
    let before = inspect(&volume).unwrap();
    assert!(before.is_consistent(), "{:?}", before.issues);
    assert_eq!(before.bpb.bytes_per_sector, 4096);
    let small = read_file(&volume, "/SMALL.BIN");
    let far = read_file(&volume, "/FAR.BIN");

    let shrink = fatresize::resize::plan(&volume, 6_000 * 4096).unwrap();
    assert_eq!(shrink.target.bytes_per_sector, 4096);
    assert!(
        shrink
            .mappings
            .iter()
            .any(|mapping| mapping.source == 7_900 && mapping.target != 7_900)
    );
    let recovery = temp_path("plan-4k-shrink");
    writer::execute(&volume, &shrink, &recovery, |_| {}).unwrap();
    assert_eq!(inspect(&volume).unwrap().bpb.total_sectors, 6_000);
    assert_eq!(read_file(&volume, "/SMALL.BIN"), small);
    assert_eq!(read_file(&volume, "/FAR.BIN"), far);

    let grow = fatresize::resize::plan(&volume, 9_000 * 4096).unwrap();
    let grow_recovery = temp_path("plan-4k-grow");
    writer::execute(&volume, &grow, &grow_recovery, |_| {}).unwrap();
    assert_eq!(inspect(&volume).unwrap().bpb.total_sectors, 9_000);
    assert_eq!(read_file(&volume, "/SMALL.BIN"), small);
    assert_eq!(read_file(&volume, "/FAR.BIN"), far);

    std::fs::remove_file(recovery).unwrap();
    std::fs::remove_file(grow_recovery).unwrap();
}

#[test]
fn every_fat16_grow_write_failure_is_recoverable() {
    let spec = FatSpec::fat16(10_000).with_device_slack(2_000);
    let mut image = FatImage::new(spec);
    image.set_chain(&[2]);
    image.put_entry(None, 0, b"LOW     BIN", 0x20, 2, 1);
    image.set_chain(&[9_890]);
    image.fill_cluster(9_890, 0x99);
    image.put_entry(None, 1, b"LAST    BIN", 0x20, 9_890, 128);
    let volume = image.into_memory();
    let original = volume.snapshot();
    let source_prefix = original[..10_000 * 512].to_vec();
    let plan = fatresize::resize::plan(&volume, 12_000 * 512).unwrap();
    assert!(
        plan.moves.steps.iter().any(|step| matches!(
            step,
            fatresize::resize::MoveStep::Copy { target_offset, length, .. }
                if target_offset + u64::from(*length) > 10_000 * 512
        )),
        "grow fixture did not move data into the new tail"
    );

    let baseline = Mem::new(original.clone());
    let baseline_recovery = temp_path("fat16-grow-baseline");
    writer::execute(&baseline, &plan, &baseline_recovery, |_| {}).unwrap();
    let write_count = baseline.writes.borrow().len();
    std::fs::remove_file(baseline_recovery).unwrap();

    for fail_at in 1..=write_count {
        let failed = Mem::failing(original.clone(), fail_at);
        let recovery = temp_path(&format!("fat16-grow-fail-{fail_at}"));
        writer::execute(&failed, &plan, &recovery, |_| {}).unwrap_err();
        let restored = Mem::new(failed.snapshot());
        writer::restore(&restored, &recovery, false).unwrap();
        assert_eq!(
            &restored.snapshot()[..10_000 * 512],
            &source_prefix[..],
            "restore after grow write failure {fail_at}"
        );
        std::fs::remove_file(recovery).unwrap();
    }
}

#[test]
fn minimum_size_compacts_fragmented_data_and_is_actually_minimal() {
    let volume = fat16_high_file();
    let report = inspect(&volume).unwrap();
    let conservative = report.resize.minimum_bytes;
    let minimum = fatresize::resize::minimum_bytes(&volume).unwrap();
    assert!(minimum < conservative);
    assert_eq!(minimum % 512, 0);
    let plan = fatresize::resize::plan(&volume, minimum).unwrap();
    assert!(plan.target.cluster_count >= report.allocated_clusters);
    assert!(
        fatresize::resize::plan(&volume, minimum - 512).is_err(),
        "minimum target was not actually minimal"
    );

    let high = read_file(&volume, "/HIGH.BIN");
    let low = read_file(&volume, "/LOW.BIN");
    let recovery = temp_path("plan-min-fat16");
    writer::execute(&volume, &plan, &recovery, |_| {}).unwrap();
    let after = inspect(&volume).unwrap();
    assert!(after.is_consistent(), "{:?}", after.issues);
    assert_eq!(after.layout.total_bytes, minimum);
    assert_eq!(read_file(&volume, "/HIGH.BIN"), high);
    assert_eq!(read_file(&volume, "/LOW.BIN"), low);
    std::fs::remove_file(recovery).unwrap();
}

#[test]
fn minimum_size_for_fat32_and_inconsistent_sources() {
    let volume = fat32_high_file();
    let report = inspect(&volume).unwrap();
    let minimum = fatresize::resize::minimum_bytes(&volume).unwrap();
    assert!(minimum < report.resize.current_bytes);
    assert!(minimum >= 65_525 * 512);
    let high = read_file(&volume, "/HIGH.BIN");
    let plan = fatresize::resize::plan(&volume, minimum).unwrap();
    assert_eq!(plan.target.fat_type, FatType::Fat32);
    let recovery = temp_path("plan-min-fat32");
    writer::execute(&volume, &plan, &recovery, |_| {}).unwrap();
    assert!(inspect(&volume).unwrap().is_consistent());
    assert_eq!(read_file(&volume, "/HIGH.BIN"), high);
    std::fs::remove_file(recovery).unwrap();

    let mut bad = FatImage::new(FatSpec::fat16(10_000));
    bad.set_chain(&[2]);
    bad.put_entry(None, 0, b"BAD     BIN", 0x20, 2, 1);
    bad.set_fat_entry(2, 0xfff7);
    assert!(matches!(
        fatresize::resize::minimum_bytes(&bad.into_memory()),
        Err(ResizeError::InconsistentSource(_))
    ));
}

#[test]
fn stale_plan_and_wrong_volume_are_refused() {
    let volume = fat16_high_file();
    let plan = fatresize::resize::plan(&volume, 5_000 * 512).unwrap();
    let recovery = temp_path("stale-and-wrong");
    writer::prepare(&volume, &plan, &recovery).unwrap();

    let mut other = volume.snapshot();
    other[43..54].copy_from_slice(b"OTHERDISK  ");
    let other = Mem::new(other);
    let error = writer::restore(&other, &recovery, false).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("selected volume does not match recovery source")
    );
    assert!(other.writes.borrow().is_empty());

    let mut changed = volume.snapshot();
    changed[39..43].copy_from_slice(&0xdead_beef_u32.to_le_bytes());
    let changed = Mem::new(changed);
    let error = writer::execute(&changed, &plan, &recovery, |_| {}).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("source filesystem changed after planning")
            || error
                .to_string()
                .contains("source filesystem is not safe to resize")
    );
    assert!(changed.writes.borrow().is_empty());

    std::fs::remove_file(recovery).unwrap();
}
