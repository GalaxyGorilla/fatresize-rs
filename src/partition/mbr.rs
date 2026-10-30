//! Master Boot Record parsing and updating, based on the public IBM/PC layout.

use std::io;

use crate::device::{ReadAt, SECTOR_SIZE, Subrange, WriteAt, invalid_data};

const TABLE_OFFSET: usize = 446;
const ENTRY_SIZE: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MbrPartition {
    pub bootable: bool,
    pub partition_type: u8,
    pub first_lba: u32,
    pub sectors: u32,
}

impl MbrPartition {
    pub const fn is_empty(self) -> bool {
        self.partition_type == 0 && self.first_lba == 0 && self.sectors == 0
    }

    pub fn last_lba(self) -> Option<u32> {
        if self.sectors == 0 {
            None
        } else {
            self.first_lba.checked_add(self.sectors - 1)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mbr {
    sector: [u8; 512],
    pub disk_signature: u32,
    pub partitions: [MbrPartition; 4],
}

impl Mbr {
    pub fn from_bytes(sector: [u8; 512]) -> io::Result<Self> {
        if sector[510..512] != [0x55, 0xaa] {
            return Err(invalid_data("missing MBR signature"));
        }
        let mut partitions = [MbrPartition::default(); 4];
        for (index, partition) in partitions.iter_mut().enumerate() {
            let offset = TABLE_OFFSET + index * ENTRY_SIZE;
            let entry = &sector[offset..offset + ENTRY_SIZE];
            let bootable = match entry[0] {
                0 => false,
                0x80 => true,
                _ => return Err(invalid_data("invalid MBR boot indicator")),
            };
            let partition_type = entry[4];
            let first_lba = le_u32(&entry[8..12]);
            let sectors = le_u32(&entry[12..16]);
            if (partition_type == 0 || sectors == 0)
                && (partition_type != 0 || first_lba != 0 || sectors != 0 || bootable)
            {
                return Err(invalid_data("inconsistent empty MBR partition"));
            }
            if partition_type != 0 && sectors == 0 {
                return Err(invalid_data("non-empty MBR partition has zero length"));
            }
            if sectors != 0 && first_lba.checked_add(sectors - 1).is_none() {
                return Err(invalid_data("MBR partition range overflows"));
            }
            *partition = MbrPartition {
                bootable,
                partition_type,
                first_lba,
                sectors,
            };
        }
        validate_non_overlapping(&partitions)?;
        Ok(Self {
            disk_signature: le_u32(&sector[440..444]),
            sector,
            partitions,
        })
    }

    pub fn read(device: &impl ReadAt) -> io::Result<Self> {
        if device.len() < SECTOR_SIZE {
            return Err(invalid_data("device is too small for an MBR"));
        }
        let mut sector = [0; 512];
        device.read_at(0, &mut sector)?;
        let mbr = Self::from_bytes(sector)?;
        mbr.validate_for_len(device.len())?;
        Ok(mbr)
    }

    /// Returns a one-based primary-partition entry.
    pub fn partition(&self, number: usize) -> Option<&MbrPartition> {
        number
            .checked_sub(1)
            .and_then(|index| self.partitions.get(index))
    }

    /// Replaces a one-based primary entry. Logical/extended traversal is
    /// deliberately outside this module's scope.
    pub fn set_partition(&mut self, number: usize, partition: MbrPartition) -> io::Result<()> {
        let index = number
            .checked_sub(1)
            .filter(|&index| index < 4)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "MBR partition is not 1-4")
            })?;
        validate_partition(partition)?;
        let mut candidate = self.partitions;
        candidate[index] = partition;
        validate_non_overlapping(&candidate)?;
        self.partitions = candidate;
        Ok(())
    }

    pub fn partition_view<'a, T: ReadAt + ?Sized>(
        &self,
        device: &'a T,
        number: usize,
    ) -> io::Result<Subrange<'a, T>> {
        let partition = self
            .partition(number)
            .filter(|entry| !entry.is_empty())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "MBR partition does not exist")
            })?;
        let offset = u64::from(partition.first_lba)
            .checked_mul(SECTOR_SIZE)
            .ok_or_else(|| invalid_data("MBR partition byte offset overflows"))?;
        let len = u64::from(partition.sectors)
            .checked_mul(SECTOR_SIZE)
            .ok_or_else(|| invalid_data("MBR partition byte length overflows"))?;
        Subrange::new(device, offset, len)
    }

    pub fn to_bytes(&self) -> io::Result<[u8; 512]> {
        for partition in self.partitions {
            validate_partition(partition)?;
        }
        validate_non_overlapping(&self.partitions)?;
        let mut sector = self.sector;
        sector[440..444].copy_from_slice(&self.disk_signature.to_le_bytes());
        for (index, partition) in self.partitions.iter().enumerate() {
            let offset = TABLE_OFFSET + index * ENTRY_SIZE;
            let entry = &mut sector[offset..offset + ENTRY_SIZE];
            entry.fill(0);
            if !partition.is_empty() {
                entry[0] = if partition.bootable { 0x80 } else { 0 };
                // CHS fields use the conventional "not representable" value.
                entry[1..4].copy_from_slice(&[0xfe, 0xff, 0xff]);
                entry[4] = partition.partition_type;
                entry[5..8].copy_from_slice(&[0xfe, 0xff, 0xff]);
                entry[8..12].copy_from_slice(&partition.first_lba.to_le_bytes());
                entry[12..16].copy_from_slice(&partition.sectors.to_le_bytes());
            }
        }
        sector[510..512].copy_from_slice(&[0x55, 0xaa]);
        Ok(sector)
    }

    pub fn write(&self, device: &impl WriteAt) -> io::Result<()> {
        self.validate_for_len(device.len())?;
        device.write_at(0, &self.to_bytes()?)
    }

    fn validate_for_len(&self, len: u64) -> io::Result<()> {
        let sectors = len / SECTOR_SIZE;
        for partition in self.partitions {
            if !partition.is_empty()
                && u64::from(partition.first_lba) + u64::from(partition.sectors) > sectors
            {
                return Err(invalid_data("MBR partition extends beyond device"));
            }
        }
        Ok(())
    }
}

fn validate_partition(partition: MbrPartition) -> io::Result<()> {
    if partition.is_empty() {
        if partition.bootable {
            return Err(invalid_data("empty MBR partition is bootable"));
        }
        return Ok(());
    }
    if partition.partition_type == 0 || partition.sectors == 0 {
        return Err(invalid_data("inconsistent MBR partition"));
    }
    partition
        .first_lba
        .checked_add(partition.sectors - 1)
        .ok_or_else(|| invalid_data("MBR partition range overflows"))?;
    Ok(())
}

fn validate_non_overlapping(partitions: &[MbrPartition; 4]) -> io::Result<()> {
    for (left_index, left) in partitions.iter().enumerate() {
        if left.is_empty() || left.partition_type == 0xee {
            continue;
        }
        let left_end = u64::from(left.first_lba) + u64::from(left.sectors);
        for right in &partitions[left_index + 1..] {
            if right.is_empty() || right.partition_type == 0xee {
                continue;
            }
            let right_end = u64::from(right.first_lba) + u64::from(right.sectors);
            if u64::from(left.first_lba) < right_end && u64::from(right.first_lba) < left_end {
                return Err(invalid_data("MBR partitions overlap"));
            }
        }
    }
    Ok(())
}

fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().expect("fixed-size field"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Memory(RefCell<Vec<u8>>);

    impl ReadAt for Memory {
        fn len(&self) -> u64 {
            self.0.borrow().len() as u64
        }
        fn read_at(&self, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
            let offset = offset as usize;
            buffer.copy_from_slice(&self.0.borrow()[offset..offset + buffer.len()]);
            Ok(())
        }
    }
    impl WriteAt for Memory {
        fn write_at(&self, offset: u64, buffer: &[u8]) -> io::Result<()> {
            let offset = offset as usize;
            self.0.borrow_mut()[offset..offset + buffer.len()].copy_from_slice(buffer);
            Ok(())
        }
        fn sync(&self) -> io::Result<()> {
            Ok(())
        }
    }

    fn valid_mbr() -> [u8; 512] {
        let mut bytes = [0; 512];
        bytes[510..].copy_from_slice(&[0x55, 0xaa]);
        bytes[446 + 4] = 0x0c;
        bytes[446 + 8..446 + 12].copy_from_slice(&1_u32.to_le_bytes());
        bytes[446 + 12..446 + 16].copy_from_slice(&10_u32.to_le_bytes());
        bytes
    }

    #[test]
    fn parses_primary_partition() {
        let mbr = Mbr::from_bytes(valid_mbr()).unwrap();
        assert_eq!(
            *mbr.partition(1).unwrap(),
            MbrPartition {
                bootable: false,
                partition_type: 0x0c,
                first_lba: 1,
                sectors: 10,
            }
        );
        assert!(mbr.partition(2).unwrap().is_empty());
        assert!(mbr.partition(0).is_none());
    }

    #[test]
    fn rejects_bad_signature_boot_flag_and_overflow() {
        let mut bytes = valid_mbr();
        bytes[510] = 0;
        assert!(Mbr::from_bytes(bytes).is_err());
        let mut bytes = valid_mbr();
        bytes[446] = 1;
        assert!(Mbr::from_bytes(bytes).is_err());
        let mut bytes = valid_mbr();
        bytes[454..458].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(Mbr::from_bytes(bytes).is_err());
    }

    #[test]
    fn rejects_overlap_and_beyond_device() {
        let mut mbr = Mbr::from_bytes(valid_mbr()).unwrap();
        assert!(
            mbr.set_partition(
                2,
                MbrPartition {
                    bootable: false,
                    partition_type: 7,
                    first_lba: 5,
                    sectors: 2,
                }
            )
            .is_err()
        );
        let memory = Memory(RefCell::new(valid_mbr().into()));
        assert!(Mbr::read(&memory).is_err());
    }

    #[test]
    fn updates_only_the_mbr_sector() {
        let mut image = vec![0xa5; 16 * 512];
        image[..512].copy_from_slice(&valid_mbr());
        let memory = Memory(RefCell::new(image));
        let mut mbr = Mbr::read(&memory).unwrap();
        mbr.set_partition(
            1,
            MbrPartition {
                bootable: true,
                partition_type: 0x0b,
                first_lba: 2,
                sectors: 4,
            },
        )
        .unwrap();
        mbr.write(&memory).unwrap();
        let image = memory.0.borrow();
        assert_eq!(image[446], 0x80);
        assert_eq!(image[450], 0x0b);
        assert!(image[512..].iter().all(|&byte| byte == 0xa5));
    }

    #[test]
    fn partition_view_is_confined_to_entry() {
        let mut image = vec![0; 16 * 512];
        image[..512].copy_from_slice(&valid_mbr());
        let memory = Memory(RefCell::new(image));
        let mbr = Mbr::read(&memory).unwrap();
        let view = mbr.partition_view(&memory, 1).unwrap();
        assert_eq!(view.len(), 10 * 512);
        assert!(view.write_at(10 * 512, &[1]).is_err());
        view.write_at(0, &[7]).unwrap();
        assert_eq!(memory.0.borrow()[512], 7);
    }
}
