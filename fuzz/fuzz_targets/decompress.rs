#![no_main]
//! One compressed block, at every compressor this crate claims to
//! decode and at both bounds that matter.
//!
//! `max_out` is 8 KiB for a metadata block and the archive block size
//! for a data block, and it is the only thing between a crafted stream
//! and an unbounded allocation. The compressor id is attacker-supplied
//! too -- it comes out of the superblock -- so a stream compressed one
//! way and claimed to be another is a real input, not a contrived one.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    for id in 1u16..=6 {
        let Ok(comp) = fs_squashfs::Compressor::from_id(id) else {
            continue;
        };
        for max_out in [0usize, 8192, 131_072] {
            let _ = fs_squashfs::decompress::decompress(comp, data, max_out);
        }
    }
});
