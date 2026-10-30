use std::io;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use fatresize::{FatType, ReadAt, inspect, inspect_path, summarize};

struct Memory(Vec<u8>);

impl ReadAt for Memory {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }

    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
        let offset = usize::try_from(offset).unwrap();
        let end = offset + buffer.len();
        if end > self.0.len() {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        buffer.copy_from_slice(&self.0[offset..end]);
        Ok(())
    }
}

fn fat16_image() -> Vec<u8> {
    const TOTAL: usize = 4_122;
    let mut image = vec![0_u8; TOTAL * 512];
    let boot = &mut image[..512];
    boot[0..3].copy_from_slice(&[0xeb, 0x3c, 0x90]);
    boot[3..11].copy_from_slice(b"MSDOS5.0");
    boot[11..13].copy_from_slice(&512_u16.to_le_bytes());
    boot[13] = 1;
    boot[14..16].copy_from_slice(&1_u16.to_le_bytes());
    boot[16] = 2;
    boot[17..19].copy_from_slice(&32_u16.to_le_bytes());
    boot[19..21].copy_from_slice(&(TOTAL as u16).to_le_bytes());
    boot[21] = 0xf8;
    boot[22..24].copy_from_slice(&17_u16.to_le_bytes());
    boot[24..26].copy_from_slice(&63_u16.to_le_bytes());
    boot[26..28].copy_from_slice(&255_u16.to_le_bytes());
    boot[36] = 0x80;
    boot[38] = 0x29;
    boot[39..43].copy_from_slice(&0x1234_5678_u32.to_le_bytes());
    boot[43..54].copy_from_slice(b"TESTVOL    ");
    boot[54..62].copy_from_slice(b"FAT16   ");
    boot[510..512].copy_from_slice(&[0x55, 0xaa]);

    for fat_index in 0..2 {
        let fat = (1 + fat_index * 17) * 512;
        image[fat..fat + 2].copy_from_slice(&0xfff8_u16.to_le_bytes());
        image[fat + 2..fat + 4].copy_from_slice(&0xffff_u16.to_le_bytes());
        image[fat + 4..fat + 6].copy_from_slice(&0xffff_u16.to_le_bytes());
    }
    let root = 35 * 512;
    let short = *b"LONGNA~1TXT";
    image[root..root + 32].copy_from_slice(&lfn_entry("LongName.txt", &short));
    image[root + 32..root + 43].copy_from_slice(&short);
    image[root + 43] = 0x20;
    image[root + 58..root + 60].copy_from_slice(&2_u16.to_le_bytes());
    image[root + 60..root + 64].copy_from_slice(&5_u32.to_le_bytes());
    let data = 37 * 512;
    image[data..data + 5].copy_from_slice(b"hello");
    image
}

fn lfn_entry(name: &str, short: &[u8; 11]) -> [u8; 32] {
    let mut entry = [0xff; 32];
    entry[0] = 0x41;
    entry[11] = 0x0f;
    entry[12] = 0;
    entry[13] = short
        .iter()
        .fold(0_u8, |sum, &byte| sum.rotate_right(1).wrapping_add(byte));
    entry[26..28].fill(0);
    let mut units = name.encode_utf16().collect::<Vec<_>>();
    units.push(0);
    units.resize(13, 0xffff);
    for (unit, offset) in units
        .into_iter()
        .zip([1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30])
    {
        entry[offset..offset + 2].copy_from_slice(&unit.to_le_bytes());
    }
    entry
}

fn fat32_image() -> Vec<u8> {
    const CLUSTERS: usize = 65_525;
    const RESERVED: usize = 32;
    const FAT_SECTORS: usize = 512;
    const TOTAL: usize = RESERVED + FAT_SECTORS * 2 + CLUSTERS;
    let mut image = vec![0_u8; TOTAL * 512];
    {
        let boot = &mut image[..512];
        boot[0..3].copy_from_slice(&[0xeb, 0x58, 0x90]);
        boot[3..11].copy_from_slice(b"MSWIN4.1");
        boot[11..13].copy_from_slice(&512_u16.to_le_bytes());
        boot[13] = 1;
        boot[14..16].copy_from_slice(&(RESERVED as u16).to_le_bytes());
        boot[16] = 2;
        boot[21] = 0xf8;
        boot[32..36].copy_from_slice(&(TOTAL as u32).to_le_bytes());
        boot[36..40].copy_from_slice(&(FAT_SECTORS as u32).to_le_bytes());
        boot[44..48].copy_from_slice(&2_u32.to_le_bytes());
        boot[48..50].copy_from_slice(&1_u16.to_le_bytes());
        boot[50..52].copy_from_slice(&6_u16.to_le_bytes());
        boot[64] = 0x80;
        boot[66] = 0x29;
        boot[67..71].copy_from_slice(&0xaabb_ccdd_u32.to_le_bytes());
        boot[71..82].copy_from_slice(b"FAT32VOL   ");
        boot[82..90].copy_from_slice(b"FAT32   ");
        boot[510..512].copy_from_slice(&[0x55, 0xaa]);
    }
    let primary = image[..512].to_vec();
    image[6 * 512..7 * 512].copy_from_slice(&primary);
    let info = &mut image[512..1024];
    info[..4].copy_from_slice(&[0x52, 0x52, 0x61, 0x41]);
    info[484..488].copy_from_slice(&[0x72, 0x72, 0x41, 0x61]);
    info[488..492].copy_from_slice(&((CLUSTERS - 2) as u32).to_le_bytes());
    info[492..496].copy_from_slice(&4_u32.to_le_bytes());
    info[508..512].copy_from_slice(&[0, 0, 0x55, 0xaa]);
    for fat_index in 0..2 {
        let fat = (RESERVED + fat_index * FAT_SECTORS) * 512;
        image[fat..fat + 4].copy_from_slice(&0x0fff_fff8_u32.to_le_bytes());
        image[fat + 4..fat + 8].copy_from_slice(&0x0fff_ffff_u32.to_le_bytes());
        image[fat + 8..fat + 12].copy_from_slice(&0x0fff_ffff_u32.to_le_bytes());
        image[fat + 12..fat + 16].copy_from_slice(&0x0fff_ffff_u32.to_le_bytes());
    }
    let root = (RESERVED + FAT_SECTORS * 2) * 512;
    image[root..root + 11].copy_from_slice(b"README  TXT");
    image[root + 11] = 0x20;
    image[root + 26..root + 28].copy_from_slice(&3_u16.to_le_bytes());
    image[root + 28..root + 32].copy_from_slice(&4_u32.to_le_bytes());
    image[root + 512..root + 516].copy_from_slice(b"data");
    image
}

#[test]
fn inspects_fat16_and_reconstructs_valid_lfn() {
    let report = inspect(&Memory(fat16_image())).unwrap();
    assert_eq!(report.bpb.fat_type, FatType::Fat16);
    assert!(report.is_consistent(), "{:?}", report.issues);
    assert_eq!(report.allocated_clusters, 1);
    assert_eq!(report.free_clusters, 4_084);
    assert_eq!(report.highest_allocated_cluster, Some(2));
    assert_eq!(report.directories_and_files[0].path, "/LongName.txt");
    assert_eq!(report.resize.minimum_bytes, 38 * 512);
}

#[test]
fn summary_skips_directory_traversal_and_full_check() {
    let image = Memory(fat16_image());
    let summary = summarize(&image).unwrap();
    assert!(!summary.is_fully_checked());
    assert!(summary.directories_and_files.is_empty());
    assert_eq!(summary.allocated_clusters, 1);

    let full = inspect(&image).unwrap();
    assert!(full.is_fully_checked());
    assert_eq!(full.directories_and_files.len(), 1);
}

#[test]
fn inspects_fat32_fsinfo_and_backup_boot() {
    let report = inspect(&Memory(fat32_image())).unwrap();
    assert_eq!(report.bpb.fat_type, FatType::Fat32);
    assert!(report.is_consistent(), "{:?}", report.issues);
    assert_eq!(report.allocated_clusters, 2);
    assert_eq!(report.fs_info.unwrap().free_clusters, Some(65_523));
    assert_eq!(report.directories_and_files[0].path, "/README.TXT");
}

#[test]
fn reports_mirror_backup_and_fsinfo_disagreement() {
    let mut image = fat32_image();
    image[(32 + 512) * 512 + 12] ^= 1;
    image[6 * 512 + 3] ^= 1;
    image[512 + 488..512 + 492].copy_from_slice(&0_u32.to_le_bytes());
    let report = inspect(&Memory(image)).unwrap();
    assert!(report.issues.iter().any(|v| v.contains("copies")));
    assert!(report.issues.iter().any(|v| v.contains("backup boot")));
    assert!(report.issues.iter().any(|v| v.contains("free count")));
}

#[test]
fn detects_chain_cycle_and_invalid_lfn_checksum() {
    let mut image = fat16_image();
    for fat_index in 0..2 {
        let fat = (1 + fat_index * 17) * 512;
        image[fat + 4..fat + 6].copy_from_slice(&2_u16.to_le_bytes());
    }
    image[35 * 512 + 13] ^= 1;
    let report = inspect(&Memory(image)).unwrap();
    assert!(report.issues.iter().any(|v| v.contains("cycle")));
    assert!(report.issues.iter().any(|v| v.contains("long filename")));
}

#[test]
fn rejects_fat12_and_truncated_volumes() {
    let mut image = fat16_image();
    image[19..21].copy_from_slice(&100_u16.to_le_bytes());
    assert!(inspect(&Memory(image)).is_err());
    let mut image = fat16_image();
    image.truncate(image.len() - 512);
    assert!(inspect(&Memory(image)).is_err());
}

#[test]
fn cli_info_json_and_check_are_read_only() {
    let path = std::env::temp_dir().join(format!(
        "fatresize-reader-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let image = fat16_image();
    std::fs::write(&path, &image).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_fatresize"))
        .args(["--info", "--json"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("\"fat_type\":\"FAT16\""));
    assert_eq!(std::fs::read(&path).unwrap(), image);
    let output = Command::new(env!("CARGO_BIN_EXE_fatresize"))
        .arg("--check")
        .arg(&path)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(std::fs::read(&path).unwrap(), image);
    let output = Command::new(env!("CARGO_BIN_EXE_fatresize"))
        .args(["--size", "2110464", "--dry-run", "--json"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(output.status.success());
    let json = String::from_utf8(output.stdout).unwrap();
    assert!(json.starts_with('{') && json.ends_with("}\n"));
    assert!(json.contains("\"dry_run\":true"));
    assert!(
        !json
            .chars()
            .any(|character| { character < '\u{20}' && !matches!(character, '\n' | '\r' | '\t') })
    );
    assert_eq!(std::fs::read(&path).unwrap(), image);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn selects_an_mbr_partition_explicitly_or_unambiguously() {
    let volume = fat16_image();
    let sectors = volume.len() / 512;
    let mut disk = vec![0_u8; volume.len() + 512];
    disk[510..512].copy_from_slice(&[0x55, 0xaa]);
    disk[446 + 4] = 0x06;
    disk[446 + 8..446 + 12].copy_from_slice(&1_u32.to_le_bytes());
    disk[446 + 12..446 + 16].copy_from_slice(&(sectors as u32).to_le_bytes());
    disk[512..].copy_from_slice(&volume);
    let path = std::env::temp_dir().join(format!(
        "fatresize-partition-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(&path, disk).unwrap();
    assert_eq!(
        inspect_path(&path, Some(1)).unwrap().bpb.fat_type,
        FatType::Fat16
    );
    assert_eq!(
        inspect_path(&path, None).unwrap().bpb.fat_type,
        FatType::Fat16
    );
    std::fs::remove_file(path).unwrap();
}
