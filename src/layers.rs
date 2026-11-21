//! rust-fs-core's three API layers over this crate: [`SquashfsVolume`]
//! implements `VolumeAPI` and `OpenVolume`, `PrimitiveAPI` and `FileAPI`,
//! and [`SquashfsAdapter`] implements `SalvageAdapter`.
//!
//! Nothing here decodes the format. Every layer is a mapping onto the
//! crate's own APIs: [`Volume`] and the superblock's faults for the volume
//! layer, the damage-tolerant primitives and [`crate::salvage`] for the
//! salvage verbs, and the strict [`Filesystem`] for files. The SquashFS
//! primitives themselves -- metablocks, lookup tables, records, data
//! blocks -- stay this crate's own, as `docs/three-layers.md` in
//! rust-fs-core requires.
//!
//! A fragment tail is a window of a decoded fragment block that several
//! files share: `Encoding { decoded, offset, length }` says which bytes.
//!
//! An inode is a record inside a compressed metadata block, and its bytes as
//! stored are the whole block, which many inodes share. A candidate's
//! `location` is that block's stored extent. Its identity is its inode
//! reference -- the block and the offset in the decoded block, in
//! [`SquashfsDetail`] -- which [`SquashfsAdapter`] returns from
//! `SalvageAdapter::candidate_id`.

use crate::damage::{Damage as OurDamage, DamageKind as OurKind, Severity as OurSeverity};
use crate::data::{BlockSource, FileBlock};
use crate::decompress::{self, Compressor};
use crate::fs::Filesystem;
use crate::inode::{FileType, Inode, InodeRecord};
use crate::records::inode_at;
use crate::salvage::{self, ClaimSource, Salvage as OurSalvage};
use crate::salvage_read::WalkEnd as OurWalkEnd;
use crate::superblock::{Superblock, SQUASHFS_MAGIC, SUPERBLOCK_SIZE};
use crate::table;
use crate::volume::Volume;
use fs_core::layers::file::{DirEntry, FileAPI, Metadata, NodeKind, Timestamp};
use fs_core::layers::primitive::{PrimitiveAPI, Structure, StructureRole};
use fs_core::layers::salvage::{
    Candidate, CandidateKind, CandidateState, Capabilities, Damage, DamageKind, DamageReport,
    Encoding, Extent, ExtentWalk, Feature, Integrity, Parent, ParentClaim, SalvageAdapter,
    SalvageError, ScanReport, Severity, Sink, Tree, TreeNode, WalkEnd, WalkExtent,
};
use fs_core::layers::volume::{
    check_bounds, ApiError, Geometry, OpenVolume, Probe, Recognition, SuperblockCopy, VolumeAPI,
    VolumeHealth, VolumeSupport,
};
use fs_core::BlockRead;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;
use std::ops::ControlFlow;
use std::sync::Arc;

/// The format name the layers report.
pub const FORMAT: &str = "squashfs";

/// What the shared layers return when they fail.
#[derive(Debug)]
pub enum LayerError {
    /// A shared refusal: bounds, not found, not a directory.
    Api(ApiError),
    /// The device failed.
    Device(fs_core::Error),
    /// The SquashFS reader refused.
    Squashfs(crate::Error),
}

impl fmt::Display for LayerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LayerError::Api(e) => e.fmt(f),
            LayerError::Device(e) => write!(f, "device: {e}"),
            LayerError::Squashfs(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for LayerError {}

impl From<ApiError> for LayerError {
    fn from(e: ApiError) -> Self {
        LayerError::Api(e)
    }
}

impl From<fs_core::Error> for LayerError {
    fn from(e: fs_core::Error) -> Self {
        LayerError::Device(e)
    }
}

impl From<crate::Error> for LayerError {
    fn from(e: crate::Error) -> Self {
        match e {
            crate::Error::Block(e) => LayerError::Device(e),
            e => LayerError::Squashfs(e),
        }
    }
}

fn shared_kind(kind: OurKind) -> DamageKind {
    match kind {
        OurKind::Signature => DamageKind::Signature,
        OurKind::Checksum => DamageKind::Checksum,
        OurKind::Mismatch => DamageKind::Mismatch,
        OurKind::Missing => DamageKind::Missing,
        OurKind::Malformed => DamageKind::Malformed,
        OurKind::Broken => DamageKind::Broken,
        OurKind::Unreadable => DamageKind::Unreadable,
    }
}

/// This crate's damage, in the shared evidence type.
pub fn shared_damage(d: &OurDamage) -> Damage {
    Damage {
        kind: shared_kind(d.kind),
        severity: match d.severity {
            OurSeverity::Notice => Severity::Notice,
            OurSeverity::Warning => Severity::Warning,
            OurSeverity::Error => Severity::Error,
        },
        location: Extent::new(d.offset, d.length),
        structure: d.structure.to_string(),
        detail: d.detail.clone(),
    }
}

fn capabilities() -> Capabilities {
    Capabilities {
        missing: vec![
            Feature::DeletedCandidates,
            Feature::DeletedExtents,
            Feature::ClusterSteps,
            Feature::GeometryHypotheses,
        ],
    }
}

/// A SquashFS volume on the shared layers.
///
/// Opened with [`OpenVolume::open`] it is ready for files and refuses an
/// image the strict reader refuses; opened with
/// [`OpenVolume::open_for_salvage`] it needs only a readable superblock,
/// and offers files only when the strict reader accepts the image.
pub struct SquashfsVolume {
    volume: Volume<'static>,
    files: Option<Filesystem>,
}

impl SquashfsVolume {
    /// The crate's own volume layer underneath.
    pub fn volume(&self) -> &Volume<'static> {
        &self.volume
    }

    fn files(&self) -> Result<&Filesystem, LayerError> {
        self.files
            .as_ref()
            .ok_or(LayerError::Api(ApiError::Unsupported(
                "files: the strict reader refuses this image",
            )))
    }
}

impl VolumeAPI for SquashfsVolume {
    type Error = LayerError;
    type Superblock = Superblock;

    fn format(&self) -> &'static str {
        FORMAT
    }

    fn size_bytes(&self) -> u64 {
        self.volume.device_size()
    }

    fn read_volume_bytes(&self, offset: u64, output: &mut [u8]) -> Result<(), LayerError> {
        check_bounds(offset, output.len(), self.size_bytes())?;
        self.volume.device().read_at(offset, output)?;
        Ok(())
    }

    /// SquashFS has no sectors: 512 is reported as the read granularity,
    /// and the allocation unit is the data block size, as the shared
    /// documentation asks of an image format. It has no linear numbering
    /// of allocation units, so no layout.
    fn geometry(&self) -> Result<Geometry, LayerError> {
        let sb = self.volume.superblock();
        let unit = if sb.header_faults().is_empty() {
            u64::from(sb.block_size)
        } else {
            0
        };
        Ok(Geometry {
            sector_size: 512,
            allocation_unit: unit,
            allocation_units: if unit == 0 {
                0
            } else {
                sb.bytes_used.div_ceil(unit)
            },
            declared_bytes: sb.bytes_used,
            layout: None,
            label: None,
            serial: None,
        })
    }

    fn superblock(&self) -> Result<Superblock, LayerError> {
        Ok(self.volume.superblock().clone())
    }

    /// SquashFS keeps one superblock, and no copy.
    fn superblocks(&self) -> Result<Vec<SuperblockCopy<Superblock>>, LayerError> {
        let faults = self.volume.superblock_faults();
        Ok(vec![SuperblockCopy {
            copy: 0,
            location: Extent::new(0, SUPERBLOCK_SIZE as u64),
            superblock: Some(self.volume.superblock().clone()),
            integrity: if faults.is_empty() {
                Integrity::Verified
            } else {
                Integrity::Failed
            },
        }])
    }

    fn support(&self) -> Result<VolumeSupport, LayerError> {
        Ok(VolumeSupport {
            files: self.files.is_some(),
            salvage: capabilities(),
            unsupported: Vec::new(),
        })
    }

    /// The superblock's faults and the lookup tables' damage: what the
    /// strict open reads before it lists anything. The metadata tables
    /// are the salvage audit's, not the quick verdict's.
    fn health(&self) -> Result<VolumeHealth, LayerError> {
        let v = &self.volume;
        let mut findings: Vec<Damage> = v
            .superblock_faults()
            .iter()
            .map(|f| shared_damage(&f.damage()))
            .collect();
        if v.superblock().header_faults().is_empty() {
            let mut tables = vec![v.id_table(), v.fragment_table()];
            tables.extend(v.export_table());
            tables.extend(v.xattr_id_table().map(|x| x.table));
            for t in tables {
                findings.extend(t.damage.iter().map(shared_damage));
            }
        }
        Ok(VolumeHealth::from_findings(findings))
    }
}

impl OpenVolume for SquashfsVolume {
    type Source = Arc<dyn BlockRead>;

    fn probe(source: &dyn BlockRead) -> Result<Probe, fs_core::Error> {
        if source.size_bytes() < SUPERBLOCK_SIZE as u64 {
            return Ok(Probe {
                format: FORMAT,
                recognition: Recognition::No,
                declared_bytes: None,
                detail: "shorter than a superblock".into(),
            });
        }
        let mut bytes = [0u8; SUPERBLOCK_SIZE];
        source.read_at(0, &mut bytes)?;
        let sb = Superblock::decode(&bytes);
        if sb.magic != SQUASHFS_MAGIC {
            return Ok(Probe {
                format: FORMAT,
                recognition: Recognition::No,
                declared_bytes: None,
                detail: "no SquashFS magic".into(),
            });
        }
        let faults = sb.faults(source.size_bytes());
        Ok(Probe {
            format: FORMAT,
            recognition: if faults.is_empty() {
                Recognition::Yes
            } else {
                Recognition::Damaged
            },
            declared_bytes: Some(sb.bytes_used),
            detail: match faults.first() {
                None => format!("SquashFS {}.{}", sb.version_major, sb.version_minor),
                Some(f) => format!(
                    "SquashFS magic, with {} superblock fault(s): {}: {}",
                    faults.len(),
                    f.field,
                    f.reason
                ),
            },
        })
    }

    fn open(source: Arc<dyn BlockRead>) -> Result<Self, LayerError> {
        let files = Filesystem::open(source.clone())?;
        Ok(SquashfsVolume {
            volume: Volume::open(source)?,
            files: Some(files),
        })
    }

    fn open_for_salvage(source: Arc<dyn BlockRead>) -> Result<Self, LayerError> {
        let volume = Volume::open(source)?;
        let files = volume.filesystem().ok();
        Ok(SquashfsVolume { volume, files })
    }
}

impl PrimitiveAPI for SquashfsVolume {
    type Adapter = SquashfsAdapter;
    type Hypothesis = ();

    fn source(&self) -> &dyn BlockRead {
        self.volume.device()
    }

    /// The adapter from the superblock, when its header -- magic, version
    /// and block size -- holds; otherwise there is no geometry to salvage
    /// with.
    fn adapter(&self) -> Result<SquashfsAdapter, SalvageError> {
        let sb = self.volume.superblock();
        if sb.header_faults().is_empty() {
            Ok(SquashfsAdapter { sb: sb.clone() })
        } else {
            Err(SalvageError::NoGeometry)
        }
    }

    fn structures(&self) -> Result<Vec<Structure>, SalvageError> {
        let v = &self.volume;
        let sb = v.superblock();
        let integrity = |clean: bool| {
            if clean {
                Integrity::Verified
            } else {
                Integrity::Failed
            }
        };
        let mut out = vec![Structure {
            name: "superblock".into(),
            role: StructureRole::Superblock,
            location: Extent::new(0, SUPERBLOCK_SIZE as u64),
            copy: 0,
            integrity: integrity(v.superblock_faults().is_empty()),
        }];
        if !sb.header_faults().is_empty() {
            return Ok(out);
        }
        let size = v.device_size();
        let within = |start: u64, end: u64| {
            let end = end.min(size);
            (start < end).then(|| Extent::new(start, end - start))
        };
        let inodes = v.inode_table();
        if let Some(location) = within(inodes.start, inodes.end) {
            out.push(Structure {
                name: "inode table".into(),
                role: StructureRole::RecordTable,
                location,
                copy: 0,
                integrity: integrity(inodes.is_clean()),
            });
        }
        let dirs = v.directory_table();
        if let Some(location) = within(dirs.start, dirs.end) {
            out.push(Structure {
                name: "directory table".into(),
                role: StructureRole::Directory,
                location,
                copy: 0,
                integrity: integrity(dirs.is_clean()),
            });
        }
        let mut tables = vec![v.id_table(), v.fragment_table()];
        tables.extend(v.export_table());
        tables.extend(v.xattr_id_table().map(|x| x.table));
        for t in tables {
            let pointers = t.blocks.len() as u64 * 8;
            if let Some(location) = within(t.index_start, t.index_start.saturating_add(pointers)) {
                out.push(Structure {
                    name: format!("{} index", t.structure),
                    role: StructureRole::AddressMap,
                    location,
                    copy: 0,
                    integrity: integrity(t.is_clean()),
                });
            }
        }
        Ok(out)
    }
}

/// A file or directory, by its packed inode reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Node(pub u64);

fn node_kind(t: FileType) -> NodeKind {
    match t {
        FileType::Dir => NodeKind::Directory,
        FileType::RegFile => NodeKind::File,
        FileType::Symlink => NodeKind::Symlink,
        _ => NodeKind::Special,
    }
}

fn metadata_of(inode: &Inode) -> Metadata {
    let kind = node_kind(inode.file_type());
    let mut m = Metadata::new(
        kind,
        match kind {
            NodeKind::Directory => 0,
            _ => inode.file_size,
        },
    );
    m.modified = Some(Timestamp {
        seconds: i64::from(inode.mtime),
        nanoseconds: 0,
    });
    m.mode = Some(u32::from(inode.permissions));
    m.links = Some(u64::from(inode.nlink));
    m
}

impl FileAPI for SquashfsVolume {
    type Node = Node;

    fn root(&self) -> Result<Node, LayerError> {
        Ok(Node(self.files()?.sb.root_inode_ref))
    }

    fn read_dir(&self, directory: &Node) -> Result<Vec<DirEntry<Node>>, LayerError> {
        let fs = self.files()?;
        let inode = fs.read_inode(directory.0)?;
        if !inode.is_dir() {
            return Err(ApiError::NotADirectory(format!("inode {}", inode.inode_number)).into());
        }
        fs.read_dir(&inode)?
            .into_iter()
            .map(|e| {
                let child = fs.read_inode(e.inode_ref)?;
                let (name, lossy) = match String::from_utf8(e.name.clone()) {
                    Ok(name) => (name, false),
                    Err(_) => (String::from_utf8_lossy(&e.name).into_owned(), true),
                };
                let meta = metadata_of(&child);
                Ok(DirEntry {
                    name,
                    bytes: e.name,
                    lossy,
                    kind: meta.kind,
                    size: meta.size,
                    node: Node(e.inode_ref),
                })
            })
            .collect()
    }

    fn metadata(&self, node: &Node) -> Result<Metadata, LayerError> {
        Ok(metadata_of(&self.files()?.read_inode(node.0)?))
    }

    fn read_at(&self, file: &Node, offset: u64, output: &mut [u8]) -> Result<usize, LayerError> {
        let fs = self.files()?;
        let inode = fs.read_inode(file.0)?;
        match inode.file_type() {
            FileType::RegFile => Ok(fs.read_file(&inode, offset, output)?),
            FileType::Dir => {
                Err(ApiError::IsADirectory(format!("inode {}", inode.inode_number)).into())
            }
            _ => Err(ApiError::Unsupported("reading a symlink or special file as data").into()),
        }
    }

    fn read_link(&self, link: &Node) -> Result<Vec<u8>, LayerError> {
        let fs = self.files()?;
        Ok(fs.read_symlink_target(&fs.read_inode(link.0)?)?)
    }
}

/// What a SquashFS candidate carries beyond the shared fields: enough to
/// find its inode again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SquashfsDetail {
    /// The packed inode reference: block offset from the inode table's
    /// start in the high 48 bits, offset in the decoded block in the low 16.
    pub inode_ref: u64,
    /// The inode number the record holds.
    pub inode_number: u32,
    /// The on-disk inode type.
    pub inode_type: u16,
    /// A directory's record of its parent's inode number.
    pub parent: Option<u32>,
    /// Found by a scan of the decoded inode table.
    pub by_scan: bool,
    /// Found through the export table.
    pub by_export: bool,
    /// Found through a directory listing, or as the root.
    pub by_directory: bool,
}

/// The SquashFS salvage adapter: the superblock it salvages under, and
/// nothing else. Every verb reads only the source it is lent.
#[derive(Clone, Debug)]
pub struct SquashfsAdapter {
    sb: Superblock,
}

impl SquashfsAdapter {
    /// An adapter salvaging under `sb`.
    pub fn new(sb: Superblock) -> Self {
        SquashfsAdapter { sb }
    }

    fn volume<'d>(&self, source: &'d dyn BlockRead) -> Volume<'d> {
        Volume::borrowed(source, self.sb.clone())
    }

    fn record(&self, salvage: &OurSalvage<'_>, detail: &SquashfsDetail) -> Option<InodeRecord> {
        inode_at(salvage.inode_table(), detail.inode_ref, self.sb.block_size).ok()
    }

    fn ours(
        &self,
        salvage: &OurSalvage<'_>,
        detail: &SquashfsDetail,
    ) -> Option<salvage::Candidate> {
        Some(salvage::Candidate {
            inode_ref: detail.inode_ref,
            record: self.record(salvage, detail)?,
            found_by: salvage::FoundBy {
                scan: detail.by_scan,
                export: detail.by_export,
                directory: detail.by_directory,
            },
        })
    }

    /// Every candidate as the shared type, in location order, with the
    /// stored extent of each one's block.
    fn shared_candidates(&self, salvage: &OurSalvage<'_>) -> Vec<Candidate<SquashfsDetail>> {
        let scan = salvage.find_candidates();
        let tree = salvage.rebuild_tree(&scan.candidates);
        let walk = salvage.inode_table();
        let mut out: Vec<(u64, Candidate<SquashfsDetail>)> = scan
            .candidates
            .iter()
            .filter_map(|c| {
                let block = walk.block_at(c.inode_ref >> 16)?;
                let (offset, length) = block.extent();
                let inode = &c.record.inode;
                let name = tree.nodes.get(&c.inode_ref).and_then(|n| {
                    n.claims
                        .iter()
                        .find(|claim| claim.source == ClaimSource::Listing)
                        .and_then(|claim| claim.name.as_ref())
                        .map(|n| String::from_utf8_lossy(n).into_owned())
                });
                let kind = match inode.file_type() {
                    FileType::Dir => CandidateKind::Directory,
                    FileType::RegFile => CandidateKind::File,
                    _ => CandidateKind::Other,
                };
                Some((
                    c.inode_ref,
                    Candidate {
                        location: Extent::new(offset, length),
                        kind,
                        state: CandidateState::Live,
                        integrity: if block.compressed() {
                            Integrity::Verified
                        } else {
                            Integrity::Unchecked
                        },
                        name,
                        size: (kind != CandidateKind::Directory).then_some(inode.file_size),
                        detail: SquashfsDetail {
                            inode_ref: c.inode_ref,
                            inode_number: inode.inode_number,
                            inode_type: inode.inode_type,
                            parent: c.record.parent,
                            by_scan: c.found_by.scan,
                            by_export: c.found_by.export,
                            by_directory: c.found_by.directory,
                        },
                    },
                ))
            })
            .collect();
        out.sort_by_key(|(r, c)| (c.location.offset, r & 0xFFFF));
        out.into_iter().map(|(_, c)| c).collect()
    }
}

fn in_source(source: &dyn BlockRead, extent: Extent) -> Result<(), SalvageError> {
    match extent.end() {
        Some(end) if end <= source.size_bytes() => Ok(()),
        _ => Err(SalvageError::Bounds),
    }
}

/// The compressor a codec token names: the image's own compressor names,
/// `gzip`, `xz`, `lz4`, `zstd`, `lzo`, `lzma`.
fn codec(name: &str) -> Option<Compressor> {
    [
        Compressor::Gzip,
        Compressor::Lzma,
        Compressor::Lzo,
        Compressor::Xz,
        Compressor::Lz4,
        Compressor::Zstd,
    ]
    .into_iter()
    .find(|c| c.name() == name)
}

impl SalvageAdapter for SquashfsAdapter {
    type Detail = SquashfsDetail;

    fn capabilities(&self) -> Capabilities {
        capabilities()
    }

    fn candidate_id(&self, candidate: &Candidate<SquashfsDetail>) -> u64 {
        candidate.detail.inode_ref
    }

    fn find_candidates(
        &self,
        source: &dyn BlockRead,
        range: Extent,
        sink: &mut dyn Sink<Candidate<SquashfsDetail>>,
    ) -> Result<ScanReport, SalvageError> {
        in_source(source, range)?;
        let end = range.end().ok_or(SalvageError::Bounds)?;
        let volume = self.volume(source);
        let salvage = OurSalvage::new(&volume);
        let inside = |offset: u64| offset >= range.offset && offset < end;
        let mut report = ScanReport::default();
        // An unreadable block's bytes run to the next block found, or to
        // the table's end: with its header unread, nothing says where it
        // stops.
        let walk = salvage.inode_table();
        report.unreadable = walk
            .blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| matches!(b.state, crate::metatable::MetablockState::Unreadable(_)))
            .map(|(i, b)| {
                let next = walk.blocks.get(i + 1).map_or(walk.end, |n| n.offset);
                let (offset, length) = b.extent();
                Extent::new(offset, length.max(next.saturating_sub(offset)))
            })
            .map(|e| {
                // Clipped to the scan.
                let start = e.offset.max(range.offset);
                let stop = e.end().unwrap_or(u64::MAX).min(end);
                Extent::new(start, stop.saturating_sub(start))
            })
            .filter(|e| e.length > 0)
            .collect();
        let candidates: Vec<_> = self
            .shared_candidates(&salvage)
            .into_iter()
            .filter(|c| inside(c.location.offset))
            .collect();
        for (i, candidate) in candidates.iter().enumerate() {
            report.candidates += 1;
            if let ControlFlow::Break(()) = sink.accept(candidate.clone()) {
                // The first byte not yet scanned. When more of this block's
                // records follow, that is the block's first byte, and a scan
                // resumed there delivers them again with the ones already
                // delivered, which `candidate_id` tells apart. Otherwise it
                // is past the block.
                let more_in_block = candidates
                    .get(i + 1)
                    .is_some_and(|n| n.location.offset == candidate.location.offset);
                report.stopped_at = if more_in_block {
                    Some(candidate.location.offset)
                } else {
                    candidate.location.end()
                };
                break;
            }
        }
        Ok(report)
    }

    fn walk_extents(
        &self,
        source: &dyn BlockRead,
        candidate: &Candidate<SquashfsDetail>,
    ) -> Result<ExtentWalk, SalvageError> {
        if candidate.state == CandidateState::Deleted {
            // Nothing in SquashFS is deleted, so nothing deleted has storage.
            return Err(SalvageError::Unsupported(Feature::DeletedExtents));
        }
        let volume = self.volume(source);
        let salvage = OurSalvage::new(&volume);
        let Some(ours) = self.ours(&salvage, &candidate.detail) else {
            return Ok(ExtentWalk {
                extents: Vec::new(),
                claimed: candidate.size.unwrap_or(0),
                end: WalkEnd::Unreadable(candidate.location),
            });
        };
        let inode = &ours.record.inode;
        if !inode.is_regular_file() {
            return Ok(ExtentWalk {
                extents: Vec::new(),
                claimed: 0,
                end: WalkEnd::Complete,
            });
        }
        let walk = salvage.walk_extents(&ours);
        let comp = volume.compressor();
        let mut extents = Vec::new();
        for (block, extent) in walk.blocks.iter().zip(&walk.extents) {
            let Some(e) = extent else { continue }; // sparse: a hole
            let size_word = match block.source {
                BlockSource::Stored { size_word, .. } => size_word,
                BlockSource::Fragment { index, .. } => {
                    crate::indirect::fragment_entry(salvage.fragment_table(), u64::from(index))
                        .map_or(0, |f| f.size)
                }
                BlockSource::Sparse => continue,
            };
            let compressed = table::data_is_compressed(size_word);
            // A full block decodes to exactly its length. A fragment
            // block's decoded size is known only by decoding it; when it
            // does not decode, the block size bounds it, and reading the
            // run then fails rather than guessing.
            let decoded = match block.source {
                BlockSource::Fragment { .. } if compressed => comp
                    .and_then(|c| {
                        let mut raw = vec![0u8; e.length as usize];
                        source.read_at(e.offset, &mut raw).ok()?;
                        decompress::decompress(c, &raw, self.sb.block_size as usize).ok()
                    })
                    .map_or(u64::from(self.sb.block_size), |d| d.len() as u64),
                _ => block.len,
            };
            let next = walk_extent(
                block,
                Extent::new(e.offset, e.length),
                compressed,
                decoded,
                comp,
            );
            // Plain extents that follow each other in the file and on the
            // source are one run.
            if let Some(last) = extents.last_mut() {
                let last: &mut WalkExtent = last;
                if last.encoded.is_none()
                    && next.encoded.is_none()
                    && !last.uninitialized
                    && last.end() == Some(next.offset)
                    && last.source.end() == Some(next.source.offset)
                {
                    last.source.length += next.source.length;
                    continue;
                }
            }
            extents.push(next);
        }
        Ok(ExtentWalk {
            extents,
            claimed: inode.file_size,
            end: match walk.end {
                OurWalkEnd::Complete => WalkEnd::Complete,
                OurWalkEnd::OutOfRange(_) => WalkEnd::OutOfRange,
                OurWalkEnd::Broken(_) => WalkEnd::Broken,
            },
        })
    }

    fn rebuild_tree(
        &self,
        source: &dyn BlockRead,
        candidates: &[Candidate<SquashfsDetail>],
    ) -> Result<Tree, SalvageError> {
        let volume = self.volume(source);
        let salvage = OurSalvage::new(&volume);
        let ours: Vec<salvage::Candidate> = candidates
            .iter()
            .filter_map(|c| self.ours(&salvage, &c.detail))
            .collect();
        let tree = salvage.rebuild_tree(&ours);
        let known: BTreeSet<u64> = candidates.iter().map(|c| c.detail.inode_ref).collect();
        let nodes = candidates
            .iter()
            .map(|c| TreeNode {
                candidate: c.detail.inode_ref,
                parents: tree
                    .nodes
                    .get(&c.detail.inode_ref)
                    .map(|n| {
                        n.claims
                            .iter()
                            .filter_map(|claim| {
                                let parent = match claim.parent {
                                    salvage::Parent::Root => Parent::Root,
                                    salvage::Parent::Candidate(r) => {
                                        Parent::Candidate(*known.get(&r)?)
                                    }
                                    salvage::Parent::Lost(_) => return None,
                                };
                                Some(ParentClaim {
                                    parent,
                                    name: claim.name.clone(),
                                    state: CandidateState::Live,
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            })
            .collect();
        Ok(Tree { nodes })
    }

    fn report_damage(
        &self,
        source: &dyn BlockRead,
        sink: &mut dyn Sink<Damage>,
    ) -> Result<DamageReport, SalvageError> {
        let volume = self.volume(source);
        let salvage = OurSalvage::new(&volume);
        let scan = salvage.find_candidates();
        let audit = salvage.report_damage(&scan.candidates, true);
        let mut report = DamageReport::default();
        for d in &audit.damage {
            report.reported += 1;
            if let ControlFlow::Break(()) = sink.accept(shared_damage(d)) {
                report.stopped = true;
                break;
            }
        }
        Ok(report)
    }

    fn read_extent(
        &self,
        source: &dyn BlockRead,
        extent: Extent,
        offset: u64,
        output: &mut [u8],
    ) -> Result<usize, SalvageError> {
        in_source(source, extent)?;
        if offset > extent.length {
            return Err(SalvageError::Bounds);
        }
        let n = output
            .len()
            .min(usize::try_from(extent.length - offset).unwrap_or(usize::MAX));
        source
            .read_at(extent.offset + offset, &mut output[..n])
            .map_err(SalvageError::Device)?;
        Ok(n)
    }

    /// Decodes the whole stored block -- a data block, or a fragment block
    /// that several files' tails share -- and reads the run's window of it.
    fn read_encoded(
        &self,
        source: &dyn BlockRead,
        stored: Extent,
        encoding: &Encoding,
        offset: u64,
        output: &mut [u8],
    ) -> Result<usize, SalvageError> {
        in_source(source, stored)?;
        if offset > encoding.length {
            return Err(SalvageError::Bounds);
        }
        let comp = codec(&encoding.codec).ok_or_else(|| {
            SalvageError::Device(fs_core::Error::Custom(format!(
                "unknown codec {:?}",
                encoding.codec
            )))
        })?;
        let mut raw = vec![0u8; usize::try_from(stored.length).map_err(|_| SalvageError::Bounds)?];
        source
            .read_at(stored.offset, &mut raw)
            .map_err(SalvageError::Device)?;
        let decoded =
            decompress::decompress(comp, &raw, self.sb.block_size as usize).map_err(|e| {
                SalvageError::Device(fs_core::Error::Custom(format!("does not decode: {e}")))
            })?;
        let start = encoding
            .offset
            .checked_add(offset)
            .ok_or(SalvageError::Bounds)?;
        let end = encoding
            .offset
            .checked_add(encoding.length)
            .ok_or(SalvageError::Bounds)?;
        if end > decoded.len() as u64 {
            // The run's window is past what the block decodes to: those
            // bytes do not exist, and are never guessed.
            return Err(SalvageError::Device(fs_core::Error::Custom(format!(
                "decodes to {} bytes, short of the run's end at {end}",
                decoded.len()
            ))));
        }
        let n = output
            .len()
            .min(usize::try_from(end - start).unwrap_or(usize::MAX));
        output[..n].copy_from_slice(&decoded[start as usize..start as usize + n]);
        Ok(n)
    }
}

/// One stored block of a file as a shared walk extent: plain for a block
/// stored uncompressed, encoded for a compressed one. A tail is a window of
/// its fragment block: plain bytes inside an uncompressed one, and
/// `decoded[offset_in_fragment ..]` of a compressed one, which several
/// files' tails share. `decoded` is what the stored block decodes to in
/// all.
fn walk_extent(
    block: &FileBlock,
    stored: Extent,
    compressed: bool,
    decoded: u64,
    comp: Option<Compressor>,
) -> WalkExtent {
    let skip = match block.source {
        BlockSource::Fragment {
            offset_in_fragment, ..
        } => u64::from(offset_in_fragment),
        _ => 0,
    };
    if !compressed {
        return WalkExtent::stored(
            block.file_offset,
            Extent::new(stored.offset.saturating_add(skip), block.len),
        );
    }
    WalkExtent {
        offset: block.file_offset,
        source: stored,
        uninitialized: false,
        encoded: Some(Encoding {
            codec: comp.map_or("unknown", |c| c.name()).to_string(),
            decoded,
            offset: skip,
            length: block.len,
        }),
    }
}
