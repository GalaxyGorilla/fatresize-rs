use crate::fat::{Bpb, FatType};

use super::ResizeError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetGeometry {
    pub fat_type: FatType,
    pub bytes_per_sector: u16,
    pub sectors_per_cluster: u8,
    pub reserved_sectors: u16,
    pub fat_count: u8,
    pub root_dir_sectors: u32,
    pub sectors_per_fat: u32,
    pub first_data_sector: u32,
    pub data_sectors: u32,
    pub cluster_count: u32,
    pub total_sectors: u32,
    pub total_bytes: u64,
}

pub fn normalize_size(requested_bytes: u64, bytes_per_sector: u16) -> Result<u64, ResizeError> {
    let sector = u64::from(bytes_per_sector);
    if requested_bytes == 0 {
        return Err(ResizeError::TargetTooSmall);
    }
    if !requested_bytes.is_multiple_of(sector) {
        return Err(ResizeError::TargetUnaligned {
            requested: requested_bytes,
            alignment: sector,
        });
    }
    Ok(requested_bytes)
}

pub fn calculate(bpb: &Bpb, requested_bytes: u64) -> Result<TargetGeometry, ResizeError> {
    let total_bytes = normalize_size(requested_bytes, bpb.bytes_per_sector)?;
    let total_sectors = u32::try_from(total_bytes / u64::from(bpb.bytes_per_sector))
        .map_err(|_| ResizeError::TargetTooLarge)?;
    let root_dir_sectors = u32::from(bpb.root_entry_count)
        .checked_mul(32)
        .ok_or(ResizeError::GeometryOverflow)?
        .div_ceil(u32::from(bpb.bytes_per_sector));
    let fixed = u32::from(bpb.reserved_sectors)
        .checked_add(root_dir_sectors)
        .ok_or(ResizeError::GeometryOverflow)?;
    if total_sectors <= fixed + u32::from(bpb.fat_count) {
        return Err(ResizeError::TargetTooSmall);
    }

    let entry_bytes = match bpb.fat_type {
        FatType::Fat16 => 2_u64,
        FatType::Fat32 => 4,
    };
    let max_fat = (total_sectors - fixed) / u32::from(bpb.fat_count);
    let sufficient = |fat_sectors: u32| {
        let metadata = fixed.saturating_add(u32::from(bpb.fat_count) * fat_sectors);
        if metadata >= total_sectors {
            return false;
        }
        let clusters = (total_sectors - metadata) / u32::from(bpb.sectors_per_cluster);
        let required =
            (u64::from(clusters + 2) * entry_bytes).div_ceil(u64::from(bpb.bytes_per_sector));
        u64::from(fat_sectors) >= required
    };
    let mut low = 1_u32;
    let mut high = max_fat;
    while low < high {
        let mid = low + (high - low) / 2;
        if sufficient(mid) {
            high = mid;
        } else {
            low = mid + 1;
        }
    }
    if !sufficient(low) {
        return Err(ResizeError::TargetTooSmall);
    }
    let sectors_per_fat = low;
    if bpb.fat_type == FatType::Fat16 && sectors_per_fat > u32::from(u16::MAX) {
        return Err(ResizeError::GeometryOverflow);
    }
    let first_data_sector = fixed
        .checked_add(u32::from(bpb.fat_count) * sectors_per_fat)
        .ok_or(ResizeError::GeometryOverflow)?;
    let data_sectors = total_sectors - first_data_sector;
    let cluster_count = data_sectors / u32::from(bpb.sectors_per_cluster);
    let calculated_type = if cluster_count < 4_085 {
        return Err(ResizeError::Fat12Unsupported);
    } else if cluster_count <= 65_524 {
        FatType::Fat16
    } else {
        FatType::Fat32
    };
    if calculated_type != bpb.fat_type {
        return Err(ResizeError::FatTypeTransition {
            source: bpb.fat_type,
            target: calculated_type,
        });
    }
    Ok(TargetGeometry {
        fat_type: bpb.fat_type,
        bytes_per_sector: bpb.bytes_per_sector,
        sectors_per_cluster: bpb.sectors_per_cluster,
        reserved_sectors: bpb.reserved_sectors,
        fat_count: bpb.fat_count,
        root_dir_sectors,
        sectors_per_fat,
        first_data_sector,
        data_sectors,
        cluster_count,
        total_sectors,
        total_bytes,
    })
}
