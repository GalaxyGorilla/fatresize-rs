//! Shared fixture builders for the advanced integration tests.
//!
//! These helpers deliberately construct images byte-by-byte so tests can make
//! targeted valid or invalid metadata without relying on external tools.

#![allow(dead_code)]

use std::cell::RefCell;
use std::io;

use fatresize::{FatType, ReadAt, WriteAt};

pub struct Mem {
    pub bytes: RefCell<Vec<u8>>,
    pub writes: RefCell<Vec<(u64, usize)>>,
    pub syncs: RefCell<usize>,
    pub fail_at: Option<usize>,
}

impl Mem {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes: RefCell::new(bytes),
            writes: RefCell::new(Vec::new()),
            syncs: RefCell::new(0),
            fail_at: None,
        }
    }

    pub fn failing(bytes: Vec<u8>, fail_at: usize) -> Self {
        let mut memory = Self::new(bytes);
        memory.fail_at = Some(fail_at);
        memory
    }

    pub fn snapshot(&self) -> Vec<u8> {
        self.bytes.borrow().clone()
    }
}

impl ReadAt for Mem {
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

impl WriteAt for Mem {
    fn write_at(&self, offset: u64, buffer: &[u8]) -> io::Result<()> {
        let write = self.writes.borrow().len() + 1;
        if self.fail_at == Some(write) {
            // Count the attempted write but leave the data untouched.
            self.writes.borrow_mut().push((offset, buffer.len()));
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
        self.writes.borrow_mut().push((offset as u64, buffer.len()));
        Ok(())
    }

    fn sync(&self) -> io::Result<()> {
        *self.syncs.borrow_mut() += 1;
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct FatSpec {
    pub fat_type: FatType,
    pub bytes_per_sector: u16,
    pub sectors_per_cluster: u8,
    pub reserved_sectors: u16,
    pub fat_count: u8,
    pub root_entry_count: u16,
    pub total_sectors: u32,
    pub device_sectors: u32,
    pub sectors_per_fat: Option<u32>,
    pub volume_id: u32,
    pub label: [u8; 11],
    pub fs_info_sector: Option<u16>,
    pub backup_boot_sector: Option<u16>,
    pub mirroring_enabled: bool,
    pub active_fat: u8,
    pub root_cluster: u32,
}

impl FatSpec {
    pub fn fat16(total_sectors: u32) -> Self {
        Self {
            fat_type: FatType::Fat16,
            bytes_per_sector: 512,
            sectors_per_cluster: 1,
            reserved_sectors: 1,
            fat_count: 2,
            root_entry_count: 512,
            total_sectors,
            device_sectors: total_sectors,
            sectors_per_fat: None,
            volume_id: 0x1234_5678,
            label: *b"TESTVOL    ",
            fs_info_sector: None,
            backup_boot_sector: None,
            mirroring_enabled: true,
            active_fat: 0,
            root_cluster: 0,
        }
    }

    pub fn fat32(total_sectors: u32) -> Self {
        Self {
            fat_type: FatType::Fat32,
            bytes_per_sector: 512,
            sectors_per_cluster: 1,
            reserved_sectors: 32,
            fat_count: 2,
            root_entry_count: 0,
            total_sectors,
            device_sectors: total_sectors,
            sectors_per_fat: None,
            volume_id: 0xaabb_ccdd,
            label: *b"TESTVOL32  ",
            fs_info_sector: Some(1),
            backup_boot_sector: Some(6),
            mirroring_enabled: true,
            active_fat: 0,
            root_cluster: 2,
        }
    }

    pub fn with_sector_size(mut self, bytes_per_sector: u16) -> Self {
        self.bytes_per_sector = bytes_per_sector;
        self
    }

    pub fn with_spc(mut self, sectors_per_cluster: u8) -> Self {
        self.sectors_per_cluster = sectors_per_cluster;
        self
    }

    pub fn with_fat_sectors(mut self, sectors_per_fat: u32) -> Self {
        self.sectors_per_fat = Some(sectors_per_fat);
        self
    }

    pub fn with_device_slack(mut self, extra_sectors: u32) -> Self {
        self.device_sectors = self.total_sectors + extra_sectors;
        self
    }

    pub fn with_active_fat(mut self, active_fat: u8) -> Self {
        self.active_fat = active_fat;
        self.mirroring_enabled = false;
        self
    }

    pub fn root_dir_sectors(&self) -> u32 {
        (u32::from(self.root_entry_count) * 32).div_ceil(u32::from(self.bytes_per_sector))
    }

    pub fn effective_fat_sectors(&self) -> u32 {
        if let Some(sectors) = self.sectors_per_fat {
            return sectors;
        }
        let entry_bytes = match self.fat_type {
            FatType::Fat16 => 2_u64,
            FatType::Fat32 => 4,
        };
        let fixed = u32::from(self.reserved_sectors) + self.root_dir_sectors();
        let mut fats = u32::from(self.fat_count);
        let mut sectors = 1_u32;
        for _ in 0..64 {
            let metadata = fixed + fats * sectors;
            assert!(metadata < self.total_sectors, "test volume is too small");
            let clusters = (self.total_sectors - metadata) / u32::from(self.sectors_per_cluster);
            let required = ((u64::from(clusters) + 2) * entry_bytes)
                .div_ceil(u64::from(self.bytes_per_sector)) as u32;
            if required <= sectors {
                return sectors;
            }
            sectors = required;
            fats = u32::from(self.fat_count);
        }
        panic!("fat size did not converge");
    }

    pub fn first_data_sector(&self) -> u32 {
        u32::from(self.reserved_sectors)
            + u32::from(self.fat_count) * self.effective_fat_sectors()
            + self.root_dir_sectors()
    }

    pub fn first_root_sector(&self) -> u32 {
        u32::from(self.reserved_sectors) + u32::from(self.fat_count) * self.effective_fat_sectors()
    }

    pub fn cluster_count(&self) -> u32 {
        (self.total_sectors - self.first_data_sector()) / u32::from(self.sectors_per_cluster)
    }

    pub fn cluster_size(&self) -> u32 {
        u32::from(self.bytes_per_sector) * u32::from(self.sectors_per_cluster)
    }
}

pub struct FatImage {
    pub spec: FatSpec,
    pub bytes: Vec<u8>,
}

impl FatImage {
    pub fn new(spec: FatSpec) -> Self {
        let bps = usize::from(spec.bytes_per_sector);
        let mut bytes = vec![0_u8; spec.device_sectors as usize * bps];
        let fat_sectors = spec.effective_fat_sectors();
        {
            let boot = &mut bytes[..bps];
            boot[..3].copy_from_slice(&[0xeb, 0x3c, 0x90]);
            boot[3..11].copy_from_slice(b"MSDOS5.0");
            boot[11..13].copy_from_slice(&spec.bytes_per_sector.to_le_bytes());
            boot[13] = spec.sectors_per_cluster;
            boot[14..16].copy_from_slice(&spec.reserved_sectors.to_le_bytes());
            boot[16] = spec.fat_count;
            boot[17..19].copy_from_slice(&spec.root_entry_count.to_le_bytes());
            boot[21] = 0xf8;
            if spec.total_sectors <= u32::from(u16::MAX) {
                boot[19..21].copy_from_slice(&(spec.total_sectors as u16).to_le_bytes());
            } else {
                boot[32..36].copy_from_slice(&spec.total_sectors.to_le_bytes());
            }
            match spec.fat_type {
                FatType::Fat16 => {
                    boot[22..24].copy_from_slice(&(fat_sectors as u16).to_le_bytes());
                    boot[38] = 0x29;
                    boot[39..43].copy_from_slice(&spec.volume_id.to_le_bytes());
                    boot[43..54].copy_from_slice(&spec.label);
                    boot[54..62].copy_from_slice(b"FAT16   ");
                }
                FatType::Fat32 => {
                    boot[32..36].copy_from_slice(&spec.total_sectors.to_le_bytes());
                    boot[36..40].copy_from_slice(&fat_sectors.to_le_bytes());
                    let flags = if spec.mirroring_enabled {
                        0_u16
                    } else {
                        0x80 | u16::from(spec.active_fat)
                    };
                    boot[40..42].copy_from_slice(&flags.to_le_bytes());
                    boot[44..48].copy_from_slice(&spec.root_cluster.to_le_bytes());
                    boot[48..50]
                        .copy_from_slice(&spec.fs_info_sector.unwrap_or(0xffff).to_le_bytes());
                    boot[50..52]
                        .copy_from_slice(&spec.backup_boot_sector.unwrap_or(0xffff).to_le_bytes());
                    boot[64] = 0x80;
                    boot[66] = 0x29;
                    boot[67..71].copy_from_slice(&spec.volume_id.to_le_bytes());
                    boot[71..82].copy_from_slice(&spec.label);
                    boot[82..90].copy_from_slice(b"FAT32   ");
                }
            }
            boot[510..512].copy_from_slice(&[0x55, 0xaa]);
        }
        if spec.fat_type == FatType::Fat32
            && let Some(backup) = spec.backup_boot_sector
        {
            let start = usize::from(backup) * bps;
            let primary = bytes[..bps].to_vec();
            bytes[start..start + bps].copy_from_slice(&primary);
        }
        // Write reserved entries into every FAT copy.
        for copy in 0..spec.fat_count {
            let offset = (usize::from(spec.reserved_sectors)
                + usize::from(copy) * fat_sectors as usize)
                * bps;
            match spec.fat_type {
                FatType::Fat16 => {
                    bytes[offset..offset + 2].copy_from_slice(&0xfff8_u16.to_le_bytes());
                    bytes[offset + 2..offset + 4].copy_from_slice(&0xffff_u16.to_le_bytes());
                }
                FatType::Fat32 => {
                    bytes[offset..offset + 4].copy_from_slice(&0x0fff_fff8_u32.to_le_bytes());
                    bytes[offset + 4..offset + 8].copy_from_slice(&0x0fff_ffff_u32.to_le_bytes());
                }
            }
        }
        let mut image = Self { spec, bytes };
        if image.spec.fat_type == FatType::Fat32 {
            image.set_fat_entry(image.spec.root_cluster, 0x0fff_ffff);
        }
        image.finalize();
        image
    }

    pub fn bps(&self) -> usize {
        usize::from(self.spec.bytes_per_sector)
    }

    pub fn cluster_offset(&self, cluster: u32) -> usize {
        (self.spec.first_data_sector() + (cluster - 2) * u32::from(self.spec.sectors_per_cluster))
            as usize
            * self.bps()
    }

    pub fn cluster_size(&self) -> usize {
        self.spec.cluster_size() as usize
    }

    pub fn fat_offset(&self, copy: u8) -> usize {
        (usize::from(self.spec.reserved_sectors)
            + usize::from(copy) * self.spec.effective_fat_sectors() as usize)
            * self.bps()
    }

    pub fn set_fat_entry(&mut self, cluster: u32, value: u32) {
        for copy in 0..self.spec.fat_count {
            let base = self.fat_offset(copy);
            match self.spec.fat_type {
                FatType::Fat16 => {
                    let offset = base + cluster as usize * 2;
                    self.bytes[offset..offset + 2].copy_from_slice(&(value as u16).to_le_bytes());
                }
                FatType::Fat32 => {
                    let offset = base + cluster as usize * 4;
                    self.bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
                }
            }
        }
    }

    pub fn set_chain(&mut self, chain: &[u32]) {
        let end = match self.spec.fat_type {
            FatType::Fat16 => 0xffff,
            FatType::Fat32 => 0x0fff_ffff,
        };
        for (index, &cluster) in chain.iter().enumerate() {
            let value = chain.get(index + 1).copied().unwrap_or(end);
            self.set_fat_entry(cluster, value);
        }
    }

    pub fn fill_cluster(&mut self, cluster: u32, seed: u8) {
        let offset = self.cluster_offset(cluster);
        let size = self.cluster_size();
        for (index, byte) in self.bytes[offset..offset + size].iter_mut().enumerate() {
            *byte = seed.wrapping_add((index % 251) as u8);
        }
    }

    pub fn write_cluster(&mut self, cluster: u32, data: &[u8]) {
        let size = self.cluster_size();
        assert!(data.len() <= size);
        let offset = self.cluster_offset(cluster);
        self.bytes[offset..offset + data.len()].copy_from_slice(data);
        self.bytes[offset + data.len()..offset + size].fill(0);
    }

    pub fn root_offset(&self) -> usize {
        self.spec.first_root_sector() as usize * self.bps()
    }

    pub fn entry_offset(&self, directory: Option<u32>, index: usize) -> usize {
        match (self.spec.fat_type, directory) {
            (FatType::Fat16, None) => self.root_offset() + index * 32,
            (FatType::Fat32, None) => self.cluster_offset(self.spec.root_cluster) + index * 32,
            (_, Some(cluster)) => self.cluster_offset(cluster) + index * 32,
        }
    }

    pub fn put_entry(
        &mut self,
        directory: Option<u32>,
        index: usize,
        name: &[u8; 11],
        attributes: u8,
        first_cluster: u32,
        size: u32,
    ) {
        let offset = self.entry_offset(directory, index);
        let entry = &mut self.bytes[offset..offset + 32];
        entry.fill(0);
        entry[..11].copy_from_slice(name);
        entry[11] = attributes;
        entry[26..28].copy_from_slice(&(first_cluster as u16).to_le_bytes());
        if self.spec.fat_type == FatType::Fat32 {
            entry[20..22].copy_from_slice(&((first_cluster >> 16) as u16).to_le_bytes());
        }
        entry[28..32].copy_from_slice(&size.to_le_bytes());
    }

    pub fn put_lfn(
        &mut self,
        directory: Option<u32>,
        index: usize,
        name: &str,
        short: &[u8; 11],
    ) -> usize {
        let entries = lfn_entries(name, short);
        for (offset, entry) in entries.iter().enumerate() {
            let start = self.entry_offset(directory, index + offset);
            self.bytes[start..start + 32].copy_from_slice(entry);
        }
        index + entries.len()
    }

    pub fn fsinfo_offset(&self) -> Option<usize> {
        self.spec
            .fs_info_sector
            .map(|sector| usize::from(sector) * self.bps())
    }

    pub fn finalize(&mut self) {
        if self.spec.fat_type != FatType::Fat32 {
            return;
        }
        if let Some(backup) = self.spec.backup_boot_sector {
            let start = usize::from(backup) * self.bps();
            let boot = self.bytes[..self.bps()].to_vec();
            self.bytes[start..start + boot.len()].copy_from_slice(&boot);
        }
        let Some(start) = self.fsinfo_offset() else {
            return;
        };
        let entry_bytes = 4_usize;
        let fat_start = self.fat_offset(0);
        let mut free = 0_u32;
        let mut next = u32::MAX;
        for cluster in 2..self.spec.cluster_count() + 2 {
            let offset = fat_start + cluster as usize * entry_bytes;
            let value = u32::from_le_bytes(self.bytes[offset..offset + 4].try_into().unwrap())
                & 0x0fff_ffff;
            if value == 0 {
                free += 1;
                if next == u32::MAX {
                    next = cluster;
                }
            }
        }
        let bps = self.bps();
        let info = &mut self.bytes[start..start + bps];
        info[..4].copy_from_slice(&[0x52, 0x52, 0x61, 0x41]);
        info[484..488].copy_from_slice(&[0x72, 0x72, 0x41, 0x61]);
        info[488..492].copy_from_slice(&free.to_le_bytes());
        info[492..496].copy_from_slice(&next.to_le_bytes());
        info[508..512].copy_from_slice(&[0, 0, 0x55, 0xaa]);
        if let (Some(backup), Some(info_sector)) =
            (self.spec.backup_boot_sector, self.spec.fs_info_sector)
        {
            let backup_sector = u32::from(info_sector) + u32::from(backup);
            if backup_sector < u32::from(self.spec.reserved_sectors) {
                let info_start = usize::from(info_sector) * self.bps();
                let source = self.bytes[info_start..info_start + self.bps()].to_vec();
                let start = backup_sector as usize * self.bps();
                self.bytes[start..start + source.len()].copy_from_slice(&source);
            }
        }
    }

    pub fn into_memory(self) -> Mem {
        Mem::new(self.bytes)
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

pub fn lfn_entries(name: &str, short: &[u8; 11]) -> Vec<[u8; 32]> {
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

pub fn read_file(device: &impl ReadAt, path: &str) -> Vec<u8> {
    let metadata = fatresize::read_metadata(device).unwrap();
    let entry = metadata
        .report
        .directories_and_files
        .iter()
        .find(|entry| entry.path == path && !entry.is_directory)
        .unwrap_or_else(|| panic!("missing file {path}"));
    let chain = metadata.fat.chain(entry.first_cluster).unwrap();
    let mut contents = Vec::new();
    for cluster in chain {
        let offset = (u64::from(metadata.report.layout.first_data_sector)
            + u64::from(cluster - 2) * u64::from(metadata.report.bpb.sectors_per_cluster))
            * u64::from(metadata.report.bpb.bytes_per_sector);
        let mut buffer = vec![0_u8; metadata.report.layout.cluster_size as usize];
        device.read_at(offset, &mut buffer).unwrap();
        contents.extend_from_slice(&buffer);
    }
    contents.truncate(entry.size as usize);
    contents
}

pub fn mbr_disk(volume: &[u8], first_lba: u32, disk_sectors: u32, partition_type: u8) -> Vec<u8> {
    let mut disk = vec![0_u8; disk_sectors as usize * 512];
    disk[510..512].copy_from_slice(&[0x55, 0xaa]);
    disk[446] = 0;
    disk[446 + 4] = partition_type;
    disk[446 + 8..446 + 12].copy_from_slice(&first_lba.to_le_bytes());
    disk[446 + 12..446 + 16].copy_from_slice(&((volume.len() / 512) as u32).to_le_bytes());
    let start = first_lba as usize * 512;
    disk[start..start + volume.len()].copy_from_slice(volume);
    disk
}

pub fn gpt_disk(volume: &[u8], first_lba: u64, disk_sectors: u64, block_size: u32) -> Vec<u8> {
    let block = u64::from(block_size);
    let mut disk = vec![0_u8; (disk_sectors * block) as usize];
    disk[510..512].copy_from_slice(&[0x55, 0xaa]);
    disk[450] = 0xee;
    disk[454..458].copy_from_slice(&1_u32.to_le_bytes());
    disk[458..462].copy_from_slice(&((disk_sectors - 1) as u32).to_le_bytes());

    const ENTRY_COUNT: u32 = 4;
    const ENTRY_SIZE: u32 = 128;
    let entry_bytes = u64::from(ENTRY_COUNT) * u64::from(ENTRY_SIZE);
    let entry_blocks = entry_bytes.div_ceil(block);
    let primary_entries_lba = 2;
    let backup_entries_lba = disk_sectors - 1 - entry_blocks;
    let first_usable = primary_entries_lba + entry_blocks;
    let last_usable = backup_entries_lba - 1;

    let mut entries = vec![0_u8; entry_bytes as usize];
    entries[..16].copy_from_slice(&[0x22; 16]);
    entries[16..32].copy_from_slice(&[0x33; 16]);
    entries[32..40].copy_from_slice(&first_lba.to_le_bytes());
    let last_lba = first_lba + (volume.len() as u64 / block) - 1;
    entries[40..48].copy_from_slice(&last_lba.to_le_bytes());
    let entries_crc = crc32(&entries);
    let primary_entries_offset = primary_entries_lba as usize * block_size as usize;
    disk[primary_entries_offset..primary_entries_offset + entries.len()].copy_from_slice(&entries);
    let backup_entries_offset = backup_entries_lba as usize * block_size as usize;
    disk[backup_entries_offset..backup_entries_offset + entries.len()].copy_from_slice(&entries);

    let primary = gpt_header(
        1,
        disk_sectors - 1,
        primary_entries_lba,
        first_usable,
        last_usable,
        entries_crc,
    );
    let backup = gpt_header(
        disk_sectors - 1,
        1,
        backup_entries_lba,
        first_usable,
        last_usable,
        entries_crc,
    );
    disk[block_size as usize..2 * block_size as usize]
        .copy_from_slice(&primary[..block_size as usize]);
    let backup_offset = (disk_sectors - 1) as usize * block_size as usize;
    disk[backup_offset..backup_offset + block_size as usize]
        .copy_from_slice(&backup[..block_size as usize]);
    let start = first_lba as usize * block_size as usize;
    disk[start..start + volume.len()].copy_from_slice(volume);
    disk
}

fn gpt_header(
    current: u64,
    backup: u64,
    entries_lba: u64,
    first_usable: u64,
    last_usable: u64,
    entries_crc: u32,
) -> [u8; 4096] {
    let mut header = [0_u8; 4096];
    header[..8].copy_from_slice(b"EFI PART");
    header[8..12].copy_from_slice(&0x0001_0000_u32.to_le_bytes());
    header[12..16].copy_from_slice(&92_u32.to_le_bytes());
    header[24..32].copy_from_slice(&current.to_le_bytes());
    header[32..40].copy_from_slice(&backup.to_le_bytes());
    header[40..48].copy_from_slice(&first_usable.to_le_bytes());
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

pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

pub struct Lcg(u64);

impl Lcg {
    pub fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0
    }

    pub fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    pub fn range(&mut self, start: u32, end: u32) -> u32 {
        assert!(start < end);
        start + self.next_u32() % (end - start)
    }
}

pub fn temp_path(name: &str) -> std::path::PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    std::env::temp_dir().join(format!(
        "fatresize-advanced-{name}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}
