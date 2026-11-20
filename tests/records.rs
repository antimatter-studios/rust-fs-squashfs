//! Inodes and listings decoded out of walked tables, over the committed
//! fixture: on an intact image every record agrees with the strict reader,
//! and a directory inode's parent is the directory that lists it.

mod common;

use common::{fixture_bytes, MemDev};
use fs_squashfs::records::{inode_at, listing_at};
use fs_squashfs::{Inode, Volume};

fn same(a: &Inode, b: &Inode) {
    assert_eq!(
        (
            a.inode_type,
            a.inode_number,
            a.permissions,
            a.mtime,
            a.nlink,
            a.file_size
        ),
        (
            b.inode_type,
            b.inode_number,
            b.permissions,
            b.mtime,
            b.nlink,
            b.file_size
        )
    );
    assert_eq!(
        (a.uid_idx, a.gid_idx, a.xattr_index),
        (b.uid_idx, b.gid_idx, b.xattr_index)
    );
    assert_eq!(
        (a.blocks_start, a.fragment_index, a.fragment_offset),
        (b.blocks_start, b.fragment_index, b.fragment_offset)
    );
    assert_eq!(a.block_sizes, b.block_sizes);
    assert_eq!(a.symlink_target, b.symlink_target);
    assert_eq!(
        (a.dir_start_block, a.dir_block_offset),
        (b.dir_start_block, b.dir_block_offset)
    );
}

#[test]
fn every_record_decodes_as_the_strict_reader_decodes_it() {
    let bytes = fixture_bytes();
    let volume = Volume::open(MemDev::arc(bytes)).unwrap();
    let fs = volume.filesystem().unwrap();
    let inodes = volume.inode_table();
    let dirs = volume.directory_table();
    let block_size = volume.superblock().block_size;
    let count = volume.superblock().inode_count;

    let root_ref = volume.superblock().root_inode_ref;
    let root = inode_at(&inodes, root_ref, block_size).expect("root");
    assert_eq!(
        root.parent,
        Some(count + 1),
        "the root's parent is one past the inode count"
    );
    same(&root.inode, &fs.root_inode().unwrap());

    let mut seen = 0;
    let mut stack = vec![root.inode];
    while let Some(dir) = stack.pop() {
        let listing = listing_at(&dirs, &dir);
        assert!(listing.fault.is_none(), "{:?}", listing.fault);
        let strict = fs.read_dir(&dir).unwrap();
        assert_eq!(listing.entries.len(), strict.len());
        for (ours, theirs) in listing.entries.iter().zip(&strict) {
            assert_eq!(
                (&ours.name, ours.inode_ref),
                (&theirs.name, theirs.inode_ref)
            );
            let record = inode_at(&inodes, ours.inode_ref, block_size).expect("decodes");
            same(&record.inode, &fs.read_inode(ours.inode_ref).unwrap());
            assert!(record.len >= 16);
            seen += 1;
            if record.inode.is_dir() {
                assert_eq!(record.parent, Some(dir.inode_number));
                stack.push(record.inode);
            } else {
                assert_eq!(record.parent, None);
            }
        }
    }
    assert!(seen >= 5, "walked only {seen} entries");
}

#[test]
fn a_reference_to_no_block_is_a_fault_not_a_panic() {
    let volume = Volume::open(MemDev::arc(fixture_bytes())).unwrap();
    let inodes = volume.inode_table();
    let fault = inode_at(&inodes, 7 << 16, 4096).unwrap_err();
    assert!(
        fault.detail.contains("no metadata block"),
        "{}",
        fault.detail
    );
    let fault = inode_at(&inodes, 0xFFFF, 4096).unwrap_err();
    assert!(fault.detail.contains("past the block"), "{}", fault.detail);
}
