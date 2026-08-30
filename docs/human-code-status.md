# Human-code findings — status

Tracks every **High** and **Medium** finding from
[`human-code-report-2026-08-28.md`](human-code-report-2026-08-28.md) and what
was done about it. The report records everything as open because it predates
the work. This file is the current position. Updated 2026-08-29.

**19 findings** — 2 High, 9 Medium, 8 Low. This covers the 11 High and Medium.

| | High | Medium |
|---|---|---|
| Fixed | 1 | 4 |
| Left for a human decision | 1 | 3 |
| Fixable, not yet done | 0 | 2 |

---

## High

### H1 — gzip could silently return a truncated block — **fixed earlier**

Fixed by [#18](https://github.com/antimatter-studios/rust-fs-squashfs/pull/18).

The one finding here that was a live bug rather than a readability problem, and
worth restating because the shape recurs: a truncated zlib stream is **not** an
`Err`. The decoder consumes what it was given, emits what it can, and reports
`Ok(Status::Ok)` — "I would like more input". Only `StreamEnd` means the whole
stream decoded. Discarding the status returned partial output as a successful
short block, which every caller treated as real file content.

`decompress_zlib` now requires `Status::StreamEnd`.

### H2 — the six-way codec dispatch has five different shapes — **needs your decision**

Real inconsistency, and the report describes it accurately. Normalising it means
restructuring the one function that stands between an untrusted archive and
every decompressor in the crate — where the interesting failures are the ones
H1 was about, and where "looks equivalent" is not the same as "is equivalent".

A worthwhile change; it wants to be its own, with the codec oracle tests as the
contract, rather than folded into a readability pass.

---

## Medium

### M1 — `Inode::read` is 107 lines decoding fourteen inode types — **needs your decision**

A god function by length, and it is also a flat dispatch over a format's own
enumeration. Splitting it is defensible and so is leaving it. Not a defect.

### M2 — the metadata-reference bit layout is open-coded in three places — **fixable, not yet done**

Genuine, three sites, and the fix is a small helper. Deferred only because it is
the same kind of change as M4 and they are better done together, with the
metablock tests as the contract.

### M3 — `is_supported()` is a hardcoded `true` guarding a dead branch — **fixed**

Confirmed unreachable: `sb.compressor()?` has already rejected an id this build
does not know, so by the time the guard runs every codec answers `true`.

**Documented rather than deleted, at both ends.** The guard is the seam for a
codec that is *recognised* but not decodable in a given build — a feature-gated
one — and deleting it would mean rediscovering where that check belongs. Both
`is_supported` and its caller now say so, including that unknown ids never reach
there.

### M4 — the same six-line FFI preamble is written out four times — **fixable, not yet done**

Real duplication. Same reasoning as M2: worth a change that is only about the
FFI layer, where a mistake reports the wrong thing to a C caller.

### M5 — `COMPRESSED_BIT` set means *un*compressed, in two modules — **fixed**

Both constants were named for the opposite of what they mean, so every use read
backwards: `raw & COMPRESSED_BIT == 0` is the test for "this block **is**
compressed".

- `metablock.rs`'s constant is private, so it is now `UNCOMPRESSED_BIT`.
- `table.rs`'s `DATA_COMPRESSED_BIT` is `pub` and published, so it stays and
  gains `DATA_UNCOMPRESSED_BIT` — the same value under a name that reads
  correctly. Internal uses moved to the new one; the old is documented as the
  published spelling.

The polarity is the format's, not a choice made here; the names now say what
the bit means rather than what it is about.

### M6 — `dir.rs` has two unrelated 256s, one named and one bare — **fixed**

`SQUASHFS_NAME_LEN` is a byte length. The bare `256` three dozen lines later is
a **count** of directory entries — an entirely unrelated quantity that happens
to share a value. It is now `MAX_ENTRIES_PER_HEADER`, with a comment saying so,
because two bare `256`s in one file is how a reader concludes one bound
explains the other.

### M7 — `cmd_tree` re-resolves the inode it just read — **fixable, not yet done**

In the binary, not the library. Real waste, small fix, no correctness impact.

### M8 — a third parallel match over `FileType` in the binary — **needs your decision**

The third copy of a mapping that already exists twice. Consolidating means
deciding where the canonical one lives and whether the binary should depend on
it, which is a structural choice.

### M9 — fixed C-buffer sizes written as bare literal pairs — **fixable, not yet done**

Same family as M2/M4 — the FFI layer. Grouped with them.

---

## Verification

`cargo test` — 32 unit, 25 + 26 integration tests pass, unchanged in number.
`chore lint` clean. None of this changes behaviour.
