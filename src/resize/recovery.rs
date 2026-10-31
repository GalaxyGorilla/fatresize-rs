use std::collections::BTreeSet;
use std::fmt;
use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::fat::FatType;

const MAGIC: &[u8; 8] = b"FATRSZ\0\x1a";
const VERSION: u16 = 3;
const STATUS_OFFSET: usize = MAGIC.len() + 2;
const STATUS_SLOT_SIZE: usize = 16;
const STATUS_SLOT_COUNT: usize = 2;
const STATUS_END: usize = STATUS_OFFSET + STATUS_SLOT_SIZE * STATUS_SLOT_COUNT;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RecoveryPhase {
    Planned,
    DataMoving,
    MetadataUpdating,
    Complete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceIdentity {
    pub volume_id: u32,
    pub volume_bytes: u64,
    pub boot_checksum: u32,
    pub identity_checksum: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceGeometry {
    pub fat_type: FatType,
    pub bytes_per_sector: u16,
    pub sectors_per_cluster: u8,
    pub reserved_sectors: u16,
    pub fat_count: u8,
    pub sectors_per_fat: u32,
    pub first_data_sector: u32,
    pub cluster_count: u32,
    pub total_sectors: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectorBackup {
    pub sector: u64,
    pub bytes: Vec<u8>,
    pub checksum: u32,
}

impl SectorBackup {
    pub fn new(sector: u64, bytes: Vec<u8>) -> Self {
        let checksum = checksum(&bytes);
        Self {
            sector,
            bytes,
            checksum,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryRecord {
    pub source: SourceIdentity,
    pub geometry: SourceGeometry,
    pub phase: RecoveryPhase,
    pub sectors: Vec<SectorBackup>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryError {
    Truncated,
    BadMagic,
    UnsupportedVersion(u16),
    InvalidValue(&'static str),
    ChecksumMismatch,
    TrailingData,
}

impl fmt::Display for RecoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => write!(formatter, "recovery record is truncated"),
            Self::BadMagic => write!(formatter, "invalid recovery record magic"),
            Self::UnsupportedVersion(version) => {
                write!(formatter, "unsupported recovery record version {version}")
            }
            Self::InvalidValue(field) => write!(formatter, "invalid recovery record {field}"),
            Self::ChecksumMismatch => write!(formatter, "recovery record checksum mismatch"),
            Self::TrailingData => write!(formatter, "trailing recovery record data"),
        }
    }
}

impl std::error::Error for RecoveryError {}

impl RecoveryRecord {
    pub fn backed_up_sectors(&self) -> u64 {
        let sector_bytes = u64::from(self.geometry.bytes_per_sector);
        self.sectors
            .iter()
            .map(|backup| backup.bytes.len() as u64 / sector_bytes)
            .sum()
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut output = Vec::new();
        output.extend_from_slice(MAGIC);
        put_u16(&mut output, VERSION);
        output.extend_from_slice(&status_slot(1, self.phase));
        output.extend_from_slice(&[0; STATUS_SLOT_SIZE]);
        put_u32(&mut output, self.source.volume_id);
        put_u64(&mut output, self.source.volume_bytes);
        put_u32(&mut output, self.source.boot_checksum);
        put_u32(&mut output, self.source.identity_checksum);
        output.push(match self.geometry.fat_type {
            FatType::Fat16 => 16,
            FatType::Fat32 => 32,
        });
        put_u16(&mut output, self.geometry.bytes_per_sector);
        output.push(self.geometry.sectors_per_cluster);
        put_u16(&mut output, self.geometry.reserved_sectors);
        output.push(self.geometry.fat_count);
        put_u32(&mut output, self.geometry.sectors_per_fat);
        put_u32(&mut output, self.geometry.first_data_sector);
        put_u32(&mut output, self.geometry.cluster_count);
        put_u32(&mut output, self.geometry.total_sectors);
        put_u32(
            &mut output,
            u32::try_from(self.sectors.len()).expect("too many recovery sectors"),
        );
        for sector in &self.sectors {
            put_u64(&mut output, sector.sector);
            put_u32(
                &mut output,
                u32::try_from(sector.bytes.len()).expect("sector is too large"),
            );
            put_u32(&mut output, sector.checksum);
            output.extend_from_slice(&sector.bytes);
        }
        let record_checksum = static_checksum(&output);
        put_u32(&mut output, record_checksum);
        output
    }

    pub fn deserialize(bytes: &[u8]) -> Result<Self, RecoveryError> {
        if bytes.len() < STATUS_END + 4 {
            return Err(RecoveryError::Truncated);
        }
        let (body, trailer) = bytes.split_at(bytes.len() - 4);
        if &body[..MAGIC.len()] != MAGIC {
            return Err(RecoveryError::BadMagic);
        }
        let version = u16::from_le_bytes(body[MAGIC.len()..STATUS_OFFSET].try_into().unwrap());
        if version != VERSION {
            return Err(RecoveryError::UnsupportedVersion(version));
        }
        if static_checksum(body) != u32::from_le_bytes(trailer.try_into().unwrap()) {
            return Err(RecoveryError::ChecksumMismatch);
        }
        let (_, _, phase) = latest_status(body)?;
        let mut input = Input::new(&body[STATUS_END..]);
        let source = SourceIdentity {
            volume_id: input.u32()?,
            volume_bytes: input.u64()?,
            boot_checksum: input.u32()?,
            identity_checksum: input.u32()?,
        };
        let fat_type = match input.byte()? {
            16 => FatType::Fat16,
            32 => FatType::Fat32,
            _ => return Err(RecoveryError::InvalidValue("FAT type")),
        };
        let geometry = SourceGeometry {
            fat_type,
            bytes_per_sector: input.u16()?,
            sectors_per_cluster: input.byte()?,
            reserved_sectors: input.u16()?,
            fat_count: input.byte()?,
            sectors_per_fat: input.u32()?,
            first_data_sector: input.u32()?,
            cluster_count: input.u32()?,
            total_sectors: input.u32()?,
        };
        if !matches!(geometry.bytes_per_sector, 512 | 1024 | 2048 | 4096)
            || geometry.sectors_per_cluster == 0
            || !geometry.sectors_per_cluster.is_power_of_two()
            || geometry.reserved_sectors == 0
            || geometry.fat_count == 0
            || geometry.sectors_per_fat == 0
            || geometry.first_data_sector >= geometry.total_sectors
            || source.volume_bytes
                != u64::from(geometry.total_sectors) * u64::from(geometry.bytes_per_sector)
        {
            return Err(RecoveryError::InvalidValue("geometry"));
        }
        let data_end = geometry
            .cluster_count
            .checked_mul(u32::from(geometry.sectors_per_cluster))
            .and_then(|sectors| geometry.first_data_sector.checked_add(sectors))
            .ok_or(RecoveryError::InvalidValue("geometry"))?;
        if data_end > geometry.total_sectors
            || geometry.total_sectors - data_end >= u32::from(geometry.sectors_per_cluster)
            || match geometry.fat_type {
                FatType::Fat16 => !(4_085..=65_524).contains(&geometry.cluster_count),
                FatType::Fat32 => geometry.cluster_count <= 65_524,
            }
        {
            return Err(RecoveryError::InvalidValue("geometry"));
        }
        let count = input.u32()?;
        let mut seen = BTreeSet::new();
        let mut sectors = Vec::new();
        for _ in 0..count {
            let sector = input.u64()?;
            let length = input.u32()? as usize;
            let stored_checksum = input.u32()?;
            let sector_bytes = usize::from(geometry.bytes_per_sector);
            if length == 0 || !length.is_multiple_of(sector_bytes) {
                return Err(RecoveryError::InvalidValue("sector backup"));
            }
            let sector_count = (length / sector_bytes) as u64;
            let end = sector
                .checked_add(sector_count)
                .ok_or(RecoveryError::InvalidValue("sector backup"))?;
            if end > u64::from(geometry.total_sectors)
                || (sector == 0 && sector_count != 1)
                || (sector..end).any(|value| !seen.insert(value))
            {
                return Err(RecoveryError::InvalidValue("sector backup"));
            }
            let data = input.take(length)?.to_vec();
            if checksum(&data) != stored_checksum {
                return Err(RecoveryError::ChecksumMismatch);
            }
            sectors.push(SectorBackup {
                sector,
                bytes: data,
                checksum: stored_checksum,
            });
        }
        if !input.is_empty() {
            return Err(RecoveryError::TrailingData);
        }
        Ok(Self {
            source,
            geometry,
            phase,
            sectors,
        })
    }
}

pub(crate) fn persist_phase(path: &Path, phase: RecoveryPhase) -> io::Result<()> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let mut header = [0_u8; STATUS_END];
    file.read_exact(&mut header)?;
    if &header[..MAGIC.len()] != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid recovery record magic",
        ));
    }
    let version = u16::from_le_bytes(header[MAGIC.len()..STATUS_OFFSET].try_into().unwrap());
    if version != VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported recovery record version",
        ));
    }
    let (active, generation, current) = latest_status(&header)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if phase <= current {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "recovery phase must advance monotonically",
        ));
    }
    let target = (active + 1) % STATUS_SLOT_COUNT;
    let next = generation
        .checked_add(1)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "phase generation overflow"))?;
    file.seek(SeekFrom::Start(
        (STATUS_OFFSET + target * STATUS_SLOT_SIZE) as u64,
    ))?;
    file.write_all(&status_slot(next, phase))?;
    file.sync_all()
}

fn status_slot(generation: u64, phase: RecoveryPhase) -> [u8; STATUS_SLOT_SIZE] {
    let mut slot = [0_u8; STATUS_SLOT_SIZE];
    slot[..8].copy_from_slice(&generation.to_le_bytes());
    slot[8] = phase_byte(phase);
    let crc = checksum(&slot[..12]);
    slot[12..].copy_from_slice(&crc.to_le_bytes());
    slot
}

fn latest_status(bytes: &[u8]) -> Result<(usize, u64, RecoveryPhase), RecoveryError> {
    let mut latest = None;
    for index in 0..STATUS_SLOT_COUNT {
        let start = STATUS_OFFSET + index * STATUS_SLOT_SIZE;
        let slot = bytes
            .get(start..start + STATUS_SLOT_SIZE)
            .ok_or(RecoveryError::Truncated)?;
        let stored = u32::from_le_bytes(slot[12..16].try_into().unwrap());
        if checksum(&slot[..12]) != stored {
            continue;
        }
        let generation = u64::from_le_bytes(slot[..8].try_into().unwrap());
        let Some(phase) = phase_from_byte(slot[8]) else {
            continue;
        };
        if slot[9..12] != [0; 3] {
            continue;
        }
        if latest.is_none_or(|(_, current, _)| generation > current) {
            latest = Some((index, generation, phase));
        }
    }
    latest.ok_or(RecoveryError::InvalidValue("status"))
}

const fn phase_byte(phase: RecoveryPhase) -> u8 {
    match phase {
        RecoveryPhase::Planned => 0,
        RecoveryPhase::DataMoving => 1,
        RecoveryPhase::MetadataUpdating => 2,
        RecoveryPhase::Complete => 3,
    }
}

const fn phase_from_byte(value: u8) -> Option<RecoveryPhase> {
    match value {
        0 => Some(RecoveryPhase::Planned),
        1 => Some(RecoveryPhase::DataMoving),
        2 => Some(RecoveryPhase::MetadataUpdating),
        3 => Some(RecoveryPhase::Complete),
        _ => None,
    }
}

fn static_checksum(bytes: &[u8]) -> u32 {
    checksum_parts([&bytes[..STATUS_OFFSET], &bytes[STATUS_END..]])
}

pub fn checksum(bytes: &[u8]) -> u32 {
    checksum_parts([bytes])
}

pub(crate) fn boot_identity_checksum(bytes: &[u8], fat_type: FatType) -> io::Result<u32> {
    if bytes.len() < 512 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "boot sector is too short for identity",
        ));
    }
    let extended = match fat_type {
        FatType::Fat16 => 36..62,
        FatType::Fat32 => 64..90,
    };
    Ok(checksum_parts([
        &bytes[..11],
        &bytes[21..22],
        &bytes[extended],
        &bytes[510..512],
    ]))
}

fn checksum_parts<'a>(parts: impl IntoIterator<Item = &'a [u8]>) -> u32 {
    const TABLE: [u32; 256] = crc32_table();
    let mut crc = u32::MAX;
    for bytes in parts {
        for &byte in bytes {
            crc = TABLE[((crc ^ u32::from(byte)) & 0xff) as usize] ^ (crc >> 8);
        }
    }
    !crc
}

const fn crc32_table() -> [u32; 256] {
    let mut table = [0_u32; 256];
    let mut index = 0;
    while index < table.len() {
        let mut value = index as u32;
        let mut bit = 0;
        while bit < 8 {
            value = (value >> 1) ^ (0xedb8_8320 & 0_u32.wrapping_sub(value & 1));
            bit += 1;
        }
        table[index] = value;
        index += 1;
    }
    table
}

struct Input<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Input<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }
    fn take(&mut self, count: usize) -> Result<&'a [u8], RecoveryError> {
        let end = self
            .position
            .checked_add(count)
            .ok_or(RecoveryError::Truncated)?;
        let value = self
            .bytes
            .get(self.position..end)
            .ok_or(RecoveryError::Truncated)?;
        self.position = end;
        Ok(value)
    }
    fn byte(&mut self) -> Result<u8, RecoveryError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, RecoveryError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, RecoveryError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, RecoveryError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn is_empty(&self) -> bool {
        self.position == self.bytes.len()
    }
}

fn put_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_le_bytes());
}
fn put_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}
fn put_u64(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn round_trip_and_reject_corruption() {
        let encoded = record().serialize();
        assert_eq!(RecoveryRecord::deserialize(&encoded).unwrap(), record());
        assert_eq!(record().backed_up_sectors(), 3);
        for end in 0..encoded.len() {
            assert!(
                RecoveryRecord::deserialize(&encoded[..end]).is_err(),
                "accepted record truncated to {end} bytes"
            );
        }
        let mut corrupt = encoded.clone();
        corrupt[STATUS_END + 1] ^= 1;
        assert_eq!(
            RecoveryRecord::deserialize(&corrupt),
            Err(RecoveryError::ChecksumMismatch)
        );
        assert!(RecoveryRecord::deserialize(&corrupt[..12]).is_err());

        let mut phased = record().serialize();
        phased[STATUS_OFFSET + STATUS_SLOT_SIZE..STATUS_END]
            .copy_from_slice(&status_slot(2, RecoveryPhase::Complete));
        assert_eq!(
            RecoveryRecord::deserialize(&phased).unwrap().phase,
            RecoveryPhase::Complete
        );

        let mut out_of_bounds = record();
        out_of_bounds.sectors[0].sector = u64::from(out_of_bounds.geometry.total_sectors);
        assert_eq!(
            RecoveryRecord::deserialize(&out_of_bounds.serialize()),
            Err(RecoveryError::InvalidValue("sector backup"))
        );
    }

    #[test]
    fn persisted_phase_only_moves_forward() {
        let path = std::env::temp_dir().join(format!(
            "fatresize-phase-{}-{}.record",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, record().serialize()).unwrap();

        persist_phase(&path, RecoveryPhase::DataMoving).unwrap();
        assert_eq!(
            RecoveryRecord::deserialize(&std::fs::read(&path).unwrap())
                .unwrap()
                .phase,
            RecoveryPhase::DataMoving
        );
        assert!(persist_phase(&path, RecoveryPhase::Planned).is_err());
        assert!(persist_phase(&path, RecoveryPhase::DataMoving).is_err());
        persist_phase(&path, RecoveryPhase::MetadataUpdating).unwrap();
        persist_phase(&path, RecoveryPhase::Complete).unwrap();
        assert!(persist_phase(&path, RecoveryPhase::MetadataUpdating).is_err());

        std::fs::remove_file(path).unwrap();
    }
}
