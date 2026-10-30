//! Bounds-checked random-access I/O for image files and block devices.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

/// Traditional disk-sector size and the minimum logical block size supported
/// by the partition-table parsers.
pub const SECTOR_SIZE: u64 = 512;

/// Exact, bounds-checked random-access reads.
pub trait ReadAt {
    fn len(&self) -> u64;
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> io::Result<()>;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn read_sector(&self, sector: u64, buffer: &mut [u8; 512]) -> io::Result<()> {
        let offset = sector
            .checked_mul(SECTOR_SIZE)
            .ok_or_else(|| invalid_input("sector offset overflows"))?;
        self.read_at(offset, buffer)
    }
}

/// Exact, bounds-checked random-access writes.
pub trait WriteAt: ReadAt {
    fn write_at(&self, offset: u64, buffer: &[u8]) -> io::Result<()>;
    fn sync(&self) -> io::Result<()>;

    fn write_sector(&self, sector: u64, buffer: &[u8; 512]) -> io::Result<()> {
        let offset = sector
            .checked_mul(SECTOR_SIZE)
            .ok_or_else(|| invalid_input("sector offset overflows"))?;
        self.write_at(offset, buffer)
    }
}

/// An opened regular image file or raw device.
#[derive(Debug)]
pub struct Device {
    file: File,
    path: PathBuf,
    len: u64,
    writable: bool,
    regular: bool,
    logical_sector_size: u32,
}

impl Device {
    pub fn open_read_only(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::open(path.as_ref(), false)
    }

    pub fn open_writable(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::open(path.as_ref(), true)
    }

    fn open(path: &Path, writable: bool) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(writable).open(path)?;
        let metadata = file.metadata()?;
        let len = device_len(&file, &metadata)?;
        let logical_sector_size = logical_sector_size(&file, &metadata)?;
        if logical_sector_size < 512 || !logical_sector_size.is_power_of_two() {
            return Err(invalid_data("unsupported logical sector size"));
        }
        if writable {
            lock_exclusive(&file)?;
        }
        Ok(Self {
            file,
            path: path.to_owned(),
            len,
            writable,
            regular: metadata.is_file(),
            logical_sector_size,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub const fn is_writable(&self) -> bool {
        self.writable
    }

    pub const fn logical_sector_size(&self) -> u32 {
        self.logical_sector_size
    }

    pub const fn is_regular_file(&self) -> bool {
        self.regular
    }

    /// Changes the length of a regular image and refreshes the bounds used by
    /// random-access I/O. Block devices are deliberately never resized here.
    pub fn set_len(&mut self, len: u64) -> io::Result<()> {
        if !self.writable {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "device was opened read-only",
            ));
        }
        if !self.regular {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "raw block-device length cannot be changed",
            ));
        }
        self.file.set_len(len)?;
        self.file.sync_all()?;
        self.len = len;
        Ok(())
    }

    pub fn is_mounted(&self) -> io::Result<bool> {
        is_mounted(&self.path)
    }

    pub fn subrange(&self, offset: u64, len: u64) -> io::Result<Subrange<'_, Self>> {
        Subrange::new(self, offset, len)
    }
}

#[cfg(target_os = "linux")]
fn lock_exclusive(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    const LOCK_EX: std::ffi::c_int = 2;
    const LOCK_NB: std::ffi::c_int = 4;
    unsafe extern "C" {
        fn flock(fd: std::ffi::c_int, operation: std::ffi::c_int) -> std::ffi::c_int;
    }
    // SAFETY: `fd` names an open file description owned by `file`; flock does
    // not retain Rust pointers. The kernel releases the advisory lock when the
    // file description is closed.
    if unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) } == -1 {
        let error = io::Error::last_os_error();
        return Err(io::Error::new(
            error.kind(),
            format!("cannot acquire exclusive advisory lock: {error}"),
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn lock_exclusive(_file: &File) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "writable access requires Linux exclusive advisory locking",
    ))
}

impl ReadAt for Device {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
        check_bounds(self.len, offset, buffer.len())?;
        read_exact_at(&self.file, offset, buffer)
    }
}

impl WriteAt for Device {
    fn write_at(&self, offset: u64, buffer: &[u8]) -> io::Result<()> {
        if !self.writable {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "device was opened read-only",
            ));
        }
        check_bounds(self.len, offset, buffer.len())?;
        write_all_at(&self.file, offset, buffer)
    }

    fn sync(&self) -> io::Result<()> {
        if !self.writable {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "device was opened read-only",
            ));
        }
        self.file.sync_all()
    }
}

/// A non-owning view that cannot access bytes outside its selected range.
#[derive(Debug, Clone, Copy)]
pub struct Subrange<'a, T: ?Sized> {
    inner: &'a T,
    offset: u64,
    len: u64,
}

impl<'a, T: ReadAt + ?Sized> Subrange<'a, T> {
    pub fn new(inner: &'a T, offset: u64, len: u64) -> io::Result<Self> {
        let end = offset
            .checked_add(len)
            .ok_or_else(|| invalid_input("subrange overflows"))?;
        if end > inner.len() {
            return Err(unexpected_eof("subrange extends beyond device"));
        }
        Ok(Self { inner, offset, len })
    }

    pub const fn offset(&self) -> u64 {
        self.offset
    }
}

impl<T: ReadAt + ?Sized> ReadAt for Subrange<'_, T> {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
        check_bounds(self.len, offset, buffer.len())?;
        self.inner.read_at(
            self.offset
                .checked_add(offset)
                .ok_or_else(|| invalid_input("offset overflows"))?,
            buffer,
        )
    }
}

impl<T: WriteAt + ?Sized> WriteAt for Subrange<'_, T> {
    fn write_at(&self, offset: u64, buffer: &[u8]) -> io::Result<()> {
        check_bounds(self.len, offset, buffer.len())?;
        self.inner.write_at(
            self.offset
                .checked_add(offset)
                .ok_or_else(|| invalid_input("offset overflows"))?,
            buffer,
        )
    }

    fn sync(&self) -> io::Result<()> {
        self.inner.sync()
    }
}

pub(crate) fn check_bounds(len: u64, offset: u64, count: usize) -> io::Result<()> {
    let count = u64::try_from(count).map_err(|_| invalid_input("I/O length is too large"))?;
    let end = offset
        .checked_add(count)
        .ok_or_else(|| invalid_input("I/O range overflows"))?;
    if end > len {
        return Err(unexpected_eof("I/O extends beyond selected bounds"));
    }
    Ok(())
}

fn invalid_input(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

pub(crate) fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn unexpected_eof(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, message)
}

#[cfg(unix)]
fn read_exact_at(file: &File, mut offset: u64, mut buffer: &mut [u8]) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    while !buffer.is_empty() {
        let count = file.read_at(buffer, offset)?;
        if count == 0 {
            return Err(unexpected_eof("short random-access read"));
        }
        offset = offset
            .checked_add(count as u64)
            .ok_or_else(|| invalid_input("offset overflows"))?;
        buffer = &mut buffer[count..];
    }
    Ok(())
}

#[cfg(windows)]
fn read_exact_at(file: &File, mut offset: u64, mut buffer: &mut [u8]) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buffer.is_empty() {
        let count = file.seek_read(buffer, offset)?;
        if count == 0 {
            return Err(unexpected_eof("short random-access read"));
        }
        offset += count as u64;
        buffer = &mut buffer[count..];
    }
    Ok(())
}

#[cfg(unix)]
fn write_all_at(file: &File, mut offset: u64, mut buffer: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    while !buffer.is_empty() {
        let count = file.write_at(buffer, offset)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "short random-access write",
            ));
        }
        offset = offset
            .checked_add(count as u64)
            .ok_or_else(|| invalid_input("offset overflows"))?;
        buffer = &buffer[count..];
    }
    Ok(())
}

#[cfg(windows)]
fn write_all_at(file: &File, mut offset: u64, mut buffer: &[u8]) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buffer.is_empty() {
        let count = file.seek_write(buffer, offset)?;
        if count == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "short write"));
        }
        offset += count as u64;
        buffer = &buffer[count..];
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
compile_error!("fatresize random-access I/O currently supports Unix and Windows");

fn device_len(file: &File, metadata: &std::fs::Metadata) -> io::Result<u64> {
    #[cfg(target_os = "linux")]
    {
        use std::os::linux::fs::MetadataExt;
        if metadata.st_mode() & 0o170000 == 0o060000 {
            return linux_ioctl_u64(file, 0x8008_1272);
        }
    }
    Ok(metadata.len())
}

fn logical_sector_size(file: &File, metadata: &std::fs::Metadata) -> io::Result<u32> {
    #[cfg(target_os = "linux")]
    {
        use std::os::linux::fs::MetadataExt;
        if metadata.st_mode() & 0o170000 == 0o060000 {
            return linux_ioctl_u32(file, 0x1268);
        }
    }
    let _ = (file, metadata);
    Ok(512)
}

#[cfg(target_os = "linux")]
fn linux_ioctl_u64(file: &File, request: std::ffi::c_ulong) -> io::Result<u64> {
    use std::os::fd::AsRawFd;
    unsafe extern "C" {
        fn ioctl(fd: std::ffi::c_int, request: std::ffi::c_ulong, ...) -> std::ffi::c_int;
    }
    let mut value = 0_u64;
    // SAFETY: BLKGETSIZE64 writes exactly one u64 to a valid, aligned pointer.
    // `file` remains open for the call and no Rust alias reads `value` concurrently.
    let result = unsafe { ioctl(file.as_raw_fd(), request, &mut value) };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(value)
    }
}

#[cfg(target_os = "linux")]
fn linux_ioctl_u32(file: &File, request: std::ffi::c_ulong) -> io::Result<u32> {
    use std::os::fd::AsRawFd;
    unsafe extern "C" {
        fn ioctl(fd: std::ffi::c_int, request: std::ffi::c_ulong, ...) -> std::ffi::c_int;
    }
    let mut value = 0_u32;
    // SAFETY: BLKSSZGET writes one C int (u32 here) to this valid aligned pointer.
    let result = unsafe { ioctl(file.as_raw_fd(), request, &mut value) };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(value)
    }
}

/// Reports whether a Linux block device, or one of its partitions, is mounted.
///
/// Linux mount IDs from `/proc/self/mountinfo` are matched to canonical sysfs
/// device paths, which avoids ambiguous textual source names. Other platforms
/// conservatively report `false`.
pub fn is_mounted(path: impl AsRef<Path>) -> io::Result<bool> {
    #[cfg(target_os = "linux")]
    {
        use std::os::linux::fs::MetadataExt;
        let metadata = std::fs::metadata(path.as_ref())?;
        if metadata.st_mode() & 0o170000 != 0o060000 {
            return Ok(false);
        }
        // Linux's userspace major()/minor() encoding for a 64-bit dev_t.
        let dev = metadata.st_rdev();
        let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & 0xffff_f000);
        let minor = (dev & 0xff) | ((dev >> 12) & 0xffff_ff00);
        let id = format!("{major}:{minor}");
        let candidate = std::fs::canonicalize(format!("/sys/dev/block/{id}"))?;
        let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")?;
        for line in mountinfo.lines() {
            let Some(device_id) = line.split_ascii_whitespace().nth(2) else {
                continue;
            };
            let Ok(mounted) = std::fs::canonicalize(format!("/sys/dev/block/{device_id}")) else {
                continue;
            };
            if mounted == candidate || mounted.starts_with(&candidate) {
                return Ok(true);
            }
        }
    }
    let _ = path;
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("fatresize-device-{}-{unique}", std::process::id()))
    }

    #[test]
    fn file_io_is_exact_and_bounded() {
        let path = temp_path();
        std::fs::write(&path, [0_u8; 1024]).unwrap();
        let device = Device::open_writable(&path).unwrap();
        assert_eq!(device.len(), 1024);
        device.write_at(510, &[1, 2, 3, 4]).unwrap();
        let mut bytes = [0; 4];
        device.read_at(510, &mut bytes).unwrap();
        assert_eq!(bytes, [1, 2, 3, 4]);
        assert_eq!(
            device.write_at(1023, &[1, 2]).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        device.sync().unwrap();
        drop(device);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn read_only_open_rejects_writes() {
        let path = temp_path();
        std::fs::write(&path, [0_u8; 8]).unwrap();
        let device = Device::open_read_only(&path).unwrap();
        assert_eq!(
            device.write_at(0, &[1]).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        drop(device);
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn writable_open_requires_exclusive_lock() {
        let path = temp_path();
        std::fs::write(&path, [0_u8; 8]).unwrap();
        let first = Device::open_writable(&path).unwrap();
        let error = Device::open_writable(&path).unwrap_err();
        assert!(error.to_string().contains("exclusive advisory lock"));
        drop(first);
        Device::open_writable(&path).unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn subrange_translates_and_confines_io() {
        let path = temp_path();
        std::fs::write(&path, [0_u8; 32]).unwrap();
        let device = Device::open_writable(&path).unwrap();
        let view = Subrange::new(&device, 8, 8).unwrap();
        view.write_at(0, &[7; 8]).unwrap();
        assert!(view.write_at(8, &[1]).is_err());
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[8..16], &[7; 8]);
        assert!(bytes[..8].iter().all(|&byte| byte == 0));
        assert!(bytes[16..].iter().all(|&byte| byte == 0));
        drop(device);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn subrange_rejects_overflow_and_out_of_bounds() {
        let path = temp_path();
        std::fs::write(&path, [0_u8; 8]).unwrap();
        let device = Device::open_read_only(&path).unwrap();
        assert!(Subrange::new(&device, 7, 2).is_err());
        assert!(Subrange::new(&device, u64::MAX, 2).is_err());
        drop(device);
        std::fs::remove_file(path).unwrap();
    }
}
