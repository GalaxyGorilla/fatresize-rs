//! Creates a deterministic FAT32 benchmark image with fragmented file chains.
//!
//! Requires `mkfs.fat` and mtools (`mcopy`, `mmd`, and `mdel`) for fixture
//! construction. Production fatresize remains Rust-only and dependency-free.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let image = arguments
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp/fatresize-fragmented.img"));
    let size_mib = arguments
        .next()
        .map(|value| value.to_string_lossy().parse::<u64>())
        .transpose()?
        .unwrap_or(256);
    if arguments.next().is_some() || size_mib < 128 {
        return Err("usage: make_fragmented [IMAGE] [SIZE_MIB>=128]".into());
    }

    let _ = fs::remove_file(&image);
    successful(
        Command::new("mkfs.fat")
            .args(["-F", "32", "-C"])
            .arg(&image)
            .arg((size_mib * 1024).to_string())
            .status()?,
        "mkfs.fat",
    )?;
    mtool(&image, "mmd", &["::/BENCH"])?;
    mtool(&image, "mmd", &["::/BENCH/Nested Directory"])?;

    let temporary =
        std::env::temp_dir().join(format!("fatresize-fragment-source-{}", std::process::id()));
    fs::create_dir_all(&temporary)?;

    // Allocate equally sized files contiguously, then remove alternating files
    // to create holes smaller than the replacement files.
    let seed_count = (size_mib * 3 / 8) as usize;
    for index in 0..seed_count {
        let source = temporary.join(format!("seed-{index:03}.bin"));
        write_payload(&source, 2 * 1024 * 1024, index as u8)?;
        mcopy(&image, &source, &format!("::/BENCH/SEED{index:03}.BIN"))?;
    }
    for index in (0..seed_count).step_by(2) {
        mtool(&image, "mdel", &[&format!("::/BENCH/SEED{index:03}.BIN")])?;
    }

    // Replacements consume the contiguous tail and then must span 2 MiB holes
    // separated by the retained seed files.
    let replacement_count = (size_mib / 8) as usize;
    for index in 0..replacement_count {
        let source = temporary.join(format!("fragmented-{index:02}.bin"));
        write_payload(&source, 3 * 1024 * 1024, 0x80 | index as u8)?;
        mcopy(
            &image,
            &source,
            &format!("::/BENCH/Fragmented payload number {index:02}.bin"),
        )?;
    }
    for index in 0..4 {
        let source = temporary.join(format!("nested-{index:02}.bin"));
        write_payload(&source, 1024 * 1024 + index * 4096, 0xc0 | index as u8)?;
        mcopy(
            &image,
            &source,
            &format!("::/BENCH/Nested Directory/Long filename sample {index:02}.dat"),
        )?;
    }
    fs::remove_dir_all(&temporary)?;

    let metadata = fatresize::read_metadata(&fatresize::Device::open_read_only(&image)?)?;
    if !metadata.report.is_consistent() {
        return Err(format!(
            "generated image is inconsistent: {:?}",
            metadata.report.issues
        )
        .into());
    }
    let mut files = 0;
    let mut fragmented = 0;
    let mut total_extents = 0;
    let mut maximum_extents = 0;
    for entry in &metadata.report.directories_and_files {
        if entry.is_directory || entry.first_cluster < 2 {
            continue;
        }
        files += 1;
        let chain = metadata
            .fat
            .chain(entry.first_cluster)
            .map_err(|error| format!("invalid generated chain: {error:?}"))?;
        let extents = 1 + chain
            .windows(2)
            .filter(|pair| pair[1] != pair[0] + 1)
            .count();
        fragmented += usize::from(extents > 1);
        total_extents += extents;
        maximum_extents = maximum_extents.max(extents);
    }
    println!("image: {}", image.display());
    println!("size: {} bytes", metadata.report.layout.total_bytes);
    println!("files: {files}");
    println!("fragmented files: {fragmented}");
    println!("total extents: {total_extents}");
    println!("maximum extents per file: {maximum_extents}");
    if fragmented == 0 {
        return Err("fixture generation did not produce fragmented files".into());
    }
    Ok(())
}

fn write_payload(path: &Path, length: usize, seed: u8) -> std::io::Result<()> {
    let mut bytes = vec![0_u8; length];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = seed.wrapping_add((index as u64 * 131 % 251) as u8);
    }
    fs::write(path, bytes)
}

fn mcopy(image: &Path, source: &Path, destination: &str) -> Result<(), Box<dyn std::error::Error>> {
    successful(
        Command::new("mcopy")
            .args(["-o", "-i"])
            .arg(image)
            .arg(source)
            .arg(destination)
            .status()?,
        "mcopy",
    )
}

fn mtool(
    image: &Path,
    program: &str,
    arguments: &[&str],
) -> Result<(), Box<dyn std::error::Error>> {
    successful(
        Command::new(program)
            .args(["-i"])
            .arg(image)
            .args(arguments)
            .status()?,
        program,
    )
}

fn successful(status: ExitStatus, program: &str) -> Result<(), Box<dyn std::error::Error>> {
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} failed with {status}").into())
    }
}
