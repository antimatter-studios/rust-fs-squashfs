//! The salvage verbs over the committed fixture: on an intact image every
//! inode is a candidate found all three ways, and the rebuilt tree is the
//! tree the strict reader lists.

mod common;

use common::{fixture_bytes, MemDev};
use fs_squashfs::salvage::{ClaimSource, Feature, Parent, Salvage};
use fs_squashfs::Volume;

#[test]
fn on_an_intact_image_every_inode_is_found_three_ways_and_the_tree_is_whole() {
    let volume = Volume::open(MemDev::arc(fixture_bytes())).unwrap();
    let salvage = Salvage::new(&volume);
    let caps = salvage.capabilities();
    for f in [
        Feature::DeletedCandidates,
        Feature::DeletedExtents,
        Feature::ClusterSteps,
    ] {
        assert!(!caps.supports(f), "{f}");
    }
    assert!(caps.supports(Feature::FindCandidates) && caps.supports(Feature::RebuildTree));

    let scan = salvage.find_candidates();
    assert!(scan.unresolved.is_empty(), "{:?}", scan.unresolved);
    assert!(scan.skipped.is_empty(), "{:?}", scan.skipped);
    assert_eq!(
        scan.candidates.len() as u32,
        volume.superblock().inode_count
    );
    for c in &scan.candidates {
        assert!(
            c.found_by.scan && c.found_by.export && c.found_by.directory,
            "{c:?}"
        );
    }

    let tree = salvage.rebuild_tree(&scan.candidates);
    assert_eq!(tree.orphans().count(), 0);
    assert_eq!(tree.conflicts().count(), 0);
    let root = volume.superblock().root_inode_ref;
    assert_eq!(
        tree.nodes[&root].parents().into_iter().collect::<Vec<_>>(),
        [Parent::Root]
    );

    // Every name the strict reader lists is a listing claim in the tree.
    let fs = volume.filesystem().unwrap();
    let mut stack = vec![(root, fs.root_inode().unwrap())];
    let mut names = 0;
    while let Some((dir_ref, dir)) = stack.pop() {
        for entry in fs.read_dir(&dir).unwrap() {
            let node = &tree.nodes[&entry.inode_ref];
            assert!(node.claims.iter().any(|c| c.source == ClaimSource::Listing
                && c.parent == Parent::Candidate(dir_ref)
                && c.name.as_deref() == Some(&entry.name[..])));
            names += 1;
            let child = fs.read_inode(entry.inode_ref).unwrap();
            if child.is_dir() {
                stack.push((entry.inode_ref, child));
            }
        }
    }
    assert!(names >= 5);
}
