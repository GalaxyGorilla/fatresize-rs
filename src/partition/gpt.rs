//! GUID Partition Table parsing and updating from the UEFI GPT format.

use std::io;

use crate::device::{ReadAt, Subrange, WriteAt, invalid_data};

const SIGNATURE: &[u8; 8] = b"EFI PART";
const MIN_HEADER_SIZE: u32 = 92;
const MIN_ENTRY_SIZE: u32 = 128;
const MAX_ENTRY_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GptHeader {
    pub revision: u32,
    pub current_lba: u64,
    pub backup_lba: u64,
    pub first_usable_lba: u64,
    pub last_usable_lba: u64,
    pub disk_guid: [u8; 16],
    pub partition_entries_lba: u64,
    pub partition_entry_count: u32,
    pub partition_entry_size: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GptPartition {
    pub type_guid: [u8; 16],
    pub unique_guid: [u8; 16],
    pub first_lba: u64,
    pub last_lba: u64,
    pub attributes: u64,
    pub name: String,
    extra: Vec<u8>,
}

impl GptPartition {
    pub fn new(
        type_guid: [u8; 16],
        unique_guid: [u8; 16],
        first_lba: u64,
        last_lba: u64,
        name: impl Into<String>,
    ) -> io::Result<Self> {
        if type_guid == [0; 16] || unique_guid == [0; 16] {
            return Err(invalid_data("GPT partition GUID cannot be zero"));
        }
        let name = name.into();
        if name.encode_utf16().count() > 36 {
            return Err(invalid_data(
                "GPT partition name is longer than 36 UTF-16 units",
            ));
        }
        if first_lba > last_lba {
            return Err(invalid_data("GPT partition range is reversed"));
        }
        Ok(Self {
            type_guid,
            unique_guid,
            first_lba,
            last_lba,
            attributes: 0,
            name,
            extra: Vec::new(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gpt {
    pub primary_header: GptHeader,
    pub backup_header: GptHeader,
    pub partitions: Vec<Option<GptPartition>>,
    block_size: u32,
    primary_header_size: u32,
    backup_header_size: u32,
}

impl Gpt {
    pub fn read(device: &impl ReadAt) -> io::Result<Self> {
        Self::read_with_block_size(device, 512)
    }

    pub fn read_with_block_size(device: &impl ReadAt, block_size: u32) -> io::Result<Self> {
        validate_block_size(block_size)?;
        if !device.len().is_multiple_of(u64::from(block_size)) {
            return Err(invalid_data("device length is not a whole logical block"));
        }
        let block_count = device.len() / u64::from(block_size);
        if block_count < 3 {
            return Err(invalid_data("device is too small for GPT"));
        }
        let (primary_header, primary_header_size) =
            read_header(device, 1, block_size, block_count)?;
        if primary_header.current_lba != 1 {
            return Err(invalid_data("primary GPT header is not at LBA 1"));
        }
        if primary_header.backup_lba != block_count - 1 {
            return Err(invalid_data("GPT backup header is not at the last LBA"));
        }
        let (backup_header, backup_header_size) =
            read_header(device, primary_header.backup_lba, block_size, block_count)?;
        validate_header_pair(&primary_header, &backup_header)?;
        validate_metadata_layout(&primary_header, &backup_header, block_size)?;

        let primary_bytes = read_entries(device, &primary_header, block_size)?;
        let backup_bytes = read_entries(device, &backup_header, block_size)?;
        if primary_bytes != backup_bytes {
            return Err(invalid_data("primary and backup GPT entries differ"));
        }
        let partitions = parse_entries(&primary_bytes, &primary_header)?;
        validate_partitions(&partitions, &primary_header)?;
        Ok(Self {
            primary_header,
            backup_header,
            partitions,
            block_size,
            primary_header_size,
            backup_header_size,
        })
    }

    pub const fn block_size(&self) -> u32 {
        self.block_size
    }

    /// Returns a one-based GPT entry.
    pub fn partition(&self, number: usize) -> Option<&GptPartition> {
        number
            .checked_sub(1)
            .and_then(|index| self.partitions.get(index))
            .and_then(Option::as_ref)
    }

    pub fn set_partition(
        &mut self,
        number: usize,
        partition: Option<GptPartition>,
    ) -> io::Result<()> {
        let index = number
            .checked_sub(1)
            .filter(|&index| index < self.partitions.len())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "GPT partition number is out of range",
                )
            })?;
        let mut candidate = self.partitions.clone();
        candidate[index] = partition;
        validate_partitions(&candidate, &self.primary_header)?;
        self.partitions = candidate;
        Ok(())
    }

    /// Changes only the ending LBA of an existing partition.
    pub fn resize_partition(&mut self, number: usize, last_lba: u64) -> io::Result<()> {
        let mut partition = self.partition(number).cloned().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "GPT partition does not exist")
        })?;
        partition.last_lba = last_lba;
        self.set_partition(number, Some(partition))
    }

    pub fn partition_view<'a, T: ReadAt + ?Sized>(
        &self,
        device: &'a T,
        number: usize,
    ) -> io::Result<Subrange<'a, T>> {
        let partition = self.partition(number).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "GPT partition does not exist")
        })?;
        let offset = partition
            .first_lba
            .checked_mul(u64::from(self.block_size))
            .ok_or_else(|| invalid_data("GPT partition byte offset overflows"))?;
        let blocks = partition
            .last_lba
            .checked_sub(partition.first_lba)
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| invalid_data("GPT partition length overflows"))?;
        let len = blocks
            .checked_mul(u64::from(self.block_size))
            .ok_or_else(|| invalid_data("GPT partition byte length overflows"))?;
        Subrange::new(device, offset, len)
    }

    /// Writes backup entries/header first, then primary entries/header, and
    /// syncs after each complete copy. No bytes outside GPT metadata are used.
    pub fn write(&self, device: &impl WriteAt) -> io::Result<()> {
        validate_block_size(self.block_size)?;
        if !device.len().is_multiple_of(u64::from(self.block_size)) {
            return Err(invalid_data("device length is not a whole logical block"));
        }
        let block_count = device.len() / u64::from(self.block_size);
        validate_header(&self.primary_header, 1, self.block_size, block_count)?;
        validate_header(
            &self.backup_header,
            block_count - 1,
            self.block_size,
            block_count,
        )?;
        validate_header_pair(&self.primary_header, &self.backup_header)?;
        validate_metadata_layout(&self.primary_header, &self.backup_header, self.block_size)?;
        validate_partitions(&self.partitions, &self.primary_header)?;

        let entries = encode_entries(
            &self.partitions,
            self.primary_header.partition_entry_count,
            self.primary_header.partition_entry_size,
        )?;
        let entries_crc = crc32(&entries);
        write_entries(device, &self.backup_header, self.block_size, &entries)?;
        write_header(
            device,
            &self.backup_header,
            self.block_size,
            self.backup_header_size,
            entries_crc,
        )?;
        device.sync()?;
        write_entries(device, &self.primary_header, self.block_size, &entries)?;
        write_header(
            device,
            &self.primary_header,
            self.block_size,
            self.primary_header_size,
            entries_crc,
        )?;
        device.sync()
    }
}

fn validate_block_size(block_size: u32) -> io::Result<()> {
    if block_size < 512 || !block_size.is_power_of_two() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "GPT logical block size must be a power of two and at least 512",
        ));
    }
    Ok(())
}

fn read_header(
    device: &impl ReadAt,
    lba: u64,
    block_size: u32,
    block_count: u64,
) -> io::Result<(GptHeader, u32)> {
    let mut block = vec![0; block_size as usize];
    let offset = lba
        .checked_mul(u64::from(block_size))
        .ok_or_else(|| invalid_data("GPT header offset overflows"))?;
    device.read_at(offset, &mut block)?;
    if &block[..8] != SIGNATURE {
        return Err(invalid_data("missing GPT header signature"));
    }
    let revision = le_u32(&block[8..12]);
    if revision != 0x0001_0000 {
        return Err(invalid_data("unsupported GPT revision"));
    }
    let header_size = le_u32(&block[12..16]);
    if header_size != MIN_HEADER_SIZE || header_size > block_size {
        return Err(invalid_data("unsupported GPT 1.0 header size"));
    }
    let stored_crc = le_u32(&block[16..20]);
    let mut crc_block = block[..header_size as usize].to_vec();
    crc_block[16..20].fill(0);
    if crc32(&crc_block) != stored_crc {
        return Err(invalid_data("GPT header CRC32 mismatch"));
    }
    if block[20..24] != [0; 4] {
        return Err(invalid_data("nonzero GPT reserved header field"));
    }
    let header = GptHeader {
        revision,
        current_lba: le_u64(&block[24..32]),
        backup_lba: le_u64(&block[32..40]),
        first_usable_lba: le_u64(&block[40..48]),
        last_usable_lba: le_u64(&block[48..56]),
        disk_guid: block[56..72].try_into().expect("fixed-size field"),
        partition_entries_lba: le_u64(&block[72..80]),
        partition_entry_count: le_u32(&block[80..84]),
        partition_entry_size: le_u32(&block[84..88]),
    };
    validate_header(&header, lba, block_size, block_count)?;
    Ok((header, header_size))
}

fn validate_header(
    header: &GptHeader,
    expected_lba: u64,
    block_size: u32,
    block_count: u64,
) -> io::Result<()> {
    if header.current_lba != expected_lba
        || header.current_lba >= block_count
        || header.backup_lba >= block_count
        || header.current_lba == header.backup_lba
    {
        return Err(invalid_data("invalid GPT header locations"));
    }
    if header.first_usable_lba > header.last_usable_lba
        || header.last_usable_lba >= block_count
        || header.disk_guid == [0; 16]
        || (header.current_lba >= header.first_usable_lba
            && header.current_lba <= header.last_usable_lba)
        || (header.backup_lba >= header.first_usable_lba
            && header.backup_lba <= header.last_usable_lba)
    {
        return Err(invalid_data("invalid GPT usable range or disk GUID"));
    }
    if header.partition_entry_count == 0
        || header.partition_entry_size < MIN_ENTRY_SIZE
        || !header.partition_entry_size.is_power_of_two()
    {
        return Err(invalid_data("invalid GPT entry count or size"));
    }
    let bytes = entry_bytes(header)?;
    if bytes > MAX_ENTRY_BYTES {
        return Err(invalid_data("GPT entry array is unreasonably large"));
    }
    let offset = header
        .partition_entries_lba
        .checked_mul(u64::from(block_size))
        .ok_or_else(|| invalid_data("GPT entry offset overflows"))?;
    let end = offset
        .checked_add(bytes)
        .ok_or_else(|| invalid_data("GPT entry array overflows"))?;
    let device_bytes = block_count
        .checked_mul(u64::from(block_size))
        .ok_or_else(|| invalid_data("device size overflows"))?;
    if end > device_bytes {
        return Err(invalid_data("GPT entry array extends beyond device"));
    }
    let array_first = header.partition_entries_lba;
    let array_blocks = bytes.div_ceil(u64::from(block_size));
    let array_end = array_first
        .checked_add(array_blocks)
        .ok_or_else(|| invalid_data("GPT entry array LBA range overflows"))?;
    if (header.current_lba >= array_first && header.current_lba < array_end)
        || (header.backup_lba >= array_first && header.backup_lba < array_end)
    {
        return Err(invalid_data("GPT entry array overlaps a header"));
    }
    if ranges_overlap(
        array_first,
        array_end,
        header.first_usable_lba,
        header.last_usable_lba + 1,
    ) {
        return Err(invalid_data("GPT entries overlap usable space"));
    }
    Ok(())
}

fn validate_header_pair(primary: &GptHeader, backup: &GptHeader) -> io::Result<()> {
    if primary.current_lba != backup.backup_lba
        || primary.backup_lba != backup.current_lba
        || primary.first_usable_lba != backup.first_usable_lba
        || primary.last_usable_lba != backup.last_usable_lba
        || primary.disk_guid != backup.disk_guid
        || primary.partition_entry_count != backup.partition_entry_count
        || primary.partition_entry_size != backup.partition_entry_size
    {
        return Err(invalid_data("primary and backup GPT headers disagree"));
    }
    Ok(())
}

fn validate_metadata_layout(
    primary: &GptHeader,
    backup: &GptHeader,
    block_size: u32,
) -> io::Result<()> {
    let block_size = u64::from(block_size);
    let primary_end = primary
        .partition_entries_lba
        .checked_add(entry_bytes(primary)?.div_ceil(block_size))
        .ok_or_else(|| invalid_data("primary GPT entry range overflows"))?;
    let backup_end = backup
        .partition_entries_lba
        .checked_add(entry_bytes(backup)?.div_ceil(block_size))
        .ok_or_else(|| invalid_data("backup GPT entry range overflows"))?;
    if ranges_overlap(
        primary.partition_entries_lba,
        primary_end,
        backup.partition_entries_lba,
        backup_end,
    ) {
        return Err(invalid_data("primary and backup GPT entries overlap"));
    }
    Ok(())
}

fn read_entries(device: &impl ReadAt, header: &GptHeader, block_size: u32) -> io::Result<Vec<u8>> {
    let byte_count = entry_bytes(header)?;
    let mut bytes = vec![
        0;
        usize::try_from(byte_count)
            .map_err(|_| invalid_data("GPT entries do not fit memory"))?
    ];
    let offset = header
        .partition_entries_lba
        .checked_mul(u64::from(block_size))
        .ok_or_else(|| invalid_data("GPT entry offset overflows"))?;
    device.read_at(offset, &mut bytes)?;

    let mut header_block = vec![0; block_size as usize];
    device.read_at(
        header.current_lba * u64::from(block_size),
        &mut header_block,
    )?;
    if crc32(&bytes) != le_u32(&header_block[88..92]) {
        return Err(invalid_data("GPT partition-entry CRC32 mismatch"));
    }
    Ok(bytes)
}

fn parse_entries(bytes: &[u8], header: &GptHeader) -> io::Result<Vec<Option<GptPartition>>> {
    let size = header.partition_entry_size as usize;
    let mut partitions = Vec::with_capacity(header.partition_entry_count as usize);
    for entry in bytes.chunks_exact(size) {
        let type_guid: [u8; 16] = entry[..16].try_into().expect("fixed-size field");
        if type_guid == [0; 16] {
            partitions.push(None);
            continue;
        }
        let name_units = entry[56..128]
            .as_chunks::<2>()
            .0
            .iter()
            .map(le_u16)
            .take_while(|&unit| unit != 0)
            .collect::<Vec<_>>();
        let name = String::from_utf16(&name_units)
            .map_err(|_| invalid_data("GPT partition name is not valid UTF-16"))?;
        partitions.push(Some(GptPartition {
            type_guid,
            unique_guid: entry[16..32].try_into().expect("fixed-size field"),
            first_lba: le_u64(&entry[32..40]),
            last_lba: le_u64(&entry[40..48]),
            attributes: le_u64(&entry[48..56]),
            name,
            extra: entry[128..].to_vec(),
        }));
    }
    Ok(partitions)
}

fn validate_partitions(partitions: &[Option<GptPartition>], header: &GptHeader) -> io::Result<()> {
    if partitions.len() != header.partition_entry_count as usize {
        return Err(invalid_data("GPT partition vector has the wrong length"));
    }
    for (index, partition) in partitions.iter().enumerate() {
        let Some(partition) = partition else { continue };
        if partition.type_guid == [0; 16] || partition.unique_guid == [0; 16] {
            return Err(invalid_data("GPT partition GUID cannot be zero"));
        }
        if partition.first_lba < header.first_usable_lba
            || partition.last_lba > header.last_usable_lba
            || partition.first_lba > partition.last_lba
        {
            return Err(invalid_data("GPT partition is outside usable space"));
        }
        if partition.name.encode_utf16().count() > 36 {
            return Err(invalid_data("GPT partition name is too long"));
        }
        for other in partitions[index + 1..].iter().flatten() {
            if ranges_overlap(
                partition.first_lba,
                partition.last_lba + 1,
                other.first_lba,
                other.last_lba + 1,
            ) {
                return Err(invalid_data("GPT partitions overlap"));
            }
            if partition.unique_guid == other.unique_guid {
                return Err(invalid_data("GPT partition GUIDs are duplicated"));
            }
        }
    }
    Ok(())
}

fn encode_entries(
    partitions: &[Option<GptPartition>],
    count: u32,
    entry_size: u32,
) -> io::Result<Vec<u8>> {
    let total = u64::from(count)
        .checked_mul(u64::from(entry_size))
        .ok_or_else(|| invalid_data("GPT entry array size overflows"))?;
    let mut bytes =
        vec![0; usize::try_from(total).map_err(|_| invalid_data("GPT entries do not fit memory"))?];
    let size = entry_size as usize;
    for (index, partition) in partitions.iter().enumerate() {
        let Some(partition) = partition else { continue };
        let entry = &mut bytes[index * size..(index + 1) * size];
        entry[..16].copy_from_slice(&partition.type_guid);
        entry[16..32].copy_from_slice(&partition.unique_guid);
        entry[32..40].copy_from_slice(&partition.first_lba.to_le_bytes());
        entry[40..48].copy_from_slice(&partition.last_lba.to_le_bytes());
        entry[48..56].copy_from_slice(&partition.attributes.to_le_bytes());
        for (slot, unit) in entry[56..128]
            .as_chunks_mut::<2>()
            .0
            .iter_mut()
            .zip(partition.name.encode_utf16())
        {
            slot.copy_from_slice(&unit.to_le_bytes());
        }
        if partition.extra.len() > entry.len() - 128 {
            return Err(invalid_data("GPT partition extension is too large"));
        }
        entry[128..128 + partition.extra.len()].copy_from_slice(&partition.extra);
    }
    Ok(bytes)
}

fn write_entries(
    device: &impl WriteAt,
    header: &GptHeader,
    block_size: u32,
    bytes: &[u8],
) -> io::Result<()> {
    let offset = header
        .partition_entries_lba
        .checked_mul(u64::from(block_size))
        .ok_or_else(|| invalid_data("GPT entry offset overflows"))?;
    device.write_at(offset, bytes)
}

fn write_header(
    device: &impl WriteAt,
    header: &GptHeader,
    block_size: u32,
    header_size: u32,
    entries_crc: u32,
) -> io::Result<()> {
    let mut block = vec![0; block_size as usize];
    block[..8].copy_from_slice(SIGNATURE);
    block[8..12].copy_from_slice(&header.revision.to_le_bytes());
    block[12..16].copy_from_slice(&header_size.to_le_bytes());
    block[24..32].copy_from_slice(&header.current_lba.to_le_bytes());
    block[32..40].copy_from_slice(&header.backup_lba.to_le_bytes());
    block[40..48].copy_from_slice(&header.first_usable_lba.to_le_bytes());
    block[48..56].copy_from_slice(&header.last_usable_lba.to_le_bytes());
    block[56..72].copy_from_slice(&header.disk_guid);
    block[72..80].copy_from_slice(&header.partition_entries_lba.to_le_bytes());
    block[80..84].copy_from_slice(&header.partition_entry_count.to_le_bytes());
    block[84..88].copy_from_slice(&header.partition_entry_size.to_le_bytes());
    block[88..92].copy_from_slice(&entries_crc.to_le_bytes());
    let header_crc = crc32(&block[..header_size as usize]);
    block[16..20].copy_from_slice(&header_crc.to_le_bytes());
    device.write_at(header.current_lba * u64::from(block_size), &block)
}

fn entry_bytes(header: &GptHeader) -> io::Result<u64> {
    u64::from(header.partition_entry_count)
        .checked_mul(u64::from(header.partition_entry_size))
        .ok_or_else(|| invalid_data("GPT entry array size overflows"))
}

fn ranges_overlap(a_start: u64, a_end: u64, b_start: u64, b_end: u64) -> bool {
    a_start < b_end && b_start < a_end
}

fn le_u16(bytes: &[u8; 2]) -> u16 {
    u16::from_le_bytes(*bytes)
}
fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().expect("fixed-size field"))
}
fn le_u64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes.try_into().expect("fixed-size field"))
}

/// IEEE CRC-32 used by GPT (polynomial 0x04c11db7, reflected representation).
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (0_u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    struct Memory {
        bytes: RefCell<Vec<u8>>,
        syncs: Cell<usize>,
    }
    impl ReadAt for Memory {
        fn len(&self) -> u64 {
            self.bytes.borrow().len() as u64
        }
        fn read_at(&self, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
            let offset = offset as usize;
            buffer.copy_from_slice(&self.bytes.borrow()[offset..offset + buffer.len()]);
            Ok(())
        }
    }
    impl WriteAt for Memory {
        fn write_at(&self, offset: u64, buffer: &[u8]) -> io::Result<()> {
            let offset = offset as usize;
            self.bytes.borrow_mut()[offset..offset + buffer.len()].copy_from_slice(buffer);
            Ok(())
        }
        fn sync(&self) -> io::Result<()> {
            self.syncs.set(self.syncs.get() + 1);
            Ok(())
        }
    }

    fn image() -> Memory {
        let memory = Memory {
            bytes: RefCell::new(vec![0xa5; 100 * 512]),
            syncs: Cell::new(0),
        };
        let primary = GptHeader {
            revision: 0x0001_0000,
            current_lba: 1,
            backup_lba: 99,
            first_usable_lba: 34,
            last_usable_lba: 97,
            disk_guid: [9; 16],
            partition_entries_lba: 2,
            partition_entry_count: 4,
            partition_entry_size: 128,
        };
        let backup = GptHeader {
            current_lba: 99,
            backup_lba: 1,
            partition_entries_lba: 98,
            ..primary.clone()
        };
        let partition = GptPartition::new([1; 16], [2; 16], 40, 49, "data").unwrap();
        let gpt = Gpt {
            primary_header: primary,
            backup_header: backup,
            partitions: vec![Some(partition), None, None, None],
            block_size: 512,
            primary_header_size: 92,
            backup_header_size: 92,
        };
        gpt.write(&memory).unwrap();
        memory.syncs.set(0);
        memory
    }

    #[test]
    fn crc32_known_vector() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn parses_matching_primary_and_backup() {
        let memory = image();
        let gpt = Gpt::read(&memory).unwrap();
        let partition = gpt.partition(1).unwrap();
        assert_eq!(partition.name, "data");
        assert_eq!((partition.first_lba, partition.last_lba), (40, 49));
        assert!(gpt.partition(2).is_none());
    }

    #[test]
    fn rejects_header_and_entry_crc_corruption() {
        let memory = image();
        memory.bytes.borrow_mut()[512 + 40] ^= 1;
        assert!(Gpt::read(&memory).is_err());

        let memory = image();
        memory.bytes.borrow_mut()[2 * 512] ^= 1;
        assert!(Gpt::read(&memory).is_err());
    }

    #[test]
    fn rejects_disagreeing_backup() {
        let memory = image();
        // Rebuild a valid backup header with a different disk GUID.
        let mut header = Gpt::read(&memory).unwrap().backup_header;
        header.disk_guid = [8; 16];
        let entries = memory.bytes.borrow()[98 * 512..99 * 512].to_vec();
        write_header(&memory, &header, 512, 92, crc32(&entries)).unwrap();
        assert!(Gpt::read(&memory).is_err());
    }

    #[test]
    fn updates_both_copies_and_preserves_non_metadata() {
        let memory = image();
        let before = memory.bytes.borrow()[10 * 512..11 * 512].to_vec();
        let mut gpt = Gpt::read(&memory).unwrap();
        let replacement = GptPartition::new([3; 16], [4; 16], 50, 59, "new").unwrap();
        gpt.set_partition(2, Some(replacement)).unwrap();
        gpt.write(&memory).unwrap();
        assert_eq!(memory.syncs.get(), 2);
        assert_eq!(&memory.bytes.borrow()[10 * 512..11 * 512], &before);
        let reread = Gpt::read(&memory).unwrap();
        assert_eq!(reread.partition(2).unwrap().name, "new");
    }

    #[test]
    fn rejects_overlap_and_bad_names() {
        let memory = image();
        let mut gpt = Gpt::read(&memory).unwrap();
        let overlap = GptPartition::new([3; 16], [4; 16], 45, 55, "bad").unwrap();
        assert!(gpt.set_partition(2, Some(overlap)).is_err());
        assert!(GptPartition::new([1; 16], [2; 16], 1, 2, "x".repeat(37)).is_err());
    }

    #[test]
    fn partition_view_has_exact_bounds() {
        let memory = image();
        let gpt = Gpt::read(&memory).unwrap();
        let view = gpt.partition_view(&memory, 1).unwrap();
        assert_eq!(view.len(), 10 * 512);
        let mut byte = [0];
        assert!(view.read_at(10 * 512, &mut byte).is_err());
    }
}
