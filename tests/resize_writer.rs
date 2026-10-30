use std::cell::{Cell, RefCell};
use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use fatresize::{FatType, ReadAt, Size, WriteAt, inspect_path};

fn temp(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "fatresize-{name}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn fat16_image() -> Vec<u8> {
    const TOTAL: usize = 10_000;
    const FAT: usize = 40;
    const CLUSTER: usize = 9_000;
    let mut image = vec![0_u8; 12_000 * 512];
    let boot = &mut image[..512];
    boot[..3].copy_from_slice(&[0xeb, 0x3c, 0x90]);
    boot[3..11].copy_from_slice(b"MSDOS5.0");
    boot[11..13].copy_from_slice(&512_u16.to_le_bytes());
    boot[13] = 1;
    boot[14..16].copy_from_slice(&1_u16.to_le_bytes());
    boot[16] = 2;
    boot[17..19].copy_from_slice(&32_u16.to_le_bytes());
    boot[19..21].copy_from_slice(&(TOTAL as u16).to_le_bytes());
    boot[21] = 0xf8;
    boot[22..24].copy_from_slice(&(FAT as u16).to_le_bytes());
    boot[38] = 0x29;
    boot[39..43].copy_from_slice(&0x1234_5678_u32.to_le_bytes());
    boot[43..54].copy_from_slice(b"WRITERTEST ");
    boot[510..512].copy_from_slice(&[0x55, 0xaa]);
    for copy in 0..2 {
        let offset = (1 + copy * FAT) * 512;
        image[offset..offset + 2].copy_from_slice(&0xfff8_u16.to_le_bytes());
        image[offset + 2..offset + 4].copy_from_slice(&0xffff_u16.to_le_bytes());
        image[offset + CLUSTER * 2..offset + CLUSTER * 2 + 2]
            .copy_from_slice(&0xffff_u16.to_le_bytes());
    }
    let root = (1 + FAT * 2) * 512;
    image[root..root + 11].copy_from_slice(b"PAYLOAD BIN");
    image[root + 11] = 0x20;
    image[root + 26..root + 28].copy_from_slice(&(CLUSTER as u16).to_le_bytes());
    image[root + 28..root + 32].copy_from_slice(&7_u32.to_le_bytes());
    let data = (83 + CLUSTER - 2) * 512;
    image[data..data + 7].copy_from_slice(b"payload");
    image
}

fn lfn_entries(name: &str, short: &[u8; 11]) -> Vec<[u8; 32]> {
    let checksum = short
        .iter()
        .fold(0_u8, |sum, &byte| sum.rotate_right(1).wrapping_add(byte));
    let mut units = name.encode_utf16().collect::<Vec<_>>();
    if !units.len().is_multiple_of(13) {
        units.push(0);
    }
    units.resize(units.len().div_ceil(13) * 13, 0xffff);
    let count = units.len() / 13;
    (0..count)
        .rev()
        .map(|chunk| {
            let mut entry = [0xff; 32];
            entry[0] = (chunk + 1) as u8 | if chunk + 1 == count { 0x40 } else { 0 };
            entry[11] = 0x0f;
            entry[12] = 0;
            entry[13] = checksum;
            entry[26..28].fill(0);
            for (&unit, offset) in units[chunk * 13..(chunk + 1) * 13]
                .iter()
                .zip([1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30])
            {
                entry[offset..offset + 2].copy_from_slice(&unit.to_le_bytes());
            }
            entry
        })
        .collect()
}

fn nested_fat16_image() -> Vec<u8> {
    const FAT: usize = 40;
    const FIRST_DATA: usize = 83;
    const DIRECTORY: usize = 8_500;
    const FILE_FIRST: usize = 9_001;
    const FILE_SECOND: usize = 9_002;
    let mut image = fat16_image();
    for copy in 0..2 {
        let fat = (1 + copy * FAT) * 512;
        image[fat + DIRECTORY * 2..fat + DIRECTORY * 2 + 2]
            .copy_from_slice(&0xffff_u16.to_le_bytes());
        image[fat + FILE_FIRST * 2..fat + FILE_FIRST * 2 + 2]
            .copy_from_slice(&(FILE_SECOND as u16).to_le_bytes());
        image[fat + FILE_SECOND * 2..fat + FILE_SECOND * 2 + 2]
            .copy_from_slice(&0xffff_u16.to_le_bytes());
    }
    let root = (1 + FAT * 2) * 512;
    let directory_short = *b"NESTED~1   ";
    image[root + 32..root + 64].copy_from_slice(&lfn_entries("Nested Folder", &directory_short)[0]);
    image[root + 64..root + 75].copy_from_slice(&directory_short);
    image[root + 75] = 0x10;
    image[root + 90..root + 92].copy_from_slice(&(DIRECTORY as u16).to_le_bytes());

    let directory = (FIRST_DATA + DIRECTORY - 2) * 512;
    image[directory..directory + 11].copy_from_slice(b".          ");
    image[directory + 11] = 0x10;
    image[directory + 26..directory + 28].copy_from_slice(&(DIRECTORY as u16).to_le_bytes());
    image[directory + 32..directory + 43].copy_from_slice(b"..         ");
    image[directory + 43] = 0x10;
    let file_short = *b"MULTIC~1BIN";
    for (index, entry) in lfn_entries("MultiCluster.bin", &file_short)
        .iter()
        .enumerate()
    {
        image[directory + 64 + index * 32..directory + 96 + index * 32].copy_from_slice(entry);
    }
    image[directory + 128..directory + 139].copy_from_slice(&file_short);
    image[directory + 139] = 0x20;
    image[directory + 154..directory + 156].copy_from_slice(&(FILE_FIRST as u16).to_le_bytes());
    image[directory + 156..directory + 160].copy_from_slice(&700_u32.to_le_bytes());
    let first = (FIRST_DATA + FILE_FIRST - 2) * 512;
    let second = (FIRST_DATA + FILE_SECOND - 2) * 512;
    image[first..first + 512].fill(b'A');
    image[second..second + 188].fill(b'B');
    image
}

fn fat32_image() -> Vec<u8> {
    const CLUSTERS: usize = 65_525;
    const RESERVED: usize = 32;
    const FAT: usize = 512;
    const TOTAL: usize = RESERVED + FAT * 2 + CLUSTERS;
    let mut image = vec![0_u8; TOTAL * 512];
    let boot = &mut image[..512];
    boot[..3].copy_from_slice(&[0xeb, 0x58, 0x90]);
    boot[3..11].copy_from_slice(b"MSWIN4.1");
    boot[11..13].copy_from_slice(&512_u16.to_le_bytes());
    boot[13] = 1;
    boot[14..16].copy_from_slice(&(RESERVED as u16).to_le_bytes());
    boot[16] = 2;
    boot[21] = 0xf8;
    boot[32..36].copy_from_slice(&(TOTAL as u32).to_le_bytes());
    boot[36..40].copy_from_slice(&(FAT as u32).to_le_bytes());
    boot[44..48].copy_from_slice(&2_u32.to_le_bytes());
    boot[48..50].copy_from_slice(&1_u16.to_le_bytes());
    boot[50..52].copy_from_slice(&6_u16.to_le_bytes());
    boot[66] = 0x29;
    boot[67..71].copy_from_slice(&0xaabb_ccdd_u32.to_le_bytes());
    boot[71..82].copy_from_slice(b"WRITER32   ");
    boot[510..512].copy_from_slice(&[0x55, 0xaa]);
    let primary = image[..512].to_vec();
    image[6 * 512..7 * 512].copy_from_slice(&primary);
    let info = &mut image[512..1024];
    info[..4].copy_from_slice(&[0x52, 0x52, 0x61, 0x41]);
    info[484..488].copy_from_slice(&[0x72, 0x72, 0x41, 0x61]);
    info[488..492].copy_from_slice(&((CLUSTERS - 2) as u32).to_le_bytes());
    info[492..496].copy_from_slice(&4_u32.to_le_bytes());
    info[508..512].copy_from_slice(&[0, 0, 0x55, 0xaa]);
    let info_copy = image[512..1024].to_vec();
    image[7 * 512..8 * 512].copy_from_slice(&info_copy);
    for copy in 0..2 {
        let offset = (RESERVED + copy * FAT) * 512;
        image[offset..offset + 4].copy_from_slice(&0x0fff_fff8_u32.to_le_bytes());
        image[offset + 4..offset + 8].copy_from_slice(&0x0fff_ffff_u32.to_le_bytes());
        image[offset + 8..offset + 12].copy_from_slice(&0x0fff_ffff_u32.to_le_bytes());
        image[offset + 12..offset + 16].copy_from_slice(&0xafff_ffff_u32.to_le_bytes());
    }
    let root = (RESERVED + FAT * 2) * 512;
    image[root..root + 11].copy_from_slice(b"PAYLOAD BIN");
    image[root + 11] = 0x20;
    image[root + 26..root + 28].copy_from_slice(&3_u16.to_le_bytes());
    image[root + 28..root + 32].copy_from_slice(&7_u32.to_le_bytes());
    image[root + 512..root + 519].copy_from_slice(b"fat32!!");
    image
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

fn gpt_header(
    current: u64,
    backup: u64,
    entries_lba: u64,
    last_usable: u64,
    entries_crc: u32,
) -> [u8; 512] {
    let mut header = [0_u8; 512];
    header[..8].copy_from_slice(b"EFI PART");
    header[8..12].copy_from_slice(&0x0001_0000_u32.to_le_bytes());
    header[12..16].copy_from_slice(&92_u32.to_le_bytes());
    header[24..32].copy_from_slice(&current.to_le_bytes());
    header[32..40].copy_from_slice(&backup.to_le_bytes());
    header[40..48].copy_from_slice(&34_u64.to_le_bytes());
    header[48..56].copy_from_slice(&last_usable.to_le_bytes());
    header[56..72].copy_from_slice(&[0x11; 16]);
    header[72..80].copy_from_slice(&entries_lba.to_le_bytes());
    header[80..84].copy_from_slice(&4_u32.to_le_bytes());
    header[84..88].copy_from_slice(&128_u32.to_le_bytes());
    header[88..92].copy_from_slice(&entries_crc.to_le_bytes());
    let checksum = crc32(&header[..92]);
    header[16..20].copy_from_slice(&checksum.to_le_bytes());
    header
}

fn gpt_disk() -> Vec<u8> {
    const SECTORS: usize = 16_000;
    const FIRST: usize = 40;
    let volume = fat16_image();
    let mut disk = vec![0_u8; SECTORS * 512];
    disk[510..512].copy_from_slice(&[0x55, 0xaa]);
    disk[450] = 0xee;
    disk[454..458].copy_from_slice(&1_u32.to_le_bytes());
    disk[458..462].copy_from_slice(&((SECTORS - 1) as u32).to_le_bytes());

    let mut entries = [0_u8; 512];
    entries[..16].copy_from_slice(&[0x22; 16]);
    entries[16..32].copy_from_slice(&[0x33; 16]);
    entries[32..40].copy_from_slice(&(FIRST as u64).to_le_bytes());
    entries[40..48].copy_from_slice(&((FIRST + 12_000 - 1) as u64).to_le_bytes());
    let entries_crc = crc32(&entries);
    disk[2 * 512..3 * 512].copy_from_slice(&entries);
    disk[(SECTORS - 2) * 512..(SECTORS - 1) * 512].copy_from_slice(&entries);
    let primary = gpt_header(
        1,
        (SECTORS - 1) as u64,
        2,
        (SECTORS - 3) as u64,
        entries_crc,
    );
    let backup = gpt_header(
        (SECTORS - 1) as u64,
        1,
        (SECTORS - 2) as u64,
        (SECTORS - 3) as u64,
        entries_crc,
    );
    disk[512..1024].copy_from_slice(&primary);
    disk[(SECTORS - 1) * 512..].copy_from_slice(&backup);
    disk[FIRST * 512..FIRST * 512 + volume.len()].copy_from_slice(&volume);
    disk
}

#[test]
fn fat16_shrink_preserves_payload_and_forced_restore_is_exact() {
    let path = temp("fat16.img");
    let recovery = temp("fat16.recovery");
    let original = fat16_image();
    std::fs::write(&path, &original).unwrap();
    fatresize::resize_path(&path, None, 5_000 * 512, &recovery, |_| {}).unwrap();
    let report = inspect_path(&path, None).unwrap();
    assert!(report.is_consistent(), "{:?}", report.issues);
    assert_eq!(report.bpb.fat_type, FatType::Fat16);
    assert_eq!(report.bpb.total_sectors, 5_000);
    let first = report.directories_and_files[0].first_cluster;
    let offset = (u64::from(report.layout.first_data_sector)
        + u64::from(first - 2) * u64::from(report.bpb.sectors_per_cluster))
        * u64::from(report.bpb.bytes_per_sector);
    let device = fatresize::Device::open_read_only(&path).unwrap();
    let mut payload = [0; 7];
    device.read_at(offset, &mut payload).unwrap();
    assert_eq!(&payload, b"payload");
    assert!(fatresize::restore_path(&path, None, &recovery, false).is_err());
    fatresize::restore_path(&path, None, &recovery, true).unwrap();
    let restored = std::fs::read(&path).unwrap();
    assert_eq!(restored.len(), 10_000 * 512);
    assert_eq!(
        restored
            .iter()
            .zip(&original[..restored.len()])
            .position(|(a, b)| a != b),
        None
    );
    std::fs::remove_file(path).unwrap();
    std::fs::remove_file(recovery).unwrap();
}

#[test]
fn shrink_preserves_nested_lfn_and_multicluster_chain() {
    let path = temp("nested.img");
    let recovery = temp("nested.recovery");
    std::fs::write(&path, nested_fat16_image()).unwrap();
    fatresize::resize_path(&path, None, 5_000 * 512, &recovery, |_| {}).unwrap();

    let report = inspect_path(&path, None).unwrap();
    assert!(report.is_consistent(), "{:?}", report.issues);
    let nested = report
        .directories_and_files
        .iter()
        .find(|entry| entry.path == "/Nested Folder/MultiCluster.bin")
        .unwrap();
    let device = fatresize::Device::open_read_only(&path).unwrap();
    let metadata = fatresize::read_metadata(&device).unwrap();
    let chain = metadata.fat.chain(nested.first_cluster).unwrap();
    assert_eq!(chain.len(), 2);
    let mut contents = vec![0_u8; 700];
    for (index, cluster) in chain.into_iter().enumerate() {
        let offset = (u64::from(report.layout.first_data_sector)
            + u64::from(cluster - 2) * u64::from(report.bpb.sectors_per_cluster))
            * u64::from(report.bpb.bytes_per_sector);
        let start = index * 512;
        let end = (start + 512).min(contents.len());
        device.read_at(offset, &mut contents[start..end]).unwrap();
    }
    assert!(contents[..512].iter().all(|&byte| byte == b'A'));
    assert!(contents[512..].iter().all(|&byte| byte == b'B'));

    std::fs::remove_file(path).unwrap();
    std::fs::remove_file(recovery).unwrap();
}

#[test]
fn mbr_partition_shrink_updates_only_length_after_filesystem() {
    let volume = fat16_image();
    let path = temp("mbr.img");
    let recovery = temp("mbr.recovery");
    let mut disk = vec![0_u8; volume.len() + 2 * 512];
    disk[510..512].copy_from_slice(&[0x55, 0xaa]);
    disk[450] = 0x06;
    disk[454..458].copy_from_slice(&1_u32.to_le_bytes());
    disk[458..462].copy_from_slice(&(12_000_u32).to_le_bytes());
    disk[512..512 + volume.len()].copy_from_slice(&volume);
    std::fs::write(&path, disk).unwrap();
    fatresize::resize_path(&path, Some(1), 5_000 * 512, &recovery, |_| {}).unwrap();
    let device = fatresize::Device::open_read_only(&path).unwrap();
    let mbr = fatresize::Mbr::read(&device).unwrap();
    let entry = mbr.partition(1).unwrap();
    assert_eq!(entry.first_lba, 1);
    assert_eq!(entry.partition_type, 0x06);
    assert_eq!(entry.sectors, 5_000);
    assert!(inspect_path(&path, Some(1)).unwrap().is_consistent());
    std::fs::remove_file(path).unwrap();
    std::fs::remove_file(recovery).unwrap();
}

#[test]
fn fat32_grow_preserves_payload_high_nibble_and_backup_metadata() {
    let path = temp("fat32.img");
    let recovery = temp("fat32.recovery");
    std::fs::write(&path, fat32_image()).unwrap();
    fatresize::resize_path(&path, None, 67_000 * 512, &recovery, |_| {}).unwrap();
    let report = inspect_path(&path, None).unwrap();
    assert!(report.is_consistent(), "{:?}", report.issues);
    assert_eq!(report.bpb.total_sectors, 67_000);
    let first = report.directories_and_files[0].first_cluster;
    let offset = (u64::from(report.layout.first_data_sector) + u64::from(first - 2)) * 512;
    let device = fatresize::Device::open_read_only(&path).unwrap();
    let mut payload = [0; 7];
    device.read_at(offset, &mut payload).unwrap();
    assert_eq!(&payload, b"fat32!!");
    let mut raw = [0; 4];
    device
        .read_at(
            u64::from(report.bpb.reserved_sectors) * 512 + u64::from(first) * 4,
            &mut raw,
        )
        .unwrap();
    assert_eq!(u32::from_le_bytes(raw) & 0xf000_0000, 0xa000_0000);
    let mut primary = vec![0; 512];
    let mut backup = vec![0; 512];
    device.read_at(0, &mut primary).unwrap();
    device.read_at(6 * 512, &mut backup).unwrap();
    assert_eq!(primary, backup);
    std::fs::remove_file(path).unwrap();
    std::fs::remove_file(recovery).unwrap();
}

#[test]
fn one_gib_target_is_honored_exactly() {
    const ONE_GIB: u64 = 1 << 30;

    let path = temp("fat32-one-gib.img");
    let recovery = temp("fat32-one-gib.recovery");
    std::fs::write(&path, fat32_image()).unwrap();

    fatresize::resize_path(&path, None, ONE_GIB, &recovery, |_| {}).unwrap();

    assert_eq!(std::fs::metadata(&path).unwrap().len(), ONE_GIB);
    let report = inspect_path(&path, None).unwrap();
    assert!(report.is_consistent(), "{:?}", report.issues);
    assert_eq!(report.layout.total_bytes, ONE_GIB);
    assert_eq!(
        u64::from(report.bpb.total_sectors) * u64::from(report.bpb.bytes_per_sector),
        ONE_GIB
    );

    std::fs::remove_file(path).unwrap();
    std::fs::remove_file(recovery).unwrap();
}

#[test]
fn gpt_partition_grow_and_shrink_update_both_tables() {
    let path = temp("gpt.img");
    let grow_recovery = temp("gpt-grow.recovery");
    let shrink_recovery = temp("gpt-shrink.recovery");
    std::fs::write(&path, gpt_disk()).unwrap();

    let before = std::fs::read(&path).unwrap();
    let dry = fatresize::plan_path(&path, Some(1), 13_000 * 512).unwrap();
    assert_eq!(dry.normalized_bytes, 13_000 * 512);
    let maximum = fatresize::plan_path(&path, Some(1), Size::MAX).unwrap();
    assert_eq!(maximum.normalized_bytes, (16_000 - 40 - 2) * 512);
    assert!(fatresize::plan_path(&path, Some(1), 15_959 * 512).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);

    fatresize::resize_path(&path, Some(1), 13_000 * 512, &grow_recovery, |_| {}).unwrap();
    let device = fatresize::Device::open_read_only(&path).unwrap();
    let gpt = fatresize::Gpt::read(&device).unwrap();
    assert_eq!(gpt.partition(1).unwrap().last_lba, 40 + 13_000 - 1);
    assert!(inspect_path(&path, Some(1)).unwrap().is_consistent());
    drop(device);

    fatresize::resize_path(&path, Some(1), 5_000 * 512, &shrink_recovery, |_| {}).unwrap();
    let device = fatresize::Device::open_read_only(&path).unwrap();
    let gpt = fatresize::Gpt::read(&device).unwrap();
    assert_eq!(gpt.partition(1).unwrap().last_lba, 40 + 5_000 - 1);
    assert!(inspect_path(&path, Some(1)).unwrap().is_consistent());

    std::fs::remove_file(path).unwrap();
    std::fs::remove_file(grow_recovery).unwrap();
    std::fs::remove_file(shrink_recovery).unwrap();
}

#[test]
fn planner_rejects_bad_clusters_and_fat_mirror_mismatch() {
    let mut bad = fat16_image();
    for copy in 0..2 {
        let fat = (1 + copy * 40) * 512;
        bad[fat + 100 * 2..fat + 100 * 2 + 2].copy_from_slice(&0xfff7_u16.to_le_bytes());
    }
    struct Bytes(Vec<u8>);
    impl ReadAt for Bytes {
        fn len(&self) -> u64 {
            self.0.len() as u64
        }
        fn read_at(&self, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
            let offset = offset as usize;
            buffer.copy_from_slice(&self.0[offset..offset + buffer.len()]);
            Ok(())
        }
    }
    assert!(matches!(
        fatresize::resize::plan(&Bytes(bad), 5_000 * 512),
        Err(fatresize::resize::ResizeError::UnsupportedAllocation { .. })
    ));

    let mut mirror = fat16_image();
    mirror[(1 + 40) * 512 + 200] ^= 1;
    assert!(matches!(
        fatresize::resize::plan(&Bytes(mirror), 5_000 * 512),
        Err(fatresize::resize::ResizeError::InconsistentSource(_))
    ));
}

#[test]
fn exact_size_image_can_plan_growth_and_max_without_writes() {
    let path = temp("exact-dry-run.img");
    let mut image = fat16_image();
    image.truncate(10_000 * 512);
    std::fs::write(&path, &image).unwrap();

    let grow = fatresize::plan_path(&path, None, 11_000 * 512).unwrap();
    assert_eq!(grow.normalized_bytes, 11_000 * 512);
    let maximum = fatresize::plan_path(&path, None, Size::MAX).unwrap();
    assert_eq!(maximum.normalized_bytes, 10_000 * 512);
    assert_eq!(std::fs::read(&path).unwrap(), image);

    std::fs::remove_file(path).unwrap();
}

struct FailingMemory {
    bytes: RefCell<Vec<u8>>,
    writes: Cell<usize>,
    fail_at: Option<usize>,
}

impl ReadAt for FailingMemory {
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

impl WriteAt for FailingMemory {
    fn write_at(&self, offset: u64, buffer: &[u8]) -> io::Result<()> {
        let write = self.writes.get() + 1;
        self.writes.set(write);
        if self.fail_at == Some(write) {
            return Err(io::Error::other("injected write failure"));
        }
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

#[test]
fn every_filesystem_write_failure_is_recoverable() {
    let original = fat16_image();
    let baseline = FailingMemory {
        bytes: RefCell::new(original.clone()),
        writes: Cell::new(0),
        fail_at: None,
    };
    let plan = fatresize::resize::plan(&baseline, 5_000 * 512).unwrap();
    let recovery = temp("failure-baseline.recovery");
    let mut stages = Vec::new();
    fatresize::resize::writer::execute(&baseline, &plan, &recovery, |stage| stages.push(stage))
        .unwrap();
    assert_eq!(
        stages,
        [
            fatresize::resize::writer::WriteStage::Recovery,
            fatresize::resize::writer::WriteStage::Data,
            fatresize::resize::writer::WriteStage::Fat,
            fatresize::resize::writer::WriteStage::Directories,
            fatresize::resize::writer::WriteStage::ReservedMetadata,
            fatresize::resize::writer::WriteStage::Boot,
            fatresize::resize::writer::WriteStage::Verify,
            fatresize::resize::writer::WriteStage::Complete,
        ]
    );
    let write_count = baseline.writes.get();
    std::fs::remove_file(recovery).unwrap();
    assert!(write_count > 0);

    for fail_at in 1..=write_count {
        let volume = FailingMemory {
            bytes: RefCell::new(original.clone()),
            writes: Cell::new(0),
            fail_at: Some(fail_at),
        };
        let plan = fatresize::resize::plan(&volume, 5_000 * 512).unwrap();
        let recovery = temp(&format!("failure-{fail_at}.recovery"));
        let error =
            fatresize::resize::writer::execute(&volume, &plan, &recovery, |_| {}).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);

        let restored = FailingMemory {
            bytes: RefCell::new(volume.bytes.into_inner()),
            writes: Cell::new(0),
            fail_at: None,
        };
        fatresize::resize::writer::restore(&restored, &recovery, false).unwrap();
        assert_eq!(
            &restored.bytes.borrow()[..10_000 * 512],
            &original[..10_000 * 512],
            "restore after injected write {fail_at}"
        );
        std::fs::remove_file(recovery).unwrap();
    }
}
