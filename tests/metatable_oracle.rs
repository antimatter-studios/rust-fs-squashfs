//! The metadata-table walk against images `mksquashfs` made, and against
//! `unsquashfs`'s verdict on the same images damaged.
//!
//! Two independent checks of the framing. First, on an intact image of
//! every compressor, every inode and directory reference `mksquashfs`
//! wrote must name a block the walk found and decoded -- the writer's
//! own record of where its blocks start, which our walk did not use.
//! Second, with one block of a table corrupted, the walk must report that
//! block and only that block, and `unsquashfs` -- which reads both tables
//! whole before it lists anything -- must refuse the image it accepted
//! intact.

mod common;

use common::{dir, file, Node};
use fs_squashfs::metatable::{MetablockState, TableWalk};
use fs_squashfs::{Filesystem, Volume};
use fs_squashfs_test_support::{oracle, ScratchDir};
use std::collections::BTreeSet;

/// Enough files that both metadata tables span several 8 KiB blocks.
fn many_files() -> Node {
    let sub = |base: usize| {
        let entries: Vec<(String, Node)> = (0..500)
            .map(|i| {
                (
                    format!("file-{:05}.txt", base + i),
                    file(format!("contents of file {}\n", base + i).as_bytes()),
                )
            })
            .collect();
        Node::Dir(entries.into_iter().collect())
    };
    dir(vec![("a", sub(0)), ("b", sub(500)), ("c", sub(1000))])
}

/// Every `(inode block, directory block)` offset the strict reader reaches
/// from the root, relative to each table's start.
fn referenced_blocks(fs: &Filesystem) -> (BTreeSet<u64>, BTreeSet<u64>) {
    let mut inodes = BTreeSet::from([fs.sb.root_inode_ref >> 16]);
    let mut dirs = BTreeSet::new();
    let mut stack = vec![fs.root_inode().expect("root inode")];
    while let Some(d) = stack.pop() {
        dirs.insert(u64::from(d.dir_start_block));
        for entry in fs.read_dir(&d).expect("read_dir") {
            inodes.insert(entry.inode_ref >> 16);
            let child = fs.read_inode(entry.inode_ref).expect("read inode");
            if child.is_dir() {
                stack.push(child);
            }
        }
    }
    (inodes, dirs)
}

fn assert_frames(walk: &TableWalk, referenced: &BTreeSet<u64>, what: &str) {
    assert!(walk.is_clean(), "[{what}] {:?}", walk.damage());
    assert!(
        walk.blocks.len() >= 2,
        "[{what}] only {} block(s): the table is too small to test framing",
        walk.blocks.len()
    );
    for &offset in referenced {
        let block = walk.block_at(offset).unwrap_or_else(|| {
            panic!("[{what}] a reference names block +{offset}, which the walk did not find")
        });
        assert!(
            block.bytes().is_some(),
            "[{what}] block +{offset} did not decode"
        );
    }
}

#[test]
fn every_reference_mksquashfs_wrote_names_a_block_the_walk_found() {
    let tree = many_files();
    for comp in ["gzip", "xz", "lz4", "zstd", "lzo", "lzma"] {
        let art = common::build_with_mksquashfs(comp, &tree);
        let fs = common::open_image(art.bytes.clone());
        let (inodes, dirs) = referenced_blocks(&fs);
        let volume = Volume::open(common::MemDev::arc(art.bytes.clone())).expect("open");
        assert_frames(
            &volume.inode_table(),
            &inodes,
            &format!("{comp} inode table"),
        );
        assert_frames(
            &volume.directory_table(),
            &dirs,
            &format!("{comp} directory table"),
        );
    }
}

/// `unsquashfs -lls` on `bytes`: whether it succeeded, and what it said.
fn unsquashfs_lists(bytes: &[u8], scratch: &ScratchDir, name: &str) -> (bool, String) {
    let path = scratch.join(name);
    std::fs::write(&path, bytes).expect("write image");
    let out = oracle("unsquashfs").arg("-lls").arg(&path).output();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn a_corrupted_block_is_the_one_the_walk_reports_and_unsquashfs_refuses_the_image() {
    let art = common::build_with_mksquashfs("gzip", &many_files());
    let scratch = ScratchDir::new("metatable-oracle");
    let (ok, said) = unsquashfs_lists(&art.bytes, &scratch, "intact.sqfs");
    assert!(ok, "unsquashfs refused the intact image: {said}");

    for table in ["inode table", "directory table"] {
        let intact = Volume::open(common::MemDev::arc(art.bytes.clone())).expect("open");
        let walk = match table {
            "inode table" => intact.inode_table(),
            _ => intact.directory_table(),
        };
        let target = &walk.blocks[1];
        let payload = target.offset as usize + 2;
        let mut bytes = art.bytes.clone();
        for b in &mut bytes[payload + 8..payload + 24] {
            *b ^= 0xA5;
        }

        let damaged = Volume::open(common::MemDev::arc(bytes.clone())).expect("open");
        let walk2 = match table {
            "inode table" => damaged.inode_table(),
            _ => damaged.directory_table(),
        };
        assert_eq!(
            walk2.blocks.len(),
            walk.blocks.len(),
            "[{table}] framing changed"
        );
        assert!(walk2.gaps.is_empty(), "[{table}] {:?}", walk2.gaps);
        for (i, block) in walk2.blocks.iter().enumerate() {
            assert_eq!(block.offset, walk.blocks[i].offset, "[{table}] block {i}");
            let decoded = !matches!(block.state, MetablockState::Undecodable(_));
            assert_eq!(decoded, i != 1, "[{table}] block {i}: {:?}", block.state);
        }
        let damage = walk2.damage();
        assert_eq!(damage.len(), 1, "[{table}] {damage:?}");
        assert_eq!(
            (damage[0].offset, damage[0].structure),
            (target.offset, table)
        );

        let (ok, said) = unsquashfs_lists(&bytes, &scratch, "damaged.sqfs");
        assert!(
            !ok,
            "[{table}] unsquashfs listed an image whose block at {} the walk reports damaged",
            target.offset
        );
        assert!(
            !said.trim().is_empty(),
            "[{table}] unsquashfs failed silently"
        );
    }
}
