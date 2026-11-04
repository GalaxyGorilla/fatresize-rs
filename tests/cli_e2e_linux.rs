#![cfg(target_os = "linux")]

//! End-to-end CLI coverage. Writable operations are Linux-only by design.

use std::ffi::OsStr;
use std::process::{Command, Output};

use fatresize::{Gpt, Mbr, inspect_path};

mod support;
use support::{FatImage, FatSpec, Mem, gpt_disk, mbr_disk, temp_path};

const BIN: &str = env!("CARGO_BIN_EXE_fatresize");

fn run(arguments: &[&str]) -> Output {
    Command::new(BIN).args(arguments).output().unwrap()
}

fn run_os(arguments: &[&OsStr]) -> Output {
    Command::new(BIN).args(arguments).output().unwrap()
}

fn json_u64(json: &str, key: &str) -> u64 {
    let needle = format!("\"{key}\":");
    let start = json
        .find(&needle)
        .unwrap_or_else(|| panic!("missing {key}"))
        + needle.len();
    let end = json[start..]
        .find(|character: char| !character.is_ascii_digit())
        .map(|offset| start + offset)
        .unwrap_or(json.len());
    json[start..end].parse().unwrap()
}

fn fat16_bytes(total_sectors: u32, high: bool) -> Vec<u8> {
    let mut image = FatImage::new(FatSpec::fat16(total_sectors));
    image.set_chain(&[2]);
    image.fill_cluster(2, 0x11);
    image.put_entry(None, 0, b"LOW     BIN", 0x20, 2, 300);
    if high {
        image.set_chain(&[9_000]);
        image.fill_cluster(9_000, 0x22);
        image.put_entry(None, 1, b"HIGH    BIN", 0x20, 9_000, 200);
    }
    image.into_bytes()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "status {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_failure(output: &Output) {
    assert!(
        !output.status.success(),
        "unexpected success\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn info_check_and_dry_run_are_read_only_and_json_is_well_formed() {
    let path = temp_path("cli-readonly.img");
    let original = fat16_bytes(10_000, true);
    std::fs::write(&path, &original).unwrap();
    let path = path.to_str().unwrap();

    for arguments in [
        vec!["--info", path],
        vec!["--info", "--json", path],
        vec!["--check", path],
        vec!["--check", "--json", path],
        vec!["--dry-run", "--size", "2560000", path],
        vec!["--dry-run", "--size", "2560000", "--json", path],
        vec!["--dry-run", "--size", "max", "--json", path],
    ] {
        let output = run(&arguments);
        assert_success(&output);
        assert_eq!(std::fs::read(path).unwrap(), original);
    }
    let output = run(&["--info", "--json", path]);
    let json = String::from_utf8(output.stdout).unwrap();
    assert!(json.starts_with('{'));
    assert!(json.trim_end().ends_with('}'));
    assert!(json.contains("\"fat_type\":\"FAT16\""));
    assert!(
        !json
            .chars()
            .any(|c| c < ' ' && !matches!(c, '\n' | '\r' | '\t'))
    );

    let broken = temp_path("cli-broken.img");
    let mut bytes = original.clone();
    bytes[(1 + 39) * 512 + 4] ^= 1;
    std::fs::write(&broken, bytes).unwrap();
    let output = run(&["--check", broken.to_str().unwrap()]);
    assert_failure(&output);
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("Issue:"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_success(&run(&["--info", broken.to_str().unwrap()]));

    std::fs::remove_file(path).unwrap();
    std::fs::remove_file(broken).unwrap();
}

#[test]
fn resize_with_default_recovery_then_restore_is_exact() {
    let path = temp_path("cli-default-recovery.img");
    let original = fat16_bytes(10_000, true);
    std::fs::write(&path, &original).unwrap();
    let recovery = fatresize::default_recovery_path(&path);
    let output = run_os(&[
        OsStr::new("--force-yes"),
        OsStr::new("--size"),
        OsStr::new("2560000"),
        path.as_os_str(),
    ]);
    assert_success(&output);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 2_560_000);
    assert!(recovery.exists());
    assert_success(&run_os(&[OsStr::new("--check"), path.as_os_str()]));
    let device = fatresize::Device::open_read_only(&path).unwrap();
    assert_eq!(
        support::read_file(&device, "/HIGH.BIN"),
        (0..200)
            .map(|index| 0x22_u8.wrapping_add((index % 251) as u8))
            .collect::<Vec<_>>()
    );
    drop(device);

    let resized = std::fs::read(&path).unwrap();
    assert_failure(&run_os(&[
        OsStr::new("--restore"),
        recovery.as_os_str(),
        path.as_os_str(),
    ]));
    assert_eq!(std::fs::read(&path).unwrap(), resized);
    assert_success(&run_os(&[
        OsStr::new("--restore"),
        recovery.as_os_str(),
        OsStr::new("--force-yes"),
        path.as_os_str(),
    ]));
    assert_eq!(std::fs::read(&path).unwrap(), original);

    std::fs::remove_file(&path).unwrap();
    std::fs::remove_file(&recovery).unwrap();
}

#[test]
fn resize_growing_beyond_the_image_is_recoverable() {
    let path = temp_path("cli-grow.img");
    let original = fat16_bytes(10_000, true);
    std::fs::write(&path, &original).unwrap();
    let recovery = temp_path("cli-grow.rec");
    assert_success(&run(&[
        "--force-yes",
        "--size",
        "6144000",
        "--backup",
        recovery.to_str().unwrap(),
        path.to_str().unwrap(),
    ]));
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 6_144_000);
    assert_success(&run(&["--check", path.to_str().unwrap()]));
    let grown = fatresize::Device::open_read_only(&path).unwrap();
    assert_eq!(
        support::read_file(&grown, "/HIGH.BIN"),
        (0..200)
            .map(|index| 0x22_u8.wrapping_add((index % 251) as u8))
            .collect::<Vec<_>>()
    );
    drop(grown);

    assert_success(&run(&[
        "--restore",
        recovery.to_str().unwrap(),
        "--force-yes",
        path.to_str().unwrap(),
    ]));
    assert_eq!(std::fs::read(&path).unwrap(), original);
    std::fs::remove_file(&path).unwrap();
    std::fs::remove_file(&recovery).unwrap();
}

#[test]
fn quiet_verbose_and_progress_flag_behaviour() {
    let path = temp_path("cli-flags.img");
    std::fs::write(&path, fat16_bytes(10_000, false)).unwrap();
    let recovery = temp_path("cli-flags.rec");

    let output = run(&[
        "--quiet",
        "--force-yes",
        "--size",
        "2560000",
        "--backup",
        recovery.to_str().unwrap(),
        path.to_str().unwrap(),
    ]);
    assert_success(&output);
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());

    let path2 = temp_path("cli-flags2.img");
    std::fs::write(&path2, fat16_bytes(10_000, false)).unwrap();
    let recovery2 = temp_path("cli-flags2.rec");
    let output = run(&[
        "--force-yes",
        "--verbose",
        "--progress",
        "--size",
        "2560000",
        "--backup",
        recovery2.to_str().unwrap(),
        path2.to_str().unwrap(),
    ]);
    assert_success(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Recovery record:"), "{stderr}");
    assert!(stderr.contains("Recovery"), "{stderr}");
    assert!(stderr.contains("Complete"), "{stderr}");

    std::fs::remove_file(path).unwrap();
    std::fs::remove_file(recovery).unwrap();
    std::fs::remove_file(path2).unwrap();
    std::fs::remove_file(recovery2).unwrap();
}

#[test]
fn standalone_backup_is_restorable_without_changing_the_volume() {
    let path = temp_path("cli-backup.img");
    let original = fat16_bytes(10_000, true);
    std::fs::write(&path, &original).unwrap();
    let recovery = temp_path("cli-backup.rec");
    assert_success(&run(&[
        "--backup",
        recovery.to_str().unwrap(),
        path.to_str().unwrap(),
    ]));
    assert!(recovery.exists());
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert_success(&run(&[
        "--restore",
        recovery.to_str().unwrap(),
        "--force-yes",
        path.to_str().unwrap(),
    ]));
    assert_eq!(std::fs::read(&path).unwrap(), original);
    std::fs::remove_file(path).unwrap();
    std::fs::remove_file(recovery).unwrap();
}

#[test]
fn mbr_partition_grow_shrink_and_restore_via_cli() {
    let path = temp_path("cli-mbr.img");
    let volume = fat16_bytes(10_000, true);
    let original = mbr_disk(&volume, 1, 12_000, 0x06);
    std::fs::write(&path, &original).unwrap();
    let grow = temp_path("cli-mbr-grow.rec");
    let shrink = temp_path("cli-mbr-shrink.rec");

    assert_success(&run(&[
        "--force-yes",
        "--size",
        "5632000",
        "--partition",
        "1",
        "--backup",
        grow.to_str().unwrap(),
        path.to_str().unwrap(),
    ]));
    let disk = std::fs::read(&path).unwrap();
    let mbr = Mbr::read(&Mem::new(disk)).unwrap();
    assert_eq!(mbr.partition(1).unwrap().sectors, 11_000);
    assert_success(&run(&[
        "--check",
        "--partition",
        "1",
        path.to_str().unwrap(),
    ]));

    assert_success(&run(&[
        "--force-yes",
        "--size",
        "4096000",
        "--partition",
        "1",
        "--backup",
        shrink.to_str().unwrap(),
        path.to_str().unwrap(),
    ]));
    let disk = std::fs::read(&path).unwrap();
    let mbr = Mbr::read(&Mem::new(disk)).unwrap();
    assert_eq!(mbr.partition(1).unwrap().sectors, 8_000);
    assert_success(&run(&[
        "--check",
        "--partition",
        "1",
        path.to_str().unwrap(),
    ]));
    assert!(inspect_path(&path, Some(1)).unwrap().is_consistent());

    assert_success(&run(&[
        "--restore",
        shrink.to_str().unwrap(),
        "--force-yes",
        "--partition",
        "1",
        path.to_str().unwrap(),
    ]));
    let disk = std::fs::read(&path).unwrap();
    let mbr = Mbr::read(&Mem::new(disk)).unwrap();
    assert_eq!(mbr.partition(1).unwrap().sectors, 11_000);

    std::fs::remove_file(path).unwrap();
    for recovery in [grow, shrink] {
        std::fs::remove_file(recovery).unwrap();
    }
}

#[test]
fn gpt_partition_grow_shrink_and_restore_via_cli() {
    let path = temp_path("cli-gpt.img");
    let volume = fat16_bytes(10_000, true);
    let original = gpt_disk(&volume, 40, 12_000, 512);
    std::fs::write(&path, &original).unwrap();
    let grow = temp_path("cli-gpt-grow.rec");
    let shrink = temp_path("cli-gpt-shrink.rec");

    assert_success(&run(&[
        "--force-yes",
        "--size",
        "5632000",
        "--partition",
        "1",
        "--backup",
        grow.to_str().unwrap(),
        path.to_str().unwrap(),
    ]));
    let disk = std::fs::read(&path).unwrap();
    let gpt = Gpt::read(&Mem::new(disk)).unwrap();
    assert_eq!(gpt.partition(1).unwrap().last_lba, 40 + 11_000 - 1);
    assert_success(&run(&[
        "--check",
        "--partition",
        "1",
        path.to_str().unwrap(),
    ]));

    assert_success(&run(&[
        "--force-yes",
        "--size",
        "4096000",
        "--partition",
        "1",
        "--backup",
        shrink.to_str().unwrap(),
        path.to_str().unwrap(),
    ]));
    let disk = std::fs::read(&path).unwrap();
    let gpt = Gpt::read(&Mem::new(disk)).unwrap();
    assert_eq!(gpt.partition(1).unwrap().last_lba, 40 + 8_000 - 1);
    assert!(inspect_path(&path, Some(1)).unwrap().is_consistent());

    assert_success(&run(&[
        "--restore",
        shrink.to_str().unwrap(),
        "--force-yes",
        "--partition",
        "1",
        path.to_str().unwrap(),
    ]));
    let disk = std::fs::read(&path).unwrap();
    let gpt = Gpt::read(&Mem::new(disk)).unwrap();
    assert_eq!(gpt.partition(1).unwrap().last_lba, 40 + 11_000 - 1);

    std::fs::remove_file(path).unwrap();
    for recovery in [grow, shrink] {
        std::fs::remove_file(recovery).unwrap();
    }
}

#[test]
fn size_min_shrinks_to_the_compacted_geometry() {
    let path = temp_path("cli-min.img");
    let original = fat16_bytes(10_000, true);
    std::fs::write(&path, &original).unwrap();
    let output = run(&[
        "--dry-run",
        "--size",
        "min",
        "--json",
        path.to_str().unwrap(),
    ]);
    assert_success(&output);
    let json = String::from_utf8(output.stdout).unwrap();
    let minimum = json_u64(&json, "normalized_bytes");
    assert!(minimum > 0 && minimum < 5_120_000, "{json}");
    assert_eq!(minimum % 512, 0);

    let recovery = temp_path("cli-min.rec");
    assert_success(&run(&[
        "--force-yes",
        "--size",
        "min",
        "--backup",
        recovery.to_str().unwrap(),
        path.to_str().unwrap(),
    ]));
    assert_eq!(std::fs::metadata(&path).unwrap().len(), minimum);
    assert_success(&run(&["--check", path.to_str().unwrap()]));
    let device = fatresize::Device::open_read_only(&path).unwrap();
    assert_eq!(
        support::read_file(&device, "/HIGH.BIN"),
        (0..200)
            .map(|index| 0x22_u8.wrapping_add((index % 251) as u8))
            .collect::<Vec<_>>()
    );
    drop(device);
    std::fs::remove_file(&path).unwrap();
    std::fs::remove_file(&recovery).unwrap();
}

#[test]
fn size_min_respects_partition_alignment_and_updates_the_table() {
    let path = temp_path("cli-min-mbr.img");
    let volume = fat16_bytes(10_000, true);
    std::fs::write(&path, mbr_disk(&volume, 1, 12_000, 0x06)).unwrap();
    let output = run(&[
        "--dry-run",
        "--size",
        "min",
        "--partition",
        "1",
        "--json",
        path.to_str().unwrap(),
    ]);
    assert_success(&output);
    let json = String::from_utf8(output.stdout).unwrap();
    let minimum = json_u64(&json, "normalized_bytes");
    assert_eq!(minimum % 512, 0);
    assert!(minimum < 10_000 * 512);

    let recovery = temp_path("cli-min-mbr.rec");
    assert_success(&run(&[
        "--force-yes",
        "--size",
        "min",
        "--partition",
        "1",
        "--backup",
        recovery.to_str().unwrap(),
        path.to_str().unwrap(),
    ]));
    let disk = std::fs::read(&path).unwrap();
    let mbr = Mbr::read(&Mem::new(disk)).unwrap();
    assert_eq!(u64::from(mbr.partition(1).unwrap().sectors) * 512, minimum);
    assert_success(&run(&[
        "--check",
        "--partition",
        "1",
        path.to_str().unwrap(),
    ]));
    std::fs::remove_file(&path).unwrap();
    std::fs::remove_file(&recovery).unwrap();
}

#[test]
fn failed_commands_leave_the_image_untouched() {
    let path = temp_path("cli-failures.img");
    let original = fat16_bytes(10_000, true);
    std::fs::write(&path, &original).unwrap();
    let recovery = temp_path("cli-failures.rec");

    for arguments in [
        vec!["--dry-run", "--size", "2560001", path.to_str().unwrap()],
        vec!["--dry-run", "--size", "1MiB", path.to_str().unwrap()],
        vec![
            "--force-yes",
            "--size",
            "2560000",
            "--partition",
            "1",
            path.to_str().unwrap(),
        ],
        vec![
            "--force-yes",
            "--size",
            "2560001",
            "--backup",
            recovery.to_str().unwrap(),
            path.to_str().unwrap(),
        ],
        vec![
            "--force-yes",
            "--size",
            "10000000",
            "--backup",
            recovery.to_str().unwrap(),
            path.to_str().unwrap(),
        ],
    ] {
        let output = run(&arguments);
        assert_failure(&output);
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert!(!recovery.exists());
    }

    std::fs::write(&recovery, b"occupied").unwrap();
    let output = run(&[
        "--force-yes",
        "--size",
        "2560000",
        "--backup",
        recovery.to_str().unwrap(),
        path.to_str().unwrap(),
    ]);
    assert_failure(&output);
    assert_eq!(std::fs::read(&path).unwrap(), original);

    std::fs::remove_file(path).unwrap();
    std::fs::remove_file(recovery).unwrap();
}

#[test]
fn max_size_for_raw_and_partition_targets() {
    let path = temp_path("cli-max.img");
    let volume = fat16_bytes(10_000, false);
    std::fs::write(&path, &volume).unwrap();
    let output = run(&[
        "--dry-run",
        "--size",
        "max",
        "--json",
        path.to_str().unwrap(),
    ]);
    assert_success(&output);
    let json = String::from_utf8(output.stdout).unwrap();
    assert!(json.contains("\"normalized_bytes\":5120000"), "{json}");

    let disk_path = temp_path("cli-max-mbr.img");
    let mut disk = mbr_disk(&volume, 1, 20_000, 0x06);
    disk[446 + 16 + 4] = 0x06;
    disk[446 + 16 + 8..446 + 16 + 12].copy_from_slice(&12_000_u32.to_le_bytes());
    disk[446 + 16 + 12..446 + 16 + 16].copy_from_slice(&8_000_u32.to_le_bytes());
    std::fs::write(&disk_path, disk).unwrap();
    let output = run(&[
        "--dry-run",
        "--size",
        "max",
        "--partition",
        "1",
        "--json",
        disk_path.to_str().unwrap(),
    ]);
    assert_success(&output);
    let json = String::from_utf8(output.stdout).unwrap();
    assert!(
        json.contains(&format!("\"normalized_bytes\":{}", (12_000 - 1) * 512)),
        "{json}"
    );

    std::fs::remove_file(path).unwrap();
    std::fs::remove_file(disk_path).unwrap();
}

#[test]
fn json_escapes_volume_labels() {
    let mut spec = FatSpec::fat16(10_000);
    spec.label = *b"A\"B\\C\tD    ";
    let mut image = FatImage::new(spec);
    image.set_chain(&[2]);
    image.put_entry(None, 0, b"DATA    BIN", 0x20, 2, 1);
    let path = temp_path("cli-json-escape.img");
    std::fs::write(&path, image.into_bytes()).unwrap();
    let output = run(&["--info", "--json", path.to_str().unwrap()]);
    assert_success(&output);
    let json = String::from_utf8(output.stdout).unwrap();
    assert!(json.contains("A\\\"B\\\\C\\tD"), "{json}");
    assert!(!json.contains("\n\n"));
    std::fs::remove_file(path).unwrap();
}
