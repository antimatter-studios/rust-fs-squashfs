//! The volume layer against the kernel: each superblock corruption the
//! volume reports as a fault is one the in-kernel SquashFS driver refuses
//! to mount, and the image it came from mounts.
//!
//! `volume.rs` has the same corruptions over the committed fixture; this
//! is the independent half, on an image `mksquashfs` made.

mod common;

use common::{dir, file, pattern, MemDev};
use fs_squashfs::Volume;
use fs_squashfs_test_support::{guest_kernel_refusal, guest_kernel_report, ScratchDir};

#[test]
fn the_superblock_faults_the_volume_reports_are_ones_the_kernel_refuses() {
    let tree = dir(vec![
        ("small.txt", file(b"small\n")),
        ("big.bin", file(&pattern(300_000))),
    ]);
    let art = common::build_with_mksquashfs("gzip", &tree);
    let size = art.bytes.len() as u64;

    let intact = Volume::open(MemDev::arc(art.bytes.clone())).expect("open");
    assert!(intact.health().opens());
    let report = guest_kernel_report(art.path.to_str().unwrap(), "the intact image");
    assert!(
        report.keys().any(|(_, path)| path.ends_with("big.bin")),
        "the kernel mounted the intact image but listed no big.bin: {report:?}"
    );

    let cases: [(&str, usize, Vec<u8>, &str); 5] = [
        ("magic", 0x00, b"XXXX".to_vec(), "magic"),
        (
            "major version",
            0x1C,
            3u16.to_le_bytes().to_vec(),
            "version_major",
        ),
        ("block_log", 0x16, 25u16.to_le_bytes().to_vec(), "block_log"),
        (
            "compressor",
            0x14,
            99u16.to_le_bytes().to_vec(),
            "compression_id",
        ),
        (
            "bytes_used past the device",
            0x28,
            (size + 4096).to_le_bytes().to_vec(),
            "bytes_used",
        ),
    ];
    let scratch = ScratchDir::new("volume-kernel");
    for (n, (what, at, value, field)) in cases.into_iter().enumerate() {
        let mut bytes = art.bytes.clone();
        bytes[at..at + value.len()].copy_from_slice(&value);

        let volume = Volume::open(MemDev::arc(bytes.clone())).expect("the volume opens");
        let faults = volume.superblock_faults();
        assert_eq!(
            faults.first().map(|f| f.field),
            Some(field),
            "[{what}] {faults:?}"
        );
        assert!(
            !volume.health().opens(),
            "[{what}] the strict open accepted it"
        );

        let path = scratch.join(&format!("case-{n}.sqfs"));
        std::fs::write(&path, &bytes).expect("write the damaged image");
        guest_kernel_refusal(path.to_str().unwrap(), what);
    }
}
