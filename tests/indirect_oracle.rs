//! The lookup tables read block by block, on images `mksquashfs` made,
//! checked against `unsquashfs`.
//!
//! An image with one fragment per file and 600 files has a fragment table
//! of two metadata blocks. Each file's tail is in its own fragment, so
//! `unsquashfs` reading a file back is reading that fragment entry: on the
//! intact image every file it extracts must match what the strict reader
//! reads through the same entries we decoded. With the first block
//! corrupted, the entries in the second block must still be exactly the
//! intact ones, and `unsquashfs` -- which reads the fragment table whole
//! -- must refuse the image.

mod common;

use common::{file, pattern, Node};
use fs_squashfs::indirect::fragment_entry;
use fs_squashfs::Volume;
use fs_squashfs_test_support::{oracle, ScratchDir};

const FILES: usize = 600;
const ENTRIES_PER_BLOCK: u64 = 8192 / 16;

/// 600 files of 3,000 incompressible bytes each, under a 4 KiB block
/// size: each tail too big to share a fragment block with another.
fn one_fragment_per_file() -> Node {
    let entries: Vec<(String, Node)> = (0..FILES)
        .map(|i| {
            let mut data = pattern(3000 + i);
            data.truncate(3000);
            data[0] = (i % 251) as u8;
            data[1] = (i / 251) as u8;
            (format!("f{i:04}"), file(&data))
        })
        .collect();
    Node::Dir(entries.into_iter().collect())
}

#[test]
fn a_damaged_fragment_block_loses_only_its_own_entries() {
    let tree = one_fragment_per_file();
    for comp in ["gzip", "xz", "zstd"] {
        let art = common::build_with_mksquashfs_args(comp, &tree, &["-no-xattrs", "-b", "4096"]);
        let volume = Volume::open(common::MemDev::arc(art.bytes.clone())).unwrap();
        let frags = volume.fragment_table();
        assert!(frags.is_clean(), "[{comp}] {:?}", frags.damage);
        assert!(
            frags.declared > ENTRIES_PER_BLOCK,
            "[{comp}] {} fragments fit one block: the test needs two",
            frags.declared
        );
        assert_eq!(frags.blocks.len(), 2, "[{comp}]");

        // unsquashfs reads every file through the fragment entries, and the
        // strict reader through the same entries we decoded: they agree.
        let fs = common::open_image(art.bytes.clone());
        for name in ["f0000", "f0300", "f0599"] {
            let theirs = common::unsquashfs_extract_file(&art.path, name);
            assert_eq!(
                common::read_whole_file(&fs, &format!("/{name}")),
                theirs,
                "[{comp}] {name}"
            );
        }

        if comp != "gzip" {
            continue;
        }
        let first = frags.blocks[0].block.as_ref().unwrap();
        let at = first.offset as usize;
        let mut bytes = art.bytes.clone();
        if first.compressed() {
            for b in &mut bytes[at + 6..at + 22] {
                *b ^= 0x3C;
            }
        } else {
            bytes[at..at + 2].copy_from_slice(&[0, 0]);
        }
        let damaged = Volume::open(common::MemDev::arc(bytes.clone()))
            .unwrap()
            .fragment_table();
        assert_eq!(damaged.damage.len(), 1, "{:?}", damaged.damage);
        for i in 0..frags.declared {
            let (was, now) = (fragment_entry(&frags, i), fragment_entry(&damaged, i));
            if i < ENTRIES_PER_BLOCK {
                assert!(now.is_none(), "entry {i} is in the damaged block");
            } else {
                let (was, now) = (was.unwrap(), now.unwrap());
                assert_eq!((now.start, now.size), (was.start, was.size), "entry {i}");
            }
        }
        assert_eq!(damaged.missing(), ENTRIES_PER_BLOCK);

        let scratch = ScratchDir::new("indirect-oracle");
        let path = scratch.join("damaged.sqfs");
        std::fs::write(&path, &bytes).unwrap();
        let out = oracle("unsquashfs").arg("-lls").arg(&path).output();
        assert!(
            !out.status.success(),
            "unsquashfs listed an image whose first fragment block the table reports damaged"
        );
    }
}
