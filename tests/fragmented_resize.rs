//! Regression coverage for shrinking a deliberately fragmented FAT16 volume.
//!
//! The fixture scatters file chains above, across, and below the new cluster
//! boundary and puts a multi-cluster nested directory at the high end as well.
//! A successful shrink therefore has to relocate high clusters, rewrite their
//! FAT links and directory first-cluster fields, and update the `..` entry of
//! the relocated nested directory.

use std::cell::RefCell;
use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use fatresize::resize::MoveStep;
use fatresize::{FatType, ReadAt, WriteAt, read_metadata};

const SECTOR: usize = 512;
const TOTAL_SECTORS: usize = 10_000;
const FAT_SECTORS: usize = 40;
const FIRST_DATA: usize = 1 + FAT_SECTORS * 2 + 2;
const ROOT_OFFSET: usize = (1 + FAT_SECTORS * 2) * SECTOR;
const IMAGE_SECTORS: usize = 12_000;

const PAYLOAD: u32 = 9_000;
const HIGH: [u32; 3] = [9_001, 9_002, 9_003];
const SCATTER: [u32; 5] = [9_004, 5_000, 9_005, 4_000, 9_006];
const OUTER: u32 = 9_009;
const INNER: [u32; 2] = [9_007, 9_008];
const INNER_FILE: [u32; 3] = [9_010, 4_500, 9_011];

struct Memory {
    bytes: RefCell<Vec<u8>>,
}

impl ReadAt for Memory {
    fn len(&self) -> u64 {
        self.bytes.borrow().len() as u64
    }

    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
        let offset = usize::try_from(offset).unwrap();
        let end = offset
            .checked_add(buffer.len())
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        let bytes = self.bytes.borrow();
        let source = bytes
            .get(offset..end)
            .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
        buffer.copy_from_slice(source);
        Ok(())
    }
}

impl WriteAt for Memory {
    fn write_at(&self, offset: u64, buffer: &[u8]) -> io::Result<()> {
        let offset = usize::try_from(offset).unwrap();
        let end = offset
            .checked_add(buffer.len())
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        let mut bytes = self.bytes.borrow_mut();
        let target = bytes
            .get_mut(offset..end)
            .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
        target.copy_from_slice(buffer);
        Ok(())
    }

    fn sync(&self) -> io::Result<()> {
        Ok(())
    }
}

fn temp(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "fatresize-fragmented-{name}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn cluster_offset(cluster: u32) -> usize {
    (FIRST_DATA + cluster as usize - 2) * SECTOR
}

fn put_fat_entry(image: &mut [u8], cluster: u32, value: u16) {
    for copy in 0..2 {
        let offset = (1 + copy * FAT_SECTORS) * SECTOR + cluster as usize * 2;
        image[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }
}

fn set_chain(image: &mut [u8], chain: &[u32]) {
    for (index, &cluster) in chain.iter().enumerate() {
        let value = chain.get(index + 1).copied().unwrap_or(0xffff) as u16;
        put_fat_entry(image, cluster, value);
    }
}

fn put_entry(image: &mut [u8], offset: usize, name: &[u8; 11], attr: u8, first: u32, size: u32) {
    let entry = &mut image[offset..offset + 32];
    entry.fill(0);
    entry[..11].copy_from_slice(name);
    entry[11] = attr;
    entry[26..28].copy_from_slice(&(first as u16).to_le_bytes());
    entry[28..32].copy_from_slice(&size.to_le_bytes());
}

fn write_payload(image: &mut [u8], chain: &[u32], size: usize, seed: u8) {
    let mut written = 0;
    for &cluster in chain {
        let offset = cluster_offset(cluster);
        let count = SECTOR.min(size - written);
        for (index, byte) in image[offset..offset + count].iter_mut().enumerate() {
            *byte = seed.wrapping_add(((written + index) % 251) as u8);
        }
        written += count;
        if written == size {
            break;
        }
    }
}

fn expected(size: usize, seed: u8) -> Vec<u8> {
    (0..size)
        .map(|index| seed.wrapping_add((index % 251) as u8))
        .collect()
}

fn boot_sector() -> Vec<u8> {
    let mut image = vec![0_u8; IMAGE_SECTORS * SECTOR];
    let boot = &mut image[..SECTOR];
    boot[..3].copy_from_slice(&[0xeb, 0x3c, 0x90]);
    boot[3..11].copy_from_slice(b"MSDOS5.0");
    boot[11..13].copy_from_slice(&(SECTOR as u16).to_le_bytes());
    boot[13] = 1;
    boot[14..16].copy_from_slice(&1_u16.to_le_bytes());
    boot[16] = 2;
    boot[17..19].copy_from_slice(&32_u16.to_le_bytes());
    boot[19..21].copy_from_slice(&(TOTAL_SECTORS as u16).to_le_bytes());
    boot[21] = 0xf8;
    boot[22..24].copy_from_slice(&(FAT_SECTORS as u16).to_le_bytes());
    boot[38] = 0x29;
    boot[39..43].copy_from_slice(&0x1234_5678_u32.to_le_bytes());
    boot[43..54].copy_from_slice(b"FRAGMENTED ");
    boot[510..512].copy_from_slice(&[0x55, 0xaa]);
    for copy in 0..2 {
        let offset = (1 + copy * FAT_SECTORS) * SECTOR;
        image[offset..offset + 2].copy_from_slice(&0xfff8_u16.to_le_bytes());
        image[offset + 2..offset + 4].copy_from_slice(&0xffff_u16.to_le_bytes());
    }
    image
}

fn fragmented_image() -> Memory {
    let mut image = boot_sector();
    set_chain(&mut image, &[PAYLOAD]);
    set_chain(&mut image, &HIGH);
    set_chain(&mut image, &SCATTER);
    set_chain(&mut image, &[OUTER]);
    set_chain(&mut image, &INNER);
    set_chain(&mut image, &INNER_FILE);

    // Root directory: the base payload plus three deliberately placed entries.
    put_entry(&mut image, ROOT_OFFSET, b"PAYLOAD BIN", 0x20, PAYLOAD, 7);
    image[cluster_offset(PAYLOAD)..cluster_offset(PAYLOAD) + 7].copy_from_slice(b"payload");
    put_entry(
        &mut image,
        ROOT_OFFSET + 32,
        b"HIGH    BIN",
        0x20,
        HIGH[0],
        1_300,
    );
    put_entry(
        &mut image,
        ROOT_OFFSET + 64,
        b"SCATTER BIN",
        0x20,
        SCATTER[0],
        2_400,
    );
    put_entry(&mut image, ROOT_OFFSET + 96, b"OUTER      ", 0x10, OUTER, 0);
    write_payload(&mut image, &HIGH, 1_300, 0x11);
    write_payload(&mut image, &SCATTER, 2_400, 0x22);
    write_payload(&mut image, &INNER_FILE, 1_400, 0x33);

    // OUTER is a relocated single-cluster directory containing INNER.
    let outer = cluster_offset(OUTER);
    put_entry(&mut image, outer, b".          ", 0x10, OUTER, 0);
    put_entry(&mut image, outer + 32, b"..         ", 0x10, 0, 0);
    put_entry(&mut image, outer + 64, b"INNER      ", 0x10, INNER[0], 0);

    // INNER spans two clusters, so its file entry lives in the second one.
    let inner_first = cluster_offset(INNER[0]);
    put_entry(&mut image, inner_first, b".          ", 0x10, INNER[0], 0);
    put_entry(&mut image, inner_first + 32, b"..         ", 0x10, OUTER, 0);
    // Mark the rest of the first cluster as deleted rather than free, so the
    // scanner walks into the second cluster and exercises its entry offsets.
    for slot in 2..16 {
        image[inner_first + slot * 32] = 0xe5;
    }
    let inner_second = cluster_offset(INNER[1]);
    put_entry(
        &mut image,
        inner_second,
        b"INNER   BIN",
        0x20,
        INNER_FILE[0],
        1_400,
    );

    Memory {
        bytes: RefCell::new(image),
    }
}

fn read_file(volume: &Memory, path: &str, size: usize) -> Vec<u8> {
    let metadata = read_metadata(volume).unwrap();
    let entry = metadata
        .report
        .directories_and_files
        .iter()
        .find(|entry| entry.path == path)
        .unwrap_or_else(|| panic!("missing file {path}"));
    assert_eq!(entry.size as usize, size, "size changed for {path}");
    let chain = metadata.fat.chain(entry.first_cluster).unwrap();
    let first_data = metadata.report.layout.first_data_sector as usize;
    let mut contents = Vec::new();
    for cluster in chain {
        let offset = (first_data + cluster as usize - 2) * SECTOR;
        contents.extend_from_slice(&volume.bytes.borrow()[offset..offset + SECTOR]);
    }
    contents.truncate(size);
    contents
}

#[test]
fn fragmented_chains_and_nested_dotdot_survive_shrink() {
    let volume = fragmented_image();
    let before = read_metadata(&volume).unwrap();
    assert!(before.report.is_consistent(), "{:?}", before.report.issues);
    assert_eq!(before.report.allocated_clusters, 15);

    let plan = fatresize::resize::plan(&volume, 5_000 * SECTOR as u64).unwrap();
    assert_eq!(plan.target.fat_type, FatType::Fat16);
    assert!(
        plan.mappings
            .iter()
            .any(|mapping| mapping.source == OUTER && mapping.target != OUTER),
        "test fixture no longer forces relocation of the nested directory"
    );
    assert!(
        plan.mappings
            .iter()
            .any(|mapping| mapping.source == 5_000 && mapping.target != 5_000),
        "test fixture no longer forces relocation of a middle cluster"
    );
    assert!(!plan.moves.steps.is_empty());

    let recovery = temp("fragmented.recovery");
    fatresize::resize::writer::execute(&volume, &plan, &recovery, |_| {}).unwrap();

    let after = read_metadata(&volume).unwrap();
    assert!(after.report.is_consistent(), "{:?}", after.report.issues);
    assert_eq!(after.report.bpb.total_sectors, 5_000);
    assert!(
        after.report.highest_allocated_cluster.unwrap() <= after.report.layout.cluster_count + 1,
        "allocated data remains outside the resized volume"
    );

    assert_eq!(read_file(&volume, "/PAYLOAD.BIN", 7), b"payload");
    assert_eq!(
        read_file(&volume, "/HIGH.BIN", 1_300),
        expected(1_300, 0x11)
    );
    assert_eq!(
        read_file(&volume, "/SCATTER.BIN", 2_400),
        expected(2_400, 0x22)
    );
    assert_eq!(
        read_file(&volume, "/OUTER/INNER/INNER.BIN", 1_400),
        expected(1_400, 0x33)
    );

    let outer_first = after
        .report
        .directories_and_files
        .iter()
        .find(|entry| entry.path == "/OUTER")
        .unwrap()
        .first_cluster;
    let inner_first = after
        .report
        .directories_and_files
        .iter()
        .find(|entry| entry.path == "/OUTER/INNER")
        .unwrap()
        .first_cluster;
    assert_ne!(outer_first, OUTER, "OUTER directory was not relocated");
    assert_ne!(inner_first, INNER[0], "INNER directory was not relocated");

    let dotdot = after
        .directory_references
        .iter()
        .find(|reference| reference.path == "/OUTER/INNER/..")
        .unwrap();
    let dot = after
        .directory_references
        .iter()
        .find(|reference| reference.path == "/OUTER/INNER/.")
        .unwrap();
    assert_eq!(dotdot.first_cluster, outer_first);
    assert_eq!(dot.first_cluster, inner_first);

    std::fs::remove_file(recovery).unwrap();
}

#[test]
fn grow_across_source_end_keeps_recovery_record_valid() {
    const TARGET_SECTORS: usize = 10_400;
    const LAST_CLUSTER: u32 = 9_918;
    const LAST_CLUSTER_NAME: &[u8; 11] = b"NEAREND BIN";

    let mut volume = fragmented_image();
    volume.bytes.borrow_mut().truncate(TARGET_SECTORS * SECTOR);
    {
        let image = volume.bytes.get_mut();
        set_chain(image, &[LAST_CLUSTER]);
        put_entry(
            image,
            ROOT_OFFSET + 4 * 32,
            LAST_CLUSTER_NAME,
            0x20,
            LAST_CLUSTER,
            16,
        );
        write_payload(image, &[LAST_CLUSTER], 16, 0x44);
    }

    let plan = fatresize::resize::plan(&volume, TARGET_SECTORS as u64 * SECTOR as u64).unwrap();
    assert_eq!(plan.target.total_sectors, TARGET_SECTORS as u32);
    let source_end = TOTAL_SECTORS as u64 * SECTOR as u64;
    assert!(
        plan.moves.steps.iter().any(|step| match *step {
            MoveStep::Copy {
                target_offset,
                length,
                ..
            }
            | MoveStep::RestoreFromScratch {
                target_offset,
                length,
                ..
            } => target_offset + u64::from(length) > source_end,
            MoveStep::SaveToScratch { .. } => false,
        }),
        "test fixture no longer moves data into the newly grown tail"
    );

    let recovery = temp("grow.recovery");
    fatresize::resize::writer::prepare(&volume, &plan, &recovery).unwrap();
    let record = fatresize::resize::writer::load_record(&recovery).unwrap();
    assert!(
        record.sectors.iter().all(|sector| {
            sector.sector * SECTOR as u64 + sector.bytes.len() as u64
                <= TOTAL_SECTORS as u64 * SECTOR as u64
        }),
        "recovery record backs up sectors outside its declared source volume"
    );

    fatresize::resize::writer::execute(&volume, &plan, &recovery, |_| {}).unwrap();
    let after = read_metadata(&volume).unwrap();
    assert!(after.report.is_consistent(), "{:?}", after.report.issues);
    assert_eq!(after.report.bpb.total_sectors, TARGET_SECTORS as u32);
    assert_eq!(read_file(&volume, "/NEAREND.BIN", 16), expected(16, 0x44));
    assert_eq!(read_file(&volume, "/PAYLOAD.BIN", 7), b"payload");

    std::fs::remove_file(recovery).unwrap();
}
