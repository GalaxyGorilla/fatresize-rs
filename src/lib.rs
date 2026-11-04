//! Clean-room building blocks for the `fatresize` command-line program.

pub mod cli;
pub mod device;
pub mod error;
pub mod fat;
pub mod partition;
pub mod resize;
pub mod size;

use std::io;
use std::path::{Path, PathBuf};

pub use device::{Device, ReadAt, SECTOR_SIZE, Subrange, WriteAt};
pub use error::{CliError, Error, SizeParseError};
pub use fat::{
    Bpb, DirectoryEntry, DirectoryReference, FatType, Layout, Metadata, Report, ResizeConstraints,
    inspect, read_metadata, summarize,
};
pub use partition::{Gpt, GptHeader, GptPartition, Mbr, MbrPartition};
pub use size::Size;

pub fn default_recovery_path(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!("{name}.fatresize-recovery"))
}

/// Creates a durable, read-only recovery snapshot without resizing.
pub fn backup_path(path: &Path, partition: Option<u32>, recovery_path: &Path) -> Result<(), Error> {
    if recovery_path.exists() {
        return Err(
            io::Error::new(io::ErrorKind::AlreadyExists, "recovery path already exists").into(),
        );
    }
    let device = Device::open_read_only(path)?;
    if let Some(number) = partition {
        let volume = selected_partition(&device, number as usize)?;
        let metadata = fat::read_metadata(&volume)?;
        let plan = resize::plan(&volume, metadata.report.layout.total_bytes)?;
        resize::writer::prepare(&volume, &plan, recovery_path)?;
    } else {
        let metadata = fat::read_metadata(&device)?;
        let plan = resize::plan(&device, metadata.report.layout.total_bytes)?;
        resize::writer::prepare(&device, &plan, recovery_path)?;
    }
    Ok(())
}

/// Resizes a FAT volume, growing its regular-file/partition container before
/// filesystem writes and shrinking it only after filesystem verification.
pub fn resize_path(
    path: &Path,
    partition: Option<u32>,
    requested_bytes: u64,
    recovery_path: &Path,
    progress: impl FnMut(resize::writer::WriteStage),
) -> Result<(), Error> {
    if recovery_path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "recovery path already exists; choose a new --backup path",
        )
        .into());
    }
    let mut device = Device::open_writable(path)?;
    ensure_raw_unmounted(&device)?;
    if let Some(number) = partition {
        return resize_partition_path(
            &device,
            number as usize,
            requested_bytes,
            recovery_path,
            progress,
        );
    }
    let requested_bytes = if requested_bytes == Size::MIN {
        resize::minimum_bytes(&device)?
    } else {
        requested_bytes
    };
    let requested_bytes = if requested_bytes == Size::MAX {
        device.len()
    } else {
        requested_bytes
    };
    let plan = if requested_bytes > device.len() {
        if !device.is_regular_file() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "raw device is smaller than requested filesystem",
            )
            .into());
        }
        let extended = ZeroExtended {
            inner: &device,
            len: requested_bytes,
        };
        let plan = resize::plan(&extended, requested_bytes)?;
        resize::writer::prepare(&extended, &plan, recovery_path)?;
        device.set_len(requested_bytes)?;
        plan
    } else {
        resize::plan(&device, requested_bytes)?
    };
    resize::writer::execute(&device, &plan, recovery_path, progress)?;
    if plan.normalized_bytes < device.len() && device.is_regular_file() {
        device.set_len(plan.normalized_bytes)?;
    }
    Ok(())
}

struct ZeroExtended<'a> {
    inner: &'a Device,
    len: u64,
}

impl ReadAt for ZeroExtended<'_> {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
        let end = offset
            .checked_add(buffer.len() as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "read range overflows"))?;
        if end > self.len {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        let available = self
            .inner
            .len()
            .saturating_sub(offset)
            .min(buffer.len() as u64) as usize;
        if available != 0 {
            self.inner.read_at(offset, &mut buffer[..available])?;
        }
        buffer[available..].fill(0);
        Ok(())
    }
}

pub fn restore_path(
    path: &Path,
    partition: Option<u32>,
    recovery_path: &Path,
    force_completed: bool,
) -> Result<(), Error> {
    let record = resize::writer::load_record(recovery_path)?;
    let mut device = Device::open_writable(path)?;
    ensure_raw_unmounted(&device)?;
    if let Some(number) = partition {
        ensure_partition_size(&device, number as usize, record.source.volume_bytes)?;
        let view = selected_partition(&device, number as usize)?;
        resize::writer::restore(&view, recovery_path, force_completed)?;
        let mbr = Mbr::read(&device)?;
        if mbr
            .partitions
            .iter()
            .any(|entry| entry.partition_type == 0xee)
        {
            set_gpt_size(&device, number as usize, record.source.volume_bytes)?;
        } else {
            set_mbr_size(&device, number as usize, record.source.volume_bytes)?;
        }
    } else {
        if record.source.volume_bytes > device.len() {
            if !device.is_regular_file() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "raw device is smaller than recovery source",
                )
                .into());
            }
            device.set_len(record.source.volume_bytes)?;
        }
        resize::writer::restore(&device, recovery_path, force_completed)?;
        if device.is_regular_file() && device.len() != record.source.volume_bytes {
            device.set_len(record.source.volume_bytes)?;
        }
    }
    Ok(())
}

fn ensure_raw_unmounted(device: &Device) -> Result<(), Error> {
    if device.is_regular_file() {
        return Ok(());
    }
    #[cfg(not(target_os = "linux"))]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "raw-device writes require Linux mounted-device detection",
    )
    .into());
    #[cfg(target_os = "linux")]
    if device.is_mounted()? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refusing to write a mounted block device",
        )
        .into());
    }
    Ok(())
}

fn resize_partition_path(
    device: &Device,
    number: usize,
    requested_bytes: u64,
    recovery_path: &Path,
    progress: impl FnMut(resize::writer::WriteStage),
) -> Result<(), Error> {
    let mbr = Mbr::read(device)?;
    if mbr
        .partitions
        .iter()
        .any(|entry| entry.partition_type == 0xee)
    {
        let gpt = Gpt::read_with_block_size(device, device.logical_sector_size())?;
        let entry = gpt.partition(number).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "GPT partition does not exist")
        })?;
        let block = u64::from(gpt.block_size());
        let current = entry
            .last_lba
            .checked_sub(entry.first_lba)
            .and_then(|blocks| blocks.checked_add(1))
            .and_then(|blocks| blocks.checked_mul(block))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "GPT size overflows"))?;
        let requested_bytes = if requested_bytes == Size::MIN {
            let current_view = Subrange::new(device, entry.first_lba * block, current)?;
            resize::minimum_bytes(&current_view)?
                .div_ceil(block)
                .checked_mul(block)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "GPT size overflows"))?
        } else {
            requested_bytes
        };
        let capacity_end = gpt
            .partitions
            .iter()
            .flatten()
            .filter(|other| other.first_lba > entry.first_lba)
            .map(|other| other.first_lba - 1)
            .min()
            .unwrap_or(gpt.primary_header.last_usable_lba);
        let capacity = capacity_end
            .checked_sub(entry.first_lba)
            .and_then(|blocks| blocks.checked_add(1))
            .and_then(|blocks| blocks.checked_mul(block))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "GPT capacity overflows"))?;
        let requested_bytes = if requested_bytes == Size::MAX {
            capacity
        } else {
            requested_bytes
        };
        if !requested_bytes.is_multiple_of(block) {
            return Err(resize::ResizeError::TargetUnaligned {
                requested: requested_bytes,
                alignment: block,
            }
            .into());
        }
        if requested_bytes > capacity {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "requested size crosses the next GPT partition or metadata boundary",
            )
            .into());
        }
        let planning_len = current.max(requested_bytes / block * block);
        let view = Subrange::new(device, entry.first_lba * block, planning_len)?;
        let plan = resize::plan(&view, requested_bytes)?;
        if !plan.normalized_bytes.is_multiple_of(block) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "filesystem size is not aligned to the GPT logical block size",
            )
            .into());
        }
        resize::writer::prepare(&view, &plan, recovery_path)?;
        if plan.normalized_bytes > current {
            set_gpt_size(device, number, plan.normalized_bytes)?;
        }
        let execution = Subrange::new(
            device,
            entry.first_lba * block,
            current.max(plan.normalized_bytes),
        )?;
        resize::writer::execute(&execution, &plan, recovery_path, progress)?;
        if plan.normalized_bytes < current {
            set_gpt_size(device, number, plan.normalized_bytes)?;
        }
        return Ok(());
    }

    let entry = *mbr
        .partition(number)
        .filter(|entry| !entry.is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "MBR partition does not exist"))?;
    let current = u64::from(entry.sectors) * SECTOR_SIZE;
    let requested_bytes = if requested_bytes == Size::MIN {
        let offset = u64::from(entry.first_lba) * SECTOR_SIZE;
        let current_view = Subrange::new(device, offset, current)?;
        resize::minimum_bytes(&current_view)?
            .div_ceil(SECTOR_SIZE)
            .checked_mul(SECTOR_SIZE)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "MBR size overflows"))?
    } else {
        requested_bytes
    };
    let capacity_end = mbr
        .partitions
        .iter()
        .filter(|other| !other.is_empty() && other.first_lba > entry.first_lba)
        .map(|other| u64::from(other.first_lba))
        .min()
        .unwrap_or(device.len() / SECTOR_SIZE);
    let capacity = capacity_end
        .checked_sub(u64::from(entry.first_lba))
        .and_then(|sectors| sectors.checked_mul(SECTOR_SIZE))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "MBR capacity overflows"))?;
    let requested_bytes = if requested_bytes == Size::MAX {
        capacity
    } else {
        requested_bytes
    };
    if requested_bytes > capacity {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "requested size crosses the next MBR partition or device boundary",
        )
        .into());
    }
    let planning_len = current.max(requested_bytes / SECTOR_SIZE * SECTOR_SIZE);
    let offset = u64::from(entry.first_lba) * SECTOR_SIZE;
    let view = Subrange::new(device, offset, planning_len)?;
    let plan = resize::plan(&view, requested_bytes)?;
    if !plan.normalized_bytes.is_multiple_of(SECTOR_SIZE) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "filesystem size is not aligned to an MBR sector",
        )
        .into());
    }
    resize::writer::prepare(&view, &plan, recovery_path)?;
    if plan.normalized_bytes > current {
        set_mbr_size(device, number, plan.normalized_bytes)?;
    }
    let execution = Subrange::new(device, offset, current.max(plan.normalized_bytes))?;
    resize::writer::execute(&execution, &plan, recovery_path, progress)?;
    if plan.normalized_bytes < current {
        set_mbr_size(device, number, plan.normalized_bytes)?;
    }
    Ok(())
}

fn selected_partition(device: &Device, number: usize) -> io::Result<Subrange<'_, Device>> {
    let mbr = Mbr::read(device)?;
    if mbr
        .partitions
        .iter()
        .any(|entry| entry.partition_type == 0xee)
    {
        return Gpt::read_with_block_size(device, device.logical_sector_size())?
            .partition_view(device, number);
    }
    mbr.partition_view(device, number)
}

fn ensure_partition_size(device: &Device, number: usize, bytes: u64) -> Result<(), Error> {
    let view = selected_partition(device, number)?;
    if view.len() < bytes {
        let mbr = Mbr::read(device)?;
        if mbr
            .partitions
            .iter()
            .any(|entry| entry.partition_type == 0xee)
        {
            set_gpt_size(device, number, bytes)?;
        } else {
            set_mbr_size(device, number, bytes)?;
        }
    }
    Ok(())
}

fn set_mbr_size(device: &Device, number: usize, bytes: u64) -> io::Result<()> {
    if !bytes.is_multiple_of(SECTOR_SIZE) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unaligned MBR size",
        ));
    }
    let mut mbr = Mbr::read(device)?;
    let mut entry = *mbr
        .partition(number)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "MBR partition does not exist"))?;
    entry.sectors = u32::try_from(bytes / SECTOR_SIZE)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "MBR size exceeds 32 bits"))?;
    mbr.set_partition(number, entry)?;
    mbr.write(device)?;
    device.sync()
}

fn set_gpt_size(device: &Device, number: usize, bytes: u64) -> io::Result<()> {
    let mut gpt = Gpt::read_with_block_size(device, device.logical_sector_size())?;
    let block = u64::from(gpt.block_size());
    if !bytes.is_multiple_of(block) || bytes == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unaligned GPT size",
        ));
    }
    let first = gpt
        .partition(number)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "GPT partition does not exist"))?
        .first_lba;
    let last = first
        .checked_add(bytes / block - 1)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "GPT size overflows"))?;
    gpt.resize_partition(number, last)?;
    gpt.write(device)
}

/// Opens a path read-only and builds a resize plan for a raw volume or selected
/// partition.
pub fn plan_path(
    path: impl AsRef<Path>,
    partition: Option<u32>,
    requested_bytes: u64,
) -> Result<resize::ResizePlan, resize::ResizeError> {
    let device = Device::open_read_only(path)?;
    if let Some(number) = partition {
        let mbr = Mbr::read(&device)?;
        if mbr
            .partitions
            .iter()
            .any(|entry| entry.partition_type == 0xee)
        {
            let gpt = Gpt::read_with_block_size(&device, device.logical_sector_size())?;
            let entry = gpt.partition(number as usize).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "GPT partition does not exist")
            })?;
            let block = u64::from(gpt.block_size());
            let current = (entry.last_lba - entry.first_lba + 1)
                .checked_mul(block)
                .ok_or(resize::ResizeError::GeometryOverflow)?;
            let requested_bytes = if requested_bytes == Size::MIN {
                let current_view = Subrange::new(&device, entry.first_lba * block, current)?;
                resize::minimum_bytes(&current_view)?
                    .div_ceil(block)
                    .checked_mul(block)
                    .ok_or(resize::ResizeError::GeometryOverflow)?
            } else {
                requested_bytes
            };
            let capacity_end = gpt
                .partitions
                .iter()
                .flatten()
                .filter(|other| other.first_lba > entry.first_lba)
                .map(|other| other.first_lba - 1)
                .min()
                .unwrap_or(gpt.primary_header.last_usable_lba);
            let capacity = (capacity_end - entry.first_lba + 1)
                .checked_mul(block)
                .ok_or(resize::ResizeError::GeometryOverflow)?;
            let target = if requested_bytes == Size::MAX {
                capacity
            } else {
                requested_bytes
            };
            if !target.is_multiple_of(block) {
                return Err(resize::ResizeError::TargetUnaligned {
                    requested: target,
                    alignment: block,
                });
            }
            if target > capacity {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "requested size crosses the next GPT partition or metadata boundary",
                )
                .into());
            }
            let view = Subrange::new(&device, entry.first_lba * block, current.max(target))?;
            return resize::plan(&view, target);
        }
        let entry = *mbr
            .partition(number as usize)
            .filter(|entry| !entry.is_empty())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "MBR partition does not exist")
            })?;
        let current = u64::from(entry.sectors) * SECTOR_SIZE;
        let requested_bytes = if requested_bytes == Size::MIN {
            let offset = u64::from(entry.first_lba) * SECTOR_SIZE;
            let current_view = Subrange::new(&device, offset, current)?;
            resize::minimum_bytes(&current_view)?
                .div_ceil(SECTOR_SIZE)
                .checked_mul(SECTOR_SIZE)
                .ok_or(resize::ResizeError::GeometryOverflow)?
        } else {
            requested_bytes
        };
        let capacity_end = mbr
            .partitions
            .iter()
            .filter(|other| !other.is_empty() && other.first_lba > entry.first_lba)
            .map(|other| u64::from(other.first_lba))
            .min()
            .unwrap_or(device.len() / SECTOR_SIZE);
        let capacity = capacity_end
            .checked_sub(u64::from(entry.first_lba))
            .and_then(|sectors| sectors.checked_mul(SECTOR_SIZE))
            .ok_or(resize::ResizeError::GeometryOverflow)?;
        let target = if requested_bytes == Size::MAX {
            capacity
        } else {
            requested_bytes
        };
        if target > capacity {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "requested size crosses the next MBR partition or device boundary",
            )
            .into());
        }
        let view = Subrange::new(
            &device,
            u64::from(entry.first_lba) * SECTOR_SIZE,
            current.max(target),
        )?;
        return resize::plan(&view, target);
    }
    let requested_bytes = if requested_bytes == Size::MIN {
        resize::minimum_bytes(&device)?
    } else {
        requested_bytes
    };
    let target = if requested_bytes == Size::MAX {
        device.len()
    } else {
        requested_bytes
    };
    if target > device.len() {
        if !device.is_regular_file() {
            return Err(resize::ResizeError::TargetTooLarge);
        }
        return resize::plan(
            &ZeroExtended {
                inner: &device,
                len: target,
            },
            target,
        );
    }
    resize::plan(&device, target)
}

/// Opens a path read-only and inspects a raw FAT volume or a selected MBR/GPT
/// partition. With no partition number, a raw volume is preferred; otherwise
/// exactly one successfully parsed partition is required.
pub fn inspect_path(path: impl AsRef<Path>, partition: Option<u32>) -> io::Result<Report> {
    let device = Device::open_read_only(path)?;
    if let Some(number) = partition {
        return inspect_partition(&device, number as usize);
    }
    if let Ok(report) = fat::inspect(&device) {
        return Ok(report);
    }
    inspect_unique_partition(&device)
}

/// Opens a path read-only and returns fast geometry/allocation information
/// without the full directory and ownership traversal performed by
/// [`inspect_path`].
pub fn summarize_path(path: impl AsRef<Path>, partition: Option<u32>) -> io::Result<Report> {
    let device = Device::open_read_only(path)?;
    if let Some(number) = partition {
        return summarize_partition(&device, number as usize);
    }
    if let Ok(report) = fat::summarize(&device) {
        return Ok(report);
    }
    summarize_unique_partition(&device)
}

fn summarize_partition(device: &Device, number: usize) -> io::Result<Report> {
    let mbr = Mbr::read(device)?;
    if mbr
        .partitions
        .iter()
        .any(|entry| entry.partition_type == 0xee)
    {
        let gpt = Gpt::read_with_block_size(device, device.logical_sector_size())?;
        return fat::summarize(&gpt.partition_view(device, number)?);
    }
    fat::summarize(&mbr.partition_view(device, number)?)
}

fn summarize_unique_partition(device: &Device) -> io::Result<Report> {
    let mbr = Mbr::read(device)?;
    let mut reports = Vec::new();
    if mbr
        .partitions
        .iter()
        .any(|entry| entry.partition_type == 0xee)
    {
        let gpt = Gpt::read_with_block_size(device, device.logical_sector_size())?;
        for number in 1..=gpt.partitions.len() {
            if let Ok(view) = gpt.partition_view(device, number)
                && let Ok(report) = fat::summarize(&view)
            {
                reports.push(report);
            }
        }
    } else {
        for number in 1..=4 {
            let Some(entry) = mbr.partition(number) else {
                continue;
            };
            if !matches!(entry.partition_type, 0x04 | 0x06 | 0x0b | 0x0c | 0x0e) {
                continue;
            }
            if let Ok(view) = mbr.partition_view(device, number)
                && let Ok(report) = fat::summarize(&view)
            {
                reports.push(report);
            }
        }
    }
    if reports.len() == 1 {
        Ok(reports.remove(0))
    } else {
        Err(fat::no_fat_found())
    }
}

fn inspect_partition(device: &Device, number: usize) -> io::Result<Report> {
    let mbr = Mbr::read(device)?;
    if mbr
        .partitions
        .iter()
        .any(|entry| entry.partition_type == 0xee)
    {
        let gpt = Gpt::read_with_block_size(device, device.logical_sector_size())?;
        return fat::inspect(&gpt.partition_view(device, number)?);
    }
    fat::inspect(&mbr.partition_view(device, number)?)
}

fn inspect_unique_partition(device: &Device) -> io::Result<Report> {
    let mbr = Mbr::read(device)?;
    let mut reports = Vec::new();
    if mbr
        .partitions
        .iter()
        .any(|entry| entry.partition_type == 0xee)
    {
        let gpt = Gpt::read_with_block_size(device, device.logical_sector_size())?;
        for number in 1..=gpt.partitions.len() {
            if let Ok(view) = gpt.partition_view(device, number)
                && let Ok(report) = fat::inspect(&view)
            {
                reports.push(report);
            }
        }
    } else {
        for number in 1..=4 {
            let Some(entry) = mbr.partition(number) else {
                continue;
            };
            if !matches!(entry.partition_type, 0x04 | 0x06 | 0x0b | 0x0c | 0x0e) {
                continue;
            }
            if let Ok(view) = mbr.partition_view(device, number)
                && let Ok(report) = fat::inspect(&view)
            {
                reports.push(report);
            }
        }
    }
    if reports.len() == 1 {
        Ok(reports.remove(0))
    } else {
        Err(fat::no_fat_found())
    }
}
