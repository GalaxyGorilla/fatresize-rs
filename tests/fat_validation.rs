use fatresize::{Bpb, FatType, inspect, read_metadata, summarize};

mod support;
use support::{FatImage, FatSpec, Mem};

fn base16() -> Mem {
    FatImage::new(FatSpec::fat16(10_000)).into_memory()
}

fn base32() -> Mem {
    FatImage::new(FatSpec::fat32(70_000)).into_memory()
}

fn fat16_with_file() -> FatImage {
    let mut image = FatImage::new(FatSpec::fat16(10_000));
    image.set_chain(&[2]);
    image.fill_cluster(2, 0x11);
    image.put_entry(None, 0, b"HELLO   TXT", 0x20, 2, 300);
    image
}

fn fat32_with_file() -> FatImage {
    let mut image = FatImage::new(FatSpec::fat32(70_000));
    image.set_chain(&[3]);
    image.fill_cluster(3, 0x22);
    image.put_entry(None, 0, b"HELLO   TXT", 0x20, 3, 300);
    image
}

fn bpb_error(image: &Mem, offset: usize, value: &[u8], label: &str) {
    let mut bytes = image.snapshot();
    bytes[offset..offset + value.len()].copy_from_slice(value);
    assert!(
        Bpb::read(&Mem::new(bytes)).is_err(),
        "boot parser accepted mutation: {label}"
    );
}

fn issues(image: &Mem) -> Vec<String> {
    inspect(image).unwrap().issues
}

fn assert_issue(image: &Mem, needle: &str) {
    let issues = issues(image);
    assert!(
        issues.iter().any(|issue| issue.contains(needle)),
        "expected issue containing {needle:?}, got {issues:?}"
    );
}

#[test]
fn parses_valid_fat16_and_fat32_geometry() {
    let fat16 = base16();
    let (bpb, layout, _) = Bpb::read(&fat16).unwrap();
    assert_eq!(bpb.fat_type, FatType::Fat16);
    assert_eq!(bpb.bytes_per_sector, 512);
    assert_eq!(bpb.sectors_per_cluster, 1);
    assert_eq!(bpb.reserved_sectors, 1);
    assert_eq!(bpb.fat_count, 2);
    assert_eq!(bpb.root_entry_count, 512);
    assert_eq!(bpb.volume_id, 0x1234_5678);
    assert_eq!(bpb.volume_label, "TESTVOL");
    assert_eq!(layout.first_root_sector, 1 + 2 * bpb.sectors_per_fat);
    assert_eq!(layout.first_data_sector, layout.first_root_sector + 32);
    assert_eq!(layout.cluster_count, 10_000 - layout.first_data_sector);
    assert!(layout.cluster_count >= 4_085);
    assert!(layout.cluster_count <= 65_524);

    let fat32 = base32();
    let (bpb, layout, _) = Bpb::read(&fat32).unwrap();
    assert_eq!(bpb.fat_type, FatType::Fat32);
    assert_eq!(bpb.reserved_sectors, 32);
    assert_eq!(bpb.root_cluster, 2);
    assert_eq!(bpb.fs_info_sector, Some(1));
    assert_eq!(bpb.backup_boot_sector, Some(6));
    assert!(bpb.mirroring_enabled);
    assert_eq!(bpb.active_fat, 0);
    assert_eq!(bpb.volume_id, 0xaabb_ccdd);
    assert!(layout.cluster_count > 65_524);
    assert_eq!(layout.first_data_sector, 32 + 2 * bpb.sectors_per_fat);
}

#[test]
fn parses_supported_fat_sector_sizes() {
    for sector_size in [512_u16, 1024, 2048, 4096] {
        let mut spec = FatSpec::fat16(20_000)
            .with_sector_size(sector_size)
            .with_spc(1);
        spec.sectors_per_fat = None;
        let image = FatImage::new(spec).into_memory();
        let (bpb, layout, _) = Bpb::read(&image).unwrap();
        assert_eq!(bpb.bytes_per_sector, sector_size);
        assert_eq!(layout.cluster_size, u32::from(sector_size));
        assert!(inspect(&image).unwrap().is_consistent());
    }
}

#[test]
fn boot_parser_rejects_invalid_geometry_and_metadata() {
    let fat16 = base16();
    let fat32 = base32();

    bpb_error(&fat16, 11, &0_u16.to_le_bytes(), "zero sector size");
    bpb_error(&fat16, 11, &256_u16.to_le_bytes(), "256-byte sectors");
    bpb_error(&fat16, 11, &8192_u16.to_le_bytes(), "8192-byte sectors");
    bpb_error(&fat16, 13, &[0], "zero sectors per cluster");
    bpb_error(&fat16, 13, &[3], "non-power-of-two sectors per cluster");
    let too_large = Mem::new(fat16.snapshot());
    too_large.bytes.borrow_mut()[11..13].copy_from_slice(&4096_u16.to_le_bytes());
    too_large.bytes.borrow_mut()[13] = 64;
    assert!(Bpb::read(&too_large).is_err(), "128KiB cluster boundary");

    bpb_error(&fat16, 14, &0_u16.to_le_bytes(), "zero reserved sectors");
    bpb_error(&fat16, 16, &[0], "zero FAT copies");
    bpb_error(&fat16, 16, &[9], "too many FAT copies");
    for media in [0_u8, 0xf7] {
        bpb_error(&fat16, 21, &[media], "invalid media descriptor");
    }
    bpb_error(&fat16, 19, &0_u16.to_le_bytes(), "zero total sectors");
    bpb_error(&fat16, 510, &[0], "missing boot signature");
    bpb_error(&fat16, 0, &[0, 0, 0], "invalid boot jump");

    let mut both = fat16.snapshot();
    both[32..36].copy_from_slice(&10_000_u32.to_le_bytes());
    assert!(Bpb::read(&Mem::new(both)).is_err(), "both total fields set");

    bpb_error(&fat16, 22, &0_u16.to_le_bytes(), "zero FAT16 size");
    let mut truncated = fat16.snapshot();
    truncated.truncate(9_999 * 512);
    assert!(
        Bpb::read(&Mem::new(truncated)).is_err(),
        "volume beyond device"
    );

    let mut tiny = fat16.snapshot();
    tiny[19..21].copy_from_slice(&100_u16.to_le_bytes());
    assert!(Bpb::read(&Mem::new(tiny)).is_err(), "FAT12 volume");
    let mut consumed = fat16.snapshot();
    consumed[19..21].copy_from_slice(&30_u16.to_le_bytes());
    assert!(
        Bpb::read(&Mem::new(consumed)).is_err(),
        "metadata consumes volume"
    );

    let mut no_root = fat16.snapshot();
    no_root[17..19].copy_from_slice(&0_u16.to_le_bytes());
    assert!(Bpb::read(&Mem::new(no_root)).is_err(), "FAT16 without root");

    let mut small_fat = fat16.snapshot();
    small_fat[22..24].copy_from_slice(&1_u16.to_le_bytes());
    assert!(Bpb::read(&Mem::new(small_fat)).is_err(), "undersized FAT16");

    for (offset, value, label) in [
        (22, &1_u16.to_le_bytes()[..], "FAT16 size on FAT32"),
        (17, &1_u16.to_le_bytes()[..], "root entries on FAT32"),
        (14, &1_u16.to_le_bytes()[..], "FAT32 reserved below two"),
        (42, &1_u16.to_le_bytes()[..], "FAT32 version"),
        (40, &0x0070_u16.to_le_bytes()[..], "FAT32 extended flags"),
        (44, &0_u32.to_le_bytes()[..], "root cluster zero"),
        (44, &1_u32.to_le_bytes()[..], "root cluster one"),
        (
            44,
            &9_999_999_u32.to_le_bytes()[..],
            "root cluster out of range",
        ),
        (48, &32_u16.to_le_bytes()[..], "FSInfo beyond reserved"),
        (50, &32_u16.to_le_bytes()[..], "backup boot beyond reserved"),
    ] {
        bpb_error(&fat32, offset, value, label);
    }
    bpb_error(&fat32, 36, &1_u32.to_le_bytes(), "undersized FAT32");

    let mut active = fat32.snapshot();
    active[40..42].copy_from_slice(&(0x80_u16 | 2).to_le_bytes());
    assert!(Bpb::read(&Mem::new(active)).is_err(), "active FAT index");

    let mut total16 = fat32.snapshot();
    total16[19..21].copy_from_slice(&1_u16.to_le_bytes());
    assert!(
        Bpb::read(&Mem::new(total16)).is_err(),
        "both total fields on FAT32"
    );

    // 0xffff means "absent" for FSInfo and backup boot and is accepted.
    let mut absent = fat32.snapshot();
    absent[48..50].copy_from_slice(&0xffff_u16.to_le_bytes());
    absent[50..52].copy_from_slice(&0xffff_u16.to_le_bytes());
    assert!(Bpb::read(&Mem::new(absent)).is_ok());
}

#[test]
fn reports_structural_filesystem_issues() {
    let mut image = fat16_with_file();
    image.finalize();
    let valid = image.into_memory();
    let report = inspect(&valid).unwrap();
    assert!(report.is_consistent(), "{:?}", report.issues);
    assert_eq!(report.allocated_clusters, 1);
    assert_eq!(report.directories_and_files[0].path, "/HELLO.TXT");
    assert_eq!(
        report.resize.minimum_bytes,
        u64::from(report.layout.first_data_sector + 1) * 512
    );

    let mut mirror = fat16_with_file().into_bytes();
    mirror[(1 + 39) * 512 + 10] ^= 1;
    assert_issue(&Mem::new(mirror), "FAT copies");

    let backup = fat32_with_file().into_memory();
    backup.bytes.borrow_mut()[6 * 512 + 3] ^= 1;
    assert_issue(&backup, "backup boot");

    let fsinfo_signature = fat32_with_file().into_memory();
    fsinfo_signature.bytes.borrow_mut()[512 + 1] ^= 1;
    assert_issue(&fsinfo_signature, "FSInfo signatures");

    let fsinfo_free = fat32_with_file().into_memory();
    fsinfo_free.bytes.borrow_mut()[512 + 488..512 + 492]
        .copy_from_slice(&0xffff_fffe_u32.to_le_bytes());
    assert_issue(&fsinfo_free, "FSInfo free count is out of range");

    let fsinfo_next = fat32_with_file().into_memory();
    fsinfo_next.bytes.borrow_mut()[512 + 492..512 + 496]
        .copy_from_slice(&9_999_999_u32.to_le_bytes());
    assert_issue(&fsinfo_next, "next-free hint is out of range");

    let fsinfo_count = fat32_with_file().into_memory();
    fsinfo_count.bytes.borrow_mut()[512 + 488..512 + 492].copy_from_slice(&0_u32.to_le_bytes());
    assert_issue(&fsinfo_count, "FSInfo free count");

    let mut reserved0 = fat16_with_file().into_bytes();
    reserved0[512..514].copy_from_slice(&0_u16.to_le_bytes());
    assert_issue(&Mem::new(reserved0), "reserved entry 0");

    let mut reserved1 = fat16_with_file().into_bytes();
    reserved1[514..516].copy_from_slice(&0_u16.to_le_bytes());
    assert_issue(&Mem::new(reserved1), "reserved entry 1");

    let mut reserved_value = fat16_with_file().into_bytes();
    reserved_value[512 + 5 * 2..512 + 5 * 2 + 2].copy_from_slice(&0xfff0_u16.to_le_bytes());
    assert_issue(&Mem::new(reserved_value), "reserved FAT value");

    let mut unclaimed = fat16_with_file().into_bytes();
    unclaimed[512 + 5 * 2..512 + 5 * 2 + 2].copy_from_slice(&0xffff_u16.to_le_bytes());
    assert_issue(&Mem::new(unclaimed), "not referenced");

    let mut cycle = fat16_with_file().into_bytes();
    cycle[512 + 2 * 2..512 + 2 * 2 + 2].copy_from_slice(&2_u16.to_le_bytes());
    assert_issue(&Mem::new(cycle), "cycle");

    let mut bad = fat16_with_file().into_bytes();
    bad[512 + 2 * 2..512 + 2 * 2 + 2].copy_from_slice(&0xfff7_u16.to_le_bytes());
    assert_issue(&Mem::new(bad), "bad cluster");

    let mut free = fat16_with_file().into_bytes();
    free[512 + 2 * 2..512 + 2 * 2 + 2].copy_from_slice(&0_u16.to_le_bytes());
    assert_issue(&Mem::new(free), "free cluster");

    let mut out_of_range = fat16_with_file().into_bytes();
    out_of_range[512 + 2 * 2..512 + 2 * 2 + 2].copy_from_slice(&60_000_u16.to_le_bytes());
    assert_issue(&Mem::new(out_of_range), "out of range");
}

#[test]
fn reports_directory_and_lfn_issues() {
    let mut cross = FatImage::new(FatSpec::fat16(10_000));
    cross.set_chain(&[2]);
    cross.put_entry(None, 0, b"FIRST   TXT", 0x20, 2, 1);
    cross.put_entry(None, 1, b"SECOND  TXT", 0x20, 2, 1);
    assert_issue(&cross.into_memory(), "cross-linked");

    let mut directory_cycle = FatImage::new(FatSpec::fat16(10_000));
    directory_cycle.set_chain(&[3]);
    directory_cycle.put_entry(None, 0, b"FIRSTDIR   ", 0x10, 3, 0);
    directory_cycle.put_entry(None, 1, b"SECONDDIR  ", 0x10, 3, 0);
    assert_issue(&directory_cycle.into_memory(), "cycle or repeated");

    let mut invalid_directory = FatImage::new(FatSpec::fat16(10_000));
    invalid_directory.set_chain(&[3]);
    invalid_directory.set_fat_entry(3, 0xfff7);
    invalid_directory.put_entry(None, 0, b"BROKENDIR  ", 0x10, 3, 0);
    assert_issue(&invalid_directory.into_memory(), "invalid directory chain");

    let mut oversized = FatImage::new(FatSpec::fat16(10_000));
    oversized.set_chain(&[2]);
    oversized.put_entry(None, 0, b"BIG     TXT", 0x20, 2, 1_000);
    assert_issue(&oversized.into_memory(), "larger than its cluster chain");

    let mut no_cluster = FatImage::new(FatSpec::fat16(10_000));
    no_cluster.put_entry(None, 0, b"EMPTY   TXT", 0x20, 0, 5);
    assert_issue(&no_cluster.into_memory(), "has no first cluster");

    let mut nonzero_directory = FatImage::new(FatSpec::fat16(10_000));
    nonzero_directory.set_chain(&[3]);
    nonzero_directory.put_entry(None, 0, b"SIZEDIR    ", 0x10, 3, 7);
    assert_issue(&nonzero_directory.into_memory(), "nonzero file size");

    let mut no_directory_cluster = FatImage::new(FatSpec::fat16(10_000));
    no_directory_cluster.put_entry(None, 0, b"EMPTYDIR   ", 0x10, 0, 0);
    assert_issue(
        &no_directory_cluster.into_memory(),
        "no valid first cluster",
    );

    let mut bad_lfn = FatImage::new(FatSpec::fat16(10_000));
    bad_lfn.set_chain(&[2]);
    let short = *b"LONGNA~1TXT";
    let next = bad_lfn.put_lfn(None, 0, "Long Name.txt", &short);
    bad_lfn.put_entry(None, next, &short, 0x20, 2, 10);
    let lfn_checksum = bad_lfn.root_offset() + 13;
    bad_lfn.bytes[lfn_checksum] ^= 1;
    assert_issue(&bad_lfn.into_memory(), "invalid long filename");

    let mut high_bit = FatImage::new(FatSpec::fat16(10_000));
    high_bit.set_chain(&[2]);
    let next = high_bit.put_lfn(None, 0, "Long Name.txt", &short);
    high_bit.put_entry(None, next, &short, 0x20, 2, 10);
    let first_lfn = high_bit.root_offset();
    high_bit.bytes[first_lfn] |= 0x80;
    assert_issue(&high_bit.into_memory(), "invalid long filename");

    let mut orphan = FatImage::new(FatSpec::fat16(10_000));
    orphan.put_lfn(None, 0, "Orphan Name.txt", &short);
    assert_issue(&orphan.into_memory(), "orphan long-filename");

    let mut label = FatImage::new(FatSpec::fat16(10_000));
    label.put_entry(None, 0, b"VOLUME     ", 0x08, 0, 0);
    label.set_chain(&[2]);
    label.put_entry(None, 1, b"FILE    TXT", 0x20, 2, 1);
    let report = inspect(&label.into_memory()).unwrap();
    assert!(report.is_consistent(), "{:?}", report.issues);
    assert_eq!(report.directories_and_files.len(), 1);

    let mut deleted = FatImage::new(FatSpec::fat16(10_000));
    deleted.put_entry(None, 0, b"GONE    TXT", 0x20, 0, 0);
    let deleted_root = deleted.root_offset();
    deleted.bytes[deleted_root] = 0xe5;
    let report = inspect(&deleted.into_memory()).unwrap();
    assert!(report.is_consistent(), "{:?}", report.issues);
    assert!(report.directories_and_files.is_empty());
}

#[test]
fn detects_dot_entries_and_collects_their_references() {
    let mut image = FatImage::new(FatSpec::fat16(10_000));
    image.set_chain(&[5]);
    image.set_chain(&[3]);
    image.set_chain(&[4]);
    image.put_entry(None, 0, b"PARENT     ", 0x10, 5, 0);
    image.put_entry(Some(5), 0, b".          ", 0x10, 5, 0);
    image.put_entry(Some(5), 1, b"..         ", 0x10, 0, 0);
    image.put_entry(Some(5), 2, b"CHILD      ", 0x10, 3, 0);
    image.put_entry(Some(3), 0, b".          ", 0x10, 3, 0);
    image.put_entry(Some(3), 1, b"..         ", 0x10, 5, 0);
    image.put_entry(Some(3), 2, b"INNER   TXT", 0x20, 4, 1);
    let memory = image.into_memory();
    let metadata = read_metadata(&memory).unwrap();
    assert!(
        metadata.report.is_consistent(),
        "{:?}",
        metadata.report.issues
    );
    assert!(
        metadata
            .report
            .directories_and_files
            .iter()
            .all(|entry| !entry.path.ends_with("/.") && !entry.path.ends_with("/.."))
    );
    assert!(
        metadata
            .directory_references
            .iter()
            .any(|reference| reference.path == "/PARENT/.")
    );
    assert!(
        metadata
            .directory_references
            .iter()
            .any(|reference| reference.path == "/PARENT/CHILD/.." && reference.first_cluster == 5)
    );
    assert_eq!(
        metadata
            .report
            .directories_and_files
            .iter()
            .find(|entry| entry.path == "/PARENT/CHILD/INNER.TXT")
            .unwrap()
            .size,
        1
    );
}

#[test]
fn summarize_skips_tree_validation_but_keeps_allocation_checks() {
    let mut image = fat16_with_file();
    image.set_fat_entry(2, 2);
    let memory = image.into_memory();
    let summary = summarize(&memory).unwrap();
    assert!(!summary.is_fully_checked());
    assert!(summary.directories_and_files.is_empty());
    assert!(summary.is_consistent(), "{:?}", summary.issues);
    let full = inspect(&memory).unwrap();
    assert!(full.is_fully_checked());
    assert!(!full.is_consistent());
    assert!(full.issues.iter().any(|issue| issue.contains("cycle")));
}

#[test]
fn resize_constraints_track_highest_cluster_and_device_size() {
    let memory = fat16_with_file().into_memory();
    let report = inspect(&memory).unwrap();
    assert_eq!(report.resize.current_bytes, 10_000 * 512);
    assert_eq!(
        report.resize.minimum_bytes,
        u64::from(report.layout.first_data_sector + 1) * 512
    );
    assert!(report.resize.maximum_bytes >= report.resize.current_bytes);

    let mut slacked = FatImage::new(FatSpec::fat16(10_000).with_device_slack(2_000));
    slacked.set_chain(&[2]);
    slacked.put_entry(None, 0, b"HELLO   TXT", 0x20, 2, 1);
    let memory = slacked.into_memory();
    let report = inspect(&memory).unwrap();
    assert_eq!(report.resize.current_bytes, 10_000 * 512);
    assert!(report.resize.maximum_bytes > report.resize.current_bytes);
}

#[test]
fn all_supported_sector_sizes_round_trip_through_reader() {
    for sector_size in [512_u16, 1024, 2048, 4096] {
        let mut spec = FatSpec::fat16(15_000).with_sector_size(sector_size);
        spec.sectors_per_fat = None;
        let mut image = FatImage::new(spec);
        image.set_chain(&[2]);
        image.fill_cluster(2, 0x33);
        image.put_entry(None, 0, b"DATA    BIN", 0x20, 2, 64);
        let memory = image.into_memory();
        let report = inspect(&memory).unwrap();
        assert!(report.is_consistent(), "{sector_size}: {:?}", report.issues);
        assert_eq!(report.bpb.bytes_per_sector, sector_size);
        let expected = (0..64)
            .map(|index| 0x33_u8.wrapping_add((index % 251) as u8))
            .collect::<Vec<_>>();
        assert_eq!(support::read_file(&memory, "/DATA.BIN"), expected);
    }
}

#[test]
fn fat16_uses_total32_for_large_volumes_and_custom_root_sizes() {
    let mut image = FatImage::new(FatSpec::fat16(70_000).with_spc(2));
    image.set_chain(&[2]);
    image.put_entry(None, 0, b"LARGE   BIN", 0x20, 2, 1);
    let memory = image.into_memory();
    let (bpb, layout, boot) = Bpb::read(&memory).unwrap();
    assert_eq!(bpb.fat_type, FatType::Fat16);
    assert_eq!(bpb.total_sectors, 70_000);
    assert_eq!(&boot[19..21], &[0, 0]);
    assert_eq!(u32::from_le_bytes(boot[32..36].try_into().unwrap()), 70_000);
    assert!(layout.cluster_count >= 4_085 && layout.cluster_count <= 65_524);
    assert!(inspect(&memory).unwrap().is_consistent());

    for entries in [16_u16, 64, 1_024] {
        let mut spec = FatSpec::fat16(10_000);
        spec.root_entry_count = entries;
        spec.sectors_per_fat = None;
        let mut image = FatImage::new(spec);
        image.set_chain(&[2]);
        image.put_entry(None, 0, b"ROOT    BIN", 0x20, 2, 1);
        let report = inspect(&image.into_memory()).unwrap();
        assert!(report.is_consistent(), "{entries}: {:?}", report.issues);
        assert_eq!(report.directories_and_files.len(), 1);
        assert_eq!(
            report.layout.root_dir_sectors,
            u32::from(entries) * 32 / 512
        );
    }
}

#[test]
fn fat32_root_directory_can_span_multiple_clusters() {
    let mut image = FatImage::new(FatSpec::fat32(70_000));
    image.set_chain(&[2, 3]);
    image.set_chain(&[4]);
    image.fill_cluster(4, 0xaa);
    for slot in 0..16 {
        let offset = image.entry_offset(None, slot);
        image.bytes[offset] = 0xe5;
    }
    image.put_entry(None, 16, b"SECOND  BIN", 0x20, 4, 64);
    image.finalize();
    let memory = image.into_memory();
    let metadata = read_metadata(&memory).unwrap();
    assert!(
        metadata.report.is_consistent(),
        "{:?}",
        metadata.report.issues
    );
    assert_eq!(
        metadata
            .report
            .directories_and_files
            .iter()
            .find(|entry| entry.path == "/SECOND.BIN")
            .unwrap()
            .first_cluster,
        4
    );
    assert_eq!(
        support::read_file(&memory, "/SECOND.BIN"),
        (0..64)
            .map(|index| 0xaa_u8.wrapping_add((index % 251) as u8))
            .collect::<Vec<_>>()
    );
}

#[test]
fn exfat_and_fat12_volumes_are_rejected() {
    let mut exfat = base16().snapshot();
    exfat[3..11].copy_from_slice(b"EXFAT   ");
    exfat[11..13].fill(0);
    assert!(Bpb::read(&Mem::new(exfat)).is_err(), "exFAT accepted");

    let mut fat12 = base16().snapshot();
    fat12[19..21].copy_from_slice(&100_u16.to_le_bytes());
    assert!(Bpb::read(&Mem::new(fat12)).is_err(), "FAT12 accepted");
}

#[test]
fn zero_length_files_and_reserved_attributes() {
    let mut image = FatImage::new(FatSpec::fat16(10_000));
    image.set_chain(&[2]);
    image.put_entry(None, 0, b"EMPTY   BIN", 0x20, 0, 0);
    image.put_entry(None, 1, b"ALLOC   BIN", 0x20, 2, 0);
    let report = inspect(&image.into_memory()).unwrap();
    assert!(report.is_consistent(), "{:?}", report.issues);
    assert_eq!(report.directories_and_files.len(), 2);

    let mut reserved = FatImage::new(FatSpec::fat16(10_000));
    reserved.set_chain(&[2]);
    reserved.put_entry(None, 0, b"RESERVEDBIN", 0x40, 2, 1);
    assert_issue(&reserved.into_memory(), "reserved attributes");
}

#[test]
fn fat32_without_fsinfo_is_reported() {
    let spec = FatSpec {
        fs_info_sector: None,
        backup_boot_sector: None,
        ..FatSpec::fat32(70_000)
    };
    let image = FatImage::new(spec).into_memory();
    let (bpb, _, _) = Bpb::read(&image).unwrap();
    assert_eq!(bpb.fs_info_sector, None);
    assert_eq!(bpb.backup_boot_sector, None);
    assert_issue(&image, "no FSInfo sector");
}

#[test]
fn read_metadata_rejects_pathological_headers_without_panicking() {
    let mut rng = support::Lcg::new(42);
    let base16 = base16().snapshot();
    let base32 = base32().snapshot();
    for _ in 0..200 {
        let mut bytes = base16.clone();
        for _ in 0..4 {
            let offset = rng.range(512, (bytes.len() - 1) as u32) as usize;
            bytes[offset] ^= (rng.next_u32() as u8) | 1;
        }
        let memory = Mem::new(bytes);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = inspect(&memory);
            let _ = summarize(&memory);
            let _ = fatresize::resize::plan(&memory, 5_000 * 512);
        }));
        assert!(result.is_ok(), "panic on mutated FAT16 image");
    }
    for _ in 0..100 {
        let mut bytes = base32.clone();
        for _ in 0..4 {
            let offset = rng.range(512, (bytes.len() - 1) as u32) as usize;
            bytes[offset] ^= (rng.next_u32() as u8) | 1;
        }
        let memory = Mem::new(bytes);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = inspect(&memory);
            let _ = summarize(&memory);
            let _ = fatresize::resize::plan(&memory, 60_000 * 512);
        }));
        assert!(result.is_ok(), "panic on mutated FAT32 image");
    }
}
