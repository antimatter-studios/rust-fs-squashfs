//! The lookup tables read block by block, over the committed fixture: the
//! entries agree with the strict readers on an intact image, and a damaged
//! table loses only its own entries while the strict open refuses it.

mod common;

use common::{fixture_bytes, MemDev};
use fs_squashfs::indirect::{export_entry, fragment_entry, id_entry};
use fs_squashfs::{table, DamageKind, Filesystem, Volume};

fn put(bytes: &mut [u8], at: u64, value: &[u8]) {
    let at = at as usize;
    bytes[at..at + value.len()].copy_from_slice(value);
}

#[test]
fn on_an_intact_image_every_entry_agrees_with_the_strict_readers() {
    let bytes = fixture_bytes();
    let dev = MemDev::arc(bytes.clone());
    let volume = Volume::open(dev.clone()).unwrap();
    let sb = volume.superblock().clone();

    let ids = volume.id_table();
    assert!(ids.is_clean(), "{:?}", ids.damage);
    let strict_ids = table::read_id_table(&*dev, &sb).unwrap();
    assert!(!strict_ids.is_empty());
    for (i, id) in strict_ids.iter().enumerate() {
        assert_eq!(id_entry(&ids, i as u64), Some(*id));
    }

    let frags = volume.fragment_table();
    assert!(frags.is_clean(), "{:?}", frags.damage);
    let strict_frags = table::read_fragment_table(&*dev, &sb).unwrap();
    assert!(!strict_frags.is_empty());
    for (i, f) in strict_frags.iter().enumerate() {
        let ours = fragment_entry(&frags, i as u64).unwrap();
        assert_eq!((ours.start, ours.size), (f.start, f.size));
    }
    assert!(fragment_entry(&frags, strict_frags.len() as u64).is_none());

    let exports = volume
        .export_table()
        .expect("the fixture has an export table");
    assert!(exports.is_clean(), "{:?}", exports.damage);
    let strict_exports = table::read_export_table(&*dev, &sb).unwrap().unwrap();
    for n in 1..=sb.inode_count {
        assert_eq!(
            export_entry(&exports, n),
            Some(strict_exports.lookup(&*dev, &sb, n, None).unwrap()),
            "inode {n}"
        );
    }
    assert_eq!(export_entry(&exports, 0), None);
}

#[test]
fn a_broken_fragment_block_loses_its_entries_and_nothing_else() {
    let mut bytes = fixture_bytes();
    let volume = Volume::open(MemDev::arc(bytes.clone())).unwrap();
    let intact = volume.fragment_table();
    let block = intact.blocks[0].block.as_ref().unwrap().offset;
    // The fixture's fragment table is a few entries, which mksquashfs
    // stores uncompressed, so there is no stream to break: the header is.
    put(&mut bytes, block, &0u16.to_le_bytes());

    let volume = Volume::open(MemDev::arc(bytes.clone())).unwrap();
    let frags = volume.fragment_table();
    assert_eq!(frags.damage.len(), 1, "{:?}", frags.damage);
    assert_eq!(frags.damage[0].offset, block);
    assert_eq!(frags.missing(), frags.declared);
    assert!(volume.id_table().is_clean(), "the id table is untouched");
    assert!(volume.export_table().unwrap().is_clean());
    assert!(
        Filesystem::open(MemDev::arc(bytes)).is_err(),
        "the strict open refuses it"
    );
}

#[test]
fn an_index_pointer_outside_the_image_is_malformed_and_reads_nothing() {
    let mut bytes = fixture_bytes();
    let volume = Volume::open(MemDev::arc(bytes.clone())).unwrap();
    let index = volume.superblock().fragment_table_start;
    put(&mut bytes, index, &0x10u64.to_le_bytes());
    let frags = Volume::open(MemDev::arc(bytes)).unwrap().fragment_table();
    assert_eq!(frags.damage.len(), 1);
    assert_eq!(frags.damage[0].kind, DamageKind::Malformed);
    assert_eq!((frags.damage[0].offset, frags.damage[0].length), (index, 8));
    assert!(frags.blocks[0].block.is_none());
}

#[test]
fn a_count_the_index_has_no_room_for_is_reported_and_bounded() {
    let mut bytes = fixture_bytes();
    put(&mut bytes, 0x10, &u32::MAX.to_le_bytes()); // fragment_entry_count
    let frags = Volume::open(MemDev::arc(bytes)).unwrap().fragment_table();
    assert_eq!(frags.damage[0].kind, DamageKind::Mismatch);
    assert!(
        frags.damage[0].detail.contains("room"),
        "{}",
        frags.damage[0].detail
    );
    assert!(
        frags.blocks.len() < 1 << 16,
        "{} pointers read",
        frags.blocks.len()
    );
}
