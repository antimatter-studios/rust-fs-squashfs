//! Decompressed data and fragment blocks are held, not re-decoded (#47).
//!
//! `read_file` decoded the data block under every call, so reading a
//! block in small chunks decoded it once per chunk, and every small file
//! whose tail lives in a shared fragment block decoded the whole block
//! again. The counter below sits under the driver with the on-disk block
//! cache switched off (`open_with_cache(.., 0)`), so each data-block
//! decode shows up as one device read and nothing else hides it.
//!
//! Runs on the committed `mksquashfs`-built fixture: block size 4096,
//! `/sub/deep/big.bin` is 20000 bytes = 4 full blocks + a tail in the
//! image's single fragment block, which `/hello.txt` and `/sub/note.md`
//! also live in.

mod common;
use common::{basic_big_bin, basic_fixture_path};

use fs_core::{CountingDevice, FileDevice};
use fs_squashfs::{Filesystem, Inode};
use std::sync::Arc;

fn open_counting() -> (Filesystem, Arc<CountingDevice>) {
    let file = FileDevice::open(basic_fixture_path()).expect("open the fixture");
    let counting = Arc::new(CountingDevice::new(Arc::new(file)));
    let fs = Filesystem::open_with_cache(counting.clone(), 0).expect("open");
    (fs, counting)
}

fn read_in_chunks(fs: &Filesystem, inode: &Inode, chunk: usize) -> Vec<u8> {
    let mut out = vec![0u8; inode.file_size as usize];
    let mut done = 0;
    while done < out.len() {
        let end = (done + chunk).min(out.len());
        let n = fs
            .read_file(inode, done as u64, &mut out[done..end])
            .expect("read_file");
        assert!(n > 0, "short read at {done}");
        done += n;
    }
    out
}

#[test]
fn a_file_read_in_small_chunks_decodes_each_block_once() {
    let (fs, counting) = open_counting();
    let big = fs.lookup_path("/sub/deep/big.bin").expect("lookup");
    counting.reset();
    // 40 reads of 500 bytes: ten per data block.
    let got = read_in_chunks(&fs, &big, 500);
    assert_eq!(got, basic_big_bin(), "wrong bytes");
    let reads = counting.reads();
    assert!(
        reads <= 5,
        "{reads} device reads to read 4 data blocks and 1 fragment in 40 chunks: \
         each chunk decoded its block again"
    );
}

#[test]
fn small_files_sharing_a_fragment_block_decode_it_once() {
    let (fs, counting) = open_counting();
    let a = fs.lookup_path("/hello.txt").expect("lookup");
    let b = fs.lookup_path("/sub/note.md").expect("lookup");
    assert!(a.has_fragment() && b.has_fragment());
    assert_eq!(
        a.fragment_index, b.fragment_index,
        "the fixture no longer shares a fragment block between these files"
    );
    counting.reset();
    assert_eq!(read_in_chunks(&fs, &a, 64), b"hi\n");
    assert_eq!(read_in_chunks(&fs, &b, 64), b"# note\nsome words here\n");
    let reads = counting.reads();
    assert_eq!(
        reads, 1,
        "{reads} device reads for two files in one fragment block"
    );
}
