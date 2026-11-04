use fatresize::{
    Gpt, GptPartition, Mbr, MbrPartition, ReadAt, Size, WriteAt, inspect_path, plan_path,
};

mod support;
use support::{FatImage, FatSpec, Mem, gpt_disk, mbr_disk, temp_path};

fn volume() -> Vec<u8> {
    let mut image = FatImage::new(FatSpec::fat16(10_000));
    image.set_chain(&[2]);
    image.fill_cluster(2, 0x11);
    image.put_entry(None, 0, b"DATA    BIN", 0x20, 2, 100);
    image.into_bytes()
}

#[test]
fn mbr_parses_and_confines_partition_views() {
    let disk = mbr_disk(&volume(), 1, 12_000, 0x06);
    let memory = Mem::new(disk);
    let mbr = Mbr::read(&memory).unwrap();
    assert_eq!(mbr.partitions[0].first_lba, 1);
    assert_eq!(mbr.partitions[0].sectors, 10_000);
    assert_eq!(mbr.partitions[0].partition_type, 0x06);
    assert!(mbr.partitions[1].is_empty());
    let view = mbr.partition_view(&memory, 1).unwrap();
    assert_eq!(view.len(), 10_000 * 512);
    let report = fatresize::inspect(&view).unwrap();
    assert!(report.is_consistent(), "{:?}", report.issues);
    assert!(view.write_at(10_000 * 512, &[1]).is_err());
}

#[test]
fn mbr_write_changes_only_the_table_sector_and_keeps_boot_code() {
    let mut disk = mbr_disk(&volume(), 1, 12_000, 0x06);
    disk[..446].fill(0x5a);
    let memory = Mem::new(disk);
    let before = memory.snapshot();
    let mut mbr = Mbr::read(&memory).unwrap();
    mbr.set_partition(
        2,
        MbrPartition {
            bootable: true,
            partition_type: 0x0c,
            first_lba: 11_000,
            sectors: 500,
        },
    )
    .unwrap();
    mbr.write(&memory).unwrap();
    let after = memory.snapshot();
    assert_eq!(&after[..446], &before[..446]);
    assert_eq!(after[510..512], [0x55, 0xaa]);
    let reread = Mbr::read(&memory).unwrap();
    assert_eq!(reread.partition(2).unwrap().first_lba, 11_000);
    assert!(reread.partition(2).unwrap().bootable);
}

#[test]
fn mbr_rejects_malformed_and_overlapping_tables() {
    let valid = mbr_disk(&volume(), 1, 12_000, 0x06);
    let mut signature = valid.clone();
    signature[510] = 0;
    assert!(
        Mbr::from_bytes(signature[..512].try_into().unwrap()).is_err(),
        "signature"
    );
    let mut boot_indicator = valid.clone();
    boot_indicator[446] = 1;
    assert!(
        Mbr::from_bytes(boot_indicator[..512].try_into().unwrap()).is_err(),
        "boot indicator"
    );
    let mut zero_length = valid.clone();
    zero_length[446 + 12..446 + 16].fill(0);
    assert!(
        Mbr::from_bytes(zero_length[..512].try_into().unwrap()).is_err(),
        "zero length"
    );
    let mut zero_lba = valid.clone();
    zero_lba[446 + 8..446 + 12].fill(0);
    assert!(
        Mbr::from_bytes(zero_lba[..512].try_into().unwrap()).is_err(),
        "first LBA zero"
    );

    let mut empty_with_data = valid.clone();
    empty_with_data[446 + 4] = 0;
    empty_with_data[446 + 8..446 + 12].copy_from_slice(&1_u32.to_le_bytes());
    assert!(
        Mbr::from_bytes(empty_with_data[..512].try_into().unwrap()).is_err(),
        "empty entry with data"
    );

    let mut overlap = valid.clone();
    overlap[446 + 16 + 4] = 0x06;
    overlap[446 + 16 + 8..446 + 16 + 12].copy_from_slice(&5_000_u32.to_le_bytes());
    overlap[446 + 16 + 12..446 + 16 + 16].copy_from_slice(&1_000_u32.to_le_bytes());
    assert!(
        Mbr::from_bytes(overlap[..512].try_into().unwrap()).is_err(),
        "overlap"
    );

    let mut extended = valid.clone();
    extended[446 + 16 + 4] = 0xee;
    extended[446 + 16 + 8..446 + 16 + 12].copy_from_slice(&5_000_u32.to_le_bytes());
    extended[446 + 16 + 12..446 + 16 + 16].copy_from_slice(&1_000_u32.to_le_bytes());
    let mbr = Mbr::from_bytes(extended[..512].try_into().unwrap()).unwrap();
    assert_eq!(mbr.partitions.len(), 4);
    let _ = mbr;

    let mut beyond = mbr_disk(&volume(), 1, 12_000, 0x06);
    beyond[446 + 8..446 + 12].copy_from_slice(&11_000_u32.to_le_bytes());
    assert!(
        Mbr::read(&Mem::new(beyond)).is_err(),
        "partition beyond device"
    );
}

#[test]
fn mbr_partition_planning_uses_next_partition_as_capacity_boundary() {
    let mut disk = mbr_disk(&volume(), 1, 20_000, 0x06);
    disk[446 + 16 + 4] = 0x06;
    disk[446 + 16 + 8..446 + 16 + 12].copy_from_slice(&12_000_u32.to_le_bytes());
    disk[446 + 16 + 12..446 + 16 + 16].copy_from_slice(&8_000_u32.to_le_bytes());
    let path = temp_path("mbr-plan.img");
    std::fs::write(&path, disk).unwrap();

    let exact = plan_path(&path, Some(1), 8_000 * 512).unwrap();
    assert_eq!(exact.normalized_bytes, 8_000 * 512);
    let maximum = plan_path(&path, Some(1), Size::MAX).unwrap();
    assert_eq!(maximum.normalized_bytes, (12_000 - 1) * 512);
    assert!(plan_path(&path, Some(1), 12_000 * 512).is_err());
    assert!(plan_path(&path, Some(2), 512).is_err());
    assert!(inspect_path(&path, Some(1)).unwrap().is_consistent());

    std::fs::remove_file(path).unwrap();
}

#[test]
fn gpt_parses_512_and_4096_byte_blocks() {
    for block_size in [512_u32, 4096] {
        let blocks = if block_size == 512 { 12_000 } else { 2_000 };
        let first = 40;
        let disk = gpt_disk(&volume(), first, blocks, block_size);
        let memory = Mem::new(disk);
        let gpt = Gpt::read_with_block_size(&memory, block_size).unwrap();
        assert_eq!(gpt.block_size(), block_size);
        assert_eq!(gpt.partition(1).unwrap().first_lba, first);
        assert_eq!(
            gpt.partition(1).unwrap().last_lba,
            first + (volume().len() as u64 / u64::from(block_size)) - 1
        );
        let view = gpt.partition_view(&memory, 1).unwrap();
        assert_eq!(view.len(), volume().len() as u64);
        let report = fatresize::inspect(&view).unwrap();
        assert!(report.is_consistent(), "{:?}", report.issues);
        assert!(gpt.partition(2).is_none());
        assert!(gpt.partition(0).is_none());
    }
}

#[test]
fn gpt_write_orders_backup_metadata_before_primary() {
    let block_size = 4096_u32;
    let disk_sectors = 2_000_u64;
    let memory = Mem::new(gpt_disk(&volume(), 40, disk_sectors, block_size));
    let mut gpt = Gpt::read_with_block_size(&memory, block_size).unwrap();
    let backup_entries = gpt.backup_header.partition_entries_lba * u64::from(block_size);
    let backup_header = (disk_sectors - 1) * u64::from(block_size);
    let primary_entries = gpt.primary_header.partition_entries_lba * u64::from(block_size);
    let primary_header = u64::from(block_size);
    memory.writes.borrow_mut().clear();
    memory.syncs.replace(0);
    let new_last = gpt.partition(1).unwrap().last_lba - 1;
    gpt.resize_partition(1, new_last).unwrap();
    gpt.write(&memory).unwrap();
    let writes = memory.writes.borrow();
    assert_eq!(writes.len(), 4);
    assert_eq!(writes[0].0, backup_entries);
    assert_eq!(writes[1].0, backup_header);
    assert_eq!(writes[2].0, primary_entries);
    assert_eq!(writes[3].0, primary_header);
    assert_eq!(*memory.syncs.borrow(), 2);
    drop(writes);
    let reread = Gpt::read_with_block_size(&memory, block_size).unwrap();
    assert_eq!(reread.partition(1).unwrap().last_lba, new_last);
    assert_eq!(reread.primary_header, gpt.primary_header);
    assert_eq!(reread.backup_header, gpt.backup_header);
}

#[test]
fn gpt_rejects_corrupt_headers_entries_and_layouts() {
    let block_size = 512_u32;
    let base = gpt_disk(&volume(), 40, 12_000, block_size);
    let mut header = base.clone();
    header[512] ^= 1;
    assert!(Gpt::read(&Mem::new(header)).is_err(), "primary signature");

    let mut revision = base.clone();
    revision[512 + 8..512 + 12].copy_from_slice(&2_u32.to_le_bytes());
    assert!(Gpt::read(&Mem::new(revision)).is_err(), "revision");

    let mut header_size = base.clone();
    header_size[512 + 12..512 + 16].copy_from_slice(&91_u32.to_le_bytes());
    assert!(Gpt::read(&Mem::new(header_size)).is_err(), "header size");

    let mut crc = base.clone();
    crc[512 + 16] ^= 1;
    assert!(Gpt::read(&Mem::new(crc)).is_err(), "header CRC");

    let mut reserved = base.clone();
    reserved[512 + 20] = 1;
    assert!(Gpt::read(&Mem::new(reserved)).is_err(), "reserved field");

    let mut entries = base.clone();
    entries[2 * 512 + 32] ^= 1;
    assert!(Gpt::read(&Mem::new(entries)).is_err(), "entries CRC");

    let mut backup = base.clone();
    backup[(12_000 - 1) * 512 + 56] ^= 1;
    assert!(Gpt::read(&Mem::new(backup)).is_err(), "backup header CRC");

    let mut truncated = base.clone();
    truncated.truncate(12_000 * 512 - 512);
    assert!(Gpt::read(&Mem::new(truncated)).is_err(), "truncated disk");

    let memory = Mem::new(base);
    assert!(Gpt::read_with_block_size(&memory, 1024).is_err());
    assert!(Gpt::read_with_block_size(&memory, 768).is_err());
}

#[test]
fn gpt_set_partition_validates_guid_names_and_overlap() {
    let memory = Mem::new(gpt_disk(&volume(), 40, 12_000, 512));
    let mut gpt = Gpt::read(&memory).unwrap();
    let overlap = GptPartition::new([3; 16], [4; 16], 50, 60, "overlap").unwrap();
    assert!(gpt.set_partition(2, Some(overlap)).is_err());
    assert!(GptPartition::new([0; 16], [1; 16], 50, 60, "zero").is_err());
    assert!(GptPartition::new([1; 16], [0; 16], 50, 60, "zero").is_err());
    assert!(GptPartition::new([1; 16], [2; 16], 60, 50, "reversed").is_err());
    assert!(
        GptPartition::new([1; 16], [2; 16], 50, 60, "x".repeat(37)).is_err(),
        "name too long"
    );
    assert!(
        gpt.resize_partition(9, 100).is_err(),
        "partition out of range"
    );
}

#[test]
fn gpt_partition_planning_uses_usable_capacity() {
    let path = temp_path("gpt-plan.img");
    std::fs::write(&path, gpt_disk(&volume(), 40, 12_000, 512)).unwrap();
    let exact = plan_path(&path, Some(1), 9_000 * 512).unwrap();
    assert_eq!(exact.normalized_bytes, 9_000 * 512);
    let maximum = plan_path(&path, Some(1), Size::MAX).unwrap();
    assert_eq!(maximum.normalized_bytes, (12_000 - 42) * 512);
    assert!(plan_path(&path, Some(1), 12_000 * 512).is_err());
    assert!(plan_path(&path, Some(2), 512).is_err());
    assert!(inspect_path(&path, Some(1)).unwrap().is_consistent());
    std::fs::remove_file(path).unwrap();
}

#[test]
fn partition_views_are_independent_subranges() {
    let disk = mbr_disk(&volume(), 1, 12_000, 0x06);
    let memory = Mem::new(disk);
    let mbr = Mbr::read(&memory).unwrap();
    let first = mbr.partition_view(&memory, 1).unwrap();
    let second = mbr.partition_view(&memory, 1).unwrap();
    first.write_at(0, &[0x7f]).unwrap();
    let mut byte = [0];
    second.read_at(0, &mut byte).unwrap();
    assert_eq!(byte, [0x7f]);
    assert!(first.write_at(first.len(), &[1]).is_err());
    assert!(first.read_at(u64::MAX, &mut byte).is_err());
}

#[test]
fn partition_readers_reject_short_and_random_inputs() {
    let short = Mem::new(vec![0_u8; 100]);
    assert!(Mbr::read(&short).is_err());
    assert!(Gpt::read(&short).is_err());

    let mut rng = support::Lcg::new(7);
    for _ in 0..100 {
        let mut bytes = vec![0_u8; 512];
        for byte in &mut bytes {
            *byte = rng.next_u32() as u8;
        }
        let memory = Mem::new(bytes);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = Mbr::read(&memory);
        }));
        assert!(result.is_ok(), "MBR parser panicked on random input");
    }
}
