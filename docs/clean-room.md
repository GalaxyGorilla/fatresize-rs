# Clean-room evidence policy

`fatresize` is developed without inspecting, copying, translating, or adapting
implementation source from GPL-licensed FAT resizing tools.

## Permitted evidence

- Published filesystem specifications and standards.
- Public manuals, man pages, help output, release notes, and user-facing bug
  reports.
- Independently designed test images and observations of externally visible
  behavior.
- Permissively licensed material whose license is compatible with this
  project's `MIT OR Apache-2.0` licensing, with attribution where required.
- Original analysis and code written from the evidence above.

## Prohibited evidence

- GPL implementation source, patches, disassembly, decompilation, or
  source-derived explanations of implementation details.
- Copying or line-by-line translation from an incompatible implementation.
- Test fixtures copied from implementation repositories unless their license
  and provenance have been reviewed and recorded.

## Recording evidence

Compatibility work should record:

1. the public source title, URL or publication identifier, and access date;
2. the behavior being established;
3. independently created inputs and observed outputs;
4. ambiguities or conflicts between sources; and
5. the resulting project test, without source-derived implementation detail.

Keep evidence notes and implementation changes separable in review. If the
provenance or license of material is uncertain, do not use it until resolved.

## Evidence register

Accessed 2026-10-30:

- Microsoft, *Microsoft Extensible Firmware Initiative FAT32 File System
  Specification, FAT: General Overview of On-Disk Format*, version 1.03:
  <https://download.microsoft.com/download/1/6/1/161ba512-40e2-4cc9-843a-923143f3456c/fatgen103.doc>.
  Used for BPB geometry, FAT16/FAT32 entry values, directory/LFN layout,
  FSInfo, backup boot sectors, and FAT type cluster thresholds.
- UEFI Forum, *UEFI Specification*, GUID Partition Table chapter:
  <https://uefi.org/specifications>. Used for GPT headers, entry arrays,
  usable-LBA boundaries, mirrored metadata, and CRC32.
- Debian manpages, `fatresize(1)`:
  <https://manpages.debian.org/bookworm/fatresize/fatresize.1.en.html>.
  Used only for public command names, short options, and the `max` size token.
- Linux man-pages, `flock(2)`:
  <https://man7.org/linux/man-pages/man2/flock.2.html>. Used for exclusive,
  nonblocking advisory-lock semantics.
- Linux man-pages, `proc_pid_mountinfo(5)`:
  <https://man7.org/linux/man-pages/man5/proc_pid_mountinfo.5.html>. Used for
  mounted block-device identity checks.

All test images in this repository are generated independently in test code.
They contain no copied implementation fixtures. Behavioral tests exercise this
project directly; no GPL implementation source or source-derived test vectors
were consulted.
