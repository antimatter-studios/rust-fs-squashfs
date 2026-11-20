//! The volume layer over the committed fixture: an image opens on its
//! superblock however damaged the rest is, names every fault at its
//! bytes, and agrees with the strict open about what it refuses.
//!
//! The kernel's verdict on the same corruptions is `volume_kernel.rs`.

mod common;

use common::{fixture_bytes, MemDev};
use fs_squashfs::{DamageKind, Error, Filesystem, SuperblockRule, Volume};

fn put(bytes: &mut [u8], at: usize, value: &[u8]) {
    bytes[at..at + value.len()].copy_from_slice(value);
}

#[test]
fn a_healthy_image_opens_with_no_faults_and_reads_its_raw_bytes() {
    let bytes = fixture_bytes();
    let volume = Volume::open(MemDev::arc(bytes.clone())).expect("open the volume");
    assert_eq!(volume.device_size(), bytes.len() as u64);
    assert!(volume.superblock_faults().is_empty());
    let health = volume.health();
    assert!(health.opens(), "{:?}", health.refusal);
    assert!(health.damage().is_empty());
    assert_eq!(volume.compressor().map(|c| c.name()), Some("gzip"));
    assert_eq!(volume.superblock().block_size, 4096);

    let mut magic = [0u8; 4];
    assert_eq!(volume.read_raw(0, &mut magic).unwrap(), 4);
    assert_eq!(&magic, b"hsqs");

    // The last 10 bytes ask for 16: a short count, not an error.
    let mut tail = [0u8; 16];
    let end = bytes.len() as u64;
    assert_eq!(volume.read_raw(end - 10, &mut tail).unwrap(), 10);
    assert_eq!(&tail[..10], &bytes[bytes.len() - 10..]);
    assert_eq!(volume.read_raw(end, &mut tail).unwrap(), 0);

    let fs = volume.filesystem().expect("the strict open accepts it");
    assert_eq!(common::read_whole_file(&fs, "/hello.txt"), b"hi\n");
}

#[test]
fn each_superblock_corruption_opens_and_names_the_field_the_strict_open_refuses() {
    let base = fixture_bytes();
    let size = base.len() as u64;
    let cases: Vec<(&str, usize, Vec<u8>, &str, SuperblockRule)> = vec![
        (
            "magic",
            0x00,
            b"XXXX".to_vec(),
            "magic",
            SuperblockRule::Magic,
        ),
        (
            "major version",
            0x1C,
            3u16.to_le_bytes().to_vec(),
            "version_major",
            SuperblockRule::Version,
        ),
        (
            "block_log",
            0x16,
            25u16.to_le_bytes().to_vec(),
            "block_log",
            SuperblockRule::BlockSize,
        ),
        (
            "compressor",
            0x14,
            99u16.to_le_bytes().to_vec(),
            "compression_id",
            SuperblockRule::Compressor,
        ),
        (
            "bytes_used past the device",
            0x28,
            (size + 4096).to_le_bytes().to_vec(),
            "bytes_used",
            SuperblockRule::BytesUsed,
        ),
        (
            "id table past bytes_used",
            0x30,
            (size * 4).to_le_bytes().to_vec(),
            "id_table_start",
            SuperblockRule::TableBounds,
        ),
    ];
    for (what, at, value, field, rule) in cases {
        let mut bytes = base.clone();
        put(&mut bytes, at, &value);
        let volume = Volume::open(MemDev::arc(bytes.clone()))
            .unwrap_or_else(|e| panic!("[{what}] the volume must open on any superblock: {e}"));
        let faults = volume.superblock_faults();
        let first = faults
            .first()
            .unwrap_or_else(|| panic!("[{what}] no fault reported"));
        assert_eq!(
            (first.field, first.rule),
            (field, rule),
            "[{what}] {faults:?}"
        );
        assert_eq!(
            first.offset, at as u64,
            "[{what}] the fault points at the field"
        );

        let strict = match Filesystem::open(MemDev::arc(bytes)) {
            Ok(_) => panic!("[{what}] the strict open accepted a corrupted superblock"),
            Err(e) => e,
        };
        assert_eq!(
            strict.to_string(),
            first.to_error(volume.superblock()).to_string(),
            "[{what}] the strict open refuses for the first fault"
        );
        let health = volume.health();
        assert!(!health.opens(), "[{what}]");
        assert_eq!(health.damage()[0].offset, at as u64);
    }
}

#[test]
fn a_truncated_image_reports_bytes_used_past_its_end() {
    let mut bytes = fixture_bytes();
    bytes.truncate(bytes.len() / 2);
    let volume = Volume::open(MemDev::arc(bytes)).expect("a superblock is there");
    let damage = volume.health().damage();
    assert_eq!(damage[0].kind, DamageKind::Mismatch);
    assert_eq!((damage[0].offset, damage[0].length), (0x28, 8));
    assert!(
        damage[0].detail.starts_with("bytes_used"),
        "{}",
        damage[0].detail
    );
    // The superblock's other fields are still the image's.
    assert_eq!(volume.superblock().block_size, 4096);
}

#[test]
fn a_device_shorter_than_a_superblock_does_not_open() {
    match Volume::open(MemDev::arc(vec![0u8; 95])) {
        Err(Error::BadSuperblock(why)) => assert!(why.contains("shorter"), "{why}"),
        Err(e) => panic!("wrong refusal: {e}"),
        Ok(_) => panic!("95 bytes opened as a volume"),
    }
}
