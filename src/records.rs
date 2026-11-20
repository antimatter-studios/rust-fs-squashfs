//! Inodes and directory listings decoded out of a walked table, past
//! damage elsewhere in it.
//!
//! The strict reader decodes a record through a cursor that reads
//! metadata blocks from the device and fails on the first bad one. Here
//! the blocks come from a [`TableWalk`], already framed and decoded, so
//! a record is lost only if its own bytes are: an inode is decoded when
//! every block it spans decoded and follows the one before it without a
//! gap, and a listing keeps every entry before the first byte it cannot
//! read or make sense of.
//!
//! The parsers are the strict reader's own ([`crate::inode`] and
//! [`crate::dir`]), so a record decoded here is decoded exactly as the
//! strict reader would have.

use crate::damage::{Damage, DamageKind, Severity};
use crate::dir::{parse_listing_partial, DirEntry};
use crate::error::{Error, Result};
use crate::inode::{self, Inode, InodeRecord, RecordReader};
use crate::metablock::MetadataRef;
use crate::metatable::TableWalk;

/// Why a record could not be decoded, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordFault {
    /// What kind of damage stopped it.
    pub kind: DamageKind,
    /// Absolute offset of the metadata block the fault is in, or of the
    /// first byte that frames into no block.
    pub offset: u64,
    /// The extent of that block on disk.
    pub length: u64,
    /// What went wrong, with the record's position in its block.
    pub detail: String,
}

impl RecordFault {
    /// The fault as damage in `structure`.
    pub fn damage(&self, structure: &'static str) -> Damage {
        Damage {
            kind: self.kind,
            severity: Severity::Error,
            offset: self.offset,
            length: self.length,
            structure,
            detail: self.detail.clone(),
        }
    }
}

/// A cursor over the decoded bytes of a walked table, starting at a
/// metadata reference and crossing into each following block only when
/// that block decoded and starts where the one before it ends.
struct WalkCursor<'w> {
    walk: &'w TableWalk,
    index: usize,
    pos: usize,
    /// The fault that stopped a read, kept for the caller: the reader
    /// trait carries only the crate's error type.
    stopped: Option<RecordFault>,
}

impl<'w> WalkCursor<'w> {
    fn new(walk: &'w TableWalk, reference: MetadataRef) -> std::result::Result<Self, RecordFault> {
        let abs = reference.start_abs(walk.start);
        let Ok(index) = walk.blocks.binary_search_by_key(&abs, |b| b.offset) else {
            return Err(RecordFault {
                kind: DamageKind::Missing,
                offset: abs,
                length: 0,
                detail: format!(
                    "no metadata block of the {} starts at +{}",
                    walk.structure, reference.block_offset
                ),
            });
        };
        let block = &walk.blocks[index];
        let (offset, length) = block.extent();
        let Some(bytes) = block.bytes() else {
            let damage = block
                .damage(walk.structure)
                .expect("an undecoded block is damage");
            return Err(RecordFault {
                kind: damage.kind,
                offset,
                length,
                detail: format!("the record's block is damaged: {}", damage.detail),
            });
        };
        if usize::from(reference.in_block) > bytes.len() {
            return Err(RecordFault {
                kind: DamageKind::Malformed,
                offset,
                length,
                detail: format!(
                    "reference +{} is past the block's {} decoded bytes",
                    reference.in_block,
                    bytes.len()
                ),
            });
        }
        Ok(WalkCursor {
            walk,
            index,
            pos: usize::from(reference.in_block),
            stopped: None,
        })
    }

    /// Step to the next block, or say why the record cannot continue.
    fn advance(&mut self) -> std::result::Result<(), RecordFault> {
        let here = &self.walk.blocks[self.index];
        let follows = here.next();
        let next = self.walk.blocks.get(self.index + 1);
        match next {
            Some(next) if Some(next.offset) == follows => {
                if next.bytes().is_some() {
                    self.index += 1;
                    self.pos = 0;
                    return Ok(());
                }
                let (offset, length) = next.extent();
                let damage = next.damage(self.walk.structure).expect("damage");
                Err(RecordFault {
                    kind: damage.kind,
                    offset,
                    length,
                    detail: format!("the record runs into a damaged block: {}", damage.detail),
                })
            }
            _ => {
                let offset = follows.unwrap_or(here.offset);
                Err(RecordFault {
                    kind: DamageKind::Broken,
                    offset,
                    length: next.map_or(0, |n| n.offset.saturating_sub(offset)),
                    detail: if offset >= self.walk.end {
                        format!(
                            "the record runs past the end of the {}",
                            self.walk.structure
                        )
                    } else {
                        "the record runs into bytes that frame into no block".to_string()
                    },
                })
            }
        }
    }

    /// Up to `n` bytes, as many as can be read, and the fault that cut
    /// them short.
    fn read_available(&mut self, n: usize) -> (Vec<u8>, Option<RecordFault>) {
        let mut out = Vec::with_capacity(n.min(1 << 16));
        while out.len() < n {
            let bytes = self.walk.blocks[self.index]
                .bytes()
                .expect("the cursor stands on decoded blocks only");
            if self.pos >= bytes.len() {
                if let Err(fault) = self.advance() {
                    return (out, Some(fault));
                }
                continue;
            }
            let take = (bytes.len() - self.pos).min(n - out.len());
            out.extend_from_slice(&bytes[self.pos..self.pos + take]);
            self.pos += take;
        }
        (out, None)
    }
}

impl RecordReader for WalkCursor<'_> {
    fn read_exact(&mut self, n: usize) -> Result<Vec<u8>> {
        let (bytes, fault) = self.read_available(n);
        match fault {
            None => Ok(bytes),
            Some(fault) => {
                self.stopped = Some(fault);
                Err(Error::BadMetadata("record runs into damaged metadata"))
            }
        }
    }
}

/// Decode the inode at `inode_ref` out of a walked inode table, for an
/// image of `block_size` data blocks.
pub fn inode_at(
    walk: &TableWalk,
    inode_ref: u64,
    block_size: u32,
) -> std::result::Result<InodeRecord, RecordFault> {
    let reference = MetadataRef::from_packed(inode_ref);
    let mut cur = WalkCursor::new(walk, reference)?;
    inode::parse(&mut cur, block_size).map_err(|e| match cur.stopped.take() {
        Some(fault) => fault,
        None => {
            let block = &walk.blocks[cur.index];
            let (offset, length) = block.extent();
            RecordFault {
                kind: DamageKind::Malformed,
                offset,
                length,
                detail: format!("inode at +{}: {e}", reference.in_block),
            }
        }
    })
}

/// A directory listing decoded as far as it goes.
#[derive(Debug, Clone)]
pub struct ListingRead {
    /// Every entry before the first fault.
    pub entries: Vec<DirEntry>,
    /// What stopped the listing, or `None` when all of it decoded.
    pub fault: Option<RecordFault>,
}

/// Decode `dir`'s listing out of a walked directory table, keeping every
/// entry before the first byte that cannot be read or makes no sense.
///
/// # Panics
///
/// Never on damage. `dir` should be a directory inode; for any other
/// type the listing is empty.
pub fn listing_at(walk: &TableWalk, dir: &Inode) -> ListingRead {
    let len = if dir.is_dir() {
        dir.dir_listing_len()
    } else {
        0
    };
    if len == 0 {
        return ListingRead {
            entries: Vec::new(),
            fault: None,
        };
    }
    let reference = MetadataRef {
        block_offset: u64::from(dir.dir_start_block),
        in_block: dir.dir_block_offset,
    };
    let mut cur = match WalkCursor::new(walk, reference) {
        Ok(cur) => cur,
        Err(fault) => {
            return ListingRead {
                entries: Vec::new(),
                fault: Some(fault),
            }
        }
    };
    let start_block = cur.index;
    let (bytes, read_fault) = cur.read_available(len);
    let listing = parse_listing_partial(&bytes);
    // A listing cut short by damage ends in a truncated record, which the
    // parse reports as its own fault: the damage is the cause.
    let fault = read_fault.or_else(|| {
        listing.fault.map(|f| {
            let (offset, length) = walk.blocks[start_block].extent();
            RecordFault {
                kind: DamageKind::Malformed,
                offset,
                length,
                detail: format!(
                    "listing at +{} of its first block, byte {} of the listing: {}",
                    reference.in_block, f.offset, f.reason
                ),
            }
        })
    });
    ListingRead {
        entries: listing.entries,
        fault,
    }
}

/// A cursor over one contiguous run of decoded bytes.
struct SliceCursor<'s> {
    bytes: &'s [u8],
    pos: usize,
}

impl RecordReader for SliceCursor<'_> {
    fn read_exact(&mut self, n: usize) -> Result<Vec<u8>> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.bytes.len())
            .ok_or(Error::BadMetadata("record runs past the decoded run"))?;
        let out = self.bytes[self.pos..end].to_vec();
        self.pos = end;
        Ok(out)
    }
}

/// An inode found by scanning the inode table, and where.
#[derive(Debug, Clone)]
pub struct ScannedInode {
    /// The packed reference that names it: block offset from the table
    /// start in the high 48 bits, offset in the decoded block in the low 16.
    pub inode_ref: u64,
    /// The inode as decoded.
    pub record: InodeRecord,
}

/// What a scan of the inode table found.
#[derive(Debug, Clone, Default)]
pub struct InodeScan {
    /// Every inode decoded, in table order.
    pub inodes: Vec<ScannedInode>,
    /// Every place the scan lost the run of records and how many bytes of
    /// decoded inode table it skipped before finding the next record:
    /// `(inode reference where it was lost, bytes skipped)`.
    pub skipped: Vec<(u64, usize)>,
}

/// Whether a decoded record is one `mksquashfs` could have written: a
/// known type and an inode number inside the image's count.
fn plausible(record: &InodeRecord, inode_count: u32) -> bool {
    let n = record.inode.inode_number;
    !matches!(record.inode.file_type(), crate::inode::FileType::Unknown)
        && n >= 1
        && n <= inode_count
}

/// Scan every run of contiguous decoded blocks in a walked inode table
/// for inodes, without needing any reference to them.
///
/// `mksquashfs` writes inodes back to back from the start of the table,
/// so the first run is decoded from its first byte. A run that starts
/// after damage may start inside a record that began in the lost block;
/// the scan then looks for the first offset where a plausible record
/// (a known type, an inode number within `inode_count`) is followed by a
/// second one, or ends the run exactly, and goes on from there. The
/// bytes passed over are reported in [`InodeScan::skipped`].
pub fn scan_inodes(walk: &TableWalk, block_size: u32, inode_count: u32) -> InodeScan {
    let mut scan = InodeScan::default();
    let mut i = 0;
    while i < walk.blocks.len() {
        if walk.blocks[i].bytes().is_none() {
            i += 1;
            continue;
        }
        // One run: decoded blocks, each starting where the last ends.
        let first = i;
        let mut stream = Vec::new();
        let mut starts = Vec::new(); // (stream position, block offset from table start)
        while let Some(bytes) = walk.blocks.get(i).and_then(|b| b.bytes()) {
            starts.push((stream.len(), walk.blocks[i].offset - walk.start));
            stream.extend_from_slice(bytes);
            let follows = walk.blocks[i].next();
            i += 1;
            if walk.blocks.get(i).map(|b| Some(b.offset)) != Some(follows) {
                break;
            }
        }
        let reference = |pos: usize| {
            let k = starts.partition_point(|&(s, _)| s <= pos) - 1;
            (starts[k].1 << 16) | (pos - starts[k].0) as u64
        };
        let parse_at = |pos: usize| {
            let mut cur = SliceCursor {
                bytes: &stream,
                pos,
            };
            inode::parse(&mut cur, block_size)
                .ok()
                .filter(|r| plausible(r, inode_count))
        };
        let anchored = |pos: usize| {
            parse_at(pos).filter(|r| {
                let next = pos + r.len;
                next == stream.len() || parse_at(next).is_some()
            })
        };
        let at_table_start = walk.blocks[first].offset == walk.start;
        let mut pos = 0;
        let mut need_anchor = !at_table_start;
        while pos < stream.len() {
            let found = if need_anchor {
                anchored(pos)
            } else {
                parse_at(pos)
            };
            match found {
                Some(record) => {
                    need_anchor = false;
                    let len = record.len;
                    scan.inodes.push(ScannedInode {
                        inode_ref: reference(pos),
                        record,
                    });
                    pos += len;
                }
                None => {
                    // Lost: look for the next anchored record.
                    let lost = pos;
                    let mut p = pos + 1;
                    while p < stream.len() && anchored(p).is_none() {
                        p += 1;
                    }
                    // Includes a run's last record when it runs into a
                    // lost block: its bytes here are all there is of it.
                    scan.skipped.push((reference(lost), p - lost));
                    need_anchor = true;
                    pos = p;
                }
            }
        }
    }
    scan
}
