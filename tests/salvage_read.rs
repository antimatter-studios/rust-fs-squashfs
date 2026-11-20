//! The reading salvage verbs over the committed fixture, intact: every
//! file materializes to exactly what the strict reader reads, its storage
//! walks complete, and an audit finds no damage.

mod common;

use common::{fixture_bytes, MemDev};
use fs_squashfs::salvage::Salvage;
use fs_squashfs::salvage_read::{MissingPolicy, RangeSource, WalkEnd};
use fs_squashfs::Volume;

#[test]
fn every_file_materializes_as_the_strict_reader_reads_it() {
    let bytes = fixture_bytes();
    let volume = Volume::open(MemDev::arc(bytes.clone())).unwrap();
    let fs = volume.filesystem().unwrap();
    let salvage = Salvage::new(&volume);
    let scan = salvage.find_candidates();

    let report = salvage.report_damage(&scan.candidates, true);
    assert!(report.damage.is_empty(), "{:?}", report.damage);
    let files = scan
        .candidates
        .iter()
        .filter(|c| c.record.inode.is_regular_file())
        .count() as u64;
    assert_eq!(report.files_checked, files);
    assert!(report.blocks_checked >= 5);

    let mut checked = 0;
    for c in &scan.candidates {
        let inode = &c.record.inode;
        if !(inode.is_regular_file() || inode.is_symlink()) {
            continue;
        }
        let mut out = Vec::new();
        let m = salvage
            .materialize(c, &mut out, MissingPolicy::Stop)
            .unwrap();
        assert!(m.complete());
        assert_eq!(m.written, out.len() as u64);
        if inode.is_symlink() {
            assert_eq!(out, fs.read_symlink_target(inode).unwrap());
            assert_eq!(m.ranges[0].source, RangeSource::Literal);
        } else {
            let mut strict = vec![0u8; inode.file_size as usize];
            fs.read_file(inode, 0, &mut strict).unwrap();
            assert_eq!(out, strict);
            let walk = salvage.walk_extents(c);
            assert_eq!(walk.end, WalkEnd::Complete);
            assert_eq!(walk.mapped(), inode.file_size);
            for e in walk.extents.iter().flatten() {
                let mut raw = vec![0u8; e.length as usize + 7];
                let n = salvage.read_extent(*e, 0, &mut raw).unwrap();
                assert_eq!(n as u64, e.length, "read_extent stays inside the extent");
                assert_eq!(
                    &raw[..n],
                    &bytes[e.offset as usize..(e.offset + e.length) as usize]
                );
            }
        }
        checked += 1;
    }
    assert!(checked >= 5);
}
