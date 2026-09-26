# rust-fs-squashfs

Pure-Rust, **read-only** [SquashFS](https://docs.kernel.org/filesystems/squashfs.html)
driver. A clean-room SquashFS 4.0 reader over the shared
[`am-fs-core`](https://github.com/antimatter-studios/rust-fs-core) block-device
trait, exposing a stable C ABI (`fs_squashfs_*`) for FFI from Swift/FSKit, C, or Go.

SquashFS is a compressed, read-only filesystem — a peer format to ext4/ntfs/erofs,
not built on top of any of them. It cannot be modified in place (you regenerate the
whole image with `mksquashfs`), so this crate has **no write path**: the C ABI is the
read subset of its sister drivers' surface — no mkfs / create / write.

## Status

| Area | Support |
|------|---------|
| On-disk version | SquashFS 4.0 |
| Compression | **gzip** (id 1), **xz** (id 4), **lz4** (id 5), **zstd** (id 6), **lzo** (LZO1X, id 3) |
| Compression (legacy) | `lzma` (id 2) — best-effort |
| Inodes | basic + extended: dir, file, symlink, dev/fifo/socket |
| Data | full blocks, sparse blocks, tail fragments |
| Lookup tables | id (uid/gid), fragment, xattr, export |
| Resolve an inode number | through the export table: `read_inode_by_number`, `fs_squashfs_stat_ino` |
| xattrs | read: `list_xattrs` / `get_xattr`, including shared sets and out-of-line values. No write path — SquashFS has none. |

Every standard compressor `mksquashfs` can emit is decoded. gzip/xz/zstd use their
container stream formats (zlib / `.xz` / zstd frames); lz4 uses the raw LZ4 block
format with the uncompressed size taken from the block geometry; lzo is a clean-room
LZO1X decoder.

## Crate layout

- `superblock` — 96-byte superblock parse + validate
- `decompress` — codec dispatch (gzip / xz / lz4 / zstd / lzo, plus legacy lzma
  best-effort, all pure-Rust). LZO1X is decoded by the separate
  [`am-lzo1x`](https://crates.io/crates/am-lzo1x) crate, a clean-room decoder
  with no liblzo2-derived code
- `metablock` — 8 KiB metadata-block reader + cross-block cursor, and the
  decompressed-metadata cache
- `table` — indirect lookup tables (id, fragment, export)
- `inode` — all SquashFS inode shapes
- `dir` — directory-listing parser
- `xattr` — extended attributes: the three-level id table and the name/value pairs
- `fs` — top-level handle: path lookup, dir listing, file/symlink read, attributes
- `error` — the crate's `Error` type and its errno mapping for the C ABI
- `capi` — C ABI exports matching `include/fs_squashfs.h`

A mount holds two caches: `fs::DEFAULT_CACHE_BLOCKS` of the archive's blocks as
they sit on disk (`Filesystem::open_with_cache` sizes it; zero disables it), and
`fs::DEFAULT_META_CACHE_BLOCKS` metadata blocks after decompression
(`Filesystem::set_meta_cache_capacity`). What each saves is measured in
[`docs/read-path-cost.md`](docs/read-path-cost.md).

## CLI

```sh
cargo run --release --bin lssquashfs -- <image> info
cargo run --release --bin lssquashfs -- <image> tree /
cargo run --release --bin lssquashfs -- <image> cat /path/to/file
```

## Library use

```rust
use std::sync::Arc;
use fs_core::{BlockRead, FileDevice};
use fs_squashfs::Filesystem;

let dev = Arc::new(FileDevice::open("image.sqfs")?) as Arc<dyn BlockRead>;
let fs = Filesystem::open(dev)?;
let inode = fs.lookup_path("/etc/hostname")?;
let mut buf = vec![0u8; inode.file_size as usize];
fs.read_file(&inode, 0, &mut buf)?;
```

## Tests

```sh
chore test        # everything, on Linux natively and in a VM anywhere else
chore test:unit   # the tiers that need no tool, no fixture and no VM
chore lint
```

The suite is tiered, and each tier writes its whole output to
`tmp/logs/<tier>.log` and prints one verdict line. A tier that prints more than
its measured budget fails; so does one that executes fewer tests than its
measured floor, because a run that stopped early reports no failures at all and
only a count can see that.

**You do not install `squashfs-tools` to run this.** The oracle tools —
`mksquashfs`, `unsquashfs`, `sqfstar` — and the kernel mounts run inside a
Debian guest that [fs-linux-test-harness][harness] boots, where they are built
from source at a pinned version with every codec. One version, one platform,
the same answers for everyone; on a Mac, where there is no SquashFS at all, the
whole suite is compiled and run in the guest instead.

The oracle tests build a fixture tree with `mksquashfs -comp
{gzip,xz,lz4,zstd,lzo,lzma}` and read every path back through the driver,
asserting exact bytes. **None of them skip**: a tool or fixture that cannot be
reached fails the run, naming the task that provides it.

[harness]: https://github.com/antimatter-studios/fs-linux-test-harness

## License

MIT — see [LICENSE](LICENSE).
