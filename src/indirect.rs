//! The lookup tables -- id, fragment, export and xattr id -- read one
//! metadata block at a time, so a damaged block loses only its own
//! entries.
//!
//! Each of these tables has the same shape: an index of raw `u64`
//! pointers at the offset the superblock names, each pointing at a
//! metadata block of fixed-size entries stored before the index. The
//! strict readers ([`crate::table`], [`crate::xattr`]) concatenate every
//! block and refuse the whole table, and with it the whole image, when
//! any one fails. [`IndirectTable`] reads each block on its own and keeps
//! what decodes: an entry is present when its block decoded, and missing,
//! with the reason reported, when it did not.
//!
//! Every entry size here divides 8 KiB, so an entry never straddles two
//! blocks: entry `i` is in block `i * size / 8192`.

use crate::damage::{Damage, DamageKind, Severity};
use crate::decompress::Compressor;
use crate::metatable::{read_metablock, MetablockRead};
use crate::superblock::{Superblock, METADATA_SIZE};
use crate::table::{FragmentEntry, NO_TABLE};
use fs_core::BlockRead;
use std::collections::HashMap;
use std::sync::Arc;

/// The most decoded bytes one table read keeps.
///
/// An IMPLEMENTATION LIMIT, not a format rule. A table's size is an entry
/// count off the superblock, and the pointers in its index can all name
/// the same small block, which decodes to 8 KiB however few bytes it has
/// on disk -- so a small image can ask for gigabytes. Real tables are far
/// below this: an id table is at most 256 KiB, and a fragment or export
/// table holds 16 or 8 bytes per fragment or inode. Blocks past the limit
/// are counted in [`IndirectTable::unread_for_budget`], never reported as
/// damage and never presented as empty.
pub const DECODE_BUDGET: usize = 256 << 20;

const POINTER_SIZE: u64 = 8;

/// One block of a table's index.
#[derive(Debug)]
pub struct IndexedBlock {
    /// The pointer as stored in the index: where the block's header is.
    pub pointer: u64,
    /// The block, or `None` when the pointer is outside the image or the
    /// block was left unread for the budget.
    pub block: Option<MetablockRead>,
}

/// A lookup table, read block by block.
#[derive(Debug)]
pub struct IndirectTable {
    /// What the table is, for damage reports: `"id table"` and so on.
    pub structure: &'static str,
    /// Absolute offset of the pointer index.
    pub index_start: u64,
    /// Bytes per entry.
    pub entry_size: usize,
    /// Entries the superblock (or the xattr id table's header) declares.
    pub declared: u64,
    /// One per pointer the index has room for, in order.
    pub blocks: Vec<IndexedBlock>,
    /// Blocks the index names but [`DECODE_BUDGET`] left unread.
    pub unread_for_budget: usize,
    /// Every piece of damage found, in index order.
    pub damage: Vec<Damage>,
}

impl IndirectTable {
    fn entries_per_block(&self) -> u64 {
        (METADATA_SIZE / self.entry_size) as u64
    }

    /// The bytes of entry `index`, or `None` when it is past the declared
    /// count or its block did not decode to cover it.
    pub fn entry_bytes(&self, index: u64) -> Option<&[u8]> {
        if index >= self.declared {
            return None;
        }
        let per = self.entries_per_block();
        let slot = self.blocks.get(usize::try_from(index / per).ok()?)?;
        let bytes = slot.block.as_ref()?.bytes()?;
        let at = (index % per) as usize * self.entry_size;
        bytes.get(at..at + self.entry_size)
    }

    /// How many declared entries are missing: past the index, in a block
    /// that did not decode, or past the end of a short block.
    pub fn missing(&self) -> u64 {
        (0..self.declared)
            .filter(|&i| self.entry_bytes(i).is_none())
            .count() as u64
    }

    /// Whether every declared entry was read.
    pub fn is_clean(&self) -> bool {
        self.damage.is_empty() && self.unread_for_budget == 0
    }
}

/// How a table names its entry count and where its index is.
struct Shape {
    structure: &'static str,
    index_start: u64,
    entry_size: usize,
    declared: u64,
}

fn read_table<R: BlockRead + ?Sized>(
    dev: &R,
    sb: &Superblock,
    comp: Option<Compressor>,
    shape: Shape,
) -> IndirectTable {
    let mut table = IndirectTable {
        structure: shape.structure,
        index_start: shape.index_start,
        entry_size: shape.entry_size,
        declared: shape.declared,
        blocks: Vec::new(),
        unread_for_budget: 0,
        damage: Vec::new(),
    };
    let total = shape.declared.saturating_mul(shape.entry_size as u64);
    let wanted = total.div_ceil(METADATA_SIZE as u64);
    // The index has to fit between its start and the end of the image:
    // the rule the kernel applies to its own index tables.
    let image_end = sb.bytes_used.min(dev.size_bytes());
    let room = image_end.saturating_sub(shape.index_start) / POINTER_SIZE;
    let have = wanted.min(room);
    if have < wanted {
        table.damage.push(Damage {
            kind: DamageKind::Mismatch,
            severity: Severity::Error,
            offset: shape.index_start,
            length: room * POINTER_SIZE,
            structure: shape.structure,
            detail: format!(
                "{} entries need {wanted} index pointers, and the image has room for {room}",
                shape.declared
            ),
        });
    }
    let mut index = vec![0u8; (have * POINTER_SIZE) as usize];
    if let Err(e) = dev.read_at(shape.index_start, &mut index) {
        table.damage.push(Damage {
            kind: DamageKind::Unreadable,
            severity: Severity::Error,
            offset: shape.index_start,
            length: index.len() as u64,
            structure: shape.structure,
            detail: format!("index unreadable: {e:?}"),
        });
        return table;
    }

    let mut decoded_bytes = 0usize;
    let mut seen: HashMap<u64, usize> = HashMap::new();
    for (i, raw) in index.chunks_exact(POINTER_SIZE as usize).enumerate() {
        let pointer = u64::from_le_bytes(raw.try_into().expect("8 bytes"));
        let at = shape.index_start + i as u64 * POINTER_SIZE;
        // The blocks are stored before their index, after the superblock.
        if !(96..shape.index_start).contains(&pointer) {
            table.damage.push(Damage {
                kind: DamageKind::Malformed,
                severity: Severity::Error,
                offset: at,
                length: POINTER_SIZE,
                structure: shape.structure,
                detail: format!(
                    "index pointer {i} is {pointer:#x}, outside [0x60, {:#x})",
                    shape.index_start
                ),
            });
            table.blocks.push(IndexedBlock {
                pointer,
                block: None,
            });
            continue;
        }
        if decoded_bytes >= DECODE_BUDGET {
            table.unread_for_budget += 1;
            table.blocks.push(IndexedBlock {
                pointer,
                block: None,
            });
            continue;
        }
        // A pointer seen before shares the block already read. Two index
        // slots never name one block in a table `mksquashfs` writes, so
        // the repeat is reported; its damage, if any, was reported once.
        if let Some(&first) = seen.get(&pointer) {
            table.damage.push(Damage {
                kind: DamageKind::Mismatch,
                severity: Severity::Warning,
                offset: at,
                length: POINTER_SIZE,
                structure: shape.structure,
                detail: format!("index pointer {i} repeats pointer {first}, {pointer:#x}"),
            });
            let block = table.blocks[first].block.as_ref().and_then(shared);
            table.blocks.push(IndexedBlock { pointer, block });
            continue;
        }
        seen.insert(pointer, i);
        let block = read_metablock(dev, comp, pointer);
        if let Some(d) = block.damage(shape.structure) {
            table.damage.push(d);
        } else if let Some(bytes) = block.bytes() {
            decoded_bytes += bytes.len();
            // Every block but the last holds a full 8 KiB of entries.
            let need = (total - i as u64 * METADATA_SIZE as u64).min(METADATA_SIZE as u64);
            if (bytes.len() as u64) < need {
                let (offset, length) = block.extent();
                table.damage.push(Damage {
                    kind: DamageKind::Mismatch,
                    severity: Severity::Error,
                    offset,
                    length,
                    structure: shape.structure,
                    detail: format!(
                        "block {i} decodes to {} bytes and its entries need {need}",
                        bytes.len()
                    ),
                });
            }
        }
        table.blocks.push(IndexedBlock {
            pointer,
            block: Some(block),
        });
    }
    table
}

/// A second view of a decoded block: the same bytes, shared.
fn shared(first: &MetablockRead) -> Option<MetablockRead> {
    Some(MetablockRead {
        offset: first.offset,
        header: first.header,
        state: crate::metatable::MetablockState::Decoded(Arc::clone(first.bytes()?)),
    })
}

/// The uid/gid id table: `id_count` 4-byte ids.
pub fn id_table<R: BlockRead + ?Sized>(dev: &R, sb: &Superblock) -> IndirectTable {
    read_table(
        dev,
        sb,
        sb.compressor().ok(),
        Shape {
            structure: "id table",
            index_start: sb.id_table_start,
            entry_size: 4,
            declared: u64::from(sb.id_count),
        },
    )
}

/// One id, decoded.
pub fn id_entry(table: &IndirectTable, index: u64) -> Option<u32> {
    Some(u32::from_le_bytes(
        table.entry_bytes(index)?.try_into().ok()?,
    ))
}

/// The fragment table: `fragment_entry_count` 16-byte entries.
pub fn fragment_table<R: BlockRead + ?Sized>(dev: &R, sb: &Superblock) -> IndirectTable {
    read_table(
        dev,
        sb,
        sb.compressor().ok(),
        Shape {
            structure: "fragment table",
            index_start: sb.fragment_table_start,
            entry_size: 16,
            declared: u64::from(sb.fragment_entry_count),
        },
    )
}

/// One fragment entry, decoded.
pub fn fragment_entry(table: &IndirectTable, index: u64) -> Option<FragmentEntry> {
    let e = table.entry_bytes(index)?;
    Some(FragmentEntry {
        start: u64::from_le_bytes(e[0..8].try_into().ok()?),
        size: u32::from_le_bytes(e[8..12].try_into().ok()?),
    })
}

/// The export table: `inode_count` packed inode references, or `None`
/// when the image was built without one.
pub fn export_table<R: BlockRead + ?Sized>(dev: &R, sb: &Superblock) -> Option<IndirectTable> {
    (sb.export_table_start != NO_TABLE).then(|| {
        read_table(
            dev,
            sb,
            sb.compressor().ok(),
            Shape {
                structure: "export table",
                index_start: sb.export_table_start,
                entry_size: 8,
                declared: u64::from(sb.inode_count),
            },
        )
    })
}

/// The packed inode reference for inode number `number` (1-based).
pub fn export_entry(table: &IndirectTable, number: u32) -> Option<u64> {
    let index = u64::from(number).checked_sub(1)?;
    Some(u64::from_le_bytes(
        table.entry_bytes(index)?.try_into().ok()?,
    ))
}

/// Bytes in the xattr id table's header: the pair table's start, the id
/// count, and four unused bytes.
const XATTR_HEADER_SIZE: u64 = 16;

/// The xattr id table, or `None` when the image has no attributes. Its
/// header, before the index, gives the entry count and where the pairs
/// start; [`XattrIdTable::pairs_start`] is that, as read.
pub fn xattr_id_table<R: BlockRead + ?Sized>(dev: &R, sb: &Superblock) -> Option<XattrIdTable> {
    let header_at = sb.xattr_id_table_start;
    if header_at == NO_TABLE {
        return None;
    }
    let mut header = [0u8; XATTR_HEADER_SIZE as usize];
    if let Err(e) = dev.read_at(header_at, &mut header) {
        let mut table = read_table(
            dev,
            sb,
            None,
            Shape {
                structure: "xattr id table",
                index_start: header_at + XATTR_HEADER_SIZE,
                entry_size: 16,
                declared: 0,
            },
        );
        table.damage.push(Damage {
            kind: DamageKind::Unreadable,
            severity: Severity::Error,
            offset: header_at,
            length: XATTR_HEADER_SIZE,
            structure: "xattr id table",
            detail: format!("header unreadable: {e:?}"),
        });
        return Some(XattrIdTable {
            pairs_start: None,
            table,
        });
    }
    let pairs_start = u64::from_le_bytes(header[0..8].try_into().expect("8 bytes"));
    let count = u32::from_le_bytes(header[8..12].try_into().expect("4 bytes"));
    let mut table = read_table(
        dev,
        sb,
        sb.compressor().ok(),
        Shape {
            structure: "xattr id table",
            index_start: header_at + XATTR_HEADER_SIZE,
            entry_size: 16,
            declared: u64::from(count),
        },
    );
    if count > 0 && pairs_start >= header_at {
        table.damage.insert(
            0,
            Damage {
                kind: DamageKind::Mismatch,
                severity: Severity::Error,
                offset: header_at,
                length: 8,
                structure: "xattr id table",
                detail: format!(
                    "the pair table starts at {pairs_start:#x}, not before its own index at {header_at:#x}"
                ),
            },
        );
    }
    Some(XattrIdTable {
        pairs_start: Some(pairs_start),
        table,
    })
}

/// The xattr id table and the header in front of its index.
#[derive(Debug)]
pub struct XattrIdTable {
    /// Where the key/value pairs start, or `None` when the header could
    /// not be read.
    pub pairs_start: Option<u64>,
    /// The id entries, each 16 bytes: a packed pair reference, a count
    /// and a total size.
    pub table: IndirectTable,
}
