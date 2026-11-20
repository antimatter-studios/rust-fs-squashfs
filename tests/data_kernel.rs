//! A file's blocks as stored, against the kernel, on an image with one
//! data block and the fragment block corrupted.
//!
//! The in-kernel driver decodes each data block on its own, so it reads
//! every intact block of a damaged file and fails the damaged ones with
//! an I/O error, and it reads a sparse block as zeros. For every block of
//! every file, [`data::read_file_block`]'s verdict -- these bytes, zeros
//! by the format's record, or missing -- must be the kernel's.
//!
//! [`data::read_file_block`]: fs_squashfs::data::read_file_block

mod common;

use common::{dir, file};
use fs_squashfs::data::{BlockContent, BlockSource};
use fs_squashfs::Volume;
use fs_squashfs_test_support::{guest_kernel_read, sha256_hex, ScratchDir};
use std::collections::BTreeMap;

const BS: usize = 4096;

/// Text that compresses, different in every block, so each block is a
/// stream a corruption breaks and no two blocks are alike.
fn text_blocks(n: usize, tail: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for b in 0..n {
        let mut block = String::new();
        let mut line = 0;
        while block.len() < BS {
            block.push_str(&format!(
                "block {b:03} line {line:04} of a file the kernel reads back\n"
            ));
            line += 1;
        }
        out.extend_from_slice(&block.as_bytes()[..BS]);
    }
    out.extend(std::iter::repeat_n(b'T', tail));
    out
}

#[test]
fn the_kernel_reads_exactly_the_blocks_read_file_block_reads() {
    let mut sparse = text_blocks(2, 0);
    sparse.extend(std::iter::repeat_n(0u8, 3 * BS));
    sparse.extend(text_blocks(1, 500));
    let tree = dir(vec![
        ("big.bin", file(&text_blocks(12, 1000))),
        ("sparse.bin", file(&sparse)),
        ("small.txt", file(b"a small file, all in the fragment\n")),
    ]);
    let art = common::build_with_mksquashfs_args(
        "gzip",
        &tree,
        &["-no-xattrs", "-b", "4096", "-always-use-fragments"],
    );
    let fs = common::open_image(art.bytes.clone());
    let volume = Volume::open(common::MemDev::arc(art.bytes.clone())).unwrap();

    let big = fs.lookup_path("/big.bin").unwrap();
    let big_blocks = volume.file_blocks(&big);
    let (data_at, _) = big_blocks[5].source.extent().unwrap();
    assert_eq!(
        big.block_sizes[5] & (1 << 24),
        0,
        "block 5 must be compressed"
    );
    let (frag_index, frag) = match big_blocks.last().unwrap().source {
        BlockSource::Fragment { index, .. } => (
            index,
            fs_squashfs::indirect::fragment_entry(&volume.fragment_table(), u64::from(index))
                .unwrap(),
        ),
        other => panic!("big.bin's tail is not in a fragment: {other:?}"),
    };
    assert_eq!(
        frag.size & (1 << 24),
        0,
        "the fragment block must be compressed"
    );

    let mut bytes = art.bytes.clone();
    for at in [data_at as usize, frag.start as usize] {
        for b in &mut bytes[at + 4..at + 20] {
            *b ^= 0x66;
        }
    }
    let damaged = Volume::open(common::MemDev::arc(bytes.clone())).unwrap();
    let frags = damaged.fragment_table();

    let mut ours = BTreeMap::new();
    let mut expect_missing = vec!["big.bin 5".to_string()];
    let mut sparse_blocks = 0;
    for name in ["big.bin", "sparse.bin", "small.txt"] {
        let inode = fs.lookup_path(&format!("/{name}")).unwrap();
        for block in damaged.file_blocks(&inode) {
            if matches!(block.source, BlockSource::Fragment { index, .. } if index == frag_index) {
                expect_missing.push(format!("{name} {}", block.index));
            }
            let verdict = match damaged.read_file_block(&frags, &block) {
                BlockContent::Bytes { bytes, .. } => sha256_hex(&bytes),
                BlockContent::Sparse(n) => {
                    sparse_blocks += 1;
                    sha256_hex(&vec![0u8; n as usize])
                }
                BlockContent::Missing(_) => "missing".to_string(),
            };
            ours.insert(format!("{name} {}", block.index), verdict);
        }
    }
    assert_eq!(
        sparse_blocks, 3,
        "mksquashfs stores the three zero blocks as sparse"
    );
    let missing: Vec<_> = ours
        .iter()
        .filter(|(_, v)| *v == "missing")
        .map(|(k, _)| k.as_str())
        .collect();
    expect_missing.sort();
    assert_eq!(
        missing, expect_missing,
        "the damaged block and every tail in the damaged fragment"
    );
    assert!(missing.len() >= 2);

    let scratch = ScratchDir::new("data-kernel");
    let image = scratch.join("damaged.sqfs");
    std::fs::write(&image, &bytes).unwrap();
    let mut script = String::from("cd \"$MNT\"\n");
    for key in ours.keys() {
        let (name, index) = key.split_once(' ').unwrap();
        script.push_str(&format!(
            "if h=$(dd if={name} bs={BS} skip={index} count=1 status=none | sha256sum); then echo \"{key} ${{h%% *}}\"; else echo \"{key} missing\"; fi\n"
        ));
    }
    let out = guest_kernel_read(image.to_str().unwrap(), &script);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let theirs: BTreeMap<String, String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.rsplit_once(' '))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    assert_eq!(theirs, ours, "the kernel's verdict per block, and ours");
}
