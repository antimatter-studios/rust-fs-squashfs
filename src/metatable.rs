//! Metadata blocks one at a time, and metadata tables walked block by
//! block, with every damaged block reported on its own.
//!
//! The strict reader ([`crate::metablock::read_block`] and the
//! [`MetaCursor`](crate::metablock::MetaCursor) over it) fails a read at
//! the first bad block, so one damaged block in the inode table loses
//! every inode a cursor would have reached through it. These primitives
//! never fail on damage: [`read_metablock`] says what one block holds or
//! why it holds nothing, and [`walk_table`] frames a whole table into
//! blocks, carrying on past a bad one.
//!
//! # Framing, and finding it again
//!
//! A metadata table is a run of blocks, each a 2-byte header (bit 15 set
//! means stored uncompressed; the low 15 bits are the stored length)
//! followed by that many bytes. The header is the only thing that says
//! where the next block starts, so a damaged header loses the framing.
//!
//! The walk then **resynchronises**: it scans forward byte by byte for
//! the next offset that holds a whole, valid block -- a header in range,
//! a payload that decodes, and 8 KiB decoded, or fewer only for a block
//! that ends exactly at the end of the table. Every block but the last in
//! a table `mksquashfs` writes decodes to exactly 8 KiB, so a false match
//! needs garbage that happens to decode to exactly that, which is the
//! same check the codec makes of real data. The bytes skipped are a
//! [`TableGap`].
//!
//! A block whose header is in range but whose payload does not decode is
//! different: its header may still be right. The walk trusts it only if
//! the block after it is valid; otherwise the header is as suspect as
//! the payload, and the walk resynchronises from the byte after it.

use crate::damage::{Damage, DamageKind, Severity};
use crate::decompress::{self, Compressor};
use crate::error::{Error, Result};
use crate::superblock::METADATA_SIZE;
use fs_core::BlockRead;
use std::sync::Arc;

/// Set means the block is stored UNCOMPRESSED.
///
/// The polarity is the format's, not a choice made here, and it is the
/// opposite of what the obvious name would suggest -- which is why the
/// name says what the bit means rather than what it is about.
pub(crate) const UNCOMPRESSED_BIT: u16 = 0x8000;
const SIZE_MASK: u16 = 0x7FFF;
const HEADER_LEN: u64 = 2;

/// What one metadata block holds.
#[derive(Debug)]
pub enum MetablockState {
    /// The payload, decoded (or as stored, for an uncompressed block).
    Decoded(Arc<Vec<u8>>),
    /// The header's stored length is zero or more than 8 KiB.
    BadHeader,
    /// The block is compressed and there is no compressor to decode it
    /// with: the superblock's compression id names none.
    NoCompressor,
    /// The payload does not decode. The error is the codec's.
    Undecodable(Error),
    /// The device refused to read the header or the payload.
    Unreadable(Error),
}

/// One metadata block, read on its own.
#[derive(Debug)]
pub struct MetablockRead {
    /// Absolute offset of the block's 2-byte header.
    pub offset: u64,
    /// The raw header, or `None` when it could not be read.
    pub header: Option<u16>,
    /// What the block holds.
    pub state: MetablockState,
}

impl MetablockRead {
    /// Whether the header says the payload is compressed.
    pub fn compressed(&self) -> bool {
        self.header.is_some_and(|h| h & UNCOMPRESSED_BIT == 0)
    }

    /// The stored payload length the header claims.
    pub fn stored_len(&self) -> u64 {
        u64::from(self.header.unwrap_or(0) & SIZE_MASK)
    }

    /// Whether the header is readable and its length in range, so that it
    /// frames the next block.
    pub fn framed(&self) -> bool {
        self.header.is_some() && (1..=METADATA_SIZE as u64).contains(&self.stored_len())
    }

    /// Where the next block starts, if this header frames one.
    pub fn next(&self) -> Option<u64> {
        self.framed()
            .then(|| self.offset.saturating_add(HEADER_LEN + self.stored_len()))
    }

    /// The decoded bytes, if the block decoded.
    pub fn bytes(&self) -> Option<&Arc<Vec<u8>>> {
        match &self.state {
            MetablockState::Decoded(b) => Some(b),
            _ => None,
        }
    }

    /// The block's bytes on disk, header included, as far as the header
    /// can be trusted to say: just the header when it frames nothing.
    pub fn extent(&self) -> (u64, u64) {
        let len = if self.framed() {
            HEADER_LEN + self.stored_len()
        } else {
            HEADER_LEN
        };
        (self.offset, len)
    }

    /// The block's damage, or `None` when it decoded.
    pub fn damage(&self, structure: &'static str) -> Option<Damage> {
        let (kind, detail) = match &self.state {
            MetablockState::Decoded(_) => return None,
            MetablockState::BadHeader => (
                DamageKind::Malformed,
                format!(
                    "metadata block header {:#06x} claims {} stored bytes (1 to 8192 allowed)",
                    self.header.unwrap_or(0),
                    self.stored_len()
                ),
            ),
            MetablockState::NoCompressor => (
                DamageKind::Missing,
                "compressed metadata block, and no compressor to decode it".to_string(),
            ),
            MetablockState::Undecodable(e) => (
                DamageKind::Checksum,
                format!("metadata block does not decode: {e}"),
            ),
            MetablockState::Unreadable(e) => (
                DamageKind::Unreadable,
                format!("metadata block unreadable: {e}"),
            ),
        };
        let (offset, length) = self.extent();
        Some(Damage {
            kind,
            severity: Severity::Error,
            offset,
            length,
            structure,
            detail,
        })
    }

    /// The strict reading of the block: its bytes and the next block's
    /// offset, or the error the strict reader has always given.
    pub fn into_result(self) -> Result<(Arc<Vec<u8>>, u64)> {
        let next = self.next();
        match self.state {
            MetablockState::Decoded(bytes) => Ok((bytes, next.expect("a decoded block is framed"))),
            MetablockState::BadHeader => {
                Err(Error::BadMetadata("metadata block size out of range"))
            }
            MetablockState::NoCompressor => Err(Error::BadMetadata(
                "compressed metadata block, and no compressor to decode it",
            )),
            MetablockState::Undecodable(e) | MetablockState::Unreadable(e) => Err(e),
        }
    }
}

/// Read and decode the metadata block whose header is at `abs`.
///
/// Never fails: whatever is wrong is in the returned state. `comp` is the
/// image's compressor, or `None` when the superblock names none; an
/// uncompressed block needs no compressor.
pub fn read_metablock<R: BlockRead + ?Sized>(
    dev: &R,
    comp: Option<Compressor>,
    abs: u64,
) -> MetablockRead {
    let mut hdr = [0u8; HEADER_LEN as usize];
    if let Err(e) = dev.read_at(abs, &mut hdr) {
        return MetablockRead {
            offset: abs,
            header: None,
            state: MetablockState::Unreadable(e.into()),
        };
    }
    let header = u16::from_le_bytes(hdr);
    let mut block = MetablockRead {
        offset: abs,
        header: Some(header),
        state: MetablockState::BadHeader,
    };
    if !block.framed() {
        return block;
    }
    let mut payload = vec![0u8; block.stored_len() as usize];
    if let Err(e) = dev.read_at(abs.saturating_add(HEADER_LEN), &mut payload) {
        block.state = MetablockState::Unreadable(e.into());
        return block;
    }
    block.state = if !block.compressed() {
        MetablockState::Decoded(Arc::new(payload))
    } else {
        match comp {
            None => MetablockState::NoCompressor,
            Some(comp) => match decompress::decompress(comp, &payload, METADATA_SIZE) {
                Ok(out) => MetablockState::Decoded(Arc::new(out)),
                Err(e) => MetablockState::Undecodable(e),
            },
        }
    };
    block
}

/// Bytes of a table the walk could not frame into blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableGap {
    /// Absolute offset of the first byte skipped.
    pub offset: u64,
    /// How many bytes were skipped.
    pub length: u64,
}

/// A metadata table, framed block by block.
#[derive(Debug)]
pub struct TableWalk {
    /// What the table is, for damage reports: `"inode table"`,
    /// `"directory table"`.
    pub structure: &'static str,
    /// Absolute offset of the table's first block.
    pub start: u64,
    /// Absolute offset the table ends at (exclusive).
    pub end: u64,
    /// Every block found, in order, damaged ones included.
    pub blocks: Vec<MetablockRead>,
    /// Every run of bytes that could not be framed into a block.
    pub gaps: Vec<TableGap>,
    /// Decoded blocks other than the last that hold fewer than 8 KiB.
    /// Their bytes are kept, but every reference past them into the
    /// table's stream is in doubt.
    pub short_blocks: Vec<u64>,
}

impl TableWalk {
    /// The block whose header is at `offset` bytes from the table start:
    /// the block a metadata reference's high 48 bits name.
    pub fn block_at(&self, offset: u64) -> Option<&MetablockRead> {
        let abs = self.start.checked_add(offset)?;
        self.blocks
            .binary_search_by_key(&abs, |b| b.offset)
            .ok()
            .map(|i| &self.blocks[i])
    }

    /// How many blocks decoded.
    pub fn decoded(&self) -> usize {
        self.blocks.iter().filter(|b| b.bytes().is_some()).count()
    }

    /// Whether every byte of the table framed into a block that decoded
    /// to its full length.
    pub fn is_clean(&self) -> bool {
        self.gaps.is_empty()
            && self.short_blocks.is_empty()
            && self.blocks.iter().all(|b| b.bytes().is_some())
    }

    /// Every piece of damage in the table: each damaged block, each gap,
    /// and each short block, in table order.
    pub fn damage(&self) -> Vec<Damage> {
        let mut out: Vec<Damage> = self
            .blocks
            .iter()
            .filter_map(|b| b.damage(self.structure))
            .collect();
        out.extend(self.gaps.iter().map(|g| Damage {
            kind: DamageKind::Broken,
            severity: Severity::Error,
            offset: g.offset,
            length: g.length,
            structure: self.structure,
            detail: "bytes that frame into no metadata block".to_string(),
        }));
        out.extend(self.short_blocks.iter().filter_map(|&at| {
            let block = self.blocks.iter().find(|b| b.offset == at)?;
            let (offset, length) = block.extent();
            Some(Damage {
                kind: DamageKind::Mismatch,
                severity: Severity::Warning,
                offset,
                length,
                structure: self.structure,
                detail: format!(
                    "metadata block decodes to {} bytes; every block but the last holds 8192",
                    block.bytes().map_or(0, |b| b.len())
                ),
            })
        }));
        out.sort_by_key(|d| d.offset);
        out
    }
}

/// Whether the block at `at` is one the walk can take as found: framed,
/// inside the table, decoded, and full unless it ends the table.
fn is_whole_block(block: &MetablockRead, end: u64) -> bool {
    let Some(next) = block.next() else {
        return false;
    };
    match block.bytes() {
        Some(bytes) => next <= end && (bytes.len() == METADATA_SIZE || next == end),
        None => false,
    }
}

/// Scan forward from `from` for the next whole block, as
/// [`is_whole_block`] judges it. `None` when there is none before `end`.
fn resync<R: BlockRead + ?Sized>(
    dev: &R,
    comp: Option<Compressor>,
    from: u64,
    end: u64,
) -> Option<MetablockRead> {
    let mut at = from;
    while at.saturating_add(HEADER_LEN + 1) <= end {
        let block = read_metablock(dev, comp, at);
        if is_whole_block(&block, end) {
            return Some(block);
        }
        at += 1;
    }
    None
}

/// Frame the metadata table `[start, end)` into blocks, carrying on past
/// damage. See the module documentation for how the framing is found
/// again after a bad header.
///
/// Every block found is in the result, damaged ones included, and so is
/// every byte that could not be framed. Nothing is dropped: a block or a
/// gap is reported, never skipped silently.
pub fn walk_table<R: BlockRead + ?Sized>(
    dev: &R,
    comp: Option<Compressor>,
    start: u64,
    end: u64,
    structure: &'static str,
) -> TableWalk {
    let end = end.min(dev.size_bytes());
    let mut walk = TableWalk {
        structure,
        start,
        end,
        blocks: Vec::new(),
        gaps: Vec::new(),
        short_blocks: Vec::new(),
    };
    let mut at = start;
    while at < end {
        let block = read_metablock(dev, comp, at);
        let next = block.next().filter(|&n| n <= end);
        let trusted = match (&block.state, next) {
            (MetablockState::Decoded(_), Some(_)) => true,
            // A payload that does not decode, under a header in range: the
            // header is trusted when what follows it is a whole block, or
            // when it ends the table exactly.
            (
                MetablockState::Undecodable(_)
                | MetablockState::NoCompressor
                | MetablockState::Unreadable(_),
                Some(n),
            ) => {
                n == end
                    || (comp.is_none() && block.compressed())
                    || is_whole_block(&read_metablock(dev, comp, n), end)
            }
            _ => false,
        };
        if trusted {
            let n = next.expect("a trusted block is framed");
            if let Some(bytes) = block.bytes() {
                if bytes.len() != METADATA_SIZE && n != end {
                    walk.short_blocks.push(block.offset);
                }
            }
            walk.blocks.push(block);
            at = n;
            continue;
        }
        // The framing is lost at `at`. The block is still reported, for
        // its header, and then the walk looks for the next whole block.
        walk.blocks.push(block);
        match resync(dev, comp, at + 1, end) {
            Some(found) => {
                let skipped_from = at + HEADER_LEN;
                if found.offset > skipped_from {
                    walk.gaps.push(TableGap {
                        offset: skipped_from,
                        length: found.offset - skipped_from,
                    });
                }
                at = found.offset;
            }
            None => {
                let skipped_from = (at + HEADER_LEN).min(end);
                if end > skipped_from {
                    walk.gaps.push(TableGap {
                        offset: skipped_from,
                        length: end - skipped_from,
                    });
                }
                break;
            }
        }
    }
    walk
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metablock::tests::{emit_meta, MemDev};
    use std::sync::Mutex;

    const GZ: Option<Compressor> = Some(Compressor::Gzip);

    /// A full 8 KiB block whose bytes say which block it is.
    fn full(tag: u8) -> Vec<u8> {
        (0..METADATA_SIZE)
            .map(|i| tag.wrapping_add((i % 251) as u8))
            .collect()
    }

    /// A table of three blocks -- two full, one short final -- and the
    /// offset of each.
    fn table() -> (Vec<u8>, [u64; 3]) {
        let mut img = Vec::new();
        let mut at = [0u64; 3];
        for (i, payload) in [full(1), full(2), b"the last block".repeat(9)]
            .iter()
            .enumerate()
        {
            at[i] = img.len() as u64;
            img.extend_from_slice(&emit_meta(payload));
        }
        (img, at)
    }

    fn walk(img: Vec<u8>, comp: Option<Compressor>) -> TableWalk {
        let end = img.len() as u64;
        walk_table(&MemDev(Mutex::new(img)), comp, 0, end, "inode table")
    }

    #[test]
    fn a_clean_table_frames_into_every_block() {
        let (img, at) = table();
        let w = walk(img, GZ);
        assert_eq!(w.blocks.iter().map(|b| b.offset).collect::<Vec<_>>(), at);
        assert!(w.is_clean(), "{:?}", w.damage());
        assert_eq!(w.decoded(), 3);
        assert_eq!(
            w.block_at(at[1]).unwrap().bytes().unwrap().as_slice(),
            full(2)
        );
        assert!(w.block_at(at[1] + 1).is_none());
    }

    #[test]
    fn a_payload_that_does_not_decode_loses_that_block_only() {
        let (mut img, at) = table();
        let mid = at[1] as usize;
        for b in &mut img[mid + 6..mid + 20] {
            *b ^= 0x5A;
        }
        let w = walk(img, GZ);
        assert_eq!(w.blocks.len(), 3);
        assert!(w.gaps.is_empty(), "{:?}", w.gaps);
        assert!(matches!(w.blocks[1].state, MetablockState::Undecodable(_)));
        assert_eq!(
            w.blocks[2].bytes().unwrap().as_slice(),
            b"the last block".repeat(9)
        );
        let damage = w.damage();
        assert_eq!(damage.len(), 1, "{damage:?}");
        assert_eq!(damage[0].kind, DamageKind::Checksum);
        assert_eq!(damage[0].offset, at[1]);
        assert_eq!(damage[0].length, at[2] - at[1]);
    }

    #[test]
    fn a_header_out_of_range_is_skipped_to_the_next_whole_block() {
        let (mut img, at) = table();
        let mid = at[1] as usize;
        img[mid..mid + 2].copy_from_slice(&0x7FFFu16.to_le_bytes());
        let w = walk(img, GZ);
        let offsets: Vec<_> = w.blocks.iter().map(|b| b.offset).collect();
        assert_eq!(offsets, at);
        assert!(matches!(w.blocks[1].state, MetablockState::BadHeader));
        assert_eq!(
            w.gaps,
            [TableGap {
                offset: at[1] + 2,
                length: at[2] - at[1] - 2
            }]
        );
        assert!(w.blocks[0].bytes().is_some() && w.blocks[2].bytes().is_some());
    }

    #[test]
    fn a_header_in_range_but_wrong_is_not_trusted_to_frame_the_next_block() {
        let (mut img, at) = table();
        let mid = at[1] as usize;
        let stored = u16::from_le_bytes([img[mid], img[mid + 1]]);
        img[mid..mid + 2].copy_from_slice(&(stored - 10).to_le_bytes());
        let w = walk(img, GZ);
        assert_eq!(w.blocks.last().unwrap().offset, at[2], "{:?}", w.blocks);
        assert_eq!(w.decoded(), 2);
        assert_eq!(w.gaps.len(), 1);
        assert_eq!(w.gaps[0].offset + w.gaps[0].length, at[2]);
    }

    #[test]
    fn trailing_bytes_that_are_no_block_are_a_gap_to_the_end() {
        let (mut img, at) = table();
        let last = at[2] as usize;
        img[last..last + 2].copy_from_slice(&0u16.to_le_bytes());
        let end = img.len() as u64;
        let w = walk(img, GZ);
        assert_eq!(w.decoded(), 2);
        assert_eq!(
            w.gaps,
            [TableGap {
                offset: at[2] + 2,
                length: end - at[2] - 2
            }]
        );
    }

    #[test]
    fn without_a_compressor_the_headers_still_frame_the_table() {
        let (img, at) = table();
        let w = walk(img, None);
        assert_eq!(w.blocks.iter().map(|b| b.offset).collect::<Vec<_>>(), at);
        assert!(w.gaps.is_empty());
        assert!(w
            .blocks
            .iter()
            .all(|b| matches!(b.state, MetablockState::NoCompressor)));
        assert_eq!(w.damage().len(), 3);
        assert!(w.damage().iter().all(|d| d.kind == DamageKind::Missing));
    }

    #[test]
    fn a_short_block_before_the_last_is_kept_and_reported() {
        let mut img = emit_meta(&full(1));
        let short_at = img.len() as u64;
        img.extend_from_slice(&emit_meta(b"short"));
        img.extend_from_slice(&emit_meta(b"last"));
        let w = walk(img, GZ);
        assert_eq!(w.decoded(), 3);
        assert_eq!(w.short_blocks, [short_at]);
        assert!(!w.is_clean());
        let damage = w.damage();
        assert_eq!(damage.len(), 1);
        assert_eq!(
            (damage[0].kind, damage[0].severity),
            (DamageKind::Mismatch, Severity::Warning)
        );
    }

    #[test]
    fn the_strict_read_and_the_primitive_agree() {
        let (img, at) = table();
        let dev = MemDev(Mutex::new(img));
        let sb =
            crate::superblock::Superblock::parse(&crate::superblock::tests::synth_sb(17, 0, 96))
                .unwrap();
        let (bytes, next) = crate::metablock::read_block(&dev, &sb, at[1], None).unwrap();
        let block = read_metablock(&dev, GZ, at[1]);
        assert_eq!(block.bytes().unwrap(), &bytes);
        assert_eq!(block.next(), Some(next));
        assert_eq!(next, at[2]);
    }
}
