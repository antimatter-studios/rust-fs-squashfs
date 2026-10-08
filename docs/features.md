# Features

What this driver does today, what it refuses, and what is coming. **Every
pull request that adds, fixes, refuses or removes behaviour updates its row
here, in the same pull request** (AGENTS.md). The reasoning behind each change
is in [CHANGELOG.md](../CHANGELOG.md).

**Since** is the release a row's current state shipped in, with the issue or
pull request the changelog cites for it. Work merged after the last release
is **Unreleased (#N)** until the next one. **Tracking** names the issue for
anything not finished.

SquashFS is read-only by format: an image is regenerated whole, never changed
in place, so this crate has no write path and no formatter.

States:

- **Supported**: works, and is checked against squashfs-tools or the Linux
  kernel in the harness VM.
- **Experimental**: works in every test, but is new.
- **Partial**: works for part of the case, and the row says which part.
- **Refused**: recognised and refused by name, rather than misread.
- **Not supported**: neither read nor refused by name.
- **Upcoming**: an open issue with a plan.

## Reading

| Feature | State | Since | Tracking | Checked by |
|---|---|---|---|---|
| SquashFS 4.0 superblock, checked against the image it describes | Supported | 0.1.1; checked 0.2.0 | | `roundtrip.rs`, `oracle_compat.rs` |
| Any other major version (3.x and older) | Refused | 0.1.1 | | |
| Compression: gzip, xz, lz4, zstd, lzo | Supported | 0.1.1 | | `oracle_compat.rs`, `kernel_readback.rs` |
| xz with a branch-converter filter (`mksquashfs -Xbcj`) | Supported | 0.2.0 (#52) | | `oracle_compat.rs` |
| Legacy lzma (id 2) | Supported, checked against `unsquashfs` only: the kernel has no lzma | 0.2.0 (#42) | | `oracle_compat.rs` |
| A short gzip block | Refused, not served as file content | 0.1.3 | | |
| Basic and extended inodes: directories, files, symlinks | Supported | 0.1.1 | | `roundtrip.rs`, `stress.rs` |
| Device nodes, FIFOs and sockets, with major and minor numbers | Supported | 0.1.1; numbers 0.2.0 | | `device_oracle.rs` |
| File data: full blocks, sparse blocks, tail fragments | Supported | 0.1.1 | | `large_files.rs`, `oracle_compat.rs` |
| Id, fragment, xattr and export lookup tables | Supported | 0.1.1 | | `oracle_compat.rs`, `export_oracle.rs` |
| Resolving an inode number through the export table | Supported | 0.2.0 | | `export_oracle.rs` |
| Extended attributes, shared sets and out-of-line values | Supported | 0.2.0 | | `xattr_oracle.rs` |
| Symlinks | Supported | 0.1.1; readlink contract 0.2.0 | | `readlink_oracle.rs` |
| 256-byte file names | Supported | 0.2.0 | | `name_length_oracle.rs` |
| Raw-block and decompressed-metadata caches | Supported | 0.2.0 (#47) | | `data_cache.rs`, `meta_cache.rs`, `read_path_cost.rs` |
| Many files in one directory, deep trees | Supported | 0.1.1 | | `stress.rs` |
| Fuzzed decoders | Supported | 0.2.0 | | `fuzz_decoders.rs` |
| Writing, creating or formatting an image | Not supported: read-only by format | | | |

## Interfaces

| Feature | State | Since | Tracking | Checked by |
|---|---|---|---|---|
| C ABI: mount (path, callbacks, fs_core device), volume info, stat, stat by inode, directory iterator, read, readlink, xattrs | Supported | 0.1.1; xattrs and stat by inode 0.2.0 | | `capi_basic.rs`, `capi_read.rs`, `export_oracle.rs`, `xattr_oracle.rs` |
| C header and Rust structs agree | Supported | 0.2.0 | | `c_header_layout.rs` |
| C ABI readlink: the target's length, `ERANGE` when the buffer is too small | Supported | 0.2.0 | | `readlink_oracle.rs` |
| C ABI paths as bytes, not UTF-8 | Supported | 0.3.0 | | `capi_read.rs` |
| `fs.squashfs` `ls`, `read`, `get`/`info` (`--features cli`) | Supported | 0.3.0 | | `cli.rs`, `cli_fs_oracle.rs`, `cli_fs_kernel.rs` |
| `fs.squashfs` `write`, `mkdir`, `set`, `resize` | Refused: "SquashFS is read-only" (exit 3) | 0.3.0 | | `tests/cli/test-fs.sh` |
| `rust-fs-squashfs doctor`, man pages, shell completions | Supported | 0.3.0 | | `cli_dispatch.rs`, `cli_docs.rs` |
