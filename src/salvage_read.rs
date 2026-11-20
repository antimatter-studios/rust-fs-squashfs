//! The salvage verbs that read: walk extents, report damage, read by
//! extent, and materialize a file from what survives.
//!
//! SquashFS stores a file's blocks compressed, so an extent names bytes
//! as stored -- a compressed stream -- not file content. Materializing
//! therefore decodes each block and checks its length; it does not copy
//! extents. A block that cannot be decoded is **missing**: the output
//! report says so at its exact range, and the bytes written for it, if
//! any, are only what the caller's [`MissingPolicy`] asked for.

use crate::damage::{Damage, DamageKind, Severity};
use crate::data::{BlockContent, BlockSource, FileBlock};
use crate::error::Result;
use crate::indirect;
use crate::salvage::{Candidate, Salvage};
use crate::superblock::SuperblockFault;
use std::io::{self, Write};

/// A run of source bytes in the image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Extent {
    /// Absolute offset in the image.
    pub offset: u64,
    /// Length in bytes.
    pub length: u64,
}

/// Why a walk of a file's storage ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalkEnd {
    /// Every block's storage was named and lies inside the image.
    Complete,
    /// A block's stored bytes reach past the end of the image.
    OutOfRange(Extent),
    /// The tail names a fragment that is not in the fragment table.
    Broken(u32),
}

/// A file's storage, as its inode and the fragment table claim it.
#[derive(Debug, Clone)]
pub struct ExtentWalk {
    /// The blocks walked, in file order, up to where the walk ended.
    pub blocks: Vec<FileBlock>,
    /// The source extent of each walked block that has one (sparse blocks
    /// have none), in the same order as `blocks`.
    pub extents: Vec<Option<Extent>>,
    /// Why the walk ended.
    pub end: WalkEnd,
}

impl ExtentWalk {
    /// File bytes the walked blocks account for.
    pub fn mapped(&self) -> u64 {
        self.blocks.iter().map(|b| b.len).sum()
    }
}

/// Everything wrong that salvage observed.
#[derive(Debug, Clone, Default)]
pub struct DamageReport {
    /// Every piece of damage, by offset.
    pub damage: Vec<Damage>,
    /// Superblock faults, which also appear in `damage`.
    pub superblock: Vec<SuperblockFault>,
    /// Files whose every data block was decoded, when data was checked.
    pub files_checked: u64,
    /// Data and fragment blocks decoded, when data was checked.
    pub blocks_checked: u64,
}

/// What to write for a range whose bytes are unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissingPolicy {
    /// Stop the output at the first missing range: what is written is a
    /// correct prefix of the file.
    Stop,
    /// Write zeros for a missing range so later ranges keep their place.
    /// The report names every such range; the zeros are not the file's.
    Zeros,
}

/// Where one range of a materialized file came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RangeSource {
    /// A data block of its own, decoded.
    Block {
        /// Its stored bytes.
        extent: Extent,
        /// Whether a compressed stream vouched for the bytes.
        checked: bool,
    },
    /// The tail, out of a decoded fragment block.
    Fragment {
        /// The fragment block's stored bytes.
        extent: Extent,
        /// Where the tail starts in the decoded fragment block.
        offset_in_fragment: u32,
        /// Whether a compressed stream vouched for the bytes.
        checked: bool,
    },
    /// A symlink's target, from its inode.
    Literal,
    /// Zeros, by the format's own record of a sparse block.
    Sparse,
    /// Unknown bytes, and why.
    Missing(Damage),
}

/// One range of a materialized file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializedRange {
    /// Where it starts in the file.
    pub file_offset: u64,
    /// How long it is.
    pub length: u64,
    /// Where its bytes came from.
    pub source: RangeSource,
    /// Whether bytes were written for it.
    pub written: bool,
}

/// The source of every byte of a materialized file.
#[derive(Debug, Clone, Default)]
pub struct MaterializeReport {
    /// Every range, in file order, written or not.
    pub ranges: Vec<MaterializedRange>,
    /// The file's length, as its inode records it.
    pub length: u64,
    /// Bytes written to the sink.
    pub written: u64,
}

impl MaterializeReport {
    /// Whether every byte written is the file's: no missing range.
    pub fn complete(&self) -> bool {
        self.missing().next().is_none()
    }

    /// The ranges whose bytes are unknown.
    pub fn missing(&self) -> impl Iterator<Item = &MaterializedRange> {
        self.ranges
            .iter()
            .filter(|r| matches!(r.source, RangeSource::Missing(_)))
    }
}

fn extent_of(block: &FileBlock, salvage: &Salvage<'_>) -> Option<Extent> {
    match block.source {
        BlockSource::Stored { .. } => block
            .source
            .extent()
            .map(|(offset, length)| Extent { offset, length }),
        BlockSource::Fragment { index, .. } => {
            indirect::fragment_entry(salvage.fragment_table(), u64::from(index)).map(|e| Extent {
                offset: e.start,
                length: u64::from(crate::table::data_on_disk_size(e.size)),
            })
        }
        BlockSource::Sparse => None,
    }
}

impl Salvage<'_> {
    fn image_end(&self) -> u64 {
        let v = self.volume();
        v.superblock().bytes_used.min(v.device_size())
    }

    /// Walk a file candidate's storage: each block's stored extent, in
    /// order, ending at a block stored past the end of the image or a
    /// tail whose fragment is not in the table. Reads nothing but the
    /// tables already read; whether a block decodes is
    /// [`Salvage::materialize`]'s and [`Salvage::report_damage`]'s
    /// question. Any other candidate has no storage, and walks complete
    /// and empty.
    pub fn walk_extents(&self, candidate: &Candidate) -> ExtentWalk {
        let mut walk = ExtentWalk {
            blocks: Vec::new(),
            extents: Vec::new(),
            end: WalkEnd::Complete,
        };
        let end = self.image_end();
        for block in self.volume().file_blocks(&candidate.record.inode) {
            let extent = extent_of(&block, self);
            if let (BlockSource::Fragment { index, .. }, None) = (block.source, extent) {
                walk.end = WalkEnd::Broken(index);
                break;
            }
            if let Some(e) = extent {
                if e.offset.saturating_add(e.length) > end {
                    walk.end = WalkEnd::OutOfRange(e);
                    break;
                }
            }
            walk.blocks.push(block);
            walk.extents.push(extent);
        }
        walk
    }

    /// Read source bytes inside `extent`, from `offset` into it: as many
    /// as fit in `buf` and the extent, and as the device holds.
    pub fn read_extent(&self, extent: Extent, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if offset >= extent.length {
            return Ok(0);
        }
        let room = usize::try_from(extent.length - offset).unwrap_or(usize::MAX);
        let n = buf.len().min(room);
        self.volume()
            .read_raw(extent.offset.saturating_add(offset), &mut buf[..n])
    }

    /// Every piece of damage salvage observed in the metadata: the
    /// superblock, both metadata tables, the lookup tables, inode table
    /// the candidate scan passed over, references to lost inodes and
    /// listings cut short. With `check_data`, every data and fragment
    /// block of every file candidate is decoded too, which reads the
    /// whole image.
    pub fn report_damage(&self, candidates: &[Candidate], check_data: bool) -> DamageReport {
        let volume = self.volume();
        let mut report = DamageReport {
            superblock: volume.superblock_faults(),
            ..DamageReport::default()
        };
        let mut damage: Vec<Damage> = report
            .superblock
            .iter()
            .map(SuperblockFault::damage)
            .collect();
        damage.extend(self.inode_table().damage());
        damage.extend(self.directory_table().damage());
        damage.extend(volume.id_table().damage);
        damage.extend(self.fragment_table().damage.iter().cloned());
        if let Some(t) = volume.export_table() {
            damage.extend(t.damage);
        }
        if let Some(t) = volume.xattr_id_table() {
            damage.extend(t.table.damage);
        }
        let scan = self.find_candidates();
        for (r, bytes) in &scan.skipped {
            let block = self.inode_table().block_at(r >> 16);
            let (offset, length) = block.map_or((self.inode_table().start, 0), |b| b.extent());
            damage.push(Damage {
                kind: DamageKind::Broken,
                severity: Severity::Warning,
                offset,
                length,
                structure: "inode table",
                detail: format!(
                    "{bytes} decoded bytes from +{} hold no inode the scan could decode",
                    r & 0xFFFF
                ),
            });
        }
        let mut lost = std::collections::BTreeSet::new();
        for (r, fault) in &scan.unresolved {
            if lost.insert(*r) {
                let mut d = fault.damage("inode table");
                d.detail = format!("inode {r:#x}, named by the image, is lost: {}", d.detail);
                damage.push(d);
            }
        }
        damage.extend(self.listing_damage(candidates));
        if check_data {
            for c in candidates
                .iter()
                .filter(|c| c.record.inode.is_regular_file())
            {
                let mut clean = true;
                for block in volume.file_blocks(&c.record.inode) {
                    if matches!(block.source, BlockSource::Sparse) {
                        continue;
                    }
                    report.blocks_checked += 1;
                    if let BlockContent::Missing(d) =
                        volume.read_file_block(self.fragment_table(), &block)
                    {
                        clean = false;
                        damage.push(d);
                    }
                }
                if clean {
                    report.files_checked += 1;
                }
            }
        }
        damage
            .sort_by(|a, b| (a.offset, a.length, &a.detail).cmp(&(b.offset, b.length, &b.detail)));
        damage.dedup();
        report.damage = damage;
        report
    }

    /// Write a regular file or symlink candidate to `sink` from what
    /// survives, and report the source of every range.
    ///
    /// Each block is decoded and checked against its length. A missing
    /// block is never written as if it were the file's: under
    /// [`MissingPolicy::Stop`] the output ends before it, and under
    /// [`MissingPolicy::Zeros`] zeros keep later ranges in place and the
    /// report names the range. Only a sparse block is zeros by the
    /// format's own record. The source is only read; the sink must not be
    /// the device the image is on.
    pub fn materialize(
        &self,
        candidate: &Candidate,
        sink: &mut dyn Write,
        policy: MissingPolicy,
    ) -> io::Result<MaterializeReport> {
        let inode = &candidate.record.inode;
        let mut report = MaterializeReport {
            length: inode.file_size,
            ..MaterializeReport::default()
        };
        if inode.is_symlink() {
            sink.write_all(&inode.symlink_target)?;
            report.written = inode.symlink_target.len() as u64;
            report.ranges.push(MaterializedRange {
                file_offset: 0,
                length: report.written,
                source: RangeSource::Literal,
                written: true,
            });
            return Ok(report);
        }
        let volume = self.volume();
        let mut stopped = false;
        for block in volume.file_blocks(inode) {
            let content = volume.read_file_block(self.fragment_table(), &block);
            let extent = extent_of(&block, self);
            let (source, bytes): (RangeSource, Option<std::borrow::Cow<'_, [u8]>>) = match content {
                BlockContent::Bytes { bytes, checked } => {
                    let source = match (block.source, extent) {
                        (
                            BlockSource::Fragment {
                                offset_in_fragment, ..
                            },
                            Some(extent),
                        ) => RangeSource::Fragment {
                            extent,
                            offset_in_fragment,
                            checked,
                        },
                        (_, Some(extent)) => RangeSource::Block { extent, checked },
                        (_, None) => unreachable!("decoded bytes come from a stored extent"),
                    };
                    (source, Some(std::borrow::Cow::Owned(bytes.to_vec())))
                }
                BlockContent::Sparse(n) => (
                    RangeSource::Sparse,
                    Some(std::borrow::Cow::Owned(vec![0u8; n as usize])),
                ),
                BlockContent::Missing(d) => {
                    let fill = match policy {
                        MissingPolicy::Zeros if !stopped => {
                            Some(std::borrow::Cow::Owned(vec![0u8; block.len as usize]))
                        }
                        _ => None,
                    };
                    if policy == MissingPolicy::Stop {
                        stopped = true;
                    }
                    (RangeSource::Missing(d), fill)
                }
            };
            let written = match bytes {
                Some(b) if !stopped => {
                    sink.write_all(&b)?;
                    report.written += b.len() as u64;
                    true
                }
                _ => false,
            };
            report.ranges.push(MaterializedRange {
                file_offset: block.file_offset,
                length: block.len,
                source,
                written,
            });
        }
        Ok(report)
    }
}
