//! Indirect lookup tables (id table, fragment table).
//!
//! SquashFS stores several tables (uid/gid ids, fragment locations, the
//! export inode map) indirectly: at `table_start` there is a raw
//! little-endian array of `u64` pointers — NOT a metadata block — one per
//! metadata block that holds the table's entries. Each pointer is the
//! absolute byte offset of a metadata block. Concatenating those
//! decompressed blocks yields the entry array.
//!
//! Entry sizes here (id = 4 bytes, fragment = 16 bytes) both divide the
//! 8 KiB metadata size evenly, so a single entry never straddles a
//! metadata-block boundary — we can decompress all blocks, concatenate,
//! and slice by index.

use crate::error::{Error, Result};
use crate::metablock::{read_block, MetaCache};
use crate::superblock::{Superblock, METADATA_SIZE};
use fs_core::BlockRead;

/// Data-block size word convention (shared by file block_sizes and
/// fragment entries): bit 24 set → stored UNCOMPRESSED; low 24 bits → the
/// on-disk size of the (possibly compressed) block.
/// Set means the data block is stored UNCOMPRESSED.
///
/// Named for what it is about rather than what it means, which reads
/// backwards at every use: `raw & DATA_COMPRESSED_BIT == 0` is the test
/// for "this block IS compressed". The polarity is the format's.
///
/// [`DATA_UNCOMPRESSED_BIT`] is the same value under a name that reads
/// correctly. This one is kept because it is `pub` and published; prefer
/// the new name in new code.
/// The value an optional table's start carries when the table is absent.
///
/// `mksquashfs -no-exports` and `-no-xattrs` both write it, and so does
/// every other switch that leaves a table out. One name for it, in the
/// module about tables, rather than one per table that can be missing.
pub const NO_TABLE: u64 = u64::MAX;

pub const DATA_COMPRESSED_BIT: u32 = 1 << 24;

/// Set means the data block is stored uncompressed. Same bit as
/// [`DATA_COMPRESSED_BIT`], named for what it means.
pub const DATA_UNCOMPRESSED_BIT: u32 = 1 << 24;
pub const DATA_SIZE_MASK: u32 = (1 << 24) - 1;

/// On-disk byte length of a data block from its size word.
pub fn data_on_disk_size(raw: u32) -> u32 {
    raw & DATA_SIZE_MASK
}

/// Whether a data block's payload is compressed (bit 24 clear).
pub fn data_is_compressed(raw: u32) -> bool {
    raw & DATA_UNCOMPRESSED_BIT == 0
}

/// One fragment-table entry: where a packed fragment block lives + its
/// on-disk size word (same convention as [`data_on_disk_size`]).
#[derive(Debug, Clone, Copy)]
pub struct FragmentEntry {
    pub start: u64,
    pub size: u32,
}

/// Read + concatenate an indirect table's metadata blocks. `total_bytes`
/// is `n_entries * entry_size`; we pull `ceil(total_bytes / 8192)` block
/// pointers from `table_start` and return the concatenated decompressed
/// bytes (length >= `total_bytes`).
///
/// Shared with [`crate::xattr`], whose id table has the same shape: a
/// raw `u64` pointer array followed by metadata blocks of fixed-size
/// entries. The bounds check below is the reason it is shared rather
/// than written twice.
pub(crate) fn read_indirect_table<R: BlockRead + ?Sized>(
    dev: &R,
    sb: &Superblock,
    table_start: u64,
    total_bytes: usize,
) -> Result<Vec<u8>> {
    if total_bytes == 0 {
        return Ok(Vec::new());
    }
    let n_blocks = total_bytes.div_ceil(METADATA_SIZE);
    // THE POINTER ARRAY HAS TO FIT WHERE IT SAYS IT IS.
    //
    // `total_bytes` is an entry count off the superblock multiplied by
    // an entry size, and the fragment count is a `u32`: 0xFFFFFFFF asks
    // for a 64 MiB pointer array and a 64 MiB read, unconditionally, on
    // every mount, from a 96-byte header. Filled with valid metablock
    // pointers it is worse -- a 4.2 MB image mounted at 1.99 GB
    // resident, memory that then stays held in the filesystem.
    //
    // The array is raw u64s starting at `table_start`, so it has to fit
    // between there and the end of the filesystem. That is the same
    // rule the kernel applies to its own index tables.
    let array_bytes = n_blocks
        .checked_mul(8)
        .ok_or(Error::BadMetadata("indirect table pointer array overflows"))?;
    let room = sb
        .bytes_used
        .min(dev.size_bytes())
        .saturating_sub(table_start);
    if array_bytes as u64 > room {
        return Err(Error::BadMetadata(
            "indirect table declares more blocks than it has room for",
        ));
    }
    let mut ptr_bytes = vec![0u8; array_bytes];
    dev.read_at(table_start, &mut ptr_bytes)?;

    // Capped, because `total_bytes` is still whatever the count said;
    // the vector grows to what is actually decoded.
    let mut out = Vec::with_capacity(total_bytes.min(1 << 20));
    for i in 0..n_blocks {
        let p = u64::from_le_bytes(ptr_bytes[i * 8..i * 8 + 8].try_into().unwrap());
        // No cache: the id and fragment tables are read once, at open,
        // and never again. Caching their blocks would evict inode and
        // directory blocks that are read over and over.
        let (block, _next) = read_block(dev, sb, p, None)?;
        out.extend_from_slice(&block);
    }
    if out.len() < total_bytes {
        return Err(Error::BadMetadata("indirect table shorter than expected"));
    }
    Ok(out)
}

/// Read the uid/gid id table → a vector of `id_count` u32 ids. `uid_idx` /
/// `gid_idx` in an inode index into this.
pub fn read_id_table<R: BlockRead + ?Sized>(dev: &R, sb: &Superblock) -> Result<Vec<u32>> {
    let n = sb.id_count as usize;
    if n == 0 {
        return Ok(Vec::new());
    }
    let bytes = read_indirect_table(dev, sb, sb.id_table_start, n * 4)?;
    let mut ids = Vec::with_capacity(n);
    for i in 0..n {
        ids.push(u32::from_le_bytes(
            bytes[i * 4..i * 4 + 4].try_into().unwrap(),
        ));
    }
    Ok(ids)
}

/// Read the fragment table → one [`FragmentEntry`] per
/// `fragment_entry_count`. A file's `fragment_index` indexes into this.
pub fn read_fragment_table<R: BlockRead + ?Sized>(
    dev: &R,
    sb: &Superblock,
) -> Result<Vec<FragmentEntry>> {
    let n = sb.fragment_entry_count as usize;
    if n == 0 {
        return Ok(Vec::new());
    }
    // Each on-disk fragment entry is 16 bytes: u64 start, u32 size, u32 unused.
    let bytes = read_indirect_table(dev, sb, sb.fragment_table_start, n * 16)?;
    let mut frags = Vec::with_capacity(n);
    for i in 0..n {
        let o = i * 16;
        let start = u64::from_le_bytes(bytes[o..o + 8].try_into().unwrap());
        let size = u32::from_le_bytes(bytes[o + 8..o + 12].try_into().unwrap());
        frags.push(FragmentEntry { start, size });
    }
    Ok(frags)
}

/// Bytes per export-table entry: one packed inode reference.
const EXPORT_ENTRY_SIZE: usize = 8;

/// The export table — an inode number back to the inode.
///
/// # What it is for
///
/// Every other way into this filesystem starts from the root and walks
/// down. That is fine for `open("/a/b/c")` and useless for a caller
/// holding nothing but a number, which is the position an NFS server is
/// in when a client hands back a file handle, and the position any layer
/// that hands out file IDs and is later asked to resolve one is in too.
/// Without this table such a caller has to keep its own map of every
/// inode it ever mentioned, or walk the whole tree again.
///
/// # Shape
///
/// A raw `u64` pointer array at `export_table_start` — the same indirect
/// shape as the id and fragment tables — whose metadata blocks hold one
/// `u64` per inode. Entry `n - 1` is the packed metadata reference for
/// inode number `n`: INODE NUMBERS ARE 1-BASED, and an off-by-one here
/// returns a real inode that is simply the wrong one, which is the
/// failure this table's user is least able to detect. Verified against
/// `mksquashfs` 4.7.5: every entry resolved to an inode whose own
/// `inode_number` field was its index plus one.
///
/// # Why the entries are not loaded at mount
///
/// The table has one entry per inode, not per distinct anything, so it
/// is the only table here that scales with the size of the image: a
/// million-inode image is eight megabytes of it. What IS loaded is the
/// pointer array — one `u64` per 8 KiB of entries, so eight kilobytes
/// for that same million inodes — and a lookup then decompresses the one
/// metadata block it needs. Those go through the metadata cache, so a
/// run of lookups in the same region pays for the block once.
///
/// Eight divides the 8 KiB metadata size, so an entry never straddles a
/// block boundary and a lookup is always one block.
#[derive(Debug, Clone)]
pub struct ExportTable {
    /// Absolute offset of each metadata block holding entries.
    block_starts: Vec<u64>,
    /// How many inodes the table covers — the superblock's `inode_count`.
    inode_count: u32,
}

impl ExportTable {
    /// How many inodes the table covers.
    pub fn inode_count(&self) -> u32 {
        self.inode_count
    }

    /// The packed metadata reference for `inode_number`.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when the number is zero or past the last
    /// inode — inode 0 does not exist, so a caller passing it has a bug
    /// rather than an unusual filesystem.
    pub fn lookup<R: BlockRead + ?Sized>(
        &self,
        dev: &R,
        sb: &Superblock,
        inode_number: u32,
        cache: Option<&MetaCache>,
    ) -> Result<u64> {
        if inode_number == 0 || inode_number > self.inode_count {
            return Err(Error::NotFound);
        }
        let index = (inode_number - 1) as usize;
        let byte = index * EXPORT_ENTRY_SIZE;
        let block = byte / METADATA_SIZE;
        let within = byte % METADATA_SIZE;
        let start = *self
            .block_starts
            .get(block)
            .ok_or(Error::BadMetadata("export table is shorter than it claims"))?;
        let (data, _next) = read_block(dev, sb, start, cache)?;
        let end = within + EXPORT_ENTRY_SIZE;
        if end > data.len() {
            return Err(Error::BadMetadata(
                "export table entry runs past its metadata block",
            ));
        }
        Ok(u64::from_le_bytes(data[within..end].try_into().unwrap()))
    }
}

/// Read the export table's pointer array, or `None` when the image was
/// built with `mksquashfs -no-exports`.
///
/// Absence is a normal answer, not an error: the table is optional, and
/// an image without one simply cannot be asked this question.
pub fn read_export_table<R: BlockRead + ?Sized>(
    dev: &R,
    sb: &Superblock,
) -> Result<Option<ExportTable>> {
    let start = sb.export_table_start;
    if start == NO_TABLE {
        return Ok(None);
    }
    let inode_count = sb.inode_count;
    if inode_count == 0 {
        return Ok(Some(ExportTable {
            block_starts: Vec::new(),
            inode_count: 0,
        }));
    }
    let total_bytes = inode_count as usize * EXPORT_ENTRY_SIZE;
    let n_blocks = total_bytes.div_ceil(METADATA_SIZE);
    // The same rule `read_indirect_table` applies, for the same reason:
    // `inode_count` is a `u32` off the superblock, so 0xFFFFFFFF asks
    // for a 32 MiB pointer array on every mount from a 96-byte header.
    // Only the pointers are read here — the entries are not — so this is
    // the only place the count can be checked against reality.
    let array_bytes = n_blocks
        .checked_mul(8)
        .ok_or(Error::BadMetadata("export table pointer array overflows"))?;
    let room = sb.bytes_used.min(dev.size_bytes()).saturating_sub(start);
    if array_bytes as u64 > room {
        return Err(Error::BadMetadata(
            "export table declares more blocks than it has room for",
        ));
    }
    let mut ptr_bytes = vec![0u8; array_bytes];
    dev.read_at(start, &mut ptr_bytes)?;
    let block_starts = (0..n_blocks)
        .map(|i| u64::from_le_bytes(ptr_bytes[i * 8..i * 8 + 8].try_into().unwrap()))
        .collect();
    Ok(Some(ExportTable {
        block_starts,
        inode_count,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_size_word_decoding() {
        // Compressed 1234-byte block: bit 24 clear.
        assert_eq!(data_on_disk_size(1234), 1234);
        assert!(data_is_compressed(1234));
        // Uncompressed 4096-byte block: bit 24 set.
        let raw = 4096 | DATA_COMPRESSED_BIT;
        assert_eq!(data_on_disk_size(raw), 4096);
        assert!(!data_is_compressed(raw));
        // Sparse block: size word 0.
        assert_eq!(data_on_disk_size(0), 0);
    }

    /// `read_fragment_table` runs unconditionally at mount, and its
    /// size comes from a `u32` in the superblock. 0xFFFFFFFF asked for
    /// a 64 MiB pointer array and a 64 MiB read from a 96-byte header.
    ///
    /// The assertion is on *which* refusal: the read comes up short
    /// either way, but only after the buffer has been allocated.
    #[test]
    fn a_fragment_table_with_no_room_for_its_pointers_is_refused_by_name() {
        use crate::metablock::tests::MemDev;
        use crate::superblock::tests::synth_sb;
        use std::sync::Mutex;

        let mut raw = synth_sb(17, 0, 96);
        raw[0x10..0x14].copy_from_slice(&u32::MAX.to_le_bytes()); // fragment_entry_count
        raw[0x28..0x30].copy_from_slice(&4000u64.to_le_bytes()); // bytes_used
        raw[0x50..0x58].copy_from_slice(&96u64.to_le_bytes()); // fragment_table_start
        let sb = Superblock::parse(&raw).unwrap();
        assert_eq!(sb.fragment_entry_count, u32::MAX);

        let dev = MemDev(Mutex::new(vec![0u8; 4000]));
        let why = format!("{:?}", read_fragment_table(&dev, &sb).err());
        assert!(
            why.contains("more blocks than it has room for"),
            "a 64 MiB fragment table in a 4000-byte image was refused as {why}, \
             which means the buffer was allocated and read first"
        );
    }

    /// An export table built by hand, so the indexing can be tested
    /// against entries whose values are known.
    ///
    /// **Necessary but not sufficient**: the encoder here writes `u64`s
    /// where the reader expects them, so a misreading of the on-disk
    /// shape would be baked into both. Whether the shape is right, and
    /// whether entry `n - 1` really is inode `n`, is settled by
    /// `tests/export_oracle.rs` against images `mksquashfs` wrote.
    fn export_image(refs: &[u64]) -> (Vec<u8>, Superblock) {
        use crate::metablock::tests::emit_meta;
        use crate::superblock::tests::synth_sb;

        let mut entries = Vec::new();
        for r in refs {
            entries.extend_from_slice(&r.to_le_bytes());
        }
        let mut img = vec![0u8; 96];
        let block_at = img.len() as u64;
        img.extend_from_slice(&emit_meta(&entries));
        let table_at = img.len() as u64;
        img.extend_from_slice(&block_at.to_le_bytes());

        let mut raw = synth_sb(17, 0, 0);
        raw[0x04..0x08].copy_from_slice(&(refs.len() as u32).to_le_bytes()); // inode_count
        raw[0x28..0x30].copy_from_slice(&(img.len() as u64 + 8).to_le_bytes()); // bytes_used
        raw[0x58..0x60].copy_from_slice(&table_at.to_le_bytes());
        let sb = Superblock::parse(&raw).unwrap();
        (img, sb)
    }

    /// Inode numbers are 1-BASED. Getting this wrong returns a real
    /// inode that is simply the wrong one, which a caller resolving a
    /// file identifier cannot detect at all.
    #[test]
    fn entry_n_minus_one_is_inode_n() {
        use crate::metablock::tests::MemDev;
        use std::sync::Mutex;

        let refs = [0x1111u64, 0x2222, 0x3333, 0x4444];
        let (img, sb) = export_image(&refs);
        let dev = MemDev(Mutex::new(img));
        let table = read_export_table(&dev, &sb).unwrap().expect("a table");
        assert_eq!(table.inode_count(), refs.len() as u32);

        for (i, want) in refs.iter().enumerate() {
            let n = i as u32 + 1;
            assert_eq!(
                table.lookup(&dev, &sb, n, None).unwrap(),
                *want,
                "inode {n} resolved to the wrong entry"
            );
        }
    }

    /// Zero is not an inode number, and neither is one past the last.
    #[test]
    fn numbers_outside_the_table_are_refused() {
        use crate::metablock::tests::MemDev;
        use std::sync::Mutex;

        let (img, sb) = export_image(&[0x1111, 0x2222]);
        let dev = MemDev(Mutex::new(img));
        let table = read_export_table(&dev, &sb).unwrap().unwrap();
        assert!(matches!(
            table.lookup(&dev, &sb, 0, None),
            Err(Error::NotFound)
        ));
        assert!(matches!(
            table.lookup(&dev, &sb, 3, None),
            Err(Error::NotFound)
        ));
        assert!(matches!(
            table.lookup(&dev, &sb, u32::MAX, None),
            Err(Error::NotFound)
        ));
    }

    /// `mksquashfs -no-exports` writes the sentinel every absent table
    /// uses, and absence has to be a normal answer rather than an error.
    #[test]
    fn an_image_without_an_export_table_reports_none() {
        use crate::metablock::tests::MemDev;
        use crate::superblock::tests::synth_sb;
        use std::sync::Mutex;

        let mut raw = synth_sb(17, 0, 0);
        raw[0x58..0x60].copy_from_slice(&NO_TABLE.to_le_bytes());
        let sb = Superblock::parse(&raw).unwrap();
        let dev = MemDev(Mutex::new(vec![0u8; 96]));
        assert!(read_export_table(&dev, &sb).unwrap().is_none());
    }

    /// `inode_count` is a `u32` off the superblock and it sizes a read
    /// at mount. 0xFFFFFFFF asks for a 32 MiB pointer array from a
    /// 96-byte header, and the assertion is on WHICH refusal: the read
    /// comes up short either way, but only after the buffer is
    /// allocated.
    #[test]
    fn an_export_table_with_no_room_for_its_pointers_is_refused_by_name() {
        use crate::metablock::tests::MemDev;
        use crate::superblock::tests::synth_sb;
        use std::sync::Mutex;

        let mut raw = synth_sb(17, 0, 96);
        raw[0x04..0x08].copy_from_slice(&u32::MAX.to_le_bytes()); // inode_count
        raw[0x28..0x30].copy_from_slice(&4000u64.to_le_bytes()); // bytes_used
        raw[0x58..0x60].copy_from_slice(&96u64.to_le_bytes());
        let sb = Superblock::parse(&raw).unwrap();

        let dev = MemDev(Mutex::new(vec![0u8; 4000]));
        let why = format!("{:?}", read_export_table(&dev, &sb).err());
        assert!(
            why.contains("more blocks than it has room for"),
            "a 32 MiB export table in a 4000-byte image was refused as {why}, \
             which means the buffer was allocated and read first"
        );
    }

    /// Eight bytes per entry divides the 8 KiB metadata size, which is
    /// what makes a lookup exactly one block. Pinned, because a lookup
    /// that spanned two blocks would have to be written differently.
    #[test]
    fn an_entry_never_straddles_a_metadata_block() {
        assert_eq!(METADATA_SIZE % EXPORT_ENTRY_SIZE, 0);
    }

    #[test]
    fn empty_tables_short_circuit() {
        use crate::metablock::tests::MemDev;
        use crate::superblock::tests::synth_sb;
        use std::sync::Mutex;
        let dev = MemDev(Mutex::new(vec![0u8; 96]));
        let sb = Superblock::parse(&synth_sb(17, 0, 0)).unwrap();
        // id_count and fragment_entry_count are 0 in the synth SB.
        assert!(read_id_table(&dev, &sb).unwrap().is_empty());
        assert!(read_fragment_table(&dev, &sb).unwrap().is_empty());
    }
}
