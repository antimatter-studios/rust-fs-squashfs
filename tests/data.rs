//! A file's blocks as stored, over the committed fixture: on an intact
//! image they read back as the strict reader reads the file, and with one
//! block damaged the others still do, while that one is missing -- never
//! zeros.

mod common;

use common::{fixture_bytes, read_whole_file, MemDev};
use fs_squashfs::data::{BlockContent, BlockSource};
use fs_squashfs::{DamageKind, Volume};

fn assemble(volume: &Volume, path: &str) -> (Vec<u8>, Vec<(u64, BlockContent)>) {
    let fs = volume.filesystem();
    let inode = match &fs {
        Ok(fs) => fs.lookup_path(path).unwrap(),
        Err(_) => panic!("open the intact image to find {path}"),
    };
    let frags = volume.fragment_table();
    let mut out = Vec::new();
    let mut contents = Vec::new();
    for block in volume.file_blocks(&inode) {
        let content = volume.read_file_block(&frags, &block);
        match &content {
            BlockContent::Bytes { bytes, .. } => out.extend_from_slice(bytes),
            BlockContent::Sparse(n) => out.resize(out.len() + *n as usize, 0),
            BlockContent::Missing(_) => {}
        }
        contents.push((block.index, content));
    }
    (out, contents)
}

#[test]
fn every_file_reads_back_block_by_block_as_the_strict_reader_reads_it() {
    let volume = Volume::open(MemDev::arc(fixture_bytes())).unwrap();
    let fs = volume.filesystem().unwrap();
    for path in [
        "/hello.txt",
        "/sub/note.md",
        "/sub/deep/big.bin",
        "/empty.txt",
    ] {
        let (ours, contents) = assemble(&volume, path);
        assert_eq!(ours, read_whole_file(&fs, path), "{path}");
        assert!(contents
            .iter()
            .all(|(_, c)| !matches!(c, BlockContent::Missing(_))));
    }
    let big = fs.lookup_path("/sub/deep/big.bin").unwrap();
    let blocks = volume.file_blocks(&big);
    assert_eq!(
        blocks.len(),
        5,
        "20000 bytes in 4 KiB blocks: four and a tail"
    );
    assert_eq!(blocks.iter().map(|b| b.len).sum::<u64>(), 20000);
    let hello = fs.lookup_path("/hello.txt").unwrap();
    let tail = volume.file_blocks(&hello);
    assert_eq!(tail.len(), 1);
    assert!(
        matches!(tail[0].source, BlockSource::Fragment { .. }),
        "{tail:?}"
    );
}

#[test]
fn a_block_past_the_end_of_a_cut_image_is_missing_and_the_others_still_read() {
    let bytes = fixture_bytes();
    let volume = Volume::open(MemDev::arc(bytes.clone())).unwrap();
    let fs = volume.filesystem().unwrap();
    let big = fs.lookup_path("/sub/deep/big.bin").unwrap();
    let intact = read_whole_file(&fs, "/sub/deep/big.bin");
    let blocks = volume.file_blocks(&big);
    let (at, len) = blocks[2].source.extent().expect("block 2 is stored");

    // Cut the image part-way through block 2.
    let cut = bytes[..(at + len / 2) as usize].to_vec();
    let volume = Volume::open(MemDev::arc(cut)).unwrap();
    let frags = Volume::open(MemDev::arc(bytes.clone()))
        .unwrap()
        .fragment_table();
    for block in &blocks {
        let content = volume.read_file_block(&frags, block);
        let range = block.file_offset as usize..(block.file_offset + block.len) as usize;
        match content {
            BlockContent::Missing(d) => {
                assert!(
                    block.index >= 2,
                    "block {} is before the cut: {d}",
                    block.index
                );
                assert_eq!(d.kind, DamageKind::Mismatch);
            }
            BlockContent::Bytes { bytes, .. } => {
                assert!(block.index < 2, "block {} is past the cut", block.index);
                assert_eq!(bytes.as_slice(), &intact[range]);
            }
            BlockContent::Sparse(_) => panic!("big.bin has no sparse blocks"),
        }
    }
}
