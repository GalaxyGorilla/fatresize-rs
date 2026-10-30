use std::collections::{HashMap, HashSet};
use std::io;

use crate::device::{ReadAt, invalid_data};

use super::boot::{Bpb, FatType, Layout};
use super::table::{ChainError, FatTable};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryEntry {
    pub path: String,
    pub short_name: String,
    pub long_name: Option<String>,
    pub is_directory: bool,
    pub size: u32,
    pub first_cluster: u32,
    /// Byte offset of the 32-byte short directory entry within the volume.
    pub entry_offset: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryReference {
    pub path: String,
    pub first_cluster: u32,
    pub entry_offset: u64,
}

pub(crate) struct DirectoryScan {
    pub entries: Vec<DirectoryEntry>,
    pub references: Vec<DirectoryReference>,
    pub issues: Vec<String>,
    pub claimed: HashSet<u32>,
}

pub(crate) fn scan(
    device: &impl ReadAt,
    bpb: &Bpb,
    layout: &Layout,
    fat: &FatTable,
) -> io::Result<DirectoryScan> {
    let mut scanner = Scanner {
        device,
        bpb,
        layout,
        fat,
        entries: Vec::new(),
        references: Vec::new(),
        issues: Vec::new(),
        claimed: HashMap::new(),
        visited_directories: HashSet::new(),
    };
    match bpb.fat_type {
        FatType::Fat16 => {
            let bytes = usize::from(bpb.root_entry_count) * 32;
            let mut data = vec![0; bytes];
            scanner.device.read_at(
                u64::from(layout.first_root_sector) * u64::from(bpb.bytes_per_sector),
                &mut data,
            )?;
            let base = u64::from(layout.first_root_sector) * u64::from(bpb.bytes_per_sector);
            scanner.parse_directory("/", &data, |index| base + index as u64 * 32)?;
        }
        FatType::Fat32 => scanner.read_directory("/", bpb.root_cluster)?,
    }
    Ok(DirectoryScan {
        entries: scanner.entries,
        references: scanner.references,
        issues: scanner.issues,
        claimed: scanner.claimed.into_keys().collect(),
    })
}

struct Scanner<'a, T> {
    device: &'a T,
    bpb: &'a Bpb,
    layout: &'a Layout,
    fat: &'a FatTable,
    entries: Vec<DirectoryEntry>,
    references: Vec<DirectoryReference>,
    issues: Vec<String>,
    claimed: HashMap<u32, String>,
    visited_directories: HashSet<u32>,
}

impl<T: ReadAt> Scanner<'_, T> {
    fn read_directory(&mut self, path: &str, start: u32) -> io::Result<()> {
        if !self.visited_directories.insert(start) {
            self.issues
                .push(format!("directory cycle or repeated directory at {path}"));
            return Ok(());
        }
        let chain = match self.fat.chain(start) {
            Ok(chain) => chain,
            Err(error) => {
                self.issues.push(format!(
                    "invalid directory chain at {path}: {}",
                    chain_error(error)
                ));
                return Ok(());
            }
        };
        self.claim(path, &chain);
        let mut data = Vec::with_capacity(chain.len() * self.layout.cluster_size as usize);
        for &cluster in &chain {
            let mut buffer = vec![0; self.layout.cluster_size as usize];
            self.device
                .read_at(cluster_offset(self.bpb, self.layout, cluster)?, &mut buffer)?;
            data.extend_from_slice(&buffer);
        }
        let first_data_sector = self.layout.first_data_sector;
        let cluster_size = self.layout.cluster_size;
        let sectors_per_cluster = self.bpb.sectors_per_cluster;
        let bytes_per_sector = self.bpb.bytes_per_sector;
        self.parse_directory(path, &data, |index| {
            let byte = index as u64 * 32;
            let cluster_index = byte / u64::from(cluster_size);
            let within = byte % u64::from(cluster_size);
            let cluster = chain[cluster_index as usize];
            (u64::from(first_data_sector) + u64::from(cluster - 2) * u64::from(sectors_per_cluster))
                * u64::from(bytes_per_sector)
                + within
        })
    }

    fn parse_directory(
        &mut self,
        parent: &str,
        data: &[u8],
        entry_offset: impl Fn(usize) -> u64,
    ) -> io::Result<()> {
        let mut lfn = Vec::<[u8; 32]>::new();
        for (raw_index, raw) in data.as_chunks::<32>().0.iter().enumerate() {
            if raw[0] == 0 {
                break;
            }
            if raw[0] == 0xe5 {
                lfn.clear();
                continue;
            }
            if raw[11] == 0x0f {
                lfn.push(*raw);
                continue;
            }
            let short_name = short_name(raw);
            let long_name = if lfn.is_empty() {
                None
            } else {
                match decode_lfn(&lfn, &raw[..11]) {
                    Ok(name) => Some(name),
                    Err(reason) => {
                        self.issues.push(format!(
                            "invalid long filename before {short_name} in {parent}: {reason}"
                        ));
                        None
                    }
                }
            };
            lfn.clear();
            let attributes = raw[11];
            if attributes & 0xc0 != 0 {
                self.issues
                    .push(format!("reserved attributes on {short_name} in {parent}"));
            }
            let first_cluster = u32::from(u16::from_le_bytes([raw[26], raw[27]]))
                | (u32::from(u16::from_le_bytes([raw[20], raw[21]])) << 16);
            let display = long_name.as_deref().unwrap_or(&short_name);
            let path = if parent == "/" {
                format!("/{display}")
            } else {
                format!("{parent}/{display}")
            };
            if attributes & 0x08 == 0 && first_cluster >= 2 {
                self.references.push(DirectoryReference {
                    path: path.clone(),
                    first_cluster,
                    entry_offset: entry_offset(raw_index),
                });
            }
            if attributes & 0x08 != 0 || short_name == "." || short_name == ".." {
                continue;
            }
            let size = u32::from_le_bytes(raw[28..32].try_into().expect("size"));
            let is_directory = attributes & 0x10 != 0;
            self.entries.push(DirectoryEntry {
                path: path.clone(),
                short_name,
                long_name,
                is_directory,
                size,
                first_cluster,
                entry_offset: entry_offset(raw_index),
            });
            if is_directory {
                if size != 0 {
                    self.issues
                        .push(format!("directory {path} has a nonzero file size"));
                }
                if first_cluster < 2 {
                    self.issues
                        .push(format!("directory {path} has no valid first cluster"));
                } else {
                    self.read_directory(&path, first_cluster)?;
                }
            } else if first_cluster == 0 {
                if size != 0 {
                    self.issues
                        .push(format!("nonempty file {path} has no first cluster"));
                }
            } else {
                match self.fat.chain(first_cluster) {
                    Ok(chain) => {
                        let capacity = chain.len() as u64 * u64::from(self.layout.cluster_size);
                        if capacity < u64::from(size) {
                            self.issues
                                .push(format!("file {path} is larger than its cluster chain"));
                        }
                        self.claim(&path, &chain);
                    }
                    Err(error) => self.issues.push(format!(
                        "invalid file chain at {path}: {}",
                        chain_error(error)
                    )),
                }
            }
        }
        if !lfn.is_empty() {
            self.issues
                .push(format!("orphan long-filename entries in {parent}"));
        }
        Ok(())
    }

    fn claim(&mut self, path: &str, chain: &[u32]) {
        for &cluster in chain {
            if let Some(other) = self.claimed.insert(cluster, path.to_owned())
                && other != path
            {
                self.issues.push(format!(
                    "cluster {cluster} is cross-linked between {other} and {path}"
                ));
            }
        }
    }
}

fn cluster_offset(bpb: &Bpb, layout: &Layout, cluster: u32) -> io::Result<u64> {
    if cluster < 2 || cluster >= layout.cluster_count + 2 {
        return Err(invalid_data("directory cluster is out of range"));
    }
    let sector = u64::from(layout.first_data_sector)
        + u64::from(cluster - 2) * u64::from(bpb.sectors_per_cluster);
    Ok(sector * u64::from(bpb.bytes_per_sector))
}

fn short_name(raw: &[u8]) -> String {
    let mut base = raw[..8].to_vec();
    if base.first() == Some(&0x05) {
        base[0] = 0xe5;
    }
    let base = String::from_utf8_lossy(&base).trim_end().to_owned();
    let ext = String::from_utf8_lossy(&raw[8..11]).trim_end().to_owned();
    if ext.is_empty() {
        base
    } else {
        format!("{base}.{ext}")
    }
}

fn decode_lfn(entries: &[[u8; 32]], short: &[u8]) -> Result<String, &'static str> {
    let first = entries.first().expect("nonempty");
    let count = usize::from(first[0] & 0x1f);
    if first[0] & 0x40 == 0 || count == 0 || count != entries.len() {
        return Err("invalid sequence length");
    }
    let checksum = lfn_checksum(short);
    let mut units = Vec::with_capacity(count * 13);
    for (index, entry) in entries.iter().enumerate() {
        let expected = count - index;
        if usize::from(entry[0] & 0x1f) != expected
            || entry[0] & 0x80 != 0
            || (index != 0 && entry[0] & 0x40 != 0)
            || entry[11] != 0x0f
            || entry[12] != 0
            || entry[13] != checksum
            || entry[26..28] != [0, 0]
        {
            return Err("sequence fields or checksum do not match");
        }
    }
    for entry in entries.iter().rev() {
        for offset in [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30] {
            units.push(u16::from_le_bytes([entry[offset], entry[offset + 1]]));
        }
    }
    let terminator = units.iter().position(|&unit| unit == 0);
    if let Some(end) = terminator {
        if units[end + 1..].iter().any(|&unit| unit != 0xffff) {
            return Err("invalid padding");
        }
        units.truncate(end);
    } else if units.contains(&0xffff) {
        return Err("padding appears before terminator");
    }
    if units.is_empty() {
        return Err("empty name");
    }
    String::from_utf16(&units).map_err(|_| "name is not valid UTF-16")
}

fn lfn_checksum(short: &[u8]) -> u8 {
    short
        .iter()
        .fold(0_u8, |sum, &byte| sum.rotate_right(1).wrapping_add(byte))
}

fn chain_error(error: ChainError) -> String {
    match error {
        ChainError::Cycle(cluster) => format!("cycle at cluster {cluster}"),
        ChainError::Bad(cluster) => format!("bad cluster marker at cluster {cluster}"),
        ChainError::Reserved(cluster, value) => {
            format!("reserved value {value:#x} at cluster {cluster}")
        }
        ChainError::Free(cluster) => format!("free cluster {cluster} in chain"),
        ChainError::OutOfRange(cluster) => format!("cluster {cluster} is out of range"),
    }
}
