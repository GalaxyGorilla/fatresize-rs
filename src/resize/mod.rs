//! Deterministic, write-free FAT16/FAT32 resize planning.

mod geometry;
mod moves;
mod recovery;
pub mod writer;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Write as _};
use std::io;

use crate::device::ReadAt;
use crate::fat::{self, Entry, FatType, Metadata};

pub use geometry::{TargetGeometry, calculate as calculate_geometry, normalize_size};
pub use moves::{DataMove, MoveSchedule, MoveStep, schedule as schedule_moves};
pub use recovery::{
    RecoveryError, RecoveryPhase, RecoveryRecord, SectorBackup, SourceGeometry, SourceIdentity,
};

#[derive(Debug)]
pub enum ResizeError {
    Io(io::Error),
    TargetTooSmall,
    TargetTooLarge,
    TargetUnaligned { requested: u64, alignment: u64 },
    GeometryOverflow,
    Fat12Unsupported,
    FatTypeTransition { source: FatType, target: FatType },
    InconsistentSource(Vec<String>),
    UnsupportedAllocation { cluster: u32, kind: &'static str },
    InsufficientSpace { allocated: u32, available: u32 },
}

impl fmt::Display for ResizeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => error.fmt(formatter),
            Self::TargetTooSmall => write!(formatter, "target is too small for this FAT geometry"),
            Self::TargetTooLarge => write!(formatter, "target exceeds FAT's sector-count limit"),
            Self::TargetUnaligned {
                requested,
                alignment,
            } => write!(
                formatter,
                "target size {requested} is not exactly representable; it must be a multiple of {alignment} bytes"
            ),
            Self::GeometryOverflow => write!(formatter, "target FAT geometry overflows"),
            Self::Fat12Unsupported => write!(formatter, "target geometry would be FAT12"),
            Self::FatTypeTransition { source, target } => write!(
                formatter,
                "implicit FAT type transition from {} to {} is unsafe",
                source.name(),
                target.name()
            ),
            Self::InconsistentSource(issues) => write!(
                formatter,
                "source filesystem is not safe to resize: {}",
                issues.join("; ")
            ),
            Self::UnsupportedAllocation { cluster, kind } => {
                write!(
                    formatter,
                    "cluster {cluster} has unsupported {kind} allocation"
                )
            }
            Self::InsufficientSpace {
                allocated,
                available,
            } => write!(
                formatter,
                "target has {available} clusters but {allocated} are allocated"
            ),
        }
    }
}

impl std::error::Error for ResizeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for ResizeError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterMapping {
    pub source: u32,
    pub target: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FatValue {
    Next(u32),
    End,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FatPatch {
    pub cluster: u32,
    pub value: FatValue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryPatch {
    pub path: String,
    pub source_entry_offset: u64,
    pub entry_offset: u64,
    pub old_first_cluster: u32,
    pub new_first_cluster: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Precondition {
    SourceIdentity {
        volume_id: u32,
        boot_checksum: u32,
        total_bytes: u64,
    },
    CleanFilesystem,
    FatType(FatType),
    ExclusiveUnmountedAccess,
    DeviceLengthAtLeast(u64),
    RecoveryRecordDurable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    VerifyPreconditions,
    PersistRecoveryRecord { sectors: u32 },
    MoveData { steps: u32, scratch_bytes: u64 },
    WriteFat { entries: u32, copies: u8 },
    PatchDirectoryEntries { entries: u32 },
    UpdateReservedMetadata,
    CommitBootGeometry,
    ResizeContainer { bytes: u64 },
    MarkRecoveryComplete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResizePlan {
    pub requested_bytes: u64,
    pub normalized_bytes: u64,
    pub source_total_bytes: u64,
    pub target: TargetGeometry,
    pub mappings: Vec<ClusterMapping>,
    pub fat_patches: Vec<FatPatch>,
    pub directory_patches: Vec<DirectoryPatch>,
    pub root_cluster: Option<(u32, u32)>,
    pub moves: MoveSchedule,
    pub preconditions: Vec<Precondition>,
    pub operations: Vec<Operation>,
    pub recovery: RecoveryRecord,
}

/// Builds a plan using reads only. The source is rejected unless all parser
/// consistency checks pass.
pub fn plan(device: &impl ReadAt, requested_bytes: u64) -> Result<ResizePlan, ResizeError> {
    let metadata = fat::read_metadata(device)?;
    if !metadata.report.issues.is_empty() {
        return Err(ResizeError::InconsistentSource(
            metadata.report.issues.clone(),
        ));
    }
    let bpb = &metadata.report.bpb;
    let normalized_bytes = normalize_size(requested_bytes, bpb.bytes_per_sector)?;
    if normalized_bytes > device.len() {
        return Err(ResizeError::TargetTooLarge);
    }
    let target = calculate_geometry(bpb, normalized_bytes)?;
    let map = allocate_clusters(&metadata, target.cluster_count)?;
    let mappings = map
        .iter()
        .map(|(&source, &target)| ClusterMapping { source, target })
        .collect::<Vec<_>>();
    let fat_patches = build_fat_patches(&metadata, &map)?;
    let directory_patches = build_directory_patches(&metadata, &map, &target);
    let root_cluster = (bpb.fat_type == FatType::Fat32).then(|| {
        (
            bpb.root_cluster,
            *map.get(&bpb.root_cluster)
                .expect("validated root cluster is allocated"),
        )
    });
    let data_moves = build_data_moves(&metadata, &map, &target);
    let moves = schedule_moves(&data_moves);
    let recovery = build_recovery(device, &metadata, &target, &directory_patches)?;
    let preconditions = vec![
        Precondition::SourceIdentity {
            volume_id: bpb.volume_id,
            boot_checksum: recovery.source.boot_checksum,
            total_bytes: metadata.report.layout.total_bytes,
        },
        Precondition::CleanFilesystem,
        Precondition::FatType(bpb.fat_type),
        Precondition::ExclusiveUnmountedAccess,
        Precondition::DeviceLengthAtLeast(normalized_bytes),
        Precondition::RecoveryRecordDurable,
    ];
    let operations = vec![
        Operation::VerifyPreconditions,
        Operation::PersistRecoveryRecord {
            sectors: recovery.backed_up_sectors() as u32,
        },
        Operation::MoveData {
            steps: moves.steps.len() as u32,
            scratch_bytes: moves.scratch_bytes,
        },
        Operation::WriteFat {
            entries: fat_patches.len() as u32,
            copies: bpb.fat_count,
        },
        Operation::PatchDirectoryEntries {
            entries: directory_patches.len() as u32,
        },
        Operation::UpdateReservedMetadata,
        Operation::CommitBootGeometry,
        Operation::ResizeContainer {
            bytes: normalized_bytes,
        },
        Operation::MarkRecoveryComplete,
    ];
    Ok(ResizePlan {
        requested_bytes,
        normalized_bytes,
        source_total_bytes: metadata.report.layout.total_bytes,
        target,
        mappings,
        fat_patches,
        directory_patches,
        root_cluster,
        moves,
        preconditions,
        operations,
        recovery,
    })
}

fn allocate_clusters(
    metadata: &Metadata,
    target_count: u32,
) -> Result<BTreeMap<u32, u32>, ResizeError> {
    let mut allocated = Vec::new();
    for cluster in 2..metadata.fat.cluster_count() + 2 {
        match metadata.fat.entry(cluster).expect("cluster in range") {
            Entry::Free => {}
            Entry::Next(_) | Entry::End => allocated.push(cluster),
            Entry::Bad => {
                return Err(ResizeError::UnsupportedAllocation {
                    cluster,
                    kind: "bad-cluster",
                });
            }
            Entry::Reserved(_) => {
                return Err(ResizeError::UnsupportedAllocation {
                    cluster,
                    kind: "reserved",
                });
            }
        }
    }
    if allocated.len() as u32 > target_count {
        return Err(ResizeError::InsufficientSpace {
            allocated: allocated.len() as u32,
            available: target_count,
        });
    }
    let target_end = target_count + 2;
    let preserved: BTreeSet<u32> = allocated
        .iter()
        .copied()
        .filter(|cluster| *cluster < target_end)
        .collect();
    let mut free_targets = (2..target_end).filter(|cluster| !preserved.contains(cluster));
    let mut result = BTreeMap::new();
    for source in allocated {
        let target = if source < target_end {
            source
        } else {
            free_targets.next().ok_or(ResizeError::InsufficientSpace {
                allocated: result.len() as u32 + 1,
                available: target_count,
            })?
        };
        result.insert(source, target);
    }
    Ok(result)
}

fn build_fat_patches(
    metadata: &Metadata,
    map: &BTreeMap<u32, u32>,
) -> Result<Vec<FatPatch>, ResizeError> {
    let mut patches = Vec::with_capacity(map.len());
    for (&source, &target) in map {
        let value = match metadata.fat.entry(source).expect("mapped cluster in range") {
            Entry::Next(next) => FatValue::Next(*map.get(&next).ok_or_else(|| {
                ResizeError::InconsistentSource(vec![format!(
                    "cluster {source} links to unallocated cluster {next}"
                )])
            })?),
            Entry::End => FatValue::End,
            _ => unreachable!("only allocated regular entries are mapped"),
        };
        patches.push(FatPatch {
            cluster: target,
            value,
        });
    }
    patches.sort_by_key(|patch| patch.cluster);
    Ok(patches)
}

fn build_directory_patches(
    metadata: &Metadata,
    map: &BTreeMap<u32, u32>,
    target: &TargetGeometry,
) -> Vec<DirectoryPatch> {
    let mut patches = metadata
        .directory_references
        .iter()
        .filter_map(|entry| {
            let &new = map.get(&entry.first_cluster)?;
            (new != entry.first_cluster).then(|| DirectoryPatch {
                path: entry.path.clone(),
                source_entry_offset: entry.entry_offset,
                entry_offset: translate_metadata_offset(metadata, map, target, entry.entry_offset),
                old_first_cluster: entry.first_cluster,
                new_first_cluster: new,
            })
        })
        .collect::<Vec<_>>();
    patches.sort_by(|left, right| {
        left.entry_offset
            .cmp(&right.entry_offset)
            .then_with(|| left.path.cmp(&right.path))
    });
    patches
}

fn translate_metadata_offset(
    metadata: &Metadata,
    map: &BTreeMap<u32, u32>,
    target: &TargetGeometry,
    source_offset: u64,
) -> u64 {
    let bpb = &metadata.report.bpb;
    let layout = &metadata.report.layout;
    let sector_bytes = u64::from(bpb.bytes_per_sector);
    let data_start = u64::from(layout.first_data_sector) * sector_bytes;
    if source_offset >= data_start {
        let relative = source_offset - data_start;
        let cluster_size = u64::from(layout.cluster_size);
        let source_cluster = 2 + (relative / cluster_size) as u32;
        if let Some(&target_cluster) = map.get(&source_cluster) {
            return (u64::from(target.first_data_sector)
                + u64::from(target_cluster - 2) * u64::from(bpb.sectors_per_cluster))
                * sector_bytes
                + relative % cluster_size;
        }
    } else if bpb.fat_type == FatType::Fat16 {
        let source_root = u64::from(layout.first_root_sector) * sector_bytes;
        if source_offset >= source_root {
            let target_root = u64::from(bpb.reserved_sectors)
                + u64::from(bpb.fat_count) * u64::from(target.sectors_per_fat);
            return target_root * sector_bytes + (source_offset - source_root);
        }
    }
    source_offset
}

fn build_data_moves(
    metadata: &Metadata,
    map: &BTreeMap<u32, u32>,
    target: &TargetGeometry,
) -> Vec<DataMove> {
    let bpb = &metadata.report.bpb;
    let layout = &metadata.report.layout;
    let sector_bytes = u64::from(bpb.bytes_per_sector);
    let cluster_moves = map
        .iter()
        .filter_map(|(&source, &destination)| {
            let source_offset = (u64::from(layout.first_data_sector)
                + u64::from(source - 2) * u64::from(bpb.sectors_per_cluster))
                * sector_bytes;
            let target_offset = (u64::from(target.first_data_sector)
                + u64::from(destination - 2) * u64::from(bpb.sectors_per_cluster))
                * sector_bytes;
            (source_offset != target_offset).then_some(DataMove {
                source_cluster: source,
                target_cluster: destination,
                source_offset,
                target_offset,
                length: layout.cluster_size,
            })
        })
        .collect::<Vec<_>>();
    let mut moves = coalesce_moves(cluster_moves);
    if bpb.fat_type == FatType::Fat16 {
        let source_offset = u64::from(layout.first_root_sector) * sector_bytes;
        let target_offset = (u64::from(bpb.reserved_sectors)
            + u64::from(bpb.fat_count) * u64::from(target.sectors_per_fat))
            * sector_bytes;
        if source_offset != target_offset {
            moves.push(DataMove {
                source_cluster: 0,
                target_cluster: 0,
                source_offset,
                target_offset,
                length: layout.root_dir_sectors * u32::from(bpb.bytes_per_sector),
            });
        }
    }
    moves
}

fn coalesce_moves(moves: Vec<DataMove>) -> Vec<DataMove> {
    let mut coalesced: Vec<DataMove> = Vec::with_capacity(moves.len());
    for movement in moves {
        if let Some(previous) = coalesced.last_mut() {
            let source_contiguous = previous
                .source_offset
                .checked_add(u64::from(previous.length))
                == Some(movement.source_offset);
            let target_contiguous = previous
                .target_offset
                .checked_add(u64::from(previous.length))
                == Some(movement.target_offset);
            if source_contiguous
                && target_contiguous
                && let Some(length) = previous.length.checked_add(movement.length)
            {
                previous.length = length;
                continue;
            }
        }
        coalesced.push(movement);
    }
    coalesced
}

fn build_recovery(
    device: &impl ReadAt,
    metadata: &Metadata,
    target: &TargetGeometry,
    directory_patches: &[DirectoryPatch],
) -> Result<RecoveryRecord, ResizeError> {
    let bpb = &metadata.report.bpb;
    let layout = &metadata.report.layout;
    let bytes_per_sector = u64::from(bpb.bytes_per_sector);
    let mut sector_numbers = BTreeSet::new();
    sector_numbers.insert(0);
    if let Some(sector) = bpb.fs_info_sector {
        sector_numbers.insert(u64::from(sector));
    }
    if let Some(sector) = bpb.backup_boot_sector {
        sector_numbers.insert(u64::from(sector));
    }
    for copy in 0..bpb.fat_count {
        let target_start =
            u64::from(bpb.reserved_sectors) + u64::from(copy) * u64::from(target.sectors_per_fat);
        for sector in target_start..target_start + u64::from(target.sectors_per_fat) {
            sector_numbers.insert(sector);
        }
        let source_start =
            u64::from(bpb.reserved_sectors) + u64::from(copy) * u64::from(bpb.sectors_per_fat);
        for sector in source_start..source_start + u64::from(bpb.sectors_per_fat) {
            sector_numbers.insert(sector);
        }
    }
    if bpb.fat_type == FatType::Fat16 {
        let target_root = u64::from(bpb.reserved_sectors)
            + u64::from(bpb.fat_count) * u64::from(target.sectors_per_fat);
        for sector in target_root..target_root + u64::from(target.root_dir_sectors) {
            sector_numbers.insert(sector);
        }
        for sector in u64::from(layout.first_root_sector)..u64::from(layout.first_data_sector) {
            sector_numbers.insert(sector);
        }
    }
    for patch in directory_patches {
        sector_numbers.insert(patch.entry_offset / bytes_per_sector);
        sector_numbers.insert(patch.source_entry_offset / bytes_per_sector);
    }
    let mut boot = vec![0; usize::from(bpb.bytes_per_sector)];
    device.read_at(0, &mut boot)?;
    let mut sectors = Vec::with_capacity(sector_numbers.len());
    for sector in sector_numbers {
        let offset = sector
            .checked_mul(bytes_per_sector)
            .ok_or(ResizeError::GeometryOverflow)?;
        let mut bytes = vec![0; usize::from(bpb.bytes_per_sector)];
        device.read_at(offset, &mut bytes)?;
        sectors.push(SectorBackup::new(sector, bytes));
    }
    Ok(RecoveryRecord {
        source: SourceIdentity {
            volume_id: bpb.volume_id,
            volume_bytes: layout.total_bytes,
            boot_checksum: recovery::checksum(&boot),
        },
        geometry: SourceGeometry {
            fat_type: bpb.fat_type,
            bytes_per_sector: bpb.bytes_per_sector,
            sectors_per_cluster: bpb.sectors_per_cluster,
            reserved_sectors: bpb.reserved_sectors,
            fat_count: bpb.fat_count,
            sectors_per_fat: bpb.sectors_per_fat,
            first_data_sector: layout.first_data_sector,
            cluster_count: layout.cluster_count,
            total_sectors: bpb.total_sectors,
        },
        phase: RecoveryPhase::Planned,
        sectors,
    })
}

impl ResizePlan {
    pub fn to_text(&self) -> String {
        let mut output = String::new();
        let direction = if self.normalized_bytes < self.source_total_bytes {
            "shrink"
        } else if self.normalized_bytes > self.source_total_bytes {
            "grow"
        } else {
            "unchanged"
        };
        let _ = writeln!(output, "Resize plan ({direction}, dry-run)");
        let _ = writeln!(output, "Requested bytes: {}", self.requested_bytes);
        let _ = writeln!(output, "Normalized bytes: {}", self.normalized_bytes);
        let _ = writeln!(output, "FAT type: {}", self.target.fat_type.name());
        let _ = writeln!(output, "Target sectors: {}", self.target.total_sectors);
        let _ = writeln!(output, "Target clusters: {}", self.target.cluster_count);
        let _ = writeln!(output, "FAT sectors: {}", self.target.sectors_per_fat);
        let relocated = self
            .mappings
            .iter()
            .filter(|mapping| mapping.source != mapping.target)
            .count();
        let _ = writeln!(output, "Relocated clusters: {relocated}");
        let _ = writeln!(output, "Data move steps: {}", self.moves.steps.len());
        let _ = writeln!(output, "Scratch bytes: {}", self.moves.scratch_bytes);
        let _ = writeln!(
            output,
            "Recovery sectors: {}",
            self.recovery.backed_up_sectors()
        );
        for (index, operation) in self.operations.iter().enumerate() {
            let _ = writeln!(output, "{}. {operation:?}", index + 1);
        }
        output
    }

    pub fn to_json(&self) -> String {
        let mappings = self
            .mappings
            .iter()
            .map(|mapping| {
                format!(
                    "{{\"source\":{},\"target\":{}}}",
                    mapping.source, mapping.target
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let operations = self
            .operations
            .iter()
            .map(|operation| format!("\"{}\"", json_escape(&format!("{operation:?}"))))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            concat!(
                "{{\"dry_run\":true,\"requested_bytes\":{},\"normalized_bytes\":{},",
                "\"source_bytes\":{},\"fat_type\":\"{}\",\"target_sectors\":{},",
                "\"target_clusters\":{},\"sectors_per_fat\":{},\"scratch_bytes\":{},",
                "\"recovery_sectors\":{},\"mappings\":[{}],\"operations\":[{}]}}"
            ),
            self.requested_bytes,
            self.normalized_bytes,
            self.source_total_bytes,
            self.target.fat_type.name(),
            self.target.total_sectors,
            self.target.cluster_count,
            self.target.sectors_per_fat,
            self.moves.scratch_bytes,
            self.recovery.backed_up_sectors(),
            mappings,
            operations
        )
    }
}

fn json_escape(value: &str) -> String {
    value
        .chars()
        .flat_map(|character| match character {
            '"' => "\\\"".chars().collect::<Vec<_>>(),
            '\\' => "\\\\".chars().collect(),
            '\n' => "\\n".chars().collect(),
            '\r' => "\\r".chars().collect(),
            '\t' => "\\t".chars().collect(),
            value => vec![value],
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ReadAt;
    use crate::fat::Bpb;
    use std::io;

    struct Memory(Vec<u8>);

    impl ReadAt for Memory {
        fn len(&self) -> u64 {
            self.0.len() as u64
        }

        fn read_at(&self, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
            let start = offset as usize;
            let end = start + buffer.len();
            let source = self
                .0
                .get(start..end)
                .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
            buffer.copy_from_slice(source);
            Ok(())
        }
    }

    fn fat16_image(chain: &[u32]) -> Memory {
        const VOLUME_SECTORS: u16 = 10_000;
        const DEVICE_SECTORS: usize = 12_000;
        const FAT_SECTORS: usize = 40;
        const ROOT_SECTOR: usize = 1 + FAT_SECTORS * 2;
        let mut image = vec![0_u8; DEVICE_SECTORS * 512];
        let boot = &mut image[..512];
        boot[..3].copy_from_slice(&[0xeb, 0x3c, 0x90]);
        boot[3..11].copy_from_slice(b"MSDOS5.0");
        boot[11..13].copy_from_slice(&512_u16.to_le_bytes());
        boot[13] = 1;
        boot[14..16].copy_from_slice(&1_u16.to_le_bytes());
        boot[16] = 2;
        boot[17..19].copy_from_slice(&32_u16.to_le_bytes());
        boot[19..21].copy_from_slice(&VOLUME_SECTORS.to_le_bytes());
        boot[21] = 0xf8;
        boot[22..24].copy_from_slice(&(FAT_SECTORS as u16).to_le_bytes());
        boot[38] = 0x29;
        boot[39..43].copy_from_slice(&0x1234_5678_u32.to_le_bytes());
        boot[43..54].copy_from_slice(b"PLANTEST   ");
        boot[510..512].copy_from_slice(&[0x55, 0xaa]);
        for copy in 0..2 {
            let fat = (1 + copy * FAT_SECTORS) * 512;
            image[fat..fat + 2].copy_from_slice(&0xfff8_u16.to_le_bytes());
            image[fat + 2..fat + 4].copy_from_slice(&0xffff_u16.to_le_bytes());
            for (index, &cluster) in chain.iter().enumerate() {
                let value = chain.get(index + 1).copied().unwrap_or(0xffff) as u16;
                let offset = fat + cluster as usize * 2;
                image[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
            }
        }
        let root = ROOT_SECTOR * 512;
        image[root..root + 11].copy_from_slice(b"PAYLOAD BIN");
        image[root + 11] = 0x20;
        if let Some(&first) = chain.first() {
            image[root + 26..root + 28].copy_from_slice(&(first as u16).to_le_bytes());
            image[root + 28..root + 32]
                .copy_from_slice(&((chain.len() * 512) as u32).to_le_bytes());
        }
        Memory(image)
    }

    fn bpb(fat_type: FatType) -> Bpb {
        Bpb {
            fat_type,
            bytes_per_sector: 512,
            sectors_per_cluster: 1,
            reserved_sectors: if fat_type == FatType::Fat16 { 1 } else { 32 },
            fat_count: 2,
            root_entry_count: if fat_type == FatType::Fat16 { 512 } else { 0 },
            total_sectors: 70_000,
            sectors_per_fat: 600,
            media: 0xf8,
            hidden_sectors: 0,
            root_cluster: 2,
            fs_info_sector: (fat_type == FatType::Fat32).then_some(1),
            backup_boot_sector: (fat_type == FatType::Fat32).then_some(6),
            mirroring_enabled: true,
            active_fat: 0,
            volume_id: 1,
            volume_label: String::new(),
        }
    }

    #[test]
    fn geometry_converges_and_rejects_transitions() {
        let fat32 = calculate_geometry(&bpb(FatType::Fat32), 80_000 * 512).unwrap();
        assert!(fat32.cluster_count > 65_524);
        assert!(fat32.sectors_per_fat >= (fat32.cluster_count + 2) * 4 / 512);
        assert!(matches!(
            calculate_geometry(&bpb(FatType::Fat32), 20_000 * 512),
            Err(ResizeError::FatTypeTransition { .. })
        ));

        let mut fat16 = bpb(FatType::Fat16);
        fat16.total_sectors = 10_000;
        let target = calculate_geometry(&fat16, 12_345 * 512).unwrap();
        assert_eq!(target.total_bytes, 12_345 * 512);
        assert_eq!(target.fat_type, FatType::Fat16);
        assert!(matches!(
            calculate_geometry(&fat16, 12_345 * 512 + 17),
            Err(ResizeError::TargetUnaligned {
                requested: 6_320_657,
                alignment: 512
            })
        ));

        let one_gib = calculate_geometry(&bpb(FatType::Fat32), 1 << 30).unwrap();
        assert_eq!(one_gib.total_bytes, 1 << 30);
        assert_eq!(one_gib.total_sectors, (1 << 30) / 512);
    }

    #[test]
    fn grow_and_shrink_plans_are_deterministic_and_relocate() {
        let image = fat16_image(&[9_000]);
        let shrink = plan(&image, 5_000 * 512).unwrap();
        assert_eq!(shrink.normalized_bytes, 5_000 * 512);
        assert_eq!(
            shrink
                .mappings
                .iter()
                .find(|mapping| mapping.source == 9_000)
                .unwrap()
                .target,
            2
        );
        assert_eq!(shrink.directory_patches[0].new_first_cluster, 2);
        assert_eq!(
            shrink.to_json(),
            plan(&image, 5_000 * 512).unwrap().to_json()
        );
        assert!(matches!(
            plan(&image, 5_000 * 512 + 99),
            Err(ResizeError::TargetUnaligned { .. })
        ));

        let grow = plan(&image, 11_000 * 512).unwrap();
        assert_eq!(grow.target.total_sectors, 11_000);
        assert!(
            grow.mappings
                .iter()
                .all(|mapping| mapping.source == mapping.target)
        );
    }

    #[test]
    fn rejects_target_with_too_few_clusters() {
        let chain = (2..5_002).collect::<Vec<_>>();
        let image = fat16_image(&chain);
        assert!(matches!(
            plan(&image, 4_500 * 512),
            Err(ResizeError::InsufficientSpace { .. })
        ));
    }

    #[test]
    fn adjacent_cluster_moves_are_coalesced_into_extents() {
        let moves = vec![
            DataMove {
                source_cluster: 2,
                target_cluster: 10,
                source_offset: 1_024,
                target_offset: 5_120,
                length: 512,
            },
            DataMove {
                source_cluster: 3,
                target_cluster: 11,
                source_offset: 1_536,
                target_offset: 5_632,
                length: 512,
            },
            DataMove {
                source_cluster: 5,
                target_cluster: 20,
                source_offset: 2_560,
                target_offset: 10_240,
                length: 512,
            },
        ];

        let result = coalesce_moves(moves);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].length, 1_024);
        assert_eq!(result[1].length, 512);
    }
}
