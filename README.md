# rust-fs-squashfs

> **Renamed to [`rust-fs-squashfs`](https://crates.io/crates/rust-fs-squashfs).**
> `am-fs-squashfs` 0.3.1 is the last version published under this name. New versions
> are published only as `rust-fs-squashfs`, starting at 0.4.0. To move, change one line
> in `Cargo.toml`:
>
> ```toml
> # before
> am-fs-squashfs = "0.3"
> # after
> rust-fs-squashfs = "0.4"
> ```
>
> The import is unchanged: `use fs_squashfs::...` keeps working.

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

## Command-line tools

One binary, `rust-fs-squashfs`, behind the `cli` feature (so the library a consumer links gains nothing from it), dispatching on the name it is started under. `fs.squashfs` is a symlink to it, and `rust-fs-squashfs fs ...` reaches the same tool under the one name nothing else on `PATH` can shadow.

```sh
fs.squashfs rootfs.sqfs ls /etc                        # JSON entries; a symlink's carries its target
fs.squashfs rootfs.sqfs read /etc/hostname > hostname  # raw bytes, or -o FILE
fs.squashfs rootfs.sqfs get                            # fs, label (null), sizes, and squashfs.*
fs.squashfs rootfs.sqfs get squashfs.compression --text
fs.squashfs --offset 1048576 firmware.bin ls /         # a filesystem embedded in a larger file
rust-fs-squashfs doctor --text                         # is the fs.squashfs on PATH this one?
```

A result is JSON on stdout (`--text` for people); a failure is `{"error": "...", "code": N}` on stderr, `N` being the exit status: 1 failed, 2 the command line was wrong, 3 the format cannot do that. `write`, `mkdir`, `set` and `resize` exist and answer "SquashFS is read-only" with status 3, so a script moved here from a writable filesystem fails loudly. `--version` prints `<tool> (am-fs-squashfs) <version>`.

`chore cli:install` builds the tools and stages them under every name in `tmp/cli/bin` (it prints the `PATH` line to use); `chore test:cli` tests them as installed.

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

## Verifying a release

From the next release onward, every version published to crates.io is
also attached to the GitHub release for its tag, with a build-provenance
attestation signed by this repository's release workflow. It proves the
crate was built by `.github/workflows/release.yml` from a commit in this
repository, not uploaded from someone's machine. To check the crates.io
download of version `X.Y.Z`:

```sh
curl -sSfLo am-fs-squashfs-X.Y.Z.crate https://static.crates.io/crates/am-fs-squashfs/am-fs-squashfs-X.Y.Z.crate
gh attestation verify am-fs-squashfs-X.Y.Z.crate \
  --repo antimatter-studios/rust-fs-squashfs \
  --signer-workflow antimatter-studios/rust-fs-squashfs/.github/workflows/release.yml
```

The workflow refuses to attest a `.crate` whose sha256 differs from the
checksum crates.io records for that version, so the file on the release
page and the crates.io download are the same bytes.

The command-line tool rides the same release, at the same version:
`am-fs-squashfs-X.Y.Z-darwin-arm64.tar.gz` and `-linux-x86_64.tar.gz`, each
an install prefix (`bin/`, `share/man/`, the shell completions,
`share/rust-fs-squashfs/CAVEATS`, `LICENSE`). They are packaged and
attested by rust-fs-core's shared `release-cli.yml` workflow, which this
repository's `release.yml` calls, so that is the workflow their
attestations name:

```sh
gh attestation verify am-fs-squashfs-X.Y.Z-darwin-arm64.tar.gz \
  --repo antimatter-studios/rust-fs-squashfs \
  --signer-workflow antimatter-studios/rust-fs-core/.github/workflows/release-cli.yml
```

Tarballs from releases up to 0.3.0 were packaged here, and their
attestations name `antimatter-studios/rust-fs-squashfs/.github/workflows/release.yml`.

## License

MIT — see [LICENSE](LICENSE).
