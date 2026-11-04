# fatresize

A safe, exact, and recoverable FAT16/FAT32 filesystem resizer written in Rust.
It can resize a filesystem stored directly in an image or inside an MBR/GPT
partition, while keeping the FAT type and file contents intact.

**Start here:** try the [disposable-image walkthrough](#safe-first-run), then
read the [real-device checklist](#before-resizing-real-storage). Use
`fatresize --info DEVICE` when you only want to inspect a volume.

## What it does

`fatresize` inspects, validates, plans, grows, shrinks, backs up, and restores
FAT16/FAT32 volumes. It rejects inexact targets and inconsistent source
filesystems before writing. Keep a separate verified backup when modifying
storage.

It deliberately does **not** repair damaged filesystems, convert FAT types, or
support FAT12/exFAT. Use a filesystem checker before resizing if the source may
already be damaged.

## Safe first run

This walkthrough touches only a disposable image. It needs `mkfs.fat`
(`dosfstools` on most Linux distributions):

```sh
truncate -s 128MiB demo.img
mkfs.fat -F 32 demo.img

cargo run --release -- --info demo.img
cargo run --release -- --dry-run --size 96MiB demo.img
cargo run --release -- --force-yes --size 96MiB \
  --backup demo.fatrsz demo.img
cargo run --release -- --check demo.img
```

The dry run prints the exact plan without opening the image writable. The real
resize creates `demo.fatrsz` first. Keep that recovery record until `--check`
passes and you have independently verified the resized filesystem.

## Common tasks

```sh
# Fast, read-only geometry and usage summary
fatresize --info DEVICE

# Full, read-only consistency check
fatresize --check DEVICE

# Preview an exact resize without writing
fatresize --dry-run --size 8GiB DEVICE

# Compact to the smallest safe size, relocating fragmented data
fatresize --dry-run --size min DEVICE

# Resize partition 1 and place recovery data on separate storage
fatresize --force-yes --size 8GiB --partition 1 \
  --backup /safe/disk.fatrsz /dev/sdX
```

For an image or block device containing only a FAT volume, pass that path
directly. To resize a partition and its table entry together, pass the
**whole-disk** path and `--partition NUMBER`, as in the final example.

## Command line

```text
fatresize [OPTIONS] DEVICE
```

Compatibility options are `--size`, `--info`, `--force-yes`, `--partition`,
`--progress`, `--quiet`, `--verbose`, and `--help`. Enhanced options are
`--dry-run`, `--check`, `--backup`, `--restore`, and `--json`. Run
`fatresize --help` for value syntax and short aliases.

Sizes without a suffix are bytes. `B`, `K`, `M`, `G`, `T`, and `P` are
accepted case-insensitively; `KB` and `KiB` forms (and their larger
counterparts) are also accepted. All units use powers of 1024. Exact decimal
fractions such as `1.5GiB` are supported, but sub-byte results are rejected.
`max` selects the safe capacity of the image or selected partition; `min`
selects the smallest geometry that can hold every allocated cluster after
relocation. Explicit target sizes are exact: the resulting BPB and image or
partition length equal the requested byte count. Targets that are not aligned
to both the FAT sector and container block size are rejected instead of
rounded.

## Before resizing real storage

1. Keep a verified backup of the data; the recovery record protects metadata,
   not every file.
2. Unmount the filesystem and stop software that may access the device.
3. Run `--check`, then run the intended resize once with `--dry-run`.
4. Put `--backup` on different reliable storage.
5. After resizing, run `--check` again and verify the filesystem independently.
6. Keep the recovery record until the result is confirmed.

## Performance

These are machine-specific median wall-clock results from one benchmark run
using release builds on an Apple M1 Pro host inside a Linux arm64 container.
They are not portable performance guarantees. Full methodology, failure
details, and fixture commands are in
[docs/performance.md](docs/performance.md).

| Operation | Rust | Original fatresize |
| --- | ---: | ---: |
| Startup/help | 0.35 ms | 0.61 ms |
| Info: FAT16 64 MiB | 0.45 ms | 1.55 ms |
| Info: fragmented FAT32 256 MiB | 3.40 ms | 8.02 ms |
| Check: fragmented FAT32 256 MiB | 73 ms | no equivalent mode |
| Shrink FAT16 64 → 48 MiB, raw volume | 220 ms | aborts (SIGABRT) |
| Shrink FAT16 64 → 48 MiB, MBR partition | 217 ms | 10 ms |
| Shrink empty FAT32 256 → 224 MiB, MBR partition | 484 ms | exits 1 |
| Shrink fragmented FAT32 256 → 224 MiB, MBR partition | 19.8 s | exits 1 |
| Shrink fragmented FAT32 256 → 224 MiB, GPT partition | 19.2 s | exits 1 |
| Compact fragmented FAT32 to `min` (208.8 MB) | 19.5 s | no `min` mode |

The fragmented fixture contains 84 files, 14 fragmented files, and 104 physical
extents. `min` packed all **401,474 allocated clusters into 401,474 available
clusters**, kept every file byte-identical, passed `fsck.fat`, and updated both
MBR and GPT partition entries.

Two results deserve context:

- The original tool is faster for the simple empty FAT16 MBR shrink because it
  writes no durable recovery record and performs no post-write verification.
  It also rounded the target to 98,343 sectors instead of the exact 98,304.
- The original aborted every raw-volume shrink and failed every FAT32 partition
  shrink tested here, so no fragmented-shrink speedup is claimed. The Rust path
  completes and verifies the operation. The ~19.5 s fragmented shrink is
  dominated by moving roughly 206 MB (196 MiB) of allocated data (resizing the FAT
  shifts the start of the data area) plus writing a large recovery record, not
  by the move scheduler.

## Supported matrix

| Area | Supported |
| --- | --- |
| Filesystems | FAT16 and FAT32, preserving the FAT type |
| Containers | Raw volume images; MBR primary partitions 1-4; GPT partitions |
| Operations | Read-only info/check/plan; grow/shrink; recovery backup/restore |
| Sector sizes | FAT sectors 512/1024/2048/4096; Linux block logical sectors >=512, power of two |
| Writable hosts | Linux only; runtime dependencies remain standard-library-only |

FAT12, exFAT, FAT type conversion, MBR logical/extended partitions, encrypted
or stacked-device discovery, and filesystem repair are not supported.

## Safety model

- Planning, `--info`, `--check`, and `--dry-run` open the target read-only.
- Writes require a nonblocking Linux `flock(2)` exclusive advisory lock.
- Raw block writes reject a mounted device or mounted child partition using
  `/proc/self/mountinfo` and sysfs identities.
- Resizing requires a fully consistent source, confines all I/O to the selected
  volume, and rejects bad/reserved clusters, cross-links, malformed chains,
  mismatched FAT mirrors, and partition-boundary crossings.
- A checksummed recovery record is written to a temporary file, synced,
  atomically published without replacement, and its parent directory synced
  before volume writes.
- Growth expands the container before filesystem writes. Shrink commits the
  smaller container only after rereading and validating the resized filesystem.
- Metadata stages are synced; GPT updates write and sync the backup copy before
  the primary copy.

Advisory locks do not stop non-cooperating programs. Mounted detection does not
identify every loop or device-mapper relationship for a regular image. A power
loss can still require recovery. Partition-table sectors are not stored in the
filesystem recovery record, so interruption during an MBR/GPT table update may
require external partition-table repair. Always keep an independent backup.

## Recovery

Choose a recovery file on different reliable storage for raw-device work:

```sh
fatresize --size 8GiB --backup /safe/disk.fatrsz --partition 1 /dev/sdX
fatresize --restore /safe/disk.fatrsz --partition 1 /dev/sdX
```

Do not delete the record until the resized filesystem and partition table have
been independently checked. Restoring a record marked complete requires
`--force-yes`; this prevents accidental rollback after success. Recovery is
repeatable and restores non-boot sectors durably before committing the primary
boot sector.

## Clean-room policy

This implementation must be developed from public specifications,
documentation, and independently observed behavior. Do not copy, translate, or
adapt source code from GPL-licensed implementations.

Evidence and public references are recorded in
[docs/clean-room.md](docs/clean-room.md). Detailed behavior and limitations are
in [docs/compatibility.md](docs/compatibility.md). Reproducible black-box
performance measurements are in [docs/performance.md](docs/performance.md).

## Documentation

- [Test strategy and hardware boundary](TESTING.md)
- [Compatibility and deliberate limits](docs/compatibility.md)
- [Recovery and format behavior](docs/compatibility.md#recovery-format)
- [Performance methodology](docs/performance.md)
- [Clean-room development policy](docs/clean-room.md)

## Building

Install a current Rust toolchain, then run:

```sh
cargo build --release --locked
cargo test --release --all-targets --locked
cargo clippy --all-targets -- -D warnings
```

The production binary is `target/release/fatresize`. To install it into
Cargo's binary directory:

```sh
cargo install --path . --locked
```

## License

Licensed under either the Apache License, Version 2.0 or the MIT License, at
your option.
