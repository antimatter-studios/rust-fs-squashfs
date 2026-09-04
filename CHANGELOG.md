# Changelog

Notable changes to `am-fs-squashfs`, newest first. This is a `0.x` crate, so the
**minor** is the compatibility boundary: a minor bump may break API, a patch
never does.

## [Unreleased]

## [0.1.4] — 2026-09-04

### Changed

- The metadata split, the buffer capacities and the null guard have names
  rather than being bare literals at the sites that depend on them.

## [0.1.3] — 2026-08-29

### Fixed

- **A short gzip block is refused instead of being served as file content.**
  A truncated block was being returned as if it were the whole thing, so a
  caller got a silently short file rather than an error.

### Changed

- Pinned toolchain moves to 1.95.0, in lockstep with the rest of the family.
- The CI lint gate can be run locally, and it is the same gate everywhere.

## [0.1.2] — 2026-08-25

### Changed

- **Uses the published `am-lzo1x` crate instead of a private copy.** The copy
  was a second implementation nobody was diffing against the first.
- Build dependencies are pinned and locked.

## [0.1.1] — 2026-06-21

### Added

- Initial release: a read-only SquashFS driver supporting every compressor the
  format defines — gzip, LZMA, LZO, XZ, LZ4 and zstd.
- The C ABI read path, with coverage for offsets, multi-block reads, fragments
  and EOF.
- `lssquashfs` CLI, with integration tests.
- Large-file read coverage across the block/fragment mix, plus stress and
  malformed-image tests.

[Unreleased]: https://github.com/antimatter-studios/rust-fs-squashfs/compare/v0.1.4...HEAD
[0.1.4]: https://github.com/antimatter-studios/rust-fs-squashfs/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/antimatter-studios/rust-fs-squashfs/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/antimatter-studios/rust-fs-squashfs/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/antimatter-studios/rust-fs-squashfs/releases/tag/v0.1.1
