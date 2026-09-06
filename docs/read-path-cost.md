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

## 2026-09-07 — and after caching the decompressed metadata

| shape | reads | bytes | wall |
|---|---:|---:|---:|
| **uncached** | | | |
| walk — list every directory (258 items) | 3904 | 2.69 MB | 20135 µs |
| stat — resolve 216 files by path | 3024 | 2.13 MB | 15715 µs |
| read — read 216 files | 3240 | 4.21 MB | 27392 µs |
| **block cache only** | | | |
| walk | **0** | 0 | 18538 µs |
| stat | **0** | 0 | 14631 µs |
| read | **0** | 0 | 26104 µs |
| **block cache + decompressed-metadata cache** | | | |
| walk | **0** | 0 | **447 µs** |
| stat | **0** | 0 | **310 µs** |
| read | **0** | 0 | **11118 µs** |

Metadata cache at the end of the third pass: 3 blocks held of 256
available, 4973 hits, 3 misses.

### The walk is forty times faster and the reads did not change

The reads column was already zero and stays zero, which is the point:
this cache sits *above* the codec, so a hit spares the decompression
rather than the read. Nothing about what reaches the device changed.

What changed is everything else. Listing every directory went from
18.5 ms to 0.45 ms, a factor of 41. Resolving 216 paths went from
14.6 ms to 0.31 ms, a factor of 47. The previous section said the read
path was bound by decompression rather than by I/O; these are the same
numbers with the decompression taken out, and they say it was bound by
decompression almost entirely.

Three metadata blocks. That is the whole working set of this fixture —
258 paths, 216 files — and it was being decompressed 4976 times.

### Why `read` only halved

Reading the files still costs 11.1 ms, and it should. That pass reads
216 files of 8 KiB of deliberately incompressible pattern data, and
their *data* blocks go through the codec on every read. This cache does
not hold them and should not: data is unbounded where metadata is not,
and a cache holding a file's blocks would evict the directory blocks
every lookup depends on. What fell out of the read pass is its metadata
half — the inode and directory reads each file needed before its bytes
could be found — which is the same ~15 ms the other two passes lost.

Caching decompressed *data* is a separate question with a separate
answer, and the number that would justify it is not this one.

### Sizing

`DEFAULT_META_CACHE_BLOCKS` is 256, and the unit is the metadata block,
which decompresses to at most 8 KiB — so the ceiling is 2 MiB. This
fixture never went past three entries. An image with tens of thousands
of files has a few megabytes of metadata in total and returns to a much
smaller subset of it, so 256 is meant to hold the working set of a walk
rather than the whole table; the line to watch on a larger image is the
miss count printed beside the figures.

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
