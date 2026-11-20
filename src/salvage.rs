//! Salvage: the family's recovery verbs, in SquashFS terms, over the
//! damage-tolerant primitives.
//!
//! The verbs are the convention's (rust-fs-exfat `docs/convention.md`,
//! section 4): find candidates, walk extents, rebuild the tree, report
//! damage, read by extent and materialize. Until rust-fs-core carries the
//! shared trait, they are this crate's own methods on [`Salvage`], with
//! types that map one to one onto the convention's.
//!
//! SquashFS is read-only by format: nothing is ever deleted from an
//! image, so every candidate is live, and the deletion-specific features
//! are reported missing by [`Salvage::capabilities`] rather than as empty
//! results. Salvage here means reading a damaged image.
//!
//! Read-only by type: [`Salvage`] borrows a [`Volume`], which holds a
//! `BlockRead`.

use crate::damage::Damage;
use crate::dir::DirEntry;
use crate::indirect::{self, IndirectTable};
use crate::inode::{FileType, InodeRecord};
use crate::metatable::TableWalk;
use crate::records::{self, inode_at, listing_at, RecordFault};
use crate::volume::Volume;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// A salvage capability, named format-neutrally as the convention does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Feature {
    /// Find possible objects with their evidence.
    FindCandidates,
    /// Follow storage claims to source extents.
    WalkExtents,
    /// Reconstruct parent relationships.
    RebuildTree,
    /// Report observed damage.
    ReportDamage,
    /// Read source bytes by extent.
    ReadExtent,
    /// Produce a file from what survives.
    Materialize,
    /// Find deleted objects. SquashFS deletes nothing.
    DeletedCandidates,
    /// Interpret the storage of deleted objects. SquashFS deletes nothing.
    DeletedExtents,
    /// Address storage as a linear run of allocation units. SquashFS has
    /// no allocation units: data blocks are packed, compressed, back to
    /// back.
    ClusterSteps,
}

impl fmt::Display for Feature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

/// What a driver lacks. An empty result from a supported verb means
/// nothing was found; an unsupported feature is listed here instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// The features this driver does not have.
    pub missing: Vec<Feature>,
}

impl Capabilities {
    /// Whether `feature` is supported.
    pub fn supports(&self, feature: Feature) -> bool {
        !self.missing.contains(&feature)
    }
}

/// Where a candidate was found. A candidate found more than one way is
/// corroborated; one found only by the scan is reachable from nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FoundBy {
    /// A sequential scan of the decoded inode table.
    pub scan: bool,
    /// The export table's entry for its inode number.
    pub export: bool,
    /// A directory entry naming it, or the superblock's root reference.
    pub directory: bool,
}

/// A possible object: an inode that decodes, and the evidence for it.
///
/// Never a trusted file. Every candidate is live, because SquashFS
/// deletes nothing.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// The packed inode reference that names it.
    pub inode_ref: u64,
    /// The inode as decoded, with a directory's parent inode number.
    pub record: InodeRecord,
    /// How it was found.
    pub found_by: FoundBy,
}

impl Candidate {
    /// The candidate's type.
    pub fn file_type(&self) -> FileType {
        self.record.inode.file_type()
    }

    /// Its inode number.
    pub fn inode_number(&self) -> u32 {
        self.record.inode.inode_number
    }
}

/// What a candidate search found, and what stopped it from finding more.
#[derive(Debug, Clone, Default)]
pub struct CandidateScan {
    /// Every candidate, by inode reference.
    pub candidates: Vec<Candidate>,
    /// References that something named but that did not decode: an
    /// export entry or a directory entry whose inode is lost.
    pub unresolved: Vec<(u64, RecordFault)>,
    /// Decoded inode table the scan passed over: `(reference, bytes)`.
    pub skipped: Vec<(u64, usize)>,
}

/// Whose child a candidate claims to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Parent {
    /// The image's root: the root inode itself claims it.
    Root,
    /// A directory candidate, by inode reference.
    Candidate(u64),
    /// An inode number no candidate has: the parent is lost.
    Lost(u32),
}

/// Where a parent claim comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ClaimSource {
    /// A directory's listing names the candidate.
    Listing,
    /// A directory inode's own record of its parent's inode number.
    ParentField,
}

/// One claim on a candidate's parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParentClaim {
    /// The claimed parent.
    pub parent: Parent,
    /// The name the claim gives the candidate, from a listing.
    pub name: Option<Vec<u8>>,
    /// Where the claim comes from.
    pub source: ClaimSource,
}

/// A candidate and every claim on its parent.
#[derive(Debug, Clone)]
pub struct TreeNode {
    /// The candidate, by inode reference.
    pub inode_ref: u64,
    /// Every parent claim, from listings and from its own parent field.
    pub claims: Vec<ParentClaim>,
}

impl TreeNode {
    /// The distinct parents claimed.
    pub fn parents(&self) -> BTreeSet<Parent> {
        self.claims.iter().map(|c| c.parent).collect()
    }
}

/// The relationships rebuilt from every claim.
#[derive(Debug, Clone, Default)]
pub struct Tree {
    /// One node per candidate, by inode reference.
    pub nodes: BTreeMap<u64, TreeNode>,
}

impl Tree {
    /// Candidates no claim places anywhere.
    pub fn orphans(&self) -> impl Iterator<Item = &TreeNode> {
        self.nodes.values().filter(|n| n.claims.is_empty())
    }

    /// Candidates claimed by more than one parent. A hard-linked file is
    /// listed in several directories legitimately, so this is evidence to
    /// weigh against its link count, not damage by itself; a directory
    /// with two parents is always a conflict.
    pub fn conflicts(&self) -> impl Iterator<Item = &TreeNode> {
        self.nodes.values().filter(|n| n.parents().len() > 1)
    }
}

/// The salvage verbs over one volume.
///
/// Built once: the inode and directory tables are walked, and the
/// lookup tables read, when it is made, so each verb works from the same
/// evidence.
pub struct Salvage<'v> {
    volume: &'v Volume<'v>,
    inodes: TableWalk,
    directories: TableWalk,
    fragments: IndirectTable,
    exports: Option<IndirectTable>,
}

impl<'v> Salvage<'v> {
    /// Walk the volume's tables and read its lookup tables.
    pub fn new(volume: &'v Volume<'v>) -> Self {
        Salvage {
            inodes: volume.inode_table(),
            directories: volume.directory_table(),
            fragments: volume.fragment_table(),
            exports: volume.export_table(),
            volume,
        }
    }

    /// The volume.
    pub fn volume(&self) -> &Volume<'v> {
        self.volume
    }

    /// The walked inode table.
    pub fn inode_table(&self) -> &TableWalk {
        &self.inodes
    }

    /// The walked directory table.
    pub fn directory_table(&self) -> &TableWalk {
        &self.directories
    }

    /// The fragment table, read block by block.
    pub fn fragment_table(&self) -> &IndirectTable {
        &self.fragments
    }

    /// What this driver lacks: the deletion-specific features, which a
    /// read-only format has nothing for, and cluster steps, which it has
    /// no allocation units for.
    pub fn capabilities(&self) -> Capabilities {
        Capabilities {
            missing: vec![
                Feature::DeletedCandidates,
                Feature::DeletedExtents,
                Feature::ClusterSteps,
            ],
        }
    }

    fn block_size(&self) -> u32 {
        self.volume.superblock().block_size
    }

    /// Every inode that decodes, found three ways and merged: a scan of
    /// the decoded inode table, the export table, and the directories
    /// reachable from the root.
    ///
    /// Nothing here needs the superblock to be right beyond the table
    /// bounds and the block size: a candidate is any record that decodes,
    /// and the evidence says how it was found.
    pub fn find_candidates(&self) -> CandidateScan {
        let sb = self.volume.superblock();
        let bs = self.block_size();
        let mut found: BTreeMap<u64, Candidate> = BTreeMap::new();
        let mut out = CandidateScan::default();

        let scan = records::scan_inodes(&self.inodes, bs, sb.inode_count.max(1));
        out.skipped = scan.skipped;
        for s in scan.inodes {
            found.insert(
                s.inode_ref,
                Candidate {
                    inode_ref: s.inode_ref,
                    record: s.record,
                    found_by: FoundBy {
                        scan: true,
                        ..FoundBy::default()
                    },
                },
            );
        }

        let note = |found: &mut BTreeMap<u64, Candidate>,
                    unresolved: &mut Vec<(u64, RecordFault)>,
                    inode_ref: u64,
                    mark: fn(&mut FoundBy)| {
            if let Some(c) = found.get_mut(&inode_ref) {
                mark(&mut c.found_by);
                return Some(c.record.clone());
            }
            match inode_at(&self.inodes, inode_ref, bs) {
                Ok(record) => {
                    let mut found_by = FoundBy::default();
                    mark(&mut found_by);
                    found.insert(
                        inode_ref,
                        Candidate {
                            inode_ref,
                            record: record.clone(),
                            found_by,
                        },
                    );
                    Some(record)
                }
                Err(fault) => {
                    unresolved.push((inode_ref, fault));
                    None
                }
            }
        };

        if let Some(exports) = &self.exports {
            for n in 1..=u32::try_from(exports.declared).unwrap_or(u32::MAX) {
                if let Some(r) = indirect::export_entry(exports, n) {
                    note(&mut found, &mut out.unresolved, r, |f| f.export = true);
                }
            }
        }

        // The directories reachable from the root, breadth first. Each
        // directory is listed once, however many entries name it.
        let mut listed = BTreeSet::new();
        let mut queue = std::collections::VecDeque::new();
        if let Some(root) = note(&mut found, &mut out.unresolved, sb.root_inode_ref, |f| {
            f.directory = true
        }) {
            queue.push_back((sb.root_inode_ref, root));
        }
        while let Some((dir_ref, dir)) = queue.pop_front() {
            if !dir.inode.is_dir() || !listed.insert(dir_ref) {
                continue;
            }
            for entry in listing_at(&self.directories, &dir.inode).entries {
                if let Some(child) = note(&mut found, &mut out.unresolved, entry.inode_ref, |f| {
                    f.directory = true
                }) {
                    queue.push_back((entry.inode_ref, child));
                }
            }
        }
        out.candidates = found.into_values().collect();
        out
    }

    /// Every directory candidate's listing, as far as it decodes.
    fn listings(&self, candidates: &[Candidate]) -> Vec<(u64, Vec<DirEntry>, Option<RecordFault>)> {
        candidates
            .iter()
            .filter(|c| c.record.inode.is_dir())
            .map(|c| {
                let l = listing_at(&self.directories, &c.record.inode);
                (c.inode_ref, l.entries, l.fault)
            })
            .collect()
    }

    /// Rebuild parent relationships from every claim: each directory
    /// listing that names a candidate, and each directory inode's own
    /// parent field. Nothing is resolved away: a candidate with no claim
    /// is an orphan, and one with several parents is a conflict.
    pub fn rebuild_tree(&self, candidates: &[Candidate]) -> Tree {
        let mut tree = Tree::default();
        for c in candidates {
            tree.nodes.insert(
                c.inode_ref,
                TreeNode {
                    inode_ref: c.inode_ref,
                    claims: Vec::new(),
                },
            );
        }
        let by_number: BTreeMap<u32, u64> = candidates
            .iter()
            .filter(|c| c.record.inode.is_dir())
            .map(|c| (c.inode_number(), c.inode_ref))
            .collect();
        for (dir_ref, entries, _) in self.listings(candidates) {
            for entry in entries {
                if let Some(node) = tree.nodes.get_mut(&entry.inode_ref) {
                    node.claims.push(ParentClaim {
                        parent: Parent::Candidate(dir_ref),
                        name: Some(entry.name),
                        source: ClaimSource::Listing,
                    });
                }
            }
        }
        let root_ref = self.volume.superblock().root_inode_ref;
        for c in candidates {
            let Some(parent) = c.record.parent else {
                continue;
            };
            let claim = if c.inode_ref == root_ref {
                Parent::Root
            } else {
                match by_number.get(&parent) {
                    Some(&r) => Parent::Candidate(r),
                    None => Parent::Lost(parent),
                }
            };
            if let Some(node) = tree.nodes.get_mut(&c.inode_ref) {
                node.claims.push(ParentClaim {
                    parent: claim,
                    name: None,
                    source: ClaimSource::ParentField,
                });
            }
        }
        tree
    }

    /// Damage found by the candidate search: every listing that stopped
    /// before its end, as damage in the directory table.
    pub fn listing_damage(&self, candidates: &[Candidate]) -> Vec<Damage> {
        self.listings(candidates)
            .into_iter()
            .filter_map(|(_, _, fault)| fault.map(|f| f.damage("directory table")))
            .collect()
    }
}
