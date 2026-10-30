use std::io;

use crate::device::{ReadAt, invalid_data};

pub const FAT16_MAX_CLUSTERS: u32 = 65_524;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FatType {
    Fat16,
    Fat32,
}

impl FatType {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Fat16 => "FAT16",
            Self::Fat32 => "FAT32",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bpb {
    pub fat_type: FatType,
    pub bytes_per_sector: u16,
    pub sectors_per_cluster: u8,
    pub reserved_sectors: u16,
    pub fat_count: u8,
    pub root_entry_count: u16,
    pub total_sectors: u32,
    pub sectors_per_fat: u32,
    pub media: u8,
    pub hidden_sectors: u32,
    pub root_cluster: u32,
    pub fs_info_sector: Option<u16>,
    pub backup_boot_sector: Option<u16>,
    pub mirroring_enabled: bool,
    pub active_fat: u8,
    pub volume_id: u32,
    pub volume_label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub root_dir_sectors: u32,
    pub first_fat_sector: u32,
    pub first_root_sector: u32,
    pub first_data_sector: u32,
    pub data_sectors: u32,
    pub cluster_count: u32,
    pub cluster_size: u32,
    pub total_bytes: u64,
}

impl Bpb {
    pub fn read(device: &impl ReadAt) -> io::Result<(Self, Layout, Vec<u8>)> {
        let mut prefix = [0; 512];
        device.read_at(0, &mut prefix)?;
        let bytes_per_sector = le16(&prefix[11..13]);
        if !matches!(bytes_per_sector, 512 | 1024 | 2048 | 4096) {
            return Err(invalid_data("invalid FAT bytes-per-sector"));
        }
        let mut sector = vec![0; usize::from(bytes_per_sector)];
        device.read_at(0, &mut sector)?;
        if sector[510..512] != [0x55, 0xaa] {
            return Err(invalid_data("missing FAT boot-sector signature"));
        }
        let jump_ok = sector[0] == 0xeb && sector[2] == 0x90 || sector[0] == 0xe9;
        if !jump_ok {
            return Err(invalid_data("invalid FAT boot jump instruction"));
        }
        let sectors_per_cluster = sector[13];
        if sectors_per_cluster == 0 || !sectors_per_cluster.is_power_of_two() {
            return Err(invalid_data("invalid FAT sectors-per-cluster"));
        }
        let cluster_size = u32::from(bytes_per_sector)
            .checked_mul(u32::from(sectors_per_cluster))
            .ok_or_else(|| invalid_data("FAT cluster size overflows"))?;
        if cluster_size > 128 * 1024 {
            return Err(invalid_data("unsupported FAT cluster size"));
        }
        let reserved_sectors = le16(&sector[14..16]);
        let fat_count = sector[16];
        let root_entry_count = le16(&sector[17..19]);
        let total16 = le16(&sector[19..21]);
        let media = sector[21];
        let fat16_size = le16(&sector[22..24]);
        let hidden_sectors = le32(&sector[28..32]);
        let total32 = le32(&sector[32..36]);
        if reserved_sectors == 0 || fat_count == 0 || fat_count > 8 {
            return Err(invalid_data(
                "invalid FAT reserved-sector or FAT-copy count",
            ));
        }
        if media != 0xf0 && media < 0xf8 {
            return Err(invalid_data("invalid FAT media descriptor"));
        }
        let total_sectors = match (total16, total32) {
            (0, 0) => return Err(invalid_data("FAT total-sector count is zero")),
            (a, 0) => u32::from(a),
            (0, b) => b,
            _ => return Err(invalid_data("both FAT total-sector fields are nonzero")),
        };
        let preliminary_fat_size = if fat16_size != 0 {
            u32::from(fat16_size)
        } else {
            le32(&sector[36..40])
        };
        if preliminary_fat_size == 0 {
            return Err(invalid_data("FAT size is zero"));
        }
        let root_dir_bytes = u32::from(root_entry_count)
            .checked_mul(32)
            .ok_or_else(|| invalid_data("FAT root directory size overflows"))?;
        let root_dir_sectors = root_dir_bytes.div_ceil(u32::from(bytes_per_sector));
        let first_root_sector = u32::from(reserved_sectors)
            .checked_add(
                u32::from(fat_count)
                    .checked_mul(preliminary_fat_size)
                    .ok_or_else(|| invalid_data("FAT layout overflows"))?,
            )
            .ok_or_else(|| invalid_data("FAT layout overflows"))?;
        let first_data_sector = first_root_sector
            .checked_add(root_dir_sectors)
            .ok_or_else(|| invalid_data("FAT layout overflows"))?;
        if first_data_sector >= total_sectors {
            return Err(invalid_data("FAT metadata consumes the volume"));
        }
        let data_sectors = total_sectors - first_data_sector;
        let cluster_count = data_sectors / u32::from(sectors_per_cluster);
        let fat_type = if cluster_count < 4_085 {
            return Err(invalid_data("FAT12 is not supported"));
        } else if cluster_count <= FAT16_MAX_CLUSTERS {
            FatType::Fat16
        } else {
            FatType::Fat32
        };
        let (
            sectors_per_fat,
            root_cluster,
            fs_info_sector,
            backup_boot_sector,
            mirroring_enabled,
            active_fat,
            volume_offset,
        ) = match fat_type {
            FatType::Fat16 => {
                if fat16_size == 0 || root_entry_count == 0 {
                    return Err(invalid_data("FAT16 BPB has FAT32-only geometry"));
                }
                (u32::from(fat16_size), 0, None, None, true, 0, 39)
            }
            FatType::Fat32 => {
                if fat16_size != 0 || root_entry_count != 0 || reserved_sectors < 2 {
                    return Err(invalid_data("FAT32 BPB has FAT16-only geometry"));
                }
                if le16(&sector[40..42]) & 0xff70 != 0 {
                    return Err(invalid_data("invalid FAT32 extended flags"));
                }
                if le16(&sector[42..44]) != 0 {
                    return Err(invalid_data("unsupported FAT32 version"));
                }
                let flags = le16(&sector[40..42]);
                let mirroring = flags & 0x80 == 0;
                let active = if mirroring { 0 } else { (flags & 0x0f) as u8 };
                if active >= fat_count {
                    return Err(invalid_data("FAT32 active FAT index is out of range"));
                }
                let root = le32(&sector[44..48]);
                if root < 2 || root >= cluster_count + 2 {
                    return Err(invalid_data("FAT32 root cluster is out of range"));
                }
                let info = le16(&sector[48..50]);
                let backup = le16(&sector[50..52]);
                for value in [info, backup] {
                    if value != 0 && value != 0xffff && value >= reserved_sectors {
                        return Err(invalid_data(
                            "FAT32 reserved metadata sector is out of range",
                        ));
                    }
                }
                (
                    preliminary_fat_size,
                    root,
                    (info != 0xffff && info != 0).then_some(info),
                    (backup != 0xffff && backup != 0).then_some(backup),
                    mirroring,
                    active,
                    67,
                )
            }
        };
        let fat_entries = u64::from(sectors_per_fat) * u64::from(bytes_per_sector)
            / match fat_type {
                FatType::Fat16 => 2,
                FatType::Fat32 => 4,
            };
        if fat_entries < u64::from(cluster_count) + 2 {
            return Err(invalid_data("FAT table is too small for data clusters"));
        }
        let total_bytes = u64::from(total_sectors) * u64::from(bytes_per_sector);
        if total_bytes > device.len() {
            return Err(invalid_data("FAT volume extends beyond selected device"));
        }
        let label = String::from_utf8_lossy(&sector[volume_offset + 4..volume_offset + 15])
            .trim_end()
            .to_owned();
        let bpb = Self {
            fat_type,
            bytes_per_sector,
            sectors_per_cluster,
            reserved_sectors,
            fat_count,
            root_entry_count,
            total_sectors,
            sectors_per_fat,
            media,
            hidden_sectors,
            root_cluster,
            fs_info_sector,
            backup_boot_sector,
            mirroring_enabled,
            active_fat,
            volume_id: le32(&sector[volume_offset..volume_offset + 4]),
            volume_label: label,
        };
        let layout = Layout {
            root_dir_sectors,
            first_fat_sector: u32::from(reserved_sectors),
            first_root_sector,
            first_data_sector,
            data_sectors,
            cluster_count,
            cluster_size,
            total_bytes,
        };
        Ok((bpb, layout, sector))
    }
}

fn le16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes(bytes.try_into().expect("fixed field"))
}
fn le32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().expect("fixed field"))
}
