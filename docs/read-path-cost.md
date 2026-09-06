# What a read costs

Measured by `tests/read_path_cost.rs`, which counts **calls to the
device** rather than wall time. Wall time on a laptop with a warm page
cache says more about the laptop than the driver; call counts are
deterministic — the same image walked the same way makes the same calls
every time — so they can be compared across months and asserted on.

Wall time is printed beside them because it is what a user feels. Here
it turns out to be the more interesting of the two.

Fixture: built by the test with `mksquashfs -comp gzip`, six wide and
three deep, 216 files of incompressible pattern data. 258 paths.

## 2026-09-06 — before and after a block cache

| shape | reads | bytes | wall |
|---|---:|---:|---:|
| **uncached** | | | |
| walk — list every directory (258 items) | 3904 | 2.69 MB | 20421 µs |
| stat — resolve 216 files by path | 3024 | 2.13 MB | 16310 µs |
| read — read 216 files | 3240 | 4.21 MB | 27145 µs |
| **32-block cache** | | | |
| walk | **0** | 0 | 18509 µs |
| stat | **0** | 0 | 14665 µs |
| read | **0** | 0 | 25603 µs |

## The reads go to zero and the time barely moves

That is the finding, and it is worth more than the reads column.

Every call to the device disappears — this fixture is small enough that
the cache holds it whole — and the walk still takes 18.5 ms instead of
20.4 ms. Nine percent. Reading 216 files with **zero** device reads
still costs 25.6 ms.

So the read path here was never I/O-bound. It is bound by
**decompression**, and the block cache does not touch that: SquashFS
keeps its metadata in 8 KiB compressed blocks, and every inode read and
every directory listing decompresses the block it lives in, from
scratch, each time. Resolving 216 paths decompresses the root's
metadata block 216 times. The bytes now come from memory rather than
the device, and are then put through gzip again regardless.

`am-fs-erofs` has the shape of the answer already: `pcluster_cache`
holds **decompressed** pclusters, keyed by position, so a hit skips the
codec as well as the read. SquashFS wants the same thing for its
metadata blocks, and that is where the next reduction is.

## Why the cache is still worth having

It removes real work — 2.7 MB of reads on a walk, 4.2 MB on a read pass
— and on a spinning disk, a network volume, or an image behind FSKit's
block-device resource, those calls cost far more than they do against a
warm page cache on a laptop. The 9% here is the *floor*, measured in
the most favourable conditions for the uncached case.

It also has to exist before a decompressed-block cache is worth
building, since that cache's misses land on this one.

## Sizing

`DEFAULT_CACHE_BLOCKS` is 32, and the unit is the **archive's** block —
128 KiB by default, up to 1 MiB. So 32 is 4 MiB at the usual size and
32 MiB at the largest, which is generous for what this holds: metadata.
File data spans more than half the cache and takes the bypass, so it
never evicts what a walk depends on.

The fixture is smaller than the cache, which is why every figure reaches
exactly zero. A real image will not, and the line to watch then is
`walk`, which touches the most distinct blocks.

## How to take the measurement again

```sh
cargo test --release --test read_path_cost -- --nocapture
```

Needs `mksquashfs` on `PATH` to build the fixture, and skips without it.
