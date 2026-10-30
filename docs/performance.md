# Performance

These are black-box measurements; no source from the original implementation
was inspected. The reproducible Rust harness is
[`benchmarks/compare.rs`](../benchmarks/compare.rs).

## Benchmark conditions

All timings below are machine-specific observations from one benchmark run,
not portable performance guarantees.

- Date: 2026-10-30
- Rust build: `cargo build --release`
- Original tool: Ubuntu `fatresize 1.1.0-2build2` (`20240401`)
- Measurements include process startup and suppress command output.
- Setup and image copying are outside the timed region.

## Results

Median wall-clock time:

| Scenario | Rust | Original | Difference |
| --- | ---: | ---: | ---: |
| Help/startup, 100 runs | 0.91 ms | 3.74 ms | Rust 4.1x faster |
| Inspect empty FAT16 64 MiB, 100 runs | 1.50 ms | 4.94 ms | Rust 3.3x faster |
| Inspect empty FAT32 256 MiB, 100 runs | 5.29 ms | 10.92 ms | Rust 2.1x faster |
| Shrink FAT16 64 MiB to exactly 48 MiB, 10 runs | 376.68 ms | No result | Original failed all runs |
| Inspect fragmented FAT32 256 MiB, 100 runs | 3.87 ms | 10.05 ms | Rust 2.6x faster |
| Fully check fragmented FAT32 256 MiB, 100 runs | 102.67 ms | No equivalent mode | — |
| Plan fragmented FAT32 shrink to 224 MiB | 330 ms | N/A | Original has no dry-run |
| Shrink fragmented FAT32 256 MiB to 224 MiB, 3 runs | 22.18 s | No result | Original failed all runs |

The Rust shrink result includes creating, checksumming, syncing, and updating a
recovery record that contains the discarded tail, performing the resize, and
post-write verification. The original executable terminated with signal 11 on
all regular-image shrink attempts during these measurements. An attempted loop-device
run terminated with a buffer-overflow abort, so reporting a comparative resize
time would be misleading.

The fragmented image contains 84 files, 14 fragmented files, 104 physical
extents, nested directories, and long filenames; the most fragmented file has
three extents. `--info` reports geometry and allocation statistics, matching
the intent of the original information operation. `--check` separately
performs full chain, ownership, mirror, directory, and LFN validation; the
original has no equivalent full-check mode.

Fragmentation initially exposed quadratic resize planning: the 224 MiB plan
did not finish within 44 seconds. The move scheduler now uses a dependency
graph and extent coalescing; the same plan completes in about 330 ms.

These numbers are not raw-device throughput claims. Storage, filesystem cache,
image allocation, sync behavior, and virtualization materially affect results.

## Reproduce

Create independent FAT images:

```sh
mkfs.fat -F 16 -C /tmp/fat16.img 65536
mkfs.fat -F 32 -C /tmp/fat32.img 262144
```

Create the deterministic fragmented workload (fixture construction requires
`mkfs.fat` and mtools, but production code has no native dependency):

```sh
cargo run --release --example make_fragmented -- \
  /tmp/fatresize-fragmented.img 256
```

Build and run the benchmark:

```sh
cargo build --release
rustc -O benchmarks/compare.rs -o target/fatresize-compare
target/fatresize-compare \
  target/release/fatresize /usr/sbin/fatresize /tmp/fat16.img 100
```

To include an isolated-copy resize attempt (capped at ten iterations):

```sh
target/fatresize-compare \
  target/release/fatresize /usr/sbin/fatresize /tmp/fat16.img 10 48Mi
```
