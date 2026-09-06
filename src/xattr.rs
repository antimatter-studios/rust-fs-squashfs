//! Extended attributes.
//!
//! # Three levels, and why
//!
//! An inode does not point at its attributes. It carries a `u32` index
//! into a table of *attribute sets*, and several inodes routinely carry
//! the same index — a tree built by one tool tends to carry the same
//! `security.selinux` label on thousands of files, and SquashFS stores
//! that set once. So:
//!
//! ```text
//!   inode.xattr_index  ──▶  id table entry ──▶ name/value pairs
//!        (a u32)              (16 bytes)        (in metadata blocks)
//! ```
//!
//! The superblock's `xattr_id_table_start` points at a 16-byte header:
//!
//! ```text
//!    0  xattr_table_start  u64   where the name/value pairs begin
//!    8  xattr_ids          u32   how many entries the id table has
//!   12  unused             u32
//! ```
//!
//! and the `u64` block-pointer array follows it immediately, in the same
//! shape as the id and fragment tables — see [`crate::table`]. The
//! blocks it points at hold `xattr_ids` entries of:
//!
//! ```text
//!    0  xattr    u64   packed reference into the pair area
//!    8  count    u32   how many name/value pairs this set holds
//!   12  size     u32   see below — it is not the size of anything on disk
//! ```
//!
//! # `size` is a checksum, if you let it be
//!
//! It is not the number of bytes the set occupies on disk, which is the
//! obvious reading and the wrong one. Measured against images
//! `mksquashfs` 4.7.5 wrote, it is
//!
//! ```text
//!   Σ  len(full name) + 1 + len(value)
//! ```
//!
//! — the size of the buffer a `listxattr` followed by a `getxattr` for
//! every name would fill. That makes it independently derivable from the
//! decoded pairs, so this module computes it while parsing and refuses a
//! set whose total does not match. It is the cheapest available detector
//! for a misread length: a `name_size` or `vsize` off by one puts every
//! subsequent field at the wrong offset, and the running total is what
//! notices.
//!
//! # The pairs
//!
//! ```text
//!    0  type       u16   prefix index, plus 0x100 when the value is
//!                        stored OUT OF LINE
//!    2  name_size  u16   length of the name AFTER its prefix
//!    4  name       name_size bytes
//!       vsize      u32
//!       value      vsize bytes
//! ```
//!
//! The name is stored without its namespace prefix; the low byte of
//! `type` selects one of [`PREFIXES`]. That is a smaller idea than the
//! sister drivers' — ext4 has a one-byte index over a longer list, and
//! EROFS keeps a dictionary of arbitrary prefixes shared across the
//! image — but the shape of the answer is the same, and callers here get
//! the assembled name.
//!
//! # Out-of-line values
//!
//! With `0x100` set in `type`, the eight bytes stored in place of the
//! value are another packed reference, and the real value lives there as
//! a bare `u32 vsize` followed by its bytes — no type, no name. This is
//! how one large or repeated value is shared between sets. Measured
//! against `mksquashfs` 4.7.5: a 300-byte value went out of line while a
//! four-byte value repeated across three sets stayed inline in each, so
//! the trigger is length rather than repetition alone.
//!
//! A reference at the far end is never itself out of line — there is no
//! type byte there to say so — so the indirection is exactly one deep
//! and cannot loop.

use crate::error::{Error, Result};
use crate::metablock::{MetaCache, MetaCursor, MetadataRef};
use crate::superblock::Superblock;
use crate::table::{self, NO_TABLE};
use fs_core::BlockRead;

/// An inode's `xattr` field when it has no attributes.
pub const SQUASHFS_INVALID_XATTR: u32 = 0xFFFF_FFFF;

/// Set in `type` when the value is a reference rather than the value.
const XATTR_VALUE_OOL: u16 = 0x0100;
/// The bits of `type` that select the namespace prefix.
const XATTR_PREFIX_MASK: u16 = 0x00FF;

/// The namespaces a `type` byte can name, in the format's own order.
///
/// SquashFS stores an attribute's name without its prefix and the prefix
/// as this index, which is why a name comes back assembled rather than
/// as it sits on disk. A fourth value has never been defined; an image
/// using one is refused rather than guessed at, because the alternative
/// is handing back a name in a namespace that does not exist.
pub const PREFIXES: [&[u8]; 3] = [b"user.", b"trusted.", b"security."];

/// Bytes per id-table entry. Divides the 8 KiB metadata size evenly, so
/// an entry never straddles a block boundary and the concatenated blocks
/// can be sliced by index.
const ID_ENTRY_SIZE: usize = 16;

/// The header at `xattr_id_table_start`, before the block pointers.
const ID_TABLE_HEADER_SIZE: usize = 16;

/// The longest attribute name this reader will assemble.
///
/// `XATTR_NAME_MAX` on Linux, and the ceiling every producer respects,
/// since a name longer than this cannot be set through `setxattr` in the
/// first place. `name_size` on disk is a `u16` and would otherwise be
/// believed up to 65535.
const MAX_NAME_LEN: usize = 255;

/// The largest value this reader will read into memory.
///
/// `XATTR_SIZE_MAX` on Linux. `vsize` is a `u32` off the disk, so
/// without this a crafted image asks for a four-gigabyte allocation per
/// attribute; with it, the memory one set can cost is bounded by
/// [`MAX_XATTRS_PER_INODE`] times this.
const MAX_VALUE_LEN: usize = 65536;

/// How many attributes one set may declare.
///
/// `count` is a `u32` and nothing in the format bounds it. Sixteen
/// thousand is far past anything a real tree carries — `mksquashfs`
/// writes one set per distinct combination, and a file with even a
/// hundred attributes is unusual — and the alternative to a bound here
/// is a `Vec` sized by a number off the disk.
const MAX_XATTRS_PER_INODE: u32 = 16384;

/// One extended attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XattrEntry {
    /// The assembled name, prefix included: `user.colour`, not
    /// `colour`. Not NUL-terminated, and not required to be UTF-8.
    pub name: Vec<u8>,
    /// The raw value. May be empty — a zero-length value is a real thing
    /// to store, and is not the same as the attribute being absent.
    pub value: Vec<u8>,
}

/// One id-table entry: where a set of pairs lives, and what it should
/// add up to.
#[derive(Debug, Clone, Copy)]
struct XattrId {
    /// Packed reference into the pair area: block offset in the high 48
    /// bits, offset within the decompressed block in the low 16.
    xattr_ref: u64,
    count: u32,
    /// See the module docs — this is a total over the decoded pairs, not
    /// a length on disk.
    size: u32,
}

/// The whole xattr id table, read once at mount.
///
/// Small by construction: one 16-byte entry per distinct *set* of
/// attributes in the image, not per file. An image whose every file
/// carries the same label has one entry.
#[derive(Debug, Clone)]
pub struct XattrIdTable {
    /// Absolute offset the packed references are relative to.
    table_start: u64,
    ids: Vec<XattrId>,
}

impl XattrIdTable {
    /// How many distinct attribute sets the image holds.
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }
}

/// Read the xattr id table, or `None` when the image has no attributes.
///
/// An image built with `-no-xattrs` writes `u64::MAX` for
/// `xattr_id_table_start`, which is the same sentinel the other optional
/// tables use, so absence is a normal answer rather than an error.
pub fn read_id_table<R: BlockRead + ?Sized>(
    dev: &R,
    sb: &Superblock,
) -> Result<Option<XattrIdTable>> {
    let header_at = sb.xattr_id_table_start;
    if header_at == NO_TABLE {
        return Ok(None);
    }
    let mut header = [0u8; ID_TABLE_HEADER_SIZE];
    dev.read_at(header_at, &mut header)?;
    let table_start = u64::from_le_bytes(header[0..8].try_into().unwrap());
    let xattr_ids = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
    if xattr_ids == 0 {
        return Ok(Some(XattrIdTable {
            table_start,
            ids: Vec::new(),
        }));
    }
    // The pairs come BEFORE their index on disk, and a reference is an
    // offset from `table_start`, so a table that starts after its own
    // index would make every reference point past it.
    if table_start >= header_at {
        return Err(Error::BadMetadata(
            "xattr pair table starts at or after its own index",
        ));
    }

    let bytes = table::read_indirect_table(
        dev,
        sb,
        header_at + ID_TABLE_HEADER_SIZE as u64,
        xattr_ids * ID_ENTRY_SIZE,
    )?;
    let mut ids = Vec::with_capacity(xattr_ids);
    for i in 0..xattr_ids {
        let e = &bytes[i * ID_ENTRY_SIZE..(i + 1) * ID_ENTRY_SIZE];
        ids.push(XattrId {
            xattr_ref: u64::from_le_bytes(e[0..8].try_into().unwrap()),
            count: u32::from_le_bytes(e[8..12].try_into().unwrap()),
            size: u32::from_le_bytes(e[12..16].try_into().unwrap()),
        });
    }
    Ok(Some(XattrIdTable { table_start, ids }))
}

/// Read the set of attributes at `index`.
///
/// # Errors
///
/// [`Error::BadMetadata`] when the index is past the table, a name or
/// value is longer than the format's own limits, an undefined namespace
/// is named, or the decoded pairs do not add up to the `size` the id
/// declared.
pub fn read_set<R: BlockRead + ?Sized>(
    dev: &R,
    sb: &Superblock,
    table: &XattrIdTable,
    index: u32,
    cache: Option<&MetaCache>,
) -> Result<Vec<XattrEntry>> {
    let id = *table
        .ids
        .get(index as usize)
        .ok_or(Error::BadMetadata("xattr index past the end of the table"))?;
    if id.count > MAX_XATTRS_PER_INODE {
        return Err(Error::BadMetadata("xattr set declares an absurd count"));
    }

    let r = MetadataRef::from_packed(id.xattr_ref);
    let mut cur = MetaCursor::new(dev, sb, r.start_abs(table.table_start), r.in_block, cache)?;

    let mut out = Vec::with_capacity(id.count as usize);
    // Kept as a `u64` so a crafted set cannot wrap it back under `size`.
    let mut total: u64 = 0;
    for _ in 0..id.count {
        let raw_type = cur.read_u16()?;
        let name_size = cur.read_u16()? as usize;
        if name_size > MAX_NAME_LEN {
            return Err(Error::BadMetadata(
                "xattr name longer than the format allows",
            ));
        }
        let suffix = cur.read_exact(name_size)?;
        let prefix = PREFIXES
            .get((raw_type & XATTR_PREFIX_MASK) as usize)
            .ok_or(Error::BadMetadata("xattr names an undefined namespace"))?;
        let mut name = Vec::with_capacity(prefix.len() + suffix.len());
        name.extend_from_slice(prefix);
        name.extend_from_slice(&suffix);
        if name.len() > MAX_NAME_LEN {
            return Err(Error::BadMetadata(
                "xattr name longer than the format allows",
            ));
        }

        let value = if raw_type & XATTR_VALUE_OOL != 0 {
            // The eight bytes here are a reference, not a value. Its
            // length is still written as a `vsize` of 8, which is
            // checked rather than assumed: a different length would mean
            // the flag and the record disagree, and reading on would
            // take the next field from the wrong place.
            let vsize = cur.read_u32()?;
            if vsize as usize != 8 {
                return Err(Error::BadMetadata(
                    "out-of-line xattr value is not an 8-byte reference",
                ));
            }
            let target = cur.read_u64()?;
            read_ool_value(dev, sb, table, target, cache)?
        } else {
            let vsize = cur.read_u32()? as usize;
            if vsize > MAX_VALUE_LEN {
                return Err(Error::BadMetadata(
                    "xattr value longer than the format allows",
                ));
            }
            cur.read_exact(vsize)?
        };

        total += name.len() as u64 + 1 + value.len() as u64;
        if total > u64::from(id.size) {
            return Err(Error::BadMetadata(
                "xattr set is longer than the size it declared",
            ));
        }
        out.push(XattrEntry { name, value });
    }
    // THE LOAD-BEARING CHECK. `size` is derivable from the decoded pairs
    // and nothing else, so a total that lands short means a length was
    // read from the wrong place and everything after it was too.
    if total != u64::from(id.size) {
        return Err(Error::BadMetadata(
            "xattr set does not add up to the size it declared",
        ));
    }
    Ok(out)
}

/// Follow an out-of-line reference to the value it names.
///
/// The target is a bare `u32 vsize` and its bytes: no type, no name, and
/// so no possibility of a second hop. The indirection is exactly one
/// deep by construction rather than by a depth counter.
fn read_ool_value<R: BlockRead + ?Sized>(
    dev: &R,
    sb: &Superblock,
    table: &XattrIdTable,
    packed: u64,
    cache: Option<&MetaCache>,
) -> Result<Vec<u8>> {
    let r = MetadataRef::from_packed(packed);
    let mut cur = MetaCursor::new(dev, sb, r.start_abs(table.table_start), r.in_block, cache)?;
    let vsize = cur.read_u32()? as usize;
    if vsize > MAX_VALUE_LEN {
        return Err(Error::BadMetadata(
            "out-of-line xattr value longer than the format allows",
        ));
    }
    cur.read_exact(vsize)
}

#[cfg(test)]
mod tests {
    //! Unit tests over hand-built tables.
    //!
    //! **Necessary but not sufficient.** The encoder below writes the
    //! fields at the offsets the parser reads them from, so a misreading
    //! of the on-disk layout would be baked into both sides and these
    //! would pass anyway. What they buy is the bounds arithmetic, the
    //! out-of-line hop, the prefix assembly and the `size` check.
    //! Whether the layout is right at all is settled by
    //! `tests/xattr_oracle.rs` against images `mksquashfs` wrote and
    //! `unsquashfs` reads back.

    use super::*;
    use crate::metablock::tests::{emit_meta, MemDev};
    use crate::superblock::tests::synth_sb;
    use std::sync::Mutex;

    /// One name/value pair, as it sits in a metadata block.
    fn pair(prefix: u16, suffix: &[u8], value: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&prefix.to_le_bytes());
        b.extend_from_slice(&(suffix.len() as u16).to_le_bytes());
        b.extend_from_slice(suffix);
        b.extend_from_slice(&(value.len() as u32).to_le_bytes());
        b.extend_from_slice(value);
        b
    }

    /// A pair whose value is a reference to `target` rather than the
    /// value itself.
    fn ool_pair(prefix: u16, suffix: &[u8], target: u64) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&(prefix | XATTR_VALUE_OOL).to_le_bytes());
        b.extend_from_slice(&(suffix.len() as u16).to_le_bytes());
        b.extend_from_slice(suffix);
        b.extend_from_slice(&8u32.to_le_bytes());
        b.extend_from_slice(&target.to_le_bytes());
        b
    }

    /// What `size` must be, computed the way the format computes it.
    fn declared_size(pairs: &[(&[u8], &[u8])]) -> u32 {
        pairs
            .iter()
            .map(|(n, v)| n.len() as u32 + 1 + v.len() as u32)
            .sum()
    }

    /// Assemble an image holding one pair area and one id table.
    ///
    /// Layout, which is the on-disk order `mksquashfs` uses: the pair
    /// blocks first, then the id table's own metadata block, then the
    /// 16-byte header and the pointer array.
    fn image(pair_area: &[u8], ids: &[(u64, u32, u32)]) -> (Vec<u8>, u64) {
        let mut img = vec![0u8; 96]; // room for a superblock
        let table_start = img.len() as u64;
        img.extend_from_slice(&emit_meta(pair_area));

        let mut id_bytes = Vec::new();
        for (xattr_ref, count, size) in ids {
            id_bytes.extend_from_slice(&xattr_ref.to_le_bytes());
            id_bytes.extend_from_slice(&count.to_le_bytes());
            id_bytes.extend_from_slice(&size.to_le_bytes());
        }
        let id_block_at = img.len() as u64;
        img.extend_from_slice(&emit_meta(&id_bytes));

        let header_at = img.len() as u64;
        img.extend_from_slice(&table_start.to_le_bytes());
        img.extend_from_slice(&(ids.len() as u32).to_le_bytes());
        img.extend_from_slice(&0u32.to_le_bytes());
        img.extend_from_slice(&id_block_at.to_le_bytes());
        (img, header_at)
    }

    fn mount(img: Vec<u8>, header_at: u64) -> (MemDev, Superblock, XattrIdTable) {
        let mut sb = Superblock::parse(&synth_sb(17, 0, 0)).unwrap();
        sb.xattr_id_table_start = header_at;
        sb.bytes_used = img.len() as u64;
        let dev = MemDev(Mutex::new(img));
        let table = read_id_table(&dev, &sb).unwrap().expect("a table");
        (dev, sb, table)
    }

    #[test]
    fn reads_a_set_of_two_and_assembles_the_prefixes() {
        let mut area = pair(0, b"colour", b"blue");
        area.extend(pair(1, b"level", b"high"));
        let size = declared_size(&[(b"user.colour", b"blue"), (b"trusted.level", b"high")]);
        let (img, at) = image(&area, &[(0, 2, size)]);
        let (dev, sb, table) = mount(img, at);

        let got = read_set(&dev, &sb, &table, 0, None).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].name, b"user.colour");
        assert_eq!(got[0].value, b"blue");
        assert_eq!(got[1].name, b"trusted.level");
        assert_eq!(got[1].value, b"high");
    }

    #[test]
    fn every_defined_prefix_is_assembled() {
        for (idx, want) in [
            (0u16, &b"user.x"[..]),
            (1, &b"trusted.x"[..]),
            (2, &b"security.x"[..]),
        ] {
            let area = pair(idx, b"x", b"v");
            let (img, at) = image(&area, &[(0, 1, want.len() as u32 + 1 + 1)]);
            let (dev, sb, table) = mount(img, at);
            assert_eq!(read_set(&dev, &sb, &table, 0, None).unwrap()[0].name, want);
        }
    }

    /// A fourth namespace has never been defined. Guessing would hand a
    /// caller a name in a namespace that does not exist, which is worse
    /// than saying so.
    #[test]
    fn an_undefined_namespace_is_refused() {
        let area = pair(3, b"x", b"v");
        let (img, at) = image(&area, &[(0, 1, 10)]);
        let (dev, sb, table) = mount(img, at);
        assert!(matches!(
            read_set(&dev, &sb, &table, 0, None),
            Err(Error::BadMetadata(_))
        ));
    }

    /// The out-of-line hop, which is how one value is shared between
    /// sets. Two sets whose values both point at the same place must
    /// both read it.
    #[test]
    fn an_out_of_line_value_is_followed_and_can_be_shared() {
        let shared = b"a value worth sharing".to_vec();
        // The value record: a bare u32 length and its bytes, with no
        // type and no name.
        let mut area = Vec::new();
        area.extend_from_slice(&(shared.len() as u32).to_le_bytes());
        area.extend_from_slice(&shared);
        let target = 0u64; // block 0, offset 0

        let first_at = area.len() as u64;
        area.extend(ool_pair(0, b"one", target));
        let second_at = area.len() as u64;
        area.extend(ool_pair(2, b"two", target));

        let size_one = declared_size(&[(b"user.one", &shared)]);
        let size_two = declared_size(&[(b"security.two", &shared)]);
        let (img, at) = image(&area, &[(first_at, 1, size_one), (second_at, 1, size_two)]);
        let (dev, sb, table) = mount(img, at);

        let a = read_set(&dev, &sb, &table, 0, None).unwrap();
        let b = read_set(&dev, &sb, &table, 1, None).unwrap();
        assert_eq!(a[0].name, b"user.one");
        assert_eq!(a[0].value, shared);
        assert_eq!(b[0].name, b"security.two");
        assert_eq!(b[0].value, shared, "the shared value read differently");
    }

    /// The flag says the value is a reference; the length says how long
    /// it is. If they disagree the record is not what it claims, and
    /// reading on would take the next field from the wrong offset.
    #[test]
    fn an_out_of_line_value_of_the_wrong_length_is_refused() {
        let mut area = Vec::new();
        area.extend_from_slice(&(XATTR_VALUE_OOL).to_le_bytes());
        area.extend_from_slice(&1u16.to_le_bytes());
        area.push(b'x');
        area.extend_from_slice(&4u32.to_le_bytes()); // not 8
        area.extend_from_slice(&0u32.to_le_bytes());
        let (img, at) = image(&area, &[(0, 1, 100)]);
        let (dev, sb, table) = mount(img, at);
        assert!(matches!(
            read_set(&dev, &sb, &table, 0, None),
            Err(Error::BadMetadata(_))
        ));
    }

    /// `size` is derivable from the decoded pairs and nothing else, so
    /// it detects a length read from the wrong place — which is the
    /// mistake that would otherwise produce plausible-looking garbage
    /// rather than an error.
    #[test]
    fn a_set_that_does_not_add_up_to_its_declared_size_is_refused() {
        let area = pair(0, b"colour", b"blue");
        let honest = declared_size(&[(b"user.colour", b"blue")]);
        for wrong in [honest - 1, honest + 1] {
            let (img, at) = image(&area, &[(0, 1, wrong)]);
            let (dev, sb, table) = mount(img, at);
            assert!(
                matches!(
                    read_set(&dev, &sb, &table, 0, None),
                    Err(Error::BadMetadata(_))
                ),
                "a set declaring {wrong} instead of {honest} was accepted"
            );
        }
    }

    #[test]
    fn a_zero_length_value_is_read_as_a_value() {
        let area = pair(0, b"flag", b"");
        let (img, at) = image(&area, &[(0, 1, declared_size(&[(b"user.flag", b"")]))]);
        let (dev, sb, table) = mount(img, at);
        let got = read_set(&dev, &sb, &table, 0, None).unwrap();
        assert_eq!(got[0].name, b"user.flag");
        assert!(got[0].value.is_empty());
    }

    #[test]
    fn an_index_past_the_table_is_refused() {
        let area = pair(0, b"x", b"v");
        let (img, at) = image(&area, &[(0, 1, 9)]);
        let (dev, sb, table) = mount(img, at);
        assert_eq!(table.len(), 1);
        assert!(matches!(
            read_set(&dev, &sb, &table, 1, None),
            Err(Error::BadMetadata(_))
        ));
    }

    /// `count` is a `u32` with nothing in the format bounding it, and it
    /// sizes a `Vec` before a single pair has been read.
    #[test]
    fn an_absurd_count_is_refused_before_anything_is_reserved() {
        let area = pair(0, b"x", b"v");
        let (img, at) = image(&area, &[(0, u32::MAX, u32::MAX)]);
        let (dev, sb, table) = mount(img, at);
        assert!(matches!(
            read_set(&dev, &sb, &table, 0, None),
            Err(Error::BadMetadata(_))
        ));
    }

    /// `vsize` is a `u32`, so without a ceiling one attribute asks for a
    /// four-gigabyte allocation.
    #[test]
    fn a_value_longer_than_the_format_allows_is_refused() {
        let mut area = Vec::new();
        area.extend_from_slice(&0u16.to_le_bytes());
        area.extend_from_slice(&1u16.to_le_bytes());
        area.push(b'x');
        area.extend_from_slice(&u32::MAX.to_le_bytes());
        let (img, at) = image(&area, &[(0, 1, u32::MAX)]);
        let (dev, sb, table) = mount(img, at);
        assert!(matches!(
            read_set(&dev, &sb, &table, 0, None),
            Err(Error::BadMetadata(_))
        ));
    }

    /// An image built with `-no-xattrs` writes the same sentinel every
    /// other absent table uses, and absence has to be a normal answer
    /// rather than an error.
    #[test]
    fn an_image_without_a_table_reports_none() {
        let mut sb = Superblock::parse(&synth_sb(17, 0, 0)).unwrap();
        sb.xattr_id_table_start = NO_TABLE;
        let dev = MemDev(Mutex::new(vec![0u8; 96]));
        assert!(read_id_table(&dev, &sb).unwrap().is_none());
    }

    /// The pairs are written before their index, so a `xattr_table_start`
    /// at or after the header means every reference points past the data
    /// it is supposed to name.
    #[test]
    fn a_pair_table_starting_after_its_own_index_is_refused() {
        let area = pair(0, b"x", b"v");
        let (mut img, at) = image(&area, &[(0, 1, 9)]);
        // Rewrite the header's table_start to point at the header.
        img[at as usize..at as usize + 8].copy_from_slice(&at.to_le_bytes());
        let mut sb = Superblock::parse(&synth_sb(17, 0, 0)).unwrap();
        sb.xattr_id_table_start = at;
        sb.bytes_used = img.len() as u64;
        let dev = MemDev(Mutex::new(img));
        assert!(matches!(
            read_id_table(&dev, &sb),
            Err(Error::BadMetadata(_))
        ));
    }

    /// The constants are the format's and the operating system's, not
    /// this module's opinion. Pinned so a change to one has to be
    /// deliberate.
    #[test]
    fn the_limits_are_the_ones_the_docs_name() {
        assert_eq!(SQUASHFS_INVALID_XATTR, u32::MAX);
        assert_eq!(NO_TABLE, u64::MAX);
        assert_eq!(XATTR_VALUE_OOL, 0x0100);
        assert_eq!(MAX_NAME_LEN, 255, "XATTR_NAME_MAX");
        assert_eq!(MAX_VALUE_LEN, 64 * 1024, "XATTR_SIZE_MAX");
        assert_eq!(ID_ENTRY_SIZE * 512, 8192, "an entry must tile a metablock");
    }
}
