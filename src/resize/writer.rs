//! Conservative execution of an already validated resize plan.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;

use crate::device::{ReadAt, WriteAt};
use crate::fat::{self, FatType};

use super::{FatValue, MoveStep, RecoveryPhase, RecoveryRecord, ResizePlan, recovery::checksum};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteStage {
    Recovery,
    Data,
    Fat,
    Directories,
    ReservedMetadata,
    Boot,
    Verify,
    Complete,
}

pub fn execute(
    volume: &impl WriteAt,
    plan: &ResizePlan,
    recovery_path: &Path,
    mut progress: impl FnMut(WriteStage),
) -> io::Result<()> {
    validate_plan_bounds(volume, plan)?;
    // Capture the active source FAT before any data move can overlap old
    // metadata sectors.
    let target_fat = build_fat(volume, plan)?;
    if recovery_path.exists() {
        let record = load_record(recovery_path)?;
        if record.phase != RecoveryPhase::Planned || record.source != plan.recovery.source {
            return Err(invalid_data(
                "existing recovery record does not match this resize plan",
            ));
        }
    } else {
        prepare(volume, plan, recovery_path)?;
        load_record(recovery_path)?;
    }
    progress(WriteStage::Recovery);

    super::recovery::persist_phase(recovery_path, RecoveryPhase::DataMoving)?;
    execute_moves(volume, plan)?;
    volume.sync()?;
    progress(WriteStage::Data);

    super::recovery::persist_phase(recovery_path, RecoveryPhase::MetadataUpdating)?;
    write_fats(volume, plan, &target_fat)?;
    volume.sync()?;
    progress(WriteStage::Fat);
    patch_directories(volume, plan)?;
    volume.sync()?;
    progress(WriteStage::Directories);
    write_reserved_metadata(volume, plan)?;
    volume.sync()?;
    progress(WriteStage::ReservedMetadata);
    write_boot_metadata(volume, plan)?;
    volume.sync()?;
    progress(WriteStage::Boot);

    let report = fat::inspect(volume)?;
    if !report.is_consistent()
        || report.bpb.total_sectors != plan.target.total_sectors
        || report.bpb.sectors_per_fat != plan.target.sectors_per_fat
        || report.layout.cluster_count != plan.target.cluster_count
    {
        return Err(invalid_data("post-resize FAT verification failed"));
    }
    progress(WriteStage::Verify);
    super::recovery::persist_phase(recovery_path, RecoveryPhase::Complete)?;
    progress(WriteStage::Complete);
    Ok(())
}

/// Persists the complete recovery data before a caller performs a container
/// growth (for example, updating a partition table).
pub fn prepare(volume: &impl ReadAt, plan: &ResizePlan, recovery_path: &Path) -> io::Result<()> {
    validate_plan_bounds(volume, plan)?;
    if recovery_path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "recovery path already exists; choose a new --backup path",
        ));
    }
    let mut record = plan.recovery.clone();
    add_overwritten_data_to_recovery(volume, plan, &mut record)?;
    persist_new_record(recovery_path, &record)
}

fn add_overwritten_data_to_recovery(
    volume: &impl ReadAt,
    plan: &ResizePlan,
    record: &mut RecoveryRecord,
) -> io::Result<()> {
    let sector_bytes = u64::from(record.geometry.bytes_per_sector);
    let mut sectors = record
        .sectors
        .iter()
        .flat_map(|backup| {
            let count = backup.bytes.len() as u64 / sector_bytes;
            backup.sector..backup.sector + count
        })
        .collect::<std::collections::BTreeSet<_>>();
    for step in &plan.moves.steps {
        let (offset, length) = match *step {
            MoveStep::Copy {
                target_offset,
                length,
                ..
            }
            | MoveStep::RestoreFromScratch {
                target_offset,
                length,
                ..
            } => (target_offset, u64::from(length)),
            MoveStep::SaveToScratch { .. } => continue,
        };
        if !offset.is_multiple_of(sector_bytes) || !length.is_multiple_of(sector_bytes) {
            return Err(invalid_data("data move is not sector aligned"));
        }
        backup_range(
            volume,
            offset / sector_bytes,
            (offset + length) / sector_bytes,
            sector_bytes,
            &mut sectors,
            record,
        )?;
    }
    if plan.target.fat_type == FatType::Fat32 {
        let fsinfo = bpb_u16(volume, record.geometry.bytes_per_sector, 48)?;
        let backup = bpb_u16(volume, record.geometry.bytes_per_sector, 50)?;
        let backup_info = u32::from(fsinfo) + u32::from(backup);
        if fsinfo != 0
            && fsinfo != u16::MAX
            && backup != 0
            && backup != u16::MAX
            && backup_info < u32::from(record.geometry.reserved_sectors)
            && sectors.insert(u64::from(backup_info))
        {
            let mut bytes = vec![0; sector_bytes as usize];
            volume.read_at(u64::from(backup_info) * sector_bytes, &mut bytes)?;
            record
                .sectors
                .push(super::SectorBackup::new(u64::from(backup_info), bytes));
        }
    }
    // A container shrink discards this tail. Backing it up makes restoration
    // reconstruct the complete source filesystem rather than metadata alone.
    if plan.normalized_bytes < record.source.volume_bytes {
        backup_range(
            volume,
            plan.normalized_bytes / sector_bytes,
            record.source.volume_bytes / sector_bytes,
            sector_bytes,
            &mut sectors,
            record,
        )?;
    }
    record.sectors.sort_by_key(|backup| backup.sector);
    Ok(())
}

fn backup_range(
    volume: &impl ReadAt,
    start_sector: u64,
    end_sector: u64,
    sector_bytes: u64,
    seen: &mut std::collections::BTreeSet<u64>,
    record: &mut RecoveryRecord,
) -> io::Result<()> {
    const MAX_READ_BYTES: u64 = 8 * 1024 * 1024;

    let max_sectors = (MAX_READ_BYTES / sector_bytes).max(1);
    let mut run_start = None;
    let mut run_end = start_sector;
    for sector in start_sector..end_sector {
        if seen.insert(sector) {
            if run_start.is_none() {
                run_start = Some(sector);
            }
            run_end = sector + 1;
            if run_end - run_start.expect("set above") >= max_sectors {
                backup_run(
                    volume,
                    run_start.take().expect("set above"),
                    run_end,
                    sector_bytes,
                    record,
                )?;
            }
        } else if let Some(start) = run_start.take() {
            backup_run(volume, start, run_end, sector_bytes, record)?;
        }
    }
    if let Some(start) = run_start {
        backup_run(volume, start, run_end, sector_bytes, record)?;
    }
    Ok(())
}

fn backup_run(
    volume: &impl ReadAt,
    start_sector: u64,
    end_sector: u64,
    sector_bytes: u64,
    record: &mut RecoveryRecord,
) -> io::Result<()> {
    let byte_count = end_sector
        .checked_sub(start_sector)
        .and_then(|count| count.checked_mul(sector_bytes))
        .and_then(|count| usize::try_from(count).ok())
        .ok_or_else(|| invalid_data("recovery backup range overflows"))?;
    let mut bytes = vec![0; byte_count];
    volume.read_at(start_sector * sector_bytes, &mut bytes)?;
    record
        .sectors
        .push(super::SectorBackup::new(start_sector, bytes));
    Ok(())
}

pub fn load_record(path: &Path) -> io::Result<RecoveryRecord> {
    let mut bytes = Vec::new();
    File::open(path)?.read_to_end(&mut bytes)?;
    RecoveryRecord::deserialize(&bytes).map_err(|error| invalid_data_owned(error.to_string()))
}

pub fn restore(
    volume: &impl WriteAt,
    recovery_path: &Path,
    force_completed: bool,
) -> io::Result<()> {
    let record = load_record(recovery_path)?;
    if record.phase == RecoveryPhase::Complete && !force_completed {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "recovery record is complete; use --force-yes to restore it",
        ));
    }
    let boot = record
        .sectors
        .iter()
        .find(|sector| sector.sector == 0)
        .ok_or_else(|| invalid_data("recovery record has no boot sector"))?;
    if checksum(&boot.bytes) != record.source.boot_checksum
        || volume_id(&boot.bytes, record.geometry.fat_type)? != record.source.volume_id
    {
        return Err(invalid_data("recovery source identity is inconsistent"));
    }
    let mut current = vec![0; usize::from(record.geometry.bytes_per_sector)];
    volume.read_at(0, &mut current)?;
    if volume_id(&current, record.geometry.fat_type)? != record.source.volume_id {
        return Err(invalid_data(
            "selected volume does not match recovery source",
        ));
    }
    let sector_bytes = u64::from(record.geometry.bytes_per_sector);
    for sector in &record.sectors {
        if checksum(&sector.bytes) != sector.checksum {
            return Err(invalid_data("recovery sector checksum mismatch"));
        }
        let offset = sector
            .sector
            .checked_mul(sector_bytes)
            .ok_or_else(|| invalid_data("recovery sector offset overflows"))?;
        checked_range(volume.len(), offset, sector.bytes.len())?;
    }
    // Restore all non-boot sectors durably before committing the primary boot
    // sector. An interrupted restore can therefore be safely repeated.
    for sector in record.sectors.iter().filter(|sector| sector.sector != 0) {
        volume.write_at(sector.sector * sector_bytes, &sector.bytes)?;
    }
    volume.sync()?;
    volume.write_at(0, &boot.bytes)?;
    volume.sync()
}

fn validate_plan_bounds(volume: &impl ReadAt, plan: &ResizePlan) -> io::Result<()> {
    if plan.normalized_bytes > volume.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "target filesystem exceeds selected volume",
        ));
    }
    for step in &plan.moves.steps {
        match *step {
            MoveStep::SaveToScratch {
                source_offset,
                length,
                ..
            } => checked_range(volume.len(), source_offset, length as usize)?,
            MoveStep::Copy {
                source_offset,
                target_offset,
                length,
            } => {
                checked_range(volume.len(), source_offset, length as usize)?;
                checked_range(volume.len(), target_offset, length as usize)?;
            }
            MoveStep::RestoreFromScratch {
                target_offset,
                length,
                ..
            } => checked_range(volume.len(), target_offset, length as usize)?,
        }
    }
    for patch in &plan.directory_patches {
        checked_range(volume.len(), patch.entry_offset, 32)?;
    }
    Ok(())
}

fn execute_moves(volume: &impl WriteAt, plan: &ResizePlan) -> io::Result<()> {
    let count = plan
        .moves
        .steps
        .iter()
        .filter_map(|step| match step {
            MoveStep::SaveToScratch { slot, .. } => Some(*slot as usize + 1),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    let mut scratch: Vec<Option<Vec<u8>>> = vec![None; count];
    for step in &plan.moves.steps {
        match *step {
            MoveStep::SaveToScratch {
                source_offset,
                length,
                slot,
            } => {
                let mut bytes = vec![0; length as usize];
                volume.read_at(source_offset, &mut bytes)?;
                scratch[slot as usize] = Some(bytes);
            }
            MoveStep::Copy {
                source_offset,
                target_offset,
                length,
            } => {
                let mut bytes = vec![0; length as usize];
                volume.read_at(source_offset, &mut bytes)?;
                volume.write_at(target_offset, &bytes)?;
            }
            MoveStep::RestoreFromScratch {
                target_offset,
                length,
                slot,
            } => {
                let bytes = scratch
                    .get_mut(slot as usize)
                    .and_then(Option::take)
                    .ok_or_else(|| invalid_data("move references an empty scratch slot"))?;
                if bytes.len() != length as usize {
                    return Err(invalid_data("scratch slot length mismatch"));
                }
                volume.write_at(target_offset, &bytes)?;
            }
        }
    }
    Ok(())
}

fn build_fat(volume: &impl ReadAt, plan: &ResizePlan) -> io::Result<Vec<u8>> {
    let geometry = &plan.recovery.geometry;
    let sector_bytes = usize::from(geometry.bytes_per_sector);
    let old_len = usize::try_from(u64::from(geometry.sectors_per_fat) * sector_bytes as u64)
        .map_err(|_| invalid_data("source FAT is too large"))?;
    let source_offset = (u64::from(geometry.reserved_sectors)
        + u64::from(active_fat(volume, geometry)?) * u64::from(geometry.sectors_per_fat))
        * sector_bytes as u64;
    let mut source = vec![0; old_len];
    volume.read_at(source_offset, &mut source)?;

    let target_len = usize::try_from(u64::from(plan.target.sectors_per_fat) * sector_bytes as u64)
        .map_err(|_| invalid_data("target FAT is too large"))?;
    let mut target = vec![0_u8; target_len];
    match geometry.fat_type {
        FatType::Fat16 => target[..4].copy_from_slice(&source[..4]),
        FatType::Fat32 => target[..8].copy_from_slice(&source[..8]),
    }
    for patch in &plan.fat_patches {
        let source_cluster = plan
            .mappings
            .iter()
            .find(|mapping| mapping.target == patch.cluster)
            .ok_or_else(|| invalid_data("FAT patch has no cluster mapping"))?
            .source;
        let value = match patch.value {
            FatValue::Next(next) => next,
            FatValue::End => match geometry.fat_type {
                FatType::Fat16 => 0xffff,
                FatType::Fat32 => 0x0fff_ffff,
            },
        };
        match geometry.fat_type {
            FatType::Fat16 => {
                let offset = patch.cluster as usize * 2;
                target
                    .get_mut(offset..offset + 2)
                    .ok_or_else(|| invalid_data("FAT16 patch exceeds target FAT"))?
                    .copy_from_slice(&(value as u16).to_le_bytes());
            }
            FatType::Fat32 => {
                let old_offset = source_cluster as usize * 4;
                let high = u32::from_le_bytes(
                    source
                        .get(old_offset..old_offset + 4)
                        .ok_or_else(|| invalid_data("FAT32 source entry is out of range"))?
                        .try_into()
                        .unwrap(),
                ) & 0xf000_0000;
                let offset = patch.cluster as usize * 4;
                target
                    .get_mut(offset..offset + 4)
                    .ok_or_else(|| invalid_data("FAT32 patch exceeds target FAT"))?
                    .copy_from_slice(&(high | value).to_le_bytes());
            }
        }
    }
    Ok(target)
}

fn write_fats(volume: &impl WriteAt, plan: &ResizePlan, target: &[u8]) -> io::Result<()> {
    let sector_bytes = u64::from(plan.target.bytes_per_sector);
    for copy in 0..plan.target.fat_count {
        let offset = (u64::from(plan.target.reserved_sectors)
            + u64::from(copy) * u64::from(plan.target.sectors_per_fat))
            * sector_bytes;
        volume.write_at(offset, target)?;
    }
    Ok(())
}

fn active_fat(volume: &impl ReadAt, geometry: &super::SourceGeometry) -> io::Result<u8> {
    if geometry.fat_type == FatType::Fat16 {
        return Ok(0);
    }
    let mut boot = vec![0; usize::from(geometry.bytes_per_sector)];
    volume.read_at(0, &mut boot)?;
    let flags = u16::from_le_bytes(boot[40..42].try_into().unwrap());
    Ok(if flags & 0x80 == 0 {
        0
    } else {
        (flags & 0x0f) as u8
    })
}

fn patch_directories(volume: &impl WriteAt, plan: &ResizePlan) -> io::Result<()> {
    for patch in &plan.directory_patches {
        let mut entry = [0_u8; 32];
        volume.read_at(patch.entry_offset, &mut entry)?;
        let current = u32::from(u16::from_le_bytes(entry[26..28].try_into().unwrap()))
            | if plan.target.fat_type == FatType::Fat32 {
                u32::from(u16::from_le_bytes(entry[20..22].try_into().unwrap())) << 16
            } else {
                0
            };
        if current != patch.old_first_cluster {
            return Err(invalid_data("directory first-cluster precondition failed"));
        }
        entry[26..28].copy_from_slice(&(patch.new_first_cluster as u16).to_le_bytes());
        if plan.target.fat_type == FatType::Fat32 {
            entry[20..22].copy_from_slice(&((patch.new_first_cluster >> 16) as u16).to_le_bytes());
        }
        volume.write_at(patch.entry_offset, &entry)?;
    }
    Ok(())
}

fn write_reserved_metadata(volume: &impl WriteAt, plan: &ResizePlan) -> io::Result<()> {
    if plan.target.fat_type != FatType::Fat32 {
        return Ok(());
    }
    let geometry = &plan.recovery.geometry;
    let fsinfo = bpb_u16(volume, geometry.bytes_per_sector, 48)?;
    if fsinfo == 0 || fsinfo == u16::MAX {
        return Err(invalid_data("FAT32 filesystem has no FSInfo sector"));
    }
    let mut info = vec![0; usize::from(geometry.bytes_per_sector)];
    let sector_bytes = u64::from(geometry.bytes_per_sector);
    volume.read_at(u64::from(fsinfo) * sector_bytes, &mut info)?;
    if info[..4] != [0x52, 0x52, 0x61, 0x41]
        || info[484..488] != [0x72, 0x72, 0x41, 0x61]
        || info[508..512] != [0, 0, 0x55, 0xaa]
    {
        return Err(invalid_data("FAT32 FSInfo signatures changed"));
    }
    let free = plan.target.cluster_count - plan.mappings.len() as u32;
    let used = plan
        .mappings
        .iter()
        .map(|mapping| mapping.target)
        .collect::<std::collections::BTreeSet<_>>();
    let next = (2..plan.target.cluster_count + 2)
        .find(|cluster| !used.contains(cluster))
        .unwrap_or(u32::MAX);
    info[488..492].copy_from_slice(&free.to_le_bytes());
    info[492..496].copy_from_slice(&next.to_le_bytes());
    volume.write_at(u64::from(fsinfo) * sector_bytes, &info)?;
    let backup = bpb_u16(volume, geometry.bytes_per_sector, 50)?;
    let backup_info = u32::from(backup) + u32::from(fsinfo);
    if backup != 0 && backup != u16::MAX && backup_info < u32::from(geometry.reserved_sectors) {
        volume.write_at(u64::from(backup_info) * sector_bytes, &info)?;
    }
    Ok(())
}

fn write_boot_metadata(volume: &impl WriteAt, plan: &ResizePlan) -> io::Result<()> {
    let mut boot = vec![0; usize::from(plan.target.bytes_per_sector)];
    volume.read_at(0, &mut boot)?;
    if boot[510..512] != [0x55, 0xaa] {
        return Err(invalid_data("boot-sector signature changed"));
    }
    if plan.target.fat_type == FatType::Fat16 && plan.target.total_sectors <= u32::from(u16::MAX) {
        boot[19..21].copy_from_slice(&(plan.target.total_sectors as u16).to_le_bytes());
        boot[32..36].fill(0);
    } else {
        boot[19..21].fill(0);
        boot[32..36].copy_from_slice(&plan.target.total_sectors.to_le_bytes());
    }
    match plan.target.fat_type {
        FatType::Fat16 => {
            let length = u16::try_from(plan.target.sectors_per_fat)
                .map_err(|_| invalid_data("FAT16 length exceeds BPB field"))?;
            boot[22..24].copy_from_slice(&length.to_le_bytes());
        }
        FatType::Fat32 => {
            boot[22..24].fill(0);
            boot[36..40].copy_from_slice(&plan.target.sectors_per_fat.to_le_bytes());
            if let Some((_, root)) = plan.root_cluster {
                boot[44..48].copy_from_slice(&root.to_le_bytes());
            }
        }
    }
    volume.write_at(0, &boot)?;
    if plan.target.fat_type == FatType::Fat32 {
        let backup = u16::from_le_bytes(boot[50..52].try_into().unwrap());
        if backup != 0 && backup != u16::MAX && backup < plan.target.reserved_sectors {
            volume.write_at(
                u64::from(backup) * u64::from(plan.target.bytes_per_sector),
                &boot,
            )?;
        }
    }
    Ok(())
}

fn bpb_u16(volume: &impl ReadAt, sector_size: u16, offset: usize) -> io::Result<u16> {
    let mut boot = vec![0; usize::from(sector_size)];
    volume.read_at(0, &mut boot)?;
    Ok(u16::from_le_bytes(
        boot[offset..offset + 2].try_into().unwrap(),
    ))
}

fn persist_new_record(path: &Path, record: &RecoveryRecord) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(&record.serialize())?;
    file.sync_all()?;
    drop(file);
    File::open(parent)?.sync_all()
}

fn volume_id(boot: &[u8], fat_type: FatType) -> io::Result<u32> {
    let offset = match fat_type {
        FatType::Fat16 => 39,
        FatType::Fat32 => 67,
    };
    boot.get(offset..offset + 4)
        .ok_or_else(|| invalid_data("boot sector is too short"))
        .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()))
}

fn checked_range(len: u64, offset: u64, count: usize) -> io::Result<()> {
    let end = offset
        .checked_add(count as u64)
        .ok_or_else(|| invalid_data("I/O range overflows"))?;
    if end > len {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "I/O extends beyond selected volume",
        ));
    }
    Ok(())
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn invalid_data_owned(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
