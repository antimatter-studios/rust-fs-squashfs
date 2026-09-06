# Changelog

Notable changes to `am-fs-squashfs`, newest first. This is a `0.x` crate, so the
**minor** is the compatibility boundary: a minor bump may break API, a patch
never does.

## [Unreleased]

### Added

- **Extended attributes can be read.** Every extended inode carried an
  `xattr` index and every one of them threw it away, while the
  superblock's `xattr_id_table_start` was parsed and never used again —
  so the driver knew where the attributes were, and which set each inode
  pointed at, and presented every file as having none. An image built
  with `mksquashfs -xattrs`, the default since squashfs-tools 4.2, lost
  everything a user had set.
  - `Filesystem::list_xattrs` and `Filesystem::get_xattr` in Rust;
    `fs_squashfs_listxattr` and `fs_squashfs_getxattr` on the C ABI, with
    the signatures and semantics `fs_ext4_*` already has.
  - Names come back assembled: SquashFS stores the namespace prefix as a
    small integer and the rest of the name after it.
  - Sets shared between inodes, and out-of-line values shared between
    sets, both resolve — that indirection is what the id table is for.
  - An image built with `-no-xattrs` reports an empty list rather than an
    error, as does a file with no attributes in an image that has them.
- **An inode number resolves back to its inode**, through the export
  table the superblock already pointed at and nothing had ever read.
  Every other way into the filesystem starts at the root and walks down,
  which is useless to a caller holding nothing but a number — an NFS file
  handle, or any identifier a layer above handed out earlier. Without
  this it had to keep its own map of every inode it ever mentioned, or
  walk the tree again.
  - `Filesystem::read_inode_by_number` and `Filesystem::is_exportable` in
    Rust; `fs_squashfs_stat_ino` and `fs_squashfs_is_exportable` on the
    C ABI.
  - Only the table's pointer array is loaded at mount — one `u64` per
    8 KiB of entries — and a lookup decompresses the single metadata
    block it needs, through the metadata cache. The table itself is the
    only one here that scales with the image: a million inodes is eight
    megabytes of it.
  - An image built with `mksquashfs -no-exports` says so rather than
    guessing.
- A cache of **decompressed** metadata blocks, keyed by their offset in
  the image. SquashFS keeps inodes and directory listings in 8 KiB
  compressed blocks; before this, every inode read and every listing put
  the block it lives in through the codec again. On the fixture in
  `tests/read_path_cost.rs` a directory walk fell from 18.5 ms to
  0.45 ms and resolving 216 paths from 14.6 ms to 0.31 ms, with the
  device reads unchanged at zero — three metadata blocks had been
  decompressed 4976 times. Size it with
  `Filesystem::set_meta_cache_capacity` (zero disables it); the default
  holds 256 blocks, 2 MiB.

### Changed

- `Error` gains `NotExportable`, for an image with no export table.
  Distinct from `NotFound` on purpose: one says the inode is not there,
  the other says the question cannot be asked of this image, and a caller
  that treated them alike would retry a lookup that can never succeed.
- `metablock::read_block`, `metablock::MetaCursor::new` and
  `Inode::read` take the cache to consult (`None` for none), and
  `read_block` hands back a shared `Arc<Vec<u8>>` rather than a fresh
  `Vec`. Callers going through `Filesystem` are unaffected.

## [0.1.5] — 2026-09-06

### Fixed

- The decompression ceiling is enforced before the memory is spent
  rather than after: three decompressors checked the limit only once
  they had already committed the allocation.
- A directory listing and the running data-block offset are bounded, so
  a crafted image cannot walk either past the end of what it declared.

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
