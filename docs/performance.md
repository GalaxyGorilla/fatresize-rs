# Performance

These are black-box measurements; no source from the original implementation
was inspected. The reproducible Rust harness is
[`benchmarks/compare.rs`](../benchmarks/compare.rs).

## Benchmark conditions

All timings are machine-specific observations from one benchmark run, not
portable performance guarantees.

- Date: 2026-11-04
- Host: Apple M1 Pro running Docker Desktop 7.0.14, Linux arm64 container,
  10 virtual CPUs, container overlay storage
- Rust build: `cargo build --release --locked`, Rust 1.99.1
- Original tool: Debian `fatresize 1.1.0-2` (`20221231`)
- Fixtures: empty FAT16 64 MiB; empty FAT32 256 MiB; fragmented FAT32 256 MiB
  containing 84 files, 14 fragmented files, 104 physical extents, nested
  directories, and long filenames
- Partition fixtures: MBR primary partition 1 and GPT partition 1 containing
  the FAT volume
- Measurements include process startup and suppress command output. Each
  scenario used one warm-up run and then took the median of 100 info runs,
  20 check runs, or 3-5 resize runs. Image setup and copying were outside the
  timed region.
- Rust resize timings include creating and syncing a checksummed recovery
  record, data movement, metadata updates, and the post-write validation pass.
  The original tool performs no durable recovery record and no equivalent
  post-write validation.

## Results

| Scenario | Rust | Original fatresize |
| --- | ---: | ---: |
| Help / startup | 0.35 ms | 0.61 ms |
| Info: FAT16 64 MiB raw | 0.45 ms | 1.55 ms |
| Info: fragmented FAT32 256 MiB raw | 3.40 ms | 8.02 ms |
| Check: fragmented FAT32 256 MiB raw | 73.2 ms | no equivalent mode |
| Shrink FAT16 64 → 48 MiB, raw volume | 220 ms | aborts (SIGABRT) |
| Shrink FAT16 64 → 48 MiB, MBR partition | 217 ms | 10 ms |
| Shrink empty FAT32 256 → 224 MiB, raw volume | 485 ms | aborts (SIGABRT) |
| Shrink empty FAT32 256 → 224 MiB, MBR partition | 484 ms | exits 1 |
| Shrink fragmented FAT32 256 → 224 MiB, raw volume | 19.53 s | aborts (SIGABRT) |
| Shrink fragmented FAT32 256 → 224 MiB, MBR partition | 19.77 s | exits 1 |
| Shrink fragmented FAT32 256 → 224 MiB, GPT partition | 19.21 s | exits 1 |
| Compact fragmented FAT32 to `min` (raw) | 19.58 s | no `min` mode |
| Compact fragmented FAT32 to `min` (MBR) | 19.56 s | no `min` mode |
| Compact fragmented FAT32 to `min` (GPT) | 19.51 s | no `min` mode |

`min` produced a 208,783,360-byte filesystem with exactly **401,474 used
clusters out of 401,474**, fully packed. It relocated 114,716 clusters by
cluster number. Every file remained byte-identical, `fsck.fat` passed, and the
MBR/GPT partition entries matched the compacted size.

### Interpreting the resize numbers

- **Original FAT16 MBR shrink is faster** (10 ms versus 217 ms) because it
  performs only the filesystem resize: it writes no recovery record and does
  no post-write verification. It also rounded the 48 MiB target to 98,343
  sectors instead of the exact 98,304 sectors used by Rust. The Rust path
  trades speed for recoverability and exact targeting.
- **Original raw-volume shrinking aborted** with `SIGABRT` in every run, for
  both FAT16 and FAT32, on regular images and loop devices.
- **Original FAT32 partition shrinking failed** with exit status 1 and left the
  partition and filesystem unchanged, for both empty and fragmented volumes.
- **Fragmented FAT32 shrinking is dominated by data movement.** Resizing the
  FAT changes the start of the data area, so every allocated cluster moves
  physically: about 401,000 clusters (206 MB / 196 MiB) in this fixture.
  Recovery also snapshots overwritten target sectors and the discarded tail;
  the 224 MiB plan wrote a 243 MB recovery record, and `min` wrote a 269 MB
  record. The resulting time is I/O-bound rather than scheduler-bound: the plan
  has only about 59 move steps for 114,716 relocated clusters.
- No resize speed-up is claimed for fragmented volumes because the original
  tool did not complete a single shrink attempt in this environment. The
  meaningful result is that the Rust implementation completes and verifies the
  operation while the original does not.

## Reproduce

Create independent FAT images:

```sh
mkfs.fat -F 16 -C /tmp/fat16.img 65536
mkfs.fat -F 32 -C /tmp/fat32.img 262144
```

Create the deterministic fragmented workload (fixture construction requires
`mkfs.fat` and mtools; production code has no native dependency):

```sh
cargo run --release --example make_fragmented -- \
  /tmp/fatresize-fragmented.img 256
```

Build the benchmark harness:

```sh
cargo build --release
rustc -O benchmarks/compare.rs -o target/fatresize-compare
target/fatresize-compare \
  target/release/fatresize /usr/sbin/fatresize /tmp/fat16.img 20
```

Benchmark a partition by adding the one-based partition number:

```sh
target/fatresize-compare \
  target/release/fatresize /usr/sbin/fatresize /tmp/disk.img 3 224Mi 1
```

Create the partition fixtures by placing an image into a partitioned disk:

```sh
# MBR, 256 MiB partition starting at sector 2048
truncate -s 400M /tmp/disk.img
printf 'label: dos\nunit: sectors\nstart=2048, size=524288, type=c\n' |
  sfdisk /tmp/disk.img
dd if=/tmp/fatresize-fragmented.img of=/tmp/disk.img \
  bs=512 seek=2048 conv=notrunc

# GPT, same partition layout
truncate -s 400M /tmp/disk-gpt.img
sgdisk -n 1:2048:+256M -t 1:0700 /tmp/disk-gpt.img
dd if=/tmp/fatresize-fragmented.img of=/tmp/disk-gpt.img \
  bs=512 seek=2048 conv=notrunc
```

To benchmark `min`, get the resolved byte count from
`fatresize --dry-run --size min --json IMAGE`, then pass that byte count to
both tools or skip the original tool because it has no `min` token.
