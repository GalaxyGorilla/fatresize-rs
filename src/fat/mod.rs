//! Read-only FAT16/FAT32 inspection based on the published FAT on-disk format.

mod boot;
mod directory;
mod table;

use std::fmt::Write as _;
use std::io;

use crate::device::{ReadAt, invalid_data};

pub use boot::{Bpb, FatType, Layout};
pub use directory::{DirectoryEntry, DirectoryReference};
pub use table::{ChainError, Entry, FatTable};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsInfo {
    pub free_clusters: Option<u32>,
    pub next_free: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResizeConstraints {
    pub minimum_bytes: u64,
    pub current_bytes: u64,
    pub maximum_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct Report {
    pub bpb: Bpb,
    pub layout: Layout,
    pub fs_info: Option<FsInfo>,
    pub allocated_clusters: u32,
    pub free_clusters: u32,
    pub bad_clusters: u32,
    pub highest_allocated_cluster: Option<u32>,
    pub directories_and_files: Vec<DirectoryEntry>,
    pub resize: ResizeConstraints,
    pub issues: Vec<String>,
    pub fully_checked: bool,
}

/// Validated, read-only metadata used by the resize planner.
#[derive(Debug, Clone)]
pub struct Metadata {
    pub report: Report,
    pub fat: FatTable,
    pub directory_references: Vec<DirectoryReference>,
}

impl Report {
    pub fn is_consistent(&self) -> bool {
        self.issues.is_empty()
    }

    pub const fn is_fully_checked(&self) -> bool {
        self.fully_checked
    }

    pub fn to_text(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "Filesystem: {}", self.bpb.fat_type.name());
        let _ = writeln!(out, "Volume label: {}", self.bpb.volume_label);
        let _ = writeln!(out, "Volume ID: {:08X}", self.bpb.volume_id);
        let _ = writeln!(out, "Bytes per sector: {}", self.bpb.bytes_per_sector);
        let _ = writeln!(out, "Sectors per cluster: {}", self.bpb.sectors_per_cluster);
        let _ = writeln!(out, "Total sectors: {}", self.bpb.total_sectors);
        let _ = writeln!(out, "Clusters: {}", self.layout.cluster_count);
        let _ = writeln!(out, "Allocated clusters: {}", self.allocated_clusters);
        let _ = writeln!(out, "Free clusters: {}", self.free_clusters);
        let _ = writeln!(out, "Bad clusters: {}", self.bad_clusters);
        let highest = self
            .highest_allocated_cluster
            .map_or_else(|| "none".to_owned(), |value| value.to_string());
        let _ = writeln!(out, "Highest allocated cluster: {highest}");
        let _ = writeln!(out, "Entries: {}", self.directories_and_files.len());
        let _ = writeln!(out, "Minimum size: {}", self.resize.minimum_bytes);
        let _ = writeln!(out, "Current size: {}", self.resize.current_bytes);
        let _ = writeln!(out, "Maximum size: {}", self.resize.maximum_bytes);
        let _ = writeln!(
            out,
            "Status: {}",
            if !self.fully_checked {
                "summary only (use --check)"
            } else if self.is_consistent() {
                "clean"
            } else {
                "issues found"
            }
        );
        for issue in &self.issues {
            let _ = writeln!(out, "Issue: {issue}");
        }
        out
    }

    pub fn to_json(&self) -> String {
        let issues = self
            .issues
            .iter()
            .map(|value| format!("\"{}\"", json_escape(value)))
            .collect::<Vec<_>>()
            .join(",");
        let entries = self
            .directories_and_files
            .iter()
            .map(|entry| {
                let long_name = entry
                    .long_name
                    .as_ref()
                    .map_or_else(|| "null".to_owned(), |v| format!("\"{}\"", json_escape(v)));
                format!(
                    "{{\"path\":\"{}\",\"short_name\":\"{}\",\"long_name\":{},\"directory\":{},\"size\":{},\"first_cluster\":{}}}",
                    json_escape(&entry.path),
                    json_escape(&entry.short_name),
                    long_name,
                    entry.is_directory,
                    entry.size,
                    entry.first_cluster
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        format!(
            concat!(
                "{{\"fat_type\":\"{}\",\"volume_label\":\"{}\",\"volume_id\":{},",
                "\"bytes_per_sector\":{},\"sectors_per_cluster\":{},\"total_sectors\":{},",
                "\"cluster_count\":{},\"allocated_clusters\":{},\"free_clusters\":{},",
                "\"bad_clusters\":{},\"highest_allocated_cluster\":{},",
                "\"resize\":{{\"minimum_bytes\":{},\"current_bytes\":{},\"maximum_bytes\":{}}},",
                "\"consistent\":{},\"fully_checked\":{},\"entries\":[{}],\"issues\":[{}]}}"
            ),
            self.bpb.fat_type.name(),
            json_escape(&self.bpb.volume_label),
            self.bpb.volume_id,
            self.bpb.bytes_per_sector,
            self.bpb.sectors_per_cluster,
            self.bpb.total_sectors,
            self.layout.cluster_count,
            self.allocated_clusters,
            self.free_clusters,
            self.bad_clusters,
            self.highest_allocated_cluster
                .map_or_else(|| "null".to_owned(), |v| v.to_string()),
            self.resize.minimum_bytes,
            self.resize.current_bytes,
            self.resize.maximum_bytes,
            self.is_consistent(),
            self.fully_checked,
            entries,
            issues
        )
    }
}

pub fn inspect(device: &impl ReadAt) -> io::Result<Report> {
    Ok(read_metadata(device)?.report)
}

/// Reads filesystem geometry and allocation statistics without traversing
/// directory trees or proving cluster ownership. This is the fast path used
/// by `--info`; use [`inspect`] for a complete consistency check.
pub fn summarize(device: &impl ReadAt) -> io::Result<Report> {
    let (bpb, layout, boot_sector) = Bpb::read(device)?;
    let (fat, mut issues) = FatTable::read(device, &bpb, &layout)?;
    let fs_info = validate_reserved_metadata(device, &bpb, &layout, &boot_sector, &mut issues)?;
    let (allocated_clusters, free_clusters, highest_allocated_cluster, bad_clusters) =
        fat.allocation();
    if let Some(info) = &fs_info
        && let Some(value) = info.free_clusters
        && value != free_clusters
    {
        issues.push(format!(
            "FSInfo free count {value} differs from FAT count {free_clusters}"
        ));
    }
    let resize = resize_constraints(device.len(), &bpb, &layout, highest_allocated_cluster);
    Ok(Report {
        bpb,
        layout,
        fs_info,
        allocated_clusters,
        free_clusters,
        bad_clusters,
        highest_allocated_cluster,
        directories_and_files: Vec::new(),
        resize,
        issues,
        fully_checked: false,
    })
}

/// Reads the same validated metadata as [`inspect`], while retaining the FAT
/// entries needed by read-only planning.
pub fn read_metadata(device: &impl ReadAt) -> io::Result<Metadata> {
    let (bpb, layout, boot_sector) = Bpb::read(device)?;
    let (fat, mut issues) = FatTable::read(device, &bpb, &layout)?;
    let fs_info = validate_reserved_metadata(device, &bpb, &layout, &boot_sector, &mut issues)?;
    let directory = directory::scan(device, &bpb, &layout, &fat)?;
    issues.extend(directory.issues);
    let (allocated_clusters, free_clusters, highest_allocated_cluster, bad_clusters) =
        fat.allocation();
    if let Some(info) = &fs_info
        && let Some(value) = info.free_clusters
        && value != free_clusters
    {
        issues.push(format!(
            "FSInfo free count {value} differs from FAT count {free_clusters}"
        ));
    }
    for cluster in 2..fat.cluster_count() + 2 {
        match fat.entry(cluster).expect("cluster in range") {
            Entry::Next(_) | Entry::End if !directory.claimed.contains(&cluster) => {
                issues.push(format!(
                    "allocated cluster {cluster} is not referenced by a directory entry"
                ));
            }
            Entry::Reserved(value) => {
                issues.push(format!(
                    "cluster {cluster} contains reserved FAT value {value:#x}"
                ));
            }
            _ => {}
        }
    }
    let resize = resize_constraints(device.len(), &bpb, &layout, highest_allocated_cluster);
    let directory_references = directory.references;
    Ok(Metadata {
        report: Report {
            bpb,
            layout,
            fs_info,
            allocated_clusters,
            free_clusters,
            bad_clusters,
            highest_allocated_cluster,
            directories_and_files: directory.entries,
            resize,
            issues,
            fully_checked: true,
        },
        fat,
        directory_references,
    })
}

fn validate_reserved_metadata(
    device: &impl ReadAt,
    bpb: &Bpb,
    layout: &Layout,
    boot: &[u8],
    issues: &mut Vec<String>,
) -> io::Result<Option<FsInfo>> {
    if bpb.fat_type != FatType::Fat32 {
        return Ok(None);
    }
    if let Some(sector) = bpb.backup_boot_sector {
        let mut backup = vec![0; usize::from(bpb.bytes_per_sector)];
        device.read_at(
            u64::from(sector) * u64::from(bpb.bytes_per_sector),
            &mut backup,
        )?;
        if backup != boot {
            issues.push("FAT32 backup boot sector differs from primary".to_owned());
        }
    }
    let Some(sector) = bpb.fs_info_sector else {
        issues.push("FAT32 BPB has no FSInfo sector".to_owned());
        return Ok(None);
    };
    let mut bytes = vec![0; usize::from(bpb.bytes_per_sector)];
    device.read_at(
        u64::from(sector) * u64::from(bpb.bytes_per_sector),
        &mut bytes,
    )?;
    if bytes[..4] != [0x52, 0x52, 0x61, 0x41]
        || bytes[484..488] != [0x72, 0x72, 0x41, 0x61]
        || bytes[508..512] != [0, 0, 0x55, 0xaa]
    {
        issues.push("FAT32 FSInfo signatures are invalid".to_owned());
        return Ok(None);
    }
    let free = le32(&bytes[488..492]);
    let next = le32(&bytes[492..496]);
    let free_clusters = if free == u32::MAX {
        None
    } else if free <= layout.cluster_count {
        Some(free)
    } else {
        issues.push("FAT32 FSInfo free count is out of range".to_owned());
        None
    };
    let next_free = if next == u32::MAX {
        None
    } else if (2..layout.cluster_count + 2).contains(&next) {
        Some(next)
    } else {
        issues.push("FAT32 FSInfo next-free hint is out of range".to_owned());
        None
    };
    Ok(Some(FsInfo {
        free_clusters,
        next_free,
    }))
}

fn resize_constraints(
    device_len: u64,
    bpb: &Bpb,
    layout: &Layout,
    highest: Option<u32>,
) -> ResizeConstraints {
    let minimum_sectors = highest.map_or(layout.first_data_sector, |cluster| {
        layout.first_data_sector + (cluster - 1) * u32::from(bpb.sectors_per_cluster)
    });
    let entry_size = match bpb.fat_type {
        FatType::Fat16 => 2_u64,
        FatType::Fat32 => 4,
    };
    let table_capacity =
        u64::from(bpb.sectors_per_fat) * u64::from(bpb.bytes_per_sector) / entry_size - 2;
    let type_capacity = match bpb.fat_type {
        FatType::Fat16 => u64::from(boot::FAT16_MAX_CLUSTERS),
        FatType::Fat32 => 0x0fff_ffed,
    };
    let max_clusters = table_capacity.min(type_capacity);
    let geometry_max =
        u64::from(layout.first_data_sector) + max_clusters * u64::from(bpb.sectors_per_cluster);
    let maximum_sectors = geometry_max
        .min(u64::from(u32::MAX))
        .min(device_len / u64::from(bpb.bytes_per_sector))
        .max(u64::from(bpb.total_sectors));
    ResizeConstraints {
        minimum_bytes: u64::from(minimum_sectors) * u64::from(bpb.bytes_per_sector),
        current_bytes: layout.total_bytes,
        maximum_bytes: maximum_sectors * u64::from(bpb.bytes_per_sector),
    }
}

fn json_escape(value: &str) -> String {
    let mut result = String::new();
    for character in value.chars() {
        match character {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            c if c < '\u{20}' => {
                let _ = write!(result, "\\u{:04x}", c as u32);
            }
            c => result.push(c),
        }
    }
    result
}

fn le32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().expect("fixed field"))
}

pub(crate) fn no_fat_found() -> io::Error {
    invalid_data("no unambiguous FAT16/FAT32 filesystem was found")
}
