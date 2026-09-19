#![no_main]
//! A decompressed directory metablock: a header giving a count and an
//! inode base, then entries whose name lengths and inode deltas are all
//! declared in the bytes being read.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = fs_squashfs::dir::parse_listing(data);
});
