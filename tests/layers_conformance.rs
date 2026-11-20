//! rust-fs-core's conformance helpers over this crate's layers, on the
//! committed fixture and damaged copies of it.

mod common;

use common::{basic_big_bin, fixture_bytes, MemDev};
use fs_core::layers::conformance::salvage::{self as salvage_set, Damaged, Driver, Evidence};
use fs_core::layers::conformance::{self, Expected, Limits};
use fs_core::layers::salvage::DamageKind;
use fs_core::layers::{FileAPI, NodeKind, OpenVolume, VolumeAPI};
use fs_core::BlockRead;
use fs_squashfs::layers::{SquashfsAdapter, SquashfsVolume};
use fs_squashfs::{Superblock, Volume};
use std::sync::Arc;

fn healthy_expected() -> Vec<Expected> {
    vec![
        Expected::file("/hello.txt", b"hi\n"),
        Expected::file("/sub/note.md", b"# note\nsome words here\n"),
        Expected::file("/sub/deep/big.bin", &basic_big_bin()),
        Expected::file("/empty.txt", b""),
        Expected::directory("/sub/deep"),
        Expected::absent("/nope"),
    ]
}

#[test]
fn the_fixture_conforms_on_every_layer() {
    let volume = SquashfsVolume::open(MemDev::arc(fixture_bytes())).expect("open");
    assert_eq!(volume.format(), "squashfs");
    conformance::check_healthy(&volume, &healthy_expected(), &Limits::default()).assert_passed();
    conformance::check_files_against_candidates(&volume, &Limits::default()).assert_passed();
}

/// Every read through the device into memory, for the strict reader,
/// which keeps an owned device.
fn owned(source: &dyn BlockRead) -> Arc<dyn BlockRead> {
    let mut bytes = vec![0u8; source.size_bytes() as usize];
    let ok = source.read_at(0, &mut bytes).is_ok();
    if ok {
        MemDev::arc(bytes)
    } else {
        Arc::new(Failing(source.size_bytes()))
    }
}

struct Failing(u64);
impl BlockRead for Failing {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        Err(fs_core::Error::ShortRead {
            offset,
            want: buf.len(),
            got: 0,
        })
    }
    fn size_bytes(&self) -> u64 {
        self.0
    }
}

struct Squashfs;

/// Corrupt the fixture at `at` with `bytes`.
fn with(at: u64, bytes: &[u8]) -> Vec<u8> {
    let mut image = fixture_bytes();
    image[at as usize..at as usize + bytes.len()].copy_from_slice(bytes);
    image
}

impl Driver for Squashfs {
    type Adapter = SquashfsAdapter;
    type StrictError = String;

    fn salvage(&self, source: &dyn BlockRead) -> SquashfsAdapter {
        let mut bytes = [0u8; 96];
        let _ = source.read_at(0, &mut bytes);
        SquashfsAdapter::new(Superblock::decode(&bytes))
    }

    /// Open strictly, then list every directory and read every file.
    fn strict(&self, source: &dyn BlockRead) -> Result<(), String> {
        let volume = SquashfsVolume::open(owned(source)).map_err(|e| e.to_string())?;
        let mut stack = vec![volume.root().map_err(|e| e.to_string())?];
        while let Some(dir) = stack.pop() {
            for entry in volume.read_dir(&dir).map_err(|e| e.to_string())? {
                match entry.kind {
                    NodeKind::Directory => stack.push(entry.node),
                    NodeKind::File => {
                        volume
                            .copy_to(&entry.node, &mut std::io::sink())
                            .map_err(|e| e.to_string())?;
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn healthy(&self) -> Vec<u8> {
        fixture_bytes()
    }

    fn damaged(&self) -> Vec<Damaged> {
        let intact = Volume::open(MemDev::arc(fixture_bytes())).unwrap();
        let sb = intact.superblock().clone();
        let inode_block = intact.inode_table().blocks[0].offset;
        let dir_block = intact.directory_table().blocks[0].offset;
        let frag_index = sb.fragment_table_start;
        vec![
            Damaged::new(
                "inode table block header zeroed",
                with(inode_block, &[0, 0]),
                Evidence::Damage(DamageKind::Malformed),
            ),
            Damaged::new(
                "directory table block header zeroed",
                with(dir_block, &[0, 0]),
                Evidence::Damage(DamageKind::Malformed),
            ),
            Damaged::new(
                "fragment index pointer outside the image",
                with(frag_index, &0x10u64.to_le_bytes()),
                Evidence::Damage(DamageKind::Malformed),
            ),
            Damaged::new(
                "bytes_used past the device",
                with(0x28, &(fixture_bytes().len() as u64 + 4096).to_le_bytes()),
                Evidence::Damage(DamageKind::Mismatch),
            ),
        ]
    }
}

#[test]
fn the_salvage_set_conforms() {
    // Inodes in one compressed metadata block share its stored extent, so
    // the adapter identifies a candidate by its inode reference
    // (`candidate_id`, rust-fs-core 0.5.1).
    salvage_set::run(&Squashfs).assert_conforms();
}

#[test]
fn a_damaged_copy_conforms_on_the_salvage_path() {
    let image = with(0x28, &(fixture_bytes().len() as u64 + 4096).to_le_bytes());
    assert!(SquashfsVolume::open(MemDev::arc(image.clone())).is_err());
    let volume = SquashfsVolume::open_for_salvage(MemDev::arc(image)).expect("opens for salvage");
    conformance::check_damaged(&volume, &Limits::default()).assert_passed();
}
