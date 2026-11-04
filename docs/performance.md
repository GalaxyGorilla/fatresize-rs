# Performance

These are black-box measurements; no source from the original implementation
was inspected. The reproducible Rust harness is
[`benchmarks/compare.rs`](../benchmarks/compare.rs).

## Benchmark conditions

All timings are machine-specific observations from benchmark runs, not
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
| Shrink FAT16 64 → 48 MiB, MBR partition | 217 ms | 10.6 ms |
| Shrink FAT16 one low file 64 → 48 MiB, MBR | 230 ms | 12.2 ms |
| Shrink FAT16 fragmented 64 → 48 MiB, MBR | 727 ms | 58 ms |
| Shrink FAT16 fragmented 64 → 40 MiB, MBR | 819 ms | 67 ms |
| Shrink FAT16 fragmented 64 → 48 MiB, GPT | 726 ms | 56 ms |
| Grow FAT16 empty 64 → 80 MiB, MBR | 11 ms | 8.4 ms |
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

- **On supported FAT16 partition resizes, the original is 12-33x faster**
  (10.6-67 ms versus 217-819 ms). It performs only the filesystem resize: it
  writes no recovery record and does no post-write verification. It also rounds
  to its own geometry: the 48, 40, and 80 MiB requests became 98,343, 81,937,
  and 163,773 sectors, while Rust targets exactly 98,304, 81,920, and 163,840.
  The Rust path trades speed for recoverability and exact targeting. The gap
  grows with the amount of data moved: 217 ms for an empty volume, 230 ms for
  one low file, and 727-819 ms for a fragmented 35-file volume.
- **Growing an empty FAT16 partition is close** (11 ms versus 8.4 ms) because
  the volume has almost no allocated data to move and the recovery record is
  small.
- **Original raw-volume shrinking aborted** with `SIGABRT` in every run, for
  both FAT16 and FAT32, on regular images and loop devices.
- **Original FAT32 partition shrinking failed** with exit status 1
  (`Unable to satisfy all constraints on the partition`) and left the partition
  and filesystem unchanged, for both empty and fragmented volumes. FAT32 grow
  failed with `No Implementation: GNU Parted cannot resize this partition to
  this size`.
- **Fragmented FAT32 shrinking is dominated by data movement.** Resizing the
  FAT changes the start of the data area, so every allocated cluster moves
  physically: about 401,000 clusters (206 MB / 196 MiB) in this fixture.
  Recovery also snapshots overwritten target sectors and the discarded tail;
  the 224 MiB plan wrote a 243 MB recovery record, and `min` wrote a 269 MB
  record. The resulting time is I/O-bound rather than scheduler-bound: the plan
  has only about 59 move steps for 114,716 relocated clusters.
- For **fragmented FAT16 partitions**, both tools complete and the original is
  faster for the reasons above; content and `fsck.fat` were verified after the
  original's 64 → 48 and 64 → 40 MiB shrinks. For **fragmented FAT32**, no
  speed-up comparison is possible because the original did not complete; the
  meaningful result is that the Rust implementation completes and verifies the
  operation.

## Reproduce

Create independent FAT images:

```sh
mkfs.fat -F 16 -C /tmp/fat16.img 65536
mkfs.fat -F 32 -C /tmp/fat32.img 262144
```

Create the deterministic fragmented FAT32 workload (fixture construction
requires `mkfs.fat` and mtools; production code has no native dependency):

```sh
cargo run --release --example make_fragmented -- \
  /tmp/fatresize-fragmented.img 256
```

Create the fragmented FAT16 fixture used for the fair partition comparison:

```sh
truncate -s 64M /tmp/fat16-frag.img
mkfs.fat -F 16 /tmp/fat16-frag.img
head -c 1048576 /dev/zero > /tmp/one.bin
for i in $(seq -w 0 59); do
  mcopy -o -i /tmp/fat16-frag.img /tmp/one.bin ::/F$i.BIN
done
for i in $(seq 0 2 58); do
  mdel -i /tmp/fat16-frag.img ::/F$(printf %02d $i).BIN
done
for i in 60 61 62 63 64; do
  mcopy -o -i /tmp/fat16-frag.img /tmp/one.bin ::/F$i.BIN
done
```

Build the benchmark harness:

```sh
cargo build --release
rustc -O benchmarks/compare.rs -o target/fatresize-compare
target/fatresize-compare \
  target/release/fatresize /usr/sbin/fatresize /tmp/fat16.img 20
```

Benchmark a partition by adding the one-based partition number. The size token
`Mi` is accepted by both tools:

```sh
target/fatresize-compare \
  target/release/fatresize /usr/sbin/fatresize /tmp/disk16.img 5 48Mi 1
```

Create partition fixtures by placing an image into a partitioned disk:

```sh
# MBR, 64 MiB FAT16 partition starting at sector 2048
truncate -s 400M /tmp/disk16.img
printf 'label: dos\nunit: sectors\nstart=2048, size=131072, type=06\n' |
  sfdisk /tmp/disk16.img
dd if=/tmp/fat16-frag.img of=/tmp/disk16.img \
  bs=512 seek=2048 conv=notrunc

# MBR, 256 MiB FAT32 partition starting at sector 2048
truncate -s 400M /tmp/disk.img
printf 'label: dos\nunit: sectors\nstart=2048, size=524288, type=c\n' |
  sfdisk /tmp/disk.img
dd if=/tmp/fatresize-fragmented.img of=/tmp/disk.img \
  bs=512 seek=2048 conv=notrunc

# GPT, same FAT32 partition layout
truncate -s 400M /tmp/disk-gpt.img
sgdisk -n 1:2048:+256M -t 1:0700 /tmp/disk-gpt.img
dd if=/tmp/fatresize-fragmented.img of=/tmp/disk-gpt.img \
  bs=512 seek=2048 conv=notrunc
```

To benchmark `min`, get the resolved byte count from
`fatresize --dry-run --size min --json IMAGE`, then pass that byte count to
both tools or skip the original tool because it has no `min` token.
