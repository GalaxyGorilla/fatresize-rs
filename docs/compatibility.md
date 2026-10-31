# Command-line compatibility

The compatibility target is user-visible behavior documented by public
`fatresize` man pages. This document does not derive from or describe another
tool's implementation.

## Command behavior

| Option | Short form | Behavior |
| --- | --- | --- |
| `--size SIZE` | `-s SIZE` | Grows or shrinks to a numeric size, or to safe container capacity with `max`. |
| `--info` | `-i` | Selects filesystem information output. |
| `--force-yes` | `-f` | Records non-interactive confirmation. |
| `--partition NUM` | `-n NUM` | Accepts a positive `u32` partition number. |
| `--progress` | `-p` | Enables progress output. |
| `--quiet` | `-q` | Suppresses non-error output; conflicts with `--verbose`. |
| `--verbose` | `-v` | Enables diagnostic output; conflicts with `--quiet`. |
| `--help` | `-h` | Prints usage without requiring a device. |

Boolean short options may be grouped, for example `-if`. `--` ends option
parsing. Duplicate options, unknown options, missing values, invalid values,
and extra positional arguments are errors.

The parser accepts a single device path, including a non-UTF-8 path on
platforms that support one. Option names and textual numeric values must be
valid UTF-8.

| Option | Behavior |
| --- | --- |
| `--dry-run` | Requires `--size`; computes a plan through a read-only descriptor. |
| `--check` | Validates boot metadata, FAT copies and chains, directories, LFN records, and allocation ownership. |
| `--backup FILE` | Alone, saves current recovery metadata; with `--size`, selects the required recovery record. |
| `--restore FILE` | Restores a recovery record; conflicts with resize, inspection, checking, and backup. |
| `--json` | Emits JSON for info/check and resize dry-runs. |

## Size syntax

An unsuffixed integer is a byte count. The case-insensitive token `max` resolves
to the selected image length or the available partition space up to the next
partition/GPT metadata boundary. Suffixes `B`, `K`, `M`, `G`, `T`, and
`P`, plus `KB`/`KiB` forms, are case-insensitive and use powers of 1024.
Decimal fractions are accepted only when they resolve to a whole number of
bytes. Negative values, unknown suffixes, sub-byte values, and `u64` overflow
are rejected.

Numeric targets are exact, not approximate. The requested byte count must be
representable by the FAT sector size and, for a partition, its logical block
size. Unaligned values fail before any write rather than being rounded up or
down. For example, `1GiB` produces a BPB and container length of exactly
1,073,741,824 bytes.

## Operation validation

At least one compatible operation mode is required. Resize conflicts with info/check and
restore; restore conflicts with every other operation; standalone backup
conflicts with info/check. `--dry-run` and `--progress` require resize.
`--json` is accepted only where structured output exists. Info and check may be
combined to print a report and return failure when issues exist.

## Filesystem and partition support

- FAT16 and FAT32 only; a resize that crosses the FAT16/FAT32 cluster threshold
  is rejected rather than converting the filesystem.
- FAT mirrors, FAT32 active-FAT flags, FSInfo, backup boot sectors, nested
  directories, valid VFAT long names, and multi-cluster files/directories are
  understood.
- MBR supports four primary entries. GPT requires matching valid primary and
  backup headers/entry arrays and valid CRC32 values.
- Partition sizes must remain sector-aligned and within usable, non-overlapping
  space.
- Writes are Linux-only. FAT images and raw devices require an exclusive
  advisory lock; raw block devices and their mounted child partitions are
  rejected.

## Recovery format

Every resize writes a versioned, checksummed recovery record before modifying
the volume. The record binds the source FAT identity and geometry and stores
every filesystem sector that the plan may overwrite or discard. Its phase is
updated durably as data movement and metadata commit progress.

An interrupted operation can be restored with `--restore FILE`. Restoring a
record already marked complete requires `--force-yes`, which prevents an
accidental rollback of a successful resize. Recovery records do not contain
MBR/GPT sectors; keep an independent partition-table backup when resizing a
partition.

## Scope boundaries

No FAT12, exFAT, FAT repair, FAT type conversion, MBR extended/logical
partitions, online resizing, or non-Linux writes. Bad-cluster markers make a
volume ineligible for resize. Locks are advisory. Loop-backed mounts are not
reliably inferred from a regular image path. Recovery records cover filesystem
sectors overwritten or discarded by the plan, not MBR/GPT sectors. Documented
JSON fields remain compatible; consumers should accept additional fields.
