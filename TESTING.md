# Testing fatresize

The suite is organized around independent failure domains rather than a single
happy-path resize.

| Layer | Evidence |
| --- | --- |
| Parsers and geometry | FAT16/FAT32 boot records, FAT mirrors, FSInfo, directories, long names, MBR/GPT, exact-size boundaries, malformed and truncated input |
| Planning | Read-only growth/shrink plans, relocation, overlap scheduling, type-preservation and insufficient-space refusal |
| Execution | FAT16/FAT32 grow and shrink, nested directories, fragmented chains, MBR/GPT partition updates and exact target sizes |
| Recovery | Checksummed serialization, every truncation point, monotonic durable phases, wrong-volume and stale-plan refusal |
| Fault injection | Failure before every filesystem write followed by exact restoration |
| Independent interoperability | Images created and checked by `mkfs.fat` and `fsck.fat`, before and after real resizes |

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

## Remaining hardware boundary

Unprivileged image tests cover the filesystem and partition formats but cannot
reproduce every block-device failure. Release candidates still need disposable
hardware tests for mounted/contended devices, 4K logical sectors, abrupt power
loss, USB/SATA error behavior, and partition-table rereads. Those tests validate
the target environment; they do not replace the deterministic suite above.
