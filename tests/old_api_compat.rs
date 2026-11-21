#![allow(deprecated)]

mod common;

use common::{fixture_bytes, MemDev};
use fs_squashfs::salvage::Salvage;
use fs_squashfs::salvage_read::MissingPolicy;
use fs_squashfs::{Filesystem, Volume};

#[test]
fn old_layer_apis_still_compile_and_work() {
    let source = MemDev::arc(fixture_bytes());
    let fs = Filesystem::open(source.clone()).unwrap();
    let root = fs.root_inode().unwrap();
    assert!(!fs.read_dir(&root).unwrap().is_empty());
    let file = fs.lookup(&root, b"hello.txt").unwrap();
    assert_eq!(
        fs.lookup_path("/hello.txt").unwrap().inode_number,
        file.inode_number
    );
    assert_eq!(
        fs.lookup_path_bytes(b"/hello.txt").unwrap().inode_number,
        file.inode_number
    );
    let mut output = [0; 3];
    assert_eq!(fs.read_file(&file, 0, &mut output).unwrap(), 3);
    assert_eq!(&output, b"hi\n");
    let link = fs.lookup_path("/link").unwrap();
    assert!(!fs.read_symlink_target(&link).unwrap().is_empty());

    let volume = Volume::open(source).unwrap();
    assert!(volume.device_size() > 0);
    let _ = volume.superblock();
    let _ = volume.health();
    let mut raw = [0; 4];
    assert_eq!(volume.read_raw(0, &mut raw).unwrap(), 4);
    let salvage = Salvage::new(&volume);
    let _ = salvage.capabilities();
    let scan = salvage.find_candidates();
    let _ = salvage.rebuild_tree(&scan.candidates);
    let _ = salvage.report_damage(&scan.candidates, true);
    let candidate = scan
        .candidates
        .iter()
        .find(|candidate| candidate.record.inode.inode_number == file.inode_number)
        .unwrap();
    let walk = salvage.walk_extents(candidate);
    if let Some(extent) = walk.extents.iter().flatten().next() {
        salvage.read_extent(*extent, 0, &mut raw).unwrap();
    }
    let mut recovered = Vec::new();
    assert!(salvage
        .materialize(candidate, &mut recovered, MissingPolicy::Stop)
        .unwrap()
        .complete());
    assert_eq!(recovered, b"hi\n");
}
