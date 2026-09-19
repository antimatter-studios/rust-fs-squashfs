#![no_main]
//! The superblock, read before anything is known. It carries the
//! compressor id that selects the decode path, the block-size log that
//! everything is shifted by, and the offsets of every table.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = fs_squashfs::Superblock::parse(data);
});
