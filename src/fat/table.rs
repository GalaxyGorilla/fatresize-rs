use std::collections::HashSet;
use std::io;

use crate::device::{ReadAt, invalid_data};

use super::boot::{Bpb, FatType, Layout};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    Free,
    Next(u32),
    End,
    Bad,
    Reserved(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainError {
    Cycle(u32),
    Bad(u32),
    Reserved(u32, u32),
    Free(u32),
    OutOfRange(u32),
}

#[derive(Debug, Clone)]
pub struct FatTable {
    entries: Vec<u32>,
    fat_type: FatType,
    cluster_count: u32,
}

impl FatTable {
    pub fn read(
        device: &impl ReadAt,
        bpb: &Bpb,
        layout: &Layout,
    ) -> io::Result<(Self, Vec<String>)> {
        let fat_bytes_u64 = u64::from(bpb.sectors_per_fat) * u64::from(bpb.bytes_per_sector);
        let fat_bytes = usize::try_from(fat_bytes_u64)
            .map_err(|_| invalid_data("FAT table does not fit memory"))?;
        let mut copies = Vec::with_capacity(usize::from(bpb.fat_count));
        for index in 0..bpb.fat_count {
            let sector =
                u64::from(bpb.reserved_sectors) + u64::from(index) * u64::from(bpb.sectors_per_fat);
            let mut bytes = vec![0; fat_bytes];
            device.read_at(sector * u64::from(bpb.bytes_per_sector), &mut bytes)?;
            copies.push(bytes);
        }
        let active = usize::from(bpb.active_fat);
        let mut issues = Vec::new();
        if bpb.mirroring_enabled {
            for index in 1..copies.len() {
                if copies[index] != copies[0] {
                    issues.push(format!("FAT copies 1 and {} differ", index + 1));
                }
            }
        }
        let raw = copies
            .get(active)
            .ok_or_else(|| invalid_data("active FAT copy is unavailable"))?;
        let mut entries = Vec::with_capacity(layout.cluster_count as usize + 2);
        match bpb.fat_type {
            FatType::Fat16 => {
                for bytes in raw
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .take(layout.cluster_count as usize + 2)
                {
                    entries.push(u32::from(u16::from_le_bytes(*bytes)));
                }
                if entries[0] & 0xff != u32::from(bpb.media) || entries[0] < 0xfff8 {
                    issues.push("FAT16 reserved entry 0 is invalid".to_owned());
                }
                if entries[1] < 0xfff8 {
                    issues.push("FAT16 reserved entry 1 is invalid".to_owned());
                }
            }
            FatType::Fat32 => {
                for bytes in raw
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .take(layout.cluster_count as usize + 2)
                {
                    entries.push(u32::from_le_bytes(*bytes) & 0x0fff_ffff);
                }
                if entries[0] & 0xff != u32::from(bpb.media) || entries[0] < 0x0fff_fff8 {
                    issues.push("FAT32 reserved entry 0 is invalid".to_owned());
                }
                if entries[1] < 0x0fff_fff8 {
                    issues.push("FAT32 reserved entry 1 is invalid".to_owned());
                }
            }
        }
        Ok((
            Self {
                entries,
                fat_type: bpb.fat_type,
                cluster_count: layout.cluster_count,
            },
            issues,
        ))
    }

    pub fn entry(&self, cluster: u32) -> Result<Entry, ChainError> {
        if cluster < 2 || cluster >= self.cluster_count + 2 {
            return Err(ChainError::OutOfRange(cluster));
        }
        let value = self.entries[cluster as usize];
        Ok(match self.fat_type {
            FatType::Fat16 => match value {
                0 => Entry::Free,
                2..=0xffef => Entry::Next(value),
                0xfff7 => Entry::Bad,
                0xfff8..=0xffff => Entry::End,
                _ => Entry::Reserved(value),
            },
            FatType::Fat32 => match value {
                0 => Entry::Free,
                2..=0x0fff_ffef => Entry::Next(value),
                0x0fff_fff7 => Entry::Bad,
                0x0fff_fff8..=0x0fff_ffff => Entry::End,
                _ => Entry::Reserved(value),
            },
        })
    }

    pub fn chain(&self, start: u32) -> Result<Vec<u32>, ChainError> {
        let mut result = Vec::new();
        let mut seen = HashSet::new();
        let mut current = start;
        loop {
            if !seen.insert(current) {
                return Err(ChainError::Cycle(current));
            }
            result.push(current);
            match self.entry(current)? {
                Entry::Next(next) => {
                    if next < 2 || next >= self.cluster_count + 2 {
                        return Err(ChainError::OutOfRange(next));
                    }
                    current = next;
                }
                Entry::End => return Ok(result),
                Entry::Bad => return Err(ChainError::Bad(current)),
                Entry::Reserved(value) => return Err(ChainError::Reserved(current, value)),
                Entry::Free => return Err(ChainError::Free(current)),
            }
        }
    }

    pub fn allocation(&self) -> (u32, u32, Option<u32>, u32) {
        let mut allocated = 0;
        let mut free = 0;
        let mut bad = 0;
        let mut highest = None;
        for cluster in 2..self.cluster_count + 2 {
            match self.entry(cluster).expect("cluster in range") {
                Entry::Free => free += 1,
                Entry::Bad => {
                    bad += 1;
                    highest = Some(cluster);
                }
                _ => {
                    allocated += 1;
                    highest = Some(cluster);
                }
            }
        }
        (allocated, free, highest, bad)
    }

    pub const fn cluster_count(&self) -> u32 {
        self.cluster_count
    }
}
