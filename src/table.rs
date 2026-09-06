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
use crate::metablock::read_block;
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
fn read_indirect_table<R: BlockRead + ?Sized>(
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
