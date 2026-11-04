# Testing fatresize

The suite is organized around independent failure domains rather than a single
happy-path resize.

| Layer | Evidence |
| --- | --- |
| Parsers and geometry | FAT16/FAT32 boot records across 512-4096-byte sectors, FAT mirrors/active FAT, FSInfo, directories, long names, multi-cluster roots, MBR/GPT at 512- and 4096-byte blocks, exact-size boundaries, malformed and truncated input |
| Planning | Read-only growth/shrink/min plans, relocation, overlap scheduling, type-preservation and insufficient-space refusal, active-FAT handling, FAT16 fixed-root and FAT32 root moves |
| Execution | FAT16/FAT32 grow and shrink, 4K FAT sectors, nested directories, fragmented chains, MBR/GPT partition updates and exact target sizes |
| Recovery | Checksummed serialization, every truncation and body-corruption point, monotonic durable phases, wrong-volume and stale-plan refusal, exhaustive write-failure injection for FAT16 grow and FAT32 shrink |
| Partition tables | MBR validation and selective writes; GPT CRCs, primary/backup ordering, usable ranges, 4 KiB blocks, overlap and GUID validation |
| Randomized checks | Thousands of generated move schedules simulated for source integrity; adversarial mutations of valid FAT16/FAT32 images with panic detection |
| Independent interoperability | Images created and checked by `mkfs.fat`, `fsck.fat`, and mtools, including fragmented FAT16/FAT32 shrink, grow, `min`, restore, and MBR/GPT partition resizing |

## Release gate

```sh
cargo fmt --check
cargo test --release --all-targets --locked
cargo clippy --all-targets -- -D warnings
cargo deny check
cargo package --locked
cargo build --release --locked
scripts/interop.sh
```

The interoperability script exits with status 77 when its external tools are
not installed. CI installs `dosfstools`, so a skipped interoperability run is a
failure there.

All other test targets are self-contained: fixtures are constructed
byte-by-byte in Rust, including fragmented chains, malformed metadata, MBR/GPT
disks, and 4K FAT sectors. Writable CLI integration tests are compiled only on
Linux because writable device access requires Linux `flock(2)` support.

## Remaining hardware boundary

Unprivileged image tests cover the filesystem and partition formats but cannot
reproduce every block-device failure. Release candidates still need disposable
hardware tests for mounted/contended devices, 4K logical sectors, abrupt power
loss, USB/SATA error behavior, and partition-table rereads. Those tests validate
the target environment; they do not replace the deterministic suite above.
