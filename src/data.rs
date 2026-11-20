//! A regular file's data as stored: where each block is, and what it
//! holds, with a sparse block and a missing one kept apart.
//!
//! A file is a run of full data blocks, each stored compressed or not, or
//! not stored at all when it is all zeros (a **sparse** block, size word
//! 0), followed by an optional tail packed into a shared **fragment**
//! block. [`file_map`] lays that out from the inode alone, reading
//! nothing. [`read_file_block`] then reads one block and says what it
//! holds:
//!
//! - [`BlockContent::Bytes`]: the block decoded to exactly its length, and
//!   whether a compressed stream vouched for it: a block stored
//!   uncompressed has no integrity check at all.
//! - [`BlockContent::Sparse`]: the format says these bytes are zeros.
//! - [`BlockContent::Missing`]: the block could not be read, did not
//!   decode, decoded to the wrong length, or names a fragment that is not
//!   there. Its bytes are unknown -- **never zeros**. A recovered file
//!   that filled them with zeros would contain content nobody wrote.

use crate::damage::{Damage, DamageKind, Severity};
use crate::decompress::{self, Compressor};
use crate::error::{Error, Result};
use crate::inode::Inode;
use crate::table::{self, FragmentEntry};
use fs_core::BlockRead;
use std::sync::Arc;

/// Where one block of a file's data is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockSource {
    /// Not stored: the format says the block is all zeros.
    Sparse,
    /// A data block of its own.
    Stored {
        /// Absolute offset of the stored bytes.
        offset: u64,
        /// The size word as stored: on-disk length, and bit 24 set when
        /// the bytes are stored uncompressed.
        size_word: u32,
    },
    /// The file's tail, inside a shared fragment block.
    Fragment {
        /// Index into the fragment table.
        index: u32,
        /// Where the tail starts in the decoded fragment block.
        offset_in_fragment: u32,
    },
}

impl BlockSource {
    /// The stored bytes' extent in the image, for a block of its own.
    pub fn extent(&self) -> Option<(u64, u64)> {
        match *self {
            BlockSource::Stored { offset, size_word } => {
                Some((offset, u64::from(table::data_on_disk_size(size_word))))
            }
            _ => None,
        }
    }
}

/// One logical block of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileBlock {
    /// The block's number in the file, counting the tail as the last.
    pub index: u64,
    /// Where the block starts in the file.
    pub file_offset: u64,
    /// How many bytes of the file it holds.
    pub len: u64,
    /// Where those bytes are.
    pub source: BlockSource,
}

/// Lay out a regular file's blocks from its inode alone: every full
/// block in order, then the tail fragment if there is one.
///
/// Reads nothing, so it works on any inode the inode table yields. A
/// non-file inode has no blocks.
pub fn file_map(inode: &Inode, block_size: u32) -> Vec<FileBlock> {
    if !inode.is_regular_file() {
        return Vec::new();
    }
    let bs = u64::from(block_size);
    let mut out = Vec::with_capacity(inode.block_sizes.len() + 1);
    let mut stored_at = inode.blocks_start;
    for (i, &size_word) in inode.block_sizes.iter().enumerate() {
        let file_offset = i as u64 * bs;
        let len = bs.min(inode.file_size.saturating_sub(file_offset));
        let on_disk = u64::from(table::data_on_disk_size(size_word));
        let source = if on_disk == 0 {
            BlockSource::Sparse
        } else {
            BlockSource::Stored {
                offset: stored_at,
                size_word,
            }
        };
        stored_at = stored_at.saturating_add(on_disk);
        out.push(FileBlock {
            index: i as u64,
            file_offset,
            len,
            source,
        });
    }
    let covered = inode.block_sizes.len() as u64 * bs;
    if inode.has_fragment() && inode.file_size > covered {
        out.push(FileBlock {
            index: inode.block_sizes.len() as u64,
            file_offset: covered,
            len: inode.file_size - covered,
            source: BlockSource::Fragment {
                index: inode.fragment_index,
                offset_in_fragment: inode.fragment_offset,
            },
        });
    }
    out
}

/// What one block of a file holds.
#[derive(Debug, Clone)]
pub enum BlockContent {
    /// The block's bytes, exactly its length.
    Bytes {
        /// The bytes.
        bytes: Arc<Vec<u8>>,
        /// Whether a compressed stream decoded to them, which is the only
        /// integrity check SquashFS has. A block stored uncompressed has
        /// none: damage to it reads back as different bytes, and nothing
        /// in the image can say so.
        checked: bool,
    },
    /// A sparse block: this many zeros, by the format's own record.
    Sparse(u64),
    /// The block's bytes are unknown, and why.
    Missing(Damage),
}

/// Decode a stored data block of `size_word` at `offset`, to at most
/// `max_out` bytes. The strict reader's own decode.
///
/// A stored length larger than the image's block size is refused: a block
/// whose compressed form is larger than the block it decodes to is stored
/// uncompressed by `mksquashfs` instead, so such a length is corrupt by
/// definition, and it would otherwise allocate up to 16 MiB per block.
pub fn decode_stored_block<R: BlockRead + ?Sized>(
    dev: &R,
    comp: Compressor,
    block_size: u32,
    offset: u64,
    size_word: u32,
    max_out: usize,
) -> Result<Vec<u8>> {
    let on_disk = table::data_on_disk_size(size_word) as usize;
    if on_disk > block_size as usize {
        return Err(Error::BadInode(
            "data block's on-disk size exceeds the archive's block size",
        ));
    }
    let mut raw = vec![0u8; on_disk];
    dev.read_at(offset, &mut raw)?;
    if table::data_is_compressed(size_word) {
        decompress::decompress(comp, &raw, max_out)
    } else {
        if raw.len() > max_out {
            return Err(Error::BadInode(
                "uncompressed data block exceeds block size",
            ));
        }
        Ok(raw)
    }
}

/// What [`read_file_block`] needs besides the block.
pub struct DataContext<'a, R: BlockRead + ?Sized> {
    /// The device.
    pub dev: &'a R,
    /// The image's compressor, or `None` when the superblock names none.
    pub comp: Option<Compressor>,
    /// The image's data block size.
    pub block_size: u32,
    /// Where the image ends: no stored block may reach past it.
    pub bytes_used: u64,
    /// The fragment table entry for an index, when it is known.
    pub fragment: &'a dyn Fn(u32) -> Option<FragmentEntry>,
}

/// Read one block of a file and say what it holds. Never fails: whatever
/// stops the read is in [`BlockContent::Missing`].
pub fn read_file_block<R: BlockRead + ?Sized>(
    ctx: &DataContext<'_, R>,
    block: &FileBlock,
) -> BlockContent {
    let missing = |kind, offset: u64, length: u64, structure: &'static str, detail: String| {
        BlockContent::Missing(Damage {
            kind,
            severity: Severity::Error,
            offset,
            length,
            structure,
            detail,
        })
    };
    let (stored_at, size_word, slice) = match block.source {
        BlockSource::Sparse => return BlockContent::Sparse(block.len),
        BlockSource::Stored { offset, size_word } => (offset, size_word, None),
        BlockSource::Fragment {
            index,
            offset_in_fragment,
        } => {
            let Some(entry) = (ctx.fragment)(index) else {
                return missing(
                    DamageKind::Missing,
                    0,
                    0,
                    "fragment table",
                    format!(
                        "block {}: fragment {index} is not in the fragment table",
                        block.index
                    ),
                );
            };
            (entry.start, entry.size, Some(offset_in_fragment))
        }
    };
    let structure = if slice.is_some() {
        "fragment block"
    } else {
        "data block"
    };
    let on_disk = u64::from(table::data_on_disk_size(size_word));
    if stored_at.saturating_add(on_disk) > ctx.bytes_used {
        return missing(
            DamageKind::Mismatch,
            stored_at,
            on_disk,
            structure,
            format!("block {}: stored past the end of the image", block.index),
        );
    }
    let comp = match (table::data_is_compressed(size_word), ctx.comp) {
        (true, None) => {
            return missing(
                DamageKind::Missing,
                stored_at,
                on_disk,
                structure,
                format!(
                    "block {}: compressed, and no compressor to decode it",
                    block.index
                ),
            )
        }
        (_, comp) => comp.unwrap_or(Compressor::Gzip),
    };
    let checked = table::data_is_compressed(size_word);
    let max_out = ctx.block_size as usize;
    let decoded =
        match decode_stored_block(ctx.dev, comp, ctx.block_size, stored_at, size_word, max_out) {
            Ok(bytes) => bytes,
            Err(e) => {
                let kind = match e {
                    Error::Block(_) => DamageKind::Unreadable,
                    Error::BadInode(_) => DamageKind::Malformed,
                    _ => DamageKind::Checksum,
                };
                return missing(
                    kind,
                    stored_at,
                    on_disk,
                    structure,
                    format!("block {}: {e}", block.index),
                );
            }
        };
    let want = block.len as usize;
    match slice {
        // A full block decodes to exactly its length: the file size and
        // the block geometry fix it, and a codec cannot check it for us.
        None if decoded.len() == want => BlockContent::Bytes {
            bytes: Arc::new(decoded),
            checked,
        },
        None => missing(
            DamageKind::Mismatch,
            stored_at,
            on_disk,
            structure,
            format!(
                "block {}: decodes to {} bytes, not {want}",
                block.index,
                decoded.len()
            ),
        ),
        Some(start) => {
            let start = start as usize;
            match decoded.get(start..start.saturating_add(want)) {
                Some(tail) if start.checked_add(want).is_some() => BlockContent::Bytes {
                    bytes: Arc::new(tail.to_vec()),
                    checked,
                },
                _ => missing(
                    DamageKind::Mismatch,
                    stored_at,
                    on_disk,
                    structure,
                    format!(
                        "tail of {want} bytes at {start} is past the fragment block's {} decoded bytes",
                        decoded.len()
                    ),
                ),
            }
        }
    }
}
