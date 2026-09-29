# Changelog

Notable changes to `am-fs-squashfs`, newest first. This is a `0.x` crate, so the
**minor** is the compatibility boundary: a minor bump may break API, a patch
never does.

## [Unreleased]

### Added

- Releases carry a build-provenance attestation: the published `.crate` is
  attached to the GitHub release for its tag, checked first against the
  crates.io checksum, and verifiable with `gh attestation verify` (see the
  README, "Verifying a release").

## [0.3.0] — 2026-09-28

### Breaking

- **Paths cross the C ABI as bytes, not UTF-8.** `fs_squashfs_stat`,
  `_dir_open`, `_read_file`, `_readlink`, `_listxattr` and `_getxattr` read
  their `const char *` as the bytes up to the NUL and compare them byte for
  byte against the names in the image. They no longer decode, so no encoding
  is assumed and none is required (#67).

  **Source-compatible for every caller passing UTF-8**, because UTF-8 is a
  byte string too. What changes is that a caller passing anything else now
  works instead of being refused, and the errno for a path naming no file is
  `ENOENT` rather than the `EINVAL` an undecodable one used to get.

  SquashFS directory entry names are raw bytes and the format has no field
  that could say what encoding they are in, so this crate carries them as
  `Vec<u8>` everywhere — and the ABI was the one place that did not. The
  asymmetry was the defect: `fs_squashfs_dir_next` fills `fs_squashfs_dirent_t.name`
  from the raw bytes, so the library REPORTED a name it then REFUSED to
  accept. A caller that walked a directory and stat'd each entry got `EINVAL`
  on exactly the entries this library had handed it a moment earlier, with no
  byte-oriented entry point to work around it. The images that reaches are
  ordinary rather than hostile: any box with a non-UTF-8 locale produces such
  names, as does anything copied off a legacy Windows or Mac volume.

  `Filesystem::lookup_path_bytes` is the resolution, and `lookup_path(&str)`
  is now a wrapper over it, so the Rust API is unchanged.

  `fs_squashfs_mount` is deliberately not in the list: its argument is a path
  on the HOST filesystem, handed to `FileDevice::open`, which is a different
  question from an in-image name.

  **A consumer whose own namespace requires valid UTF-8** — macOS, where APFS
  and FSKit do — should escape such a name *reversibly* (percent-encoding, or
  surrogate escapes), so it can be turned back into these bytes. A lossy
  conversion maps distinct names onto one: `caf\xe9` and `caf\xea` both become
  `caf<U+FFFD>`, and two files become indistinguishable.

## [0.2.0] — 2026-09-27

### Breaking

- **`fs_squashfs_readlink` returns the target's length, not 0.** On
  success it returns the length in bytes excluding the NUL, as Linux
  `readlink(2)` does, and still writes the target NUL-terminated. A
  caller testing `== 0` for success must test `>= 0`. The same contract
  is adopted by every driver in the family, so a consumer linking several
  needs no per-driver wrapper (#121).
- **A buffer too small is `ERANGE` and writes nothing, including
  `bufsize == 0`**, which was `EINVAL`. The last error names the size
  needed. `bufsize` must be at least the length plus one; exactly that
  succeeds. NULL `fs`, `path` or `buf` stays `EINVAL`.

### Added

- **The parsers are fuzzed, on two tiers.** SquashFS is read-only and
  mounted from sources the reader did not produce — a distribution image,
  a container layer, a firmware payload — and nothing here had a fuzz
  target. `fuzz/` holds four `cargo-fuzz` targets and runs nightly on a
  bounded budget; `tests/fuzz_decoders.rs` is the gate, replaying and
  mutating the same corpus deterministically on the stable toolchain in
  under five seconds.

  The corpus is six images `mksquashfs` wrote — one per compressor this
  crate claims to decode — plus the superblock, a decompressed directory
  metablock and a compressed metablock cut out of each. The `image`
  target opens and walks a mutated one, which is the only way the
  metablock cache, the id, fragment and xattr tables and the export
  lookup get fuzzed at all: none of them takes a byte slice.

  Two of the tests are oracles rather than fuel.
  `every_committed_image_opens_and_lists_its_root` keeps a seed from
  quietly becoming unreadable, since a mutation of an unreadable image is
  also unreadable. `the_corpus_decompresses_under_its_own_codec` decodes
  each committed metablock with the compressor that produced it and
  requires all six to be represented — so a codec this crate stopped
  decoding fails here, on a machine with no `mksquashfs` installed (#108).

- **Device nodes keep their major and minor numbers.** The parser read
  each block and character device's `rdev` and dropped it, so every
  device in an image reported 0:0. `Inode::rdev` holds the raw value
  (Linux `new_encode_dev` packing), with `rdev_major` / `rdev_minor` to
  decode it; `lssquashfs ls` prints `major, minor` where `ls -l` does.
  **Breaking:** `fs_squashfs_attr_t` gains a trailing `uint32_t rdev`, so
  the struct is larger and C consumers must rebuild; `Inode` gains a
  public field, so code building one with a struct literal must add it.
- **A 256-byte filename lists at full length through the C ABI.** The
  format allows 256 bytes and the parser accepted them, but
  `fs_squashfs_dirent_t` had `char name[256]` and a `uint8_t name_len`,
  so the last byte went to the NUL: the listing showed a 255-byte name
  that did not open. **Breaking ABI:** `name` is now `char name[257]` and
  `name_len` is `uint16_t`, which moves `name`'s offset; C consumers must
  rebuild against the new header.
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

### Fixed

- **A superblock is checked against the image it describes.** Only the
  magic, block size and major version were checked, so `bytes_used` = 1 TiB
  over a 20 KiB file mounted and was published through
  `fs_squashfs_get_volume_info`, and table starts past the end, inside the
  superblock or out of order were accepted. `Filesystem::open` now refuses,
  each with its own `BadSuperblock` reason: `bytes_used` beyond the device;
  an unknown minor version (the kernel's rule); a table start outside
  `[96, bytes_used)`; table starts out of the order `mksquashfs` writes them;
  a root inode reference outside the inode table. A 3.x image is refused
  as the wrong version rather than as a bad block size.

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

[0.2.0]: https://github.com/antimatter-studios/rust-fs-squashfs/compare/v0.1.5...v0.2.0
[0.1.5]: https://github.com/antimatter-studios/rust-fs-squashfs/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/antimatter-studios/rust-fs-squashfs/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/antimatter-studios/rust-fs-squashfs/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/antimatter-studios/rust-fs-squashfs/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/antimatter-studios/rust-fs-squashfs/releases/tag/v0.1.1
