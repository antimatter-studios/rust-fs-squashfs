#![no_main]
//! A whole image, opened and walked.
//!
//! This is the target with the most reach: the metablock cache, the id,
//! fragment and xattr tables and the export lookup all take a device
//! rather than a byte slice, so opening an image and walking it is the
//! only way any of them is fuzzed.
//!
//! SquashFS is read-only and mounted from sources the reader did not
//! produce -- a distribution image, a container layer, a firmware
//! payload -- which is exactly the threat model a fuzzer is for.
use fs_squashfs_fuzz::walk;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    walk(data);
});
