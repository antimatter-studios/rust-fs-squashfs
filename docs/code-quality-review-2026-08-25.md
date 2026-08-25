# Code quality review — 2026-08-25

**Scope:** `src/`, 2,058 production lines across 11 files (test modules excluded from
every count below).
**Findings:** 0 high, 2 medium, 1 low. No fixes applied — this is a read of the code
as it stands.

A small, healthy crate. No duplication, no unnamed offsets, no `#[allow]`
suppressions, and one function over 60 lines. The findings are minor and none of them
is urgent.

---

## M1 — `Inode::read` is 108 lines and decodes every inode type in one function

**`src/inode.rs:162`**

SquashFS has eight inode types sharing a 16-byte common header and then diverging
completely — a directory inode and a symlink inode have nothing in common past byte 16.
This function reads the header and then handles all eight tails in sequence.

The result is one function where a reader interested in one inode type reads past seven
they do not care about, and where the common header's fields are 90 lines away from the
point some of the tails use them.

**Shape of the fix.** Read the common header into a struct, then dispatch on
`inode_type` to one function per type. The dispatch already exists; it is just spelled
as a long sequence rather than as a `match` over named readers.

**Why it is medium and not high.** The function is at one abstraction level throughout
— it is all field decoding — and the `MetaCursor` abstraction keeps the reads uniform.
It is long rather than tangled.

---

## M2 — `capi.rs` is 570 lines, 28% of the crate

**`src/capi.rs`**

The C ABI is more than a quarter of a crate whose subject is reading a filesystem. As
in `rust-fs-core`, that ratio is a consequence of a small core with a foreign-function
surface, and the file itself is a flat list of entry points with a consistent shape —
the one kind of long file that stays navigable.

`fs_squashfs_read_file` takes five parameters because the ABI dictates them.

**Recommendation:** leave it. Recorded so the ratio is deliberate rather than
unnoticed.

---

## L3 — Ten lines indented 24 columns or deeper

**mostly `src/inode.rs`**

Six levels of nesting, almost all inside `Inode::read`, where the per-type tails nest
inside the type dispatch inside the error handling. This will largely resolve with M1.

---

## What is good

- **No duplication.** Zero repeated eight-line blocks.
- **No unnamed multi-digit offsets** — notable in a crate that decodes a
  variable-length metadata stream, where offsets are easy to leave bare.
- **No `#[allow(...)]` anywhere.**
- **`MetaCursor` is the right abstraction.** SquashFS stores metadata in compressed
  8 KiB blocks that entries may straddle, and the cursor makes that invisible to every
  caller: `read_u16` and `read_u32` cross a block boundary without the caller knowing.
  Almost all of the crate's potential complexity is absorbed by this one type.
- **One file per concept**, with names that say what they hold: `metablock.rs`,
  `table.rs`, `decompress.rs`, `dir.rs`, `inode.rs`, `superblock.rs`.
- **`clippy -D warnings` and `rustfmt` are clean**, and CI enforces both.

## A note on the LZO extraction

The crate's LZO1X decompressor used to live here as `src/lzo1x.rs`. It is now the
separate `am-lzo1x` crate and consumed as a dependency.

That was the right call and it has paid for itself twice over: it is a general-purpose
algorithm with no SquashFS in it, and a sibling driver has since needed exactly the
same decompressor. It is worth recording here as the precedent for the same question
elsewhere — `decompress.rs` dispatches five compressors, and if any of the others turn
out to be hand-rolled rather than delegated, they are candidates for the same
treatment.

## Suggested order

M1, which is the only finding with real reading cost attached. Nothing else needs
doing.
