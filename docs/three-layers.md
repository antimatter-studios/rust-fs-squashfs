# The three API layers

Every rust-fs driver exposes the same three layers, so a consumer talks to any
filesystem the same way:

- **VolumeAPI** — open, the superblock, compression, and the health of the
  volume as a whole.
- **PrimitiveAPI** — read-only access to the on-disk structures, tolerant of
  damage, with the salvage verbs on top.
- **FileAPI** — browse, read and copy on a healthy volume.

The shared traits are being moved into rust-fs-core (rust-fs-core issue #289's
work). The salvage verbs and their evidence types come from the
`SalvageAdapter` convention that rust-fs-exfat defines in
`src/salvage_adapter.rs` and `docs/convention.md` section 4. Until the core
traits are released, this crate builds each layer as its own SquashFS API, so
that adopting the traits later is a thin wrapper and not a second decoder.

This page is the survey: what exists today, which layer it belongs to, what is
missing, and the order the missing parts land in.

## SquashFS is read-only, so salvage means reading damage

A SquashFS image cannot be modified in place. There is no allocator, no free
space and no deletion: every inode in the inode table is live, and nothing is
ever unlinked and left behind. So:

- **No undelete.** The deletion-specific salvage features (`DeletedCandidates`,
  `DeletedExtents` in the convention) are reported as unsupported, never as an
  empty result.
- **Salvage is reading a damaged image.** The useful work is getting every
  file out of an image whose superblock, tables, metadata blocks or data
  blocks are partly unreadable, and saying exactly which parts were lost.
- **Read-only by type.** Every layer reads through `fs_core::BlockRead`. No
  API in this crate takes a writable device, and none ever will. This is a
  property of the types, not a runtime flag.

Compression is the format's integrity check. A metadata or data block whose
stream does not decode, or decodes to the wrong length, is damaged. A block
that decodes cleanly to exactly the length the geometry requires is very
likely intact, but SquashFS has no checksum, so "decoded" is the strongest
claim available. Salvage reports it as such and never as "verified".

## What exists today

All of it is the strict reader: one error anywhere fails the whole call.

| Module | Public items | Layer |
|---|---|---|
| `superblock` | `Superblock::parse`, `validate_against_device`, `read`, table-end helpers | Volume (open), Primitive (decode) |
| `decompress` | `Compressor`, `decompress` | Primitive |
| `metablock` | `read_block`, `MetaCursor`, `MetadataRef`, `MetaCache` | Primitive |
| `table` | `read_id_table`, `read_fragment_table`, `read_export_table`, `ExportTable`, data size-word helpers | Primitive |
| `xattr` | `read_id_table`, `read_set`, `XattrEntry` | Primitive |
| `inode` | `Inode::read`, `FileType`, type constants | Primitive (decode), File (metadata) |
| `dir` | `parse_listing`, `DirEntry` | Primitive (decode), File (listing) |
| `fs` | `Filesystem`: `open`, `lookup_path`, `read_dir`, `read_file`, `read_symlink_target`, xattrs, `read_inode_by_number` | Volume (open), File |
| `capi` | `fs_squashfs_*` | C ABI over the File layer |

Where the strict reader stops on damage, measured from the code:

- `Superblock::parse` refuses a bad magic, a version other than 4.0, or a
  block size that disagrees with `block_log`. Nothing is returned, so a
  consumer cannot see the other 90 bytes.
- `Filesystem::open` refuses the whole image if the id, fragment, xattr id or
  export table fails to read, even though most files need only some of them.
- `metablock::read_block` returns one error for a bad header, a short read or
  a failed decode, and a `MetaCursor` stops at the first one. One bad block
  in the inode table loses every inode after it on the same cursor, and the
  inode table is only reachable through references from directories.
- `dir::parse_listing` stops at the first malformed header or entry and
  returns nothing, losing the good entries before it.
- `read_file` fails a whole read when any one block in the range is bad.
  There is no way to get the good blocks of a damaged file.

## VolumeAPI

| Operation | Today | Target |
|---|---|---|
| Open | `Filesystem::open` | Unchanged for healthy images. A volume handle that opens on the superblock alone, so raw access works when the strict open fails. |
| Superblock | `Filesystem::sb` (strict parse) | The raw superblock with a finding per field the strict parse would refuse. |
| Compression | `Filesystem::compressor`, `Compressor::name` | Unchanged, plus the compressor options block when the `COMPRESSOR_OPTIONS` flag is set. |
| Geometry | superblock fields | Block size, `bytes_used`, device size, and each table's start and bound. |
| Health | none | A summary over the primitive findings: which tables read, how many metadata and data blocks failed, and whether the strict open would succeed. |
| Raw reads | none | Bounded reads of source bytes by offset, which work without any valid structure. |

## PrimitiveAPI

Primitives return what the bytes say and report damage. They never fail
because a structure is damaged; they fail only when they cannot do the work
(an unreadable range, an offset no device has), and they say so. The strict
reader rejects on top of the same primitives.

| Structure | Primitive | Damage it reports |
|---|---|---|
| Superblock | raw fields, each with a validity flag | bad magic, unsupported version, `block_log`/`block_size` mismatch, unknown compressor, table starts outside `[96, bytes_used)` or out of order, `bytes_used` past the device |
| Metadata block | one block at an offset: header, compressed flag, on-disk extent, decoded bytes or the reason they are missing | header size 0 or over 8 KiB, short read, decode failure, a non-final block that decodes to less than 8 KiB |
| Metadata table (inode, directory) | every block from the table start to its bound, each reported on its own; a bad header is followed by a resynchronising scan for the next block that decodes | each bad block and the gap it leaves, as source byte ranges |
| Indirect tables (id, fragment, export, xattr id) | the pointer array and each pointed-to block read separately; entries come back as present or missing, never all-or-nothing | pointer outside the image, block that fails, table shorter than its count |
| Inode | decode one inode from a position in the decoded inode stream; the fields read before damage are kept | unknown type, a record that runs past the decoded bytes, block count that disagrees with the file size |
| Directory listing | decode a listing, keeping every entry before a fault and naming the fault's offset | truncated header or entry, count over 256, bad name |
| Data block | the on-disk extent, the compressed flag, sparse or not, and a decode with its expected length | decode failure, wrong decoded length, extent past `bytes_used` |
| Fragment | the fragment block's extent and this file's tail range in it | fragment index past the table, tail past the decoded block |
| Xattrs | id table entry and the key/value pairs, each read on its own | unknown prefix, out-of-line value reference outside the table |

### Salvage verbs

The six convention verbs, in SquashFS terms:

| Verb | SquashFS meaning |
|---|---|
| Find candidates | Every inode that can be decoded from the inode table, found three ways and cross-checked: a sequential walk of the decoded table, the export table (inode number to reference) when present, and references from directory entries. Each candidate carries its inode reference, inode number, type and where it was found. All are `Live`; none is `Deleted`. |
| Walk extents | A regular file's data blocks as source byte extents, in order, with sparse blocks as holes and the fragment tail as a final extent. The walk ends `Broken` at a block that fails to decode and `OutOfRange` at one past `bytes_used`. |
| Rebuild the tree | Parent claims from two independent sources: directory entries (parent to child) and the parent inode number every directory inode stores (child to parent). An inode with no claim is an orphan; disagreeing claims are a conflict. |
| Report damage | Every finding above, as damage with a byte range, the structure it was in and a severity. |
| Read by extent | Source bytes, as stored. |
| Materialize from a plan | A file from its decodable blocks. |

Two points need the core traits to account for SquashFS, and are raised with
rust-fs-core #289:

1. **Extents are compressed.** A file extent names bytes as stored, and the
   bytes as stored are a compressed stream, not file content. A plan step
   that copies a source extent is not enough to rebuild a file; materializing
   needs a step that decodes a block with the image's compressor and checks
   its length. Until the core plan types carry one, SquashFS materializes
   through its own block step.
2. **There are no clusters.** SquashFS has no linear allocation unit, so
   `ClusterSteps` is reported as unsupported.

A block that cannot be decoded is **missing**, never zeros. Only a block whose
size word is zero is sparse and reads as zeros. A plan reports both kinds
apart, so a recovered file never contains invented content.

## FileAPI

The existing `Filesystem` is the FileAPI and stays as it is for healthy
images: `lookup_path`, `read_dir`, `read_file`, `read_symlink_target`,
`list_xattrs` and `get_xattr`, `read_inode_by_number`, and the C ABI over
them. It presents only what decodes cleanly and refuses the rest. It never
returns a salvage candidate as an ordinary file.

When the core traits are released, `Filesystem` implements the shared FileAPI
trait, and the current inherent methods stay for one release as deprecated
wrappers over it.

## Testing against something that is not us

Every primitive and every verb is checked against images made by `mksquashfs`
and read by the Linux kernel's SquashFS driver and by `unsquashfs`, in the
fs-linux-test-harness guest, as the rest of the suite is.

- **Healthy images.** The verbs on an intact image must find every inode the
  kernel lists, with the same tree, and materialize every file to the bytes
  the kernel reads, for each compressor.
- **Damaged images.** Each is an intact image with one structure corrupted:
  a byte in one inode-table block, one directory-table block, one data block,
  the fragment block, the id table, the export table, a superblock field, and
  an image truncated part-way through. For each, the files outside the damage
  must come back byte-identical to what the kernel reads from the intact
  image; the files inside it must be reported missing or partial at the right
  byte range; and the strict open or read must still refuse it.

## Order of work

Small pull requests, each on top of the last:

1. rust-fs-core pinned by version, with a local `[patch]` for a checkout.
2. This page.
3. The raw superblock and the volume handle that opens without a strict
   parse.
4. Metadata blocks and tables read one block at a time, with per-block damage
   and resynchronisation.
5. Indirect tables read entry block by entry block.
6. Inode and directory decoding that keeps what precedes a fault.
7. Data blocks and fragments, with sparse and missing kept apart.
8. The salvage verbs over those primitives.
9. Oracle tests over damaged images.

Then, once rust-fs-core releases the shared traits, each layer implements its
trait, and the current API stays one release as a deprecated wrapper.
