# Human-code review — 2026-08-28

> **This is analysis only. No code was changed.** Phases 0, 1 and 3 of the
> `human-code` skill were run; Phase 2 (the dev-loop that would apply fixes) was
> deliberately not entered. The working tree is unmodified apart from this file.

**Scope:** the whole crate — `src/lib.rs`, `src/error.rs`, `src/superblock.rs`,
`src/decompress.rs`, `src/metablock.rs`, `src/table.rs`, `src/dir.rs`,
`src/inode.rs`, `src/fs.rs`, `src/capi.rs`, `src/bin/lssquashfs.rs`
(2,495 lines across 11 files, of which ~2,060 are production code; in-file `#[cfg(test)]`
modules were read for coverage but not triaged as targets).

**Counts:** 19 findings — **2 high**, **9 medium**, **8 low**. 0 fixed, 19 open.

**Baseline (unchanged, captured for reference):** 109 tests passing, 0 failing,
17 ignored (all gated on `squashfs-tools` being installed). `cargo clippy
--all-targets -- -D warnings` clean. The 5 ignored `oracle_compat` tests were run
manually for this review and all pass on this host.

---

## A note on the previous review

`docs/code-quality-review-2026-08-25.md` (three days ago) recorded 0 high, 2 medium,
1 low and stated "no duplication, no unnamed offsets". This review disagrees on both
counts, because it looked for different things: that review measured *shape* (function
length, file ratios, nesting depth), this one traces *meaning* across module
boundaries. Its M1 (`Inode::read` is 108 lines) is confirmed and carried forward here
as M1. Its "no duplication" claim holds for literal repeated blocks but not for
repeated *decisions* — the same size-word convention decoded in two modules, the same
FFI preamble written out four times, the same metadata-reference bit layout open-coded
in three places. Those are H2, M2 and M4 below.

---

## Findings

### High

#### H1 — gzip is the one codec of six that can silently return a truncated block

**`src/decompress.rs:107`–`119`, consequence at `src/fs.rs:141`–`153`**

```rust
let mut dec = Decompress::new(true);
dec.decompress(input, &mut out, FlushDecompress::Finish)
    .map_err(|_| Error::BadMetadata("zlib decompression failed"))?;
let produced = dec.total_out() as usize;
```

`Decompress::decompress` returns `Result<Status, DecompressError>`. The `Err` arm is
handled; the `Ok(Status)` payload is dropped. `Status` is the three-way distinction
between `StreamEnd` (the block finished), `Ok` (the decoder wants more input) and
`BufError`. On truncated or clipped input miniz_oxide returns `Ok(Status::Ok)`, not an
error, so this function returns a **short block with no error at all**.

The other five codecs do not behave this way: `xz`, `lzma` and `zstd` surface a decode
error on truncation, and `lz4_flex` rejects a malformed block. Only gzip degrades
quietly — and gzip is the default compressor, the one the committed fixture uses, and
the only one `cargo test` exercises without `--ignored`.

Downstream this is worse than a wrong error message. In `fs.rs:141`:

```rust
let block = self.read_logical_block(inode, block_idx, &block_offsets)?;
if block_off >= block.len() {
    break;                       // <-- short block exits the loop
}
```

a short block breaks the copy loop, and `read_file` returns `Ok(written)` with
`written < to_read`. A corrupt gzip data block therefore reaches the caller as a
*successful short read*, indistinguishable from a legitimate read crossing EOF.

**Category:** dropped-result / defensive handling that doesn't defend.
**Test coverage:** none. `malformed_gzip_errors` (`decompress.rs:237`) feeds
`[0xFF; 4]`, which fails the zlib *header* check and does produce an `Err` — it does
not cover a valid header with a truncated body. No test asserts that a short block is
an error rather than a short read.

#### H2 — the six-way codec dispatch has five different shapes

**`src/decompress.rs:88`–`167`**

The dispatch the crate is organised around — the thing a reader opens this file to
understand — does not present the six compressors as six of the same thing:

```rust
match comp {
    Compressor::Gzip => decompress_zlib(input, max_out),
    Compressor::Xz   => decompress_xz(input, max_out),
    Compressor::Lzma => decompress_lzma_alone(input, max_out),
    Compressor::Lz4  => decompress_lz4(input, max_out),
    Compressor::Zstd => decompress_zstd(input, max_out),
    Compressor::Lzo => {
        // ...six lines of error mapping inlined in the arm...
        lzo1x::decompress(input, max_out).map_err(|e| { /* ... */ })
    }
}
```

Five arms delegate to a `decompress_*` helper; the sixth inlines its body, so the
`match` no longer reads as a table. Below it, the helpers diverge again on details
that are not codec-specific:

| | over-max guard | scratch allocation | error string |
|---|---|---|---|
| `decompress_zlib` | `produced > max_out` (**dead** — output buffer is `max_out`) | `vec![0u8; max_out]` | "zlib decompression failed" |
| `decompress_xz` | `out.len() > max_out` | `with_capacity(max_out.min(1 << 20))` | "xz decompression failed" |
| `decompress_lzma_alone` | `out.len() > max_out` | `with_capacity(max_out.min(1 << 20))` | "lzma decompression failed" |
| `decompress_lz4` | none (bounded by buffer) | `vec![0u8; max_out]` | "lz4 decompression failed" |
| `decompress_zstd` | `out.len() > max_out` | `with_capacity(max_out.min(1 << 20))` | "zstd decompression failed" |
| lzo (inline) | delegated to the crate | — | "malformed LZO1X stream" |

Three concrete smells stack here:

1. `return Err(Error::BadMetadata("decompressed block exceeds max size"))` is written
   out four times, character-identical, in four functions.
2. `max_out.min(1 << 20)` appears three times (`:122`, `:137`, `:160`). The `1 << 20`
   is unnamed and unexplained — it is a 1 MiB pre-allocation ceiling, which happens to
   equal the largest legal SquashFS block, but nothing says so and nothing ties it to
   `MAX_BLOCK_LOG = 20` in `superblock.rs:22`, which is the same number.
3. Two of the guards are unreachable: `decompress_zlib` writes into a buffer that is
   exactly `max_out` long, so `produced > max_out` cannot hold.

The shared shape is real and simple — *decode into a bounded buffer, reject anything
over the bound, name the codec in the error* — but it is currently re-derived per
branch. A reader auditing "can any codec exceed `max_out`?" has to read six functions
and reason about each buffer separately, and gets a different answer for gzip (H1)
that nothing in the file's shape hints at.

**Category:** duplication + magic number + speculative code.
**Test coverage:** partial and mostly opt-in. `decompress.rs` unit tests round-trip
**gzip and lz4 only**. The committed fixture `test-disks/squashfs-basic.sqfs` is gzip
(compression id 1). Every xz, zstd, lzo and lzma decode path is covered *only* by
`tests/oracle_compat.rs`, which is `#[ignore]`-gated on `mksquashfs`. **A default
`cargo test` run would not catch a refactor that broke four of the six codecs.** Any
work here must be gated on `cargo test -- --ignored` (verified passing on this host).

### Medium

#### M1 — `Inode::read` is 107 lines decoding fourteen inode types in one function

**`src/inode.rs:162`–`269`**

Carried forward from the 2026-08-25 review, unchanged. A 16-byte common header, then a
14-arm `match` where each arm transcribes a different on-disk struct tail. Reader cost
is proportional to types they don't care about. The arms group naturally into four
readers — dir, file, symlink, device/fifo/socket — each of which already has a
basic/extended pair sharing most of its body.

Two smaller things live inside it: the "construct with defaults then mutate" pattern
means `inode.file_size` is assigned in seven different arms with three different
meanings (byte length, target length, listing length + 3), and the fallback arm nests a
second `match` purely to choose between two error strings (`:261`–`:266`).

**Coverage:** good — `tests/stress.rs`, `tests/capi_basic.rs` and the oracle suite
exercise dirs, files, symlinks and both basic and extended forms.

#### M2 — the metadata-reference bit layout is open-coded in three places

**`src/inode.rs:163`–`164`, `src/dir.rs:81`, documented again at `src/superblock.rs:40` and `src/metablock.rs:74`**

The `u64` "metadata reference" (high 48 bits = block offset relative to a table start,
low 16 bits = offset within the decompressed block) is the crate's central encoding.
It is decoded here:

```rust
let start_abs = sb.inode_table_start + (inode_ref >> 16);
let in_block  = (inode_ref & 0xFFFF) as u16;
```

constructed here:

```rust
let inode_ref = ((start as u64) << 16) | (offset as u64);
```

and described in prose in two more doc comments — including `MetaCursor::new`'s, which
explains the layout to a caller it cannot enforce it on. `>> 16`, `& 0xFFFF` and
`<< 16` are bare in the code; three modules each know the layout independently. A
`MetaRef(u64)` newtype with `block_offset()` / `in_block()` accessors, or even a pair
of free functions beside `METADATA_SIZE`, would make the encoding one thing with one
definition. The "+ table_start" step is also repeated with two different table starts
(`inode_table_start` at `inode.rs:163`, `directory_table_start` at `fs.rs:84`).

**Coverage:** good — `dir.rs` unit tests assert the constructed refs, and every
integration test resolves paths through the decode side.

#### M3 — `Compressor::is_supported()` is a hardcoded `true` guarding a dead branch

**`src/decompress.rs:76`–`81`, gate at `src/fs.rs:38`–`40`**

```rust
pub fn is_supported(&self) -> bool { true }
```

```rust
let comp = sb.compressor()?;
if !comp.is_supported() {
    return Err(Error::UnsupportedCompression(sb.compression_id));
}
```

The branch cannot be taken. Rejection of unknown compressors already happened one line
earlier inside `Compressor::from_id`, which is the only constructor. What is left is a
capability gate that looks like a support matrix and is not one — the shape strongly
suggests a reader could add a codec by flipping a flag here, and a maintainer *removing*
a codec would plausibly edit this function and see no behaviour change at all.

The doc comment compounds it: it says legacy lzma "is best-effort but still reported as
supported so the read path attempts it", describing a decision the function does not
implement, and `lib.rs:14` repeats the "best-effort" framing.

**Category:** speculative code for a scenario that can't happen.
**Coverage:** `all_known_codecs_supported` (`decompress.rs:197`) asserts the constant
against itself — it passes for any implementation that returns `true` and would keep
passing if the whole mechanism were deleted.

#### M4 — the same six-line FFI preamble is written out four times

**`src/capi.rs:374`, `:405`, `:481`, `:529`**

`fs_squashfs_stat`, `_dir_open`, `_read_file` and `_readlink` each open with:

```rust
clear_last_error();
if fs.is_null() || path.is_null() || ...  { set_err_msg("null ...", errno::EINVAL); return FAIL; }
let fs   = unsafe { &(*fs).fs };
let path = unsafe { cstr_to_str(path) };
let inode = match fs.lookup_path(path) {
    Ok(i) => i,
    Err(e) => { set_err_from(&e, &format!("{op} {path}")); return FAIL; }
};
```

Four instances, well past the three-instance extraction threshold, and each one repeats
an `unsafe` deref. The variation between them is exactly two things: the failure
sentinel (`-1`, `null_mut()`, `null()`) and the operation name in the error string. A
`with_fs_and_path(fs, path, fail, "stat", |fs, inode| ...)` helper would leave each
entry point as its distinctive part only, and would concentrate the four raw pointer
dereferences into one audited place — which matters more here than in ordinary code,
because every one of these is an `unsafe` block at a C boundary.

The seven `ffi_guard(fail, AssertUnwindSafe(|| { clear_last_error(); ...null checks... }))`
wrappers are a milder version of the same repetition.

**Coverage:** excellent — `capi_basic.rs` and `capi_read.rs` assert the null-argument,
missing-path and wrong-type outcomes for every one of these four functions (51 tests).
This is the best-covered refactor target in the crate.

#### M5 — `COMPRESSED_BIT` set means *un*compressed, in two modules

**`src/metablock.rs:20`–`21` and `:35`; `src/table.rs:23`–`24` and `:32`**

```rust
const COMPRESSED_BIT: u16 = 0x8000;
let is_compressed = raw & COMPRESSED_BIT == 0;          // metablock.rs:35
```
```rust
pub const DATA_COMPRESSED_BIT: u32 = 1 << 24;
pub fn data_is_compressed(raw: u32) -> bool { raw & DATA_COMPRESSED_BIT == 0 }  // table.rs:32
```

Both constants are named for the opposite of what they mean: the bit being *set* marks
the payload as stored uncompressed. `raw & COMPRESSED_BIT == 0` therefore reads as
"the compressed bit is clear, so it is compressed", which is a double-take every time.

In mitigation, this mirrors `squashfs_fs.h`, where the constant is genuinely called
`SQUASHFS_COMPRESSED_BIT` and the accompanying macro does the same inversion — and the
module headers state that field names mirror the C struct. That is a real argument for
leaving the constant alone. It is not an argument for leaving the *derived predicate*
un-named: `metablock.rs` gets it right (`is_compressed` is a named local, and the
module doc explains the inversion at `:7`), `table.rs` exposes `data_is_compressed`
which is also fine. The residual cost is that two modules independently define the same
"bit N set means uncompressed" convention for two different bit positions, with no
cross-reference between them, and `table.rs:20`'s doc comment is the only place that
notes the convention is shared with file `block_sizes`.

**Coverage:** `data_size_word_decoding` (`table.rs:118`) covers the data variant
including the sparse-block case; `read_uncompressed_block` (`metablock.rs:215`) covers
the metadata variant.

#### M6 — `dir.rs` has two unrelated 256s, one named and one bare

**`src/dir.rs:22`, `:45`, `:57`, `:62`**

```rust
const SQUASHFS_NAME_LEN: usize = 256;      // max name length
...
if p + 12 > buf.len() { ... }              // 12 = directory header size
if entries > 256 { ... }                   // 256 = max entries per header
if p + 8  > buf.len() { ... }              // 8  = directory entry size
```

Three bare structural sizes (`12`, `8`, and the entries cap), and the entries cap is
`256` — the same literal as the *named* `SQUASHFS_NAME_LEN` four lines above it, for a
completely unrelated quantity. A reader who greps `256` in this file gets two different
concepts. `DIR_HEADER_SIZE = 12`, `DIR_ENTRY_SIZE = 8` and
`MAX_ENTRIES_PER_HEADER = 256` alongside the existing constant would separate them, and
the `p + 12` / `p + 8` bounds checks would then say what they are checking.

**Coverage:** good — `rejects_truncated_header`, `rejects_truncated_name`,
`parses_single_header` cover all three bounds.

#### M7 — `cmd_tree` throws away the inode it just read and re-resolves it from the root

**`src/bin/lssquashfs.rs:90`–`115`**

```rust
let child = fs.read_inode(e.inode_ref)...;   // we have the child inode here
if child.is_dir() {
    let sub = if path == "/" { format!("/{name}") } else { format!("{path}/{name}") };
    cmd_tree(fs, &sub, depth + 1);           // ...and this looks it up again from /
}
```

The recursive call takes a path string, so its first act (`lookup(fs, path)` at `:91`)
walks from the root doing a linear scan of every directory on the way down — for an
inode the caller is already holding. Cost is O(depth²) directory scans over a tree, but
the readability problem is that the code builds a string to re-find something it has in
hand, and the reason (there isn't one — `cmd_tree` simply takes the wrong parameter) is
invisible. Taking `&Inode` plus a display path, with a thin `cmd_tree` entry point that
resolves once, removes both the cost and the string juggling.

**Coverage:** `tree_shows_nested_structure` (`cli.rs:80`) covers the output shape.

#### M8 — a third parallel match over `FileType`, living in the binary

**`src/bin/lssquashfs.rs:148`–`157`, vs `src/inode.rs:68` and `:82`**

`FileType` has `to_abi()` (ABI byte) and `mode_bits()` (POSIX `S_IF*`) as sibling
methods in `inode.rs`. The binary adds a third total mapping over the same eight
variants — to the `ls` type character — as a local `match` in a print helper. Adding a
variant to `FileType` breaks two mappings loudly (non-exhaustive match) and the third
one only if you happen to build the binary. A `ls_char()` method beside its two
siblings puts all three mappings where the enum is, which is the "group by domain"
rule in the same shape as the constants: three total functions of the same type belong
together.

**Coverage:** `ls_root_lists_all_entries`, `ls_on_a_file_prints_that_one_entry`
(`cli.rs`) cover the dir/file/symlink characters; device, fifo, socket and unknown are
uncovered in all three mappings.

#### M9 — fixed C-buffer sizes written as bare literal pairs

**`src/capi.rs:135`, `:189`–`:194`, `:142`, `:355`–`:359`**

```rust
pub name: [c_char; 256],
let mut name = [0 as c_char; 256];
let copy = e.name.len().min(255);
```
```rust
pub compression_name: [c_char; 16],
let n = cname.len().min(15);
```

Four bare literals in two `size` / `size - 1` pairs, and each `256` / `16` also has to
stay in lockstep with `include/fs_squashfs.h:56` and `:63`. `dir.rs` already names its
`256` (`SQUASHFS_NAME_LEN`) for a related-but-not-identical purpose, so the crate now
holds the same number under a name in one module and bare in another. Named
`DIRENT_NAME_CAP` / `COMPRESSION_NAME_CAP` constants with the `-1` derived rather than
written would make the header correspondence checkable.

**Coverage:** `dir_open_root_lists_all_entries`, `long_and_unicode_names_gzip`
(`stress.rs:203`) and `volume_info_reports_expected_fields` cover the copies; nothing
covers a name at or beyond the 255-byte boundary (see L7).

### Low

#### L1 — two dead defensive checks

**`src/metablock.rs:48`–`50`** — `if payload.len() > METADATA_SIZE` cannot fire:
`payload` is `vec![0u8; on_disk]` and `on_disk > METADATA_SIZE` was already rejected at
`:36`.
**`src/decompress.rs:114`–`116`** — `if produced > max_out` cannot fire; the output
buffer is exactly `max_out` bytes (see H2). Both read as safety nets and are neither.
**Coverage:** unreachable, so uncoverable.

#### L2 — data-block failures are reported under metadata/inode error variants

**`src/fs.rs:215`, `:218`; error strings throughout `src/decompress.rs`**

`read_data_block` routes a *file data* block through `decompress::decompress`, whose
failures are all `Error::BadMetadata("... decompression failed")`, and reports an
oversized uncompressed data block as `Error::BadInode("uncompressed data block exceeds
block size")` — a data problem filed under the inode. Both map to `EIO` so the C ABI
behaviour is identical, but the message a user sees for a corrupt file block says
"malformed metadata block" or "malformed inode". A `BadData` variant, or threading a
context string through `decompress`, would make the message match the failure.
**Coverage:** no test asserts the *variant* for a corrupt data block.

#### L3 — superblock field offsets are bare hex, and `96` is written twice

**`src/superblock.rs:56`–`103`**

The struct-literal form pairs each offset with its field name, which carries most of
the weight — `inode_count: rd_u32(0x04)` is legible. The two that aren't are the
validation reads that happen before the literal (`rd_u16(0x16)` for `block_log`,
`rd_u16(0x1C)`/`rd_u16(0x1E)` for the version at `:68`–`:80`), where the offset stands
alone. Separately, `SUPERBLOCK_SIZE` is `96` at `:14` and the error string at `:57`
hardcodes "shorter than 96 bytes" — the two can drift.
**Coverage:** `rejects_short_buffer`, `rejects_bad_magic`, `rejects_non_v4`,
`rejects_block_size_mismatch` cover the validation paths.

#### L4 — `MetaCursor::read_exact` has the name of a std method with different semantics

**`src/metablock.rs:118`–`123`**

`std::io::Read::read_exact(&mut self, buf: &mut [u8]) -> io::Result<()>` fills a
caller-supplied buffer. This one is `read_exact(&mut self, n: usize) -> Result<Vec<u8>>`
and allocates. Every scalar accessor goes through it, so reading a 16-byte inode header
allocates six `Vec`s for 2–4 bytes each. `take(n)` or `read_bytes(n)` would not borrow
the std contract; the allocation is a separate (minor) cost.
**Coverage:** `cursor_spans_block_boundary` covers the cross-block case.

#### L5 — `read_file` rebuilds the whole block-offset table on every call

**`src/fs.rs:138`, `:158`–`:166`**

`block_offsets` is a prefix sum over `inode.block_sizes`, recomputed per `read_file`
call. The comment is accurate about what it buys *within* a call. Across calls — which
is the FSKit access pattern, many small reads against one inode — a 1 GiB file at
128 KiB blocks rebuilds an 8,192-entry `Vec` for every read. Caching it on the `Inode`,
or computing only the offsets the read touches, would fix it. Filed Low because it is
a cost, not a comprehension barrier.
**Coverage:** `read_each_block_boundary_aligned`, `committed_big_bin_offset_reads_at_every_boundary`.

#### L6 — `permissions & 0o7777` masked in two places for a field documented as already masked

**`src/capi.rs:179`, `src/bin/lssquashfs.rs:167`**

`Inode::permissions` is documented at `inode.rs:99` as "Permission bits only (no type
bits)". Two consumers mask it anyway. Either the doc is right and both masks are noise,
or the doc is wrong and every *other* consumer is missing a mask. Two instances, below
the extraction threshold; the fix is to decide which and say so once.
**Coverage:** `stat_root_is_directory`, `stat_regular_file_reports_size`.

#### L7 — a 256-byte name is silently truncated to 255 at the ABI

**`src/capi.rs:190`–`:194`**

`dir.rs` admits names up to `SQUASHFS_NAME_LEN` (256) bytes; `dir_entry_to_abi` copies
`min(255)` and NUL-terminates, and `name_len` is a `u8` which cannot express 256. A
256-byte name therefore reaches a C consumer one byte short with no error and no
signal. Behaviourally this is the header's constraint, not a crate bug — but nothing in
the code says the truncation is intentional.
**Coverage:** `long_and_unicode_names_gzip` (`stress.rs:203`) exercises long names but
not the boundary.

#### L8 — `lssquashfs` argument parsing has no per-subcommand arity

**`src/bin/lssquashfs.rs:26`–`27`**

```rust
let cmd  = args.get(2).map(String::as_str).unwrap_or("ls");
let path = args.get(3).map(String::as_str).unwrap_or("/");
```

`cat` and `readlink` require a path; both default it to `/` and then fail deeper in
with "not a regular file" / "not a symlink" rather than a usage error. `info` accepts a
path argument and ignores it. The usage string at `:22` documents `[path]` as optional
for all five subcommands, which the code makes true by accident rather than intent.
**Coverage:** `cat_directory_fails`, `no_args_exits_two_with_usage`, `unknown_command_fails`.

---

## What to fix first

The compression area is where the reading cost and the risk coincide, and it is also
the part of the crate with the weakest default test coverage — so the order below is
deliberately "make it testable, then make it uniform".

1. **H1 first, on its own, as a bug fix rather than a refactor.** Check
   `Status::StreamEnd` in `decompress_zlib` and return
   `Error::BadMetadata("zlib stream did not finish")` otherwise. Add a test that feeds
   a valid zlib header with a truncated body and asserts an `Err`, and a `read_file`
   test that asserts a corrupt data block is an error rather than a short read. This is
   a handful of lines and it closes a silent-corruption path; it should not wait behind
   any cleanup.

2. **Establish the codec test gate before touching H2.** The refactor in H2 is safe
   only if xz/zstd/lzo are actually exercised. Either make `cargo test -- --ignored`
   part of the contract for this work (it passes on this host today), or commit one
   small non-gzip fixture per codec to `test-disks/` so the default run covers all six.
   Without this, four of the six branches are being restructured blind.

3. **H2 — unify the six branches.** Give lzo a `decompress_lzo` helper so the `match`
   is six delegations; hoist the over-max guard and the codec name into one shared
   `finish(out, max_out, codec)`; name the `1 << 20` (and tie it to `MAX_BLOCK_LOG` if
   that is what it means); delete the two dead guards. The gain is that "can this codec
   exceed the bound / how does it fail" becomes one answer instead of six.

4. **M4 — the FFI preamble.** Highest-coverage, lowest-risk item in the crate (51
   tests already assert every one of its failure modes), and it collapses four `unsafe`
   dereferences into one.

5. **M3 — delete the fake support gate**, or give it a real body. Two lines, and it
   stops the file from advertising a mechanism that isn't there.

6. **M2 and M1 together.** A `MetaRef` newtype (M2) is most of the setup for splitting
   `Inode::read` (M1), since the split readers all take a decoded reference. Doing M2
   first makes M1 a smaller change.

7. **M5, M6, M8, M9 — the naming and constants pass.** Mechanical, well covered,
   independent of each other; a reasonable single batch.

8. **M7, then the Lows.** M7 is a real algorithmic improvement in a tool used for
   manual inspection. L1 (delete two dead checks) is free and can ride along with H2.

Two items to consider *not* doing: **M5**'s constant rename, because mirroring
`squashfs_fs.h` naming is a defensible deliberate choice and the derived predicates are
already named correctly — a cross-reference comment between `metablock.rs` and
`table.rs` may be the whole fix; and **L5**, which is a performance note dressed as a
readability one and should be measured before it is changed.

---

## Test results

No code was changed, so before and after are identical. Recorded as the baseline any
Phase 2 work must hold.

| | Before | After |
|---|---|---|
| Tests passing | 109 | 109 (unchanged — no code modified) |
| Tests failing | 0 | 0 |
| Tests ignored | 17 (require `squashfs-tools`) | 17 |
| `clippy --all-targets -D warnings` | clean | clean |
| Ignored suite (`oracle_compat`, run manually) | 5/5 pass | 5/5 pass |

Per-target breakdown of the 109: lib unit tests 29, `capi_basic` 25, `capi_read` 26
(+1 ignored), `cli` 17, `stress` 8 (+5 ignored), `large_files` 3 (+6 ignored),
`roundtrip` 1, `oracle_compat` 0 (+5 ignored), doc-tests 0.

**Coverage gap most relevant to this report:** the only committed fixture
(`test-disks/squashfs-basic.sqfs`) is gzip, and the only codec unit tests are gzip and
lz4. xz, zstd, lzo and lzma have zero coverage in a default `cargo test`.
