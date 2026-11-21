mod common;

use common::{basic_big_bin, fixture_bytes, MemDev};
use fs_core::layers::salvage::{Extent, Plan, PlanStep, SalvageAdapter};
use fs_core::layers::{OpenVolume, PrimitiveAPI};
use fs_core::BlockRead;
use fs_squashfs::layers::SquashfsVolume;
use fs_squashfs::Filesystem;
use std::sync::Arc;

struct DamagedDevice {
    source: Arc<dyn BlockRead>,
    bad: Extent,
}

impl BlockRead for DamagedDevice {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        if offset < self.bad.offset + self.bad.length
            && offset.saturating_add(buf.len() as u64) > self.bad.offset
        {
            return Err(fs_core::Error::ShortRead {
                offset,
                want: buf.len(),
                got: 0,
            });
        }
        self.source.read_at(offset, buf)
    }

    fn size_bytes(&self) -> u64 {
        self.source.size_bytes()
    }
}

#[test]
fn damaged_data_is_reported_and_other_files_remain_recoverable() {
    let image = fixture_bytes();
    let original = MemDev::arc(image.clone());
    let fs = Filesystem::open(original.clone()).unwrap();
    let big = fs.lookup_path("/sub/deep/big.bin").unwrap();
    let hello = fs.lookup_path("/hello.txt").unwrap();
    let volume = SquashfsVolume::open_for_salvage(original.clone()).unwrap();
    let adapter = volume.adapter().unwrap();
    let mut candidates = Vec::new();
    adapter
        .find_candidates(
            &*original,
            Extent::new(0, original.size_bytes()),
            &mut candidates,
        )
        .unwrap();
    let big_candidate = candidates
        .iter()
        .find(|candidate| candidate.detail.inode_number == big.inode_number)
        .unwrap();
    let big_walk = adapter.walk_extents(&*original, big_candidate).unwrap();
    let bad = Extent::new(big_walk.extents[0].source.offset, 4096);
    let damaged: Arc<dyn BlockRead> = Arc::new(DamagedDevice {
        source: original,
        bad,
    });
    let volume = SquashfsVolume::open_for_salvage(damaged.clone()).unwrap();
    let adapter = volume.adapter().unwrap();
    let mut found = Vec::new();
    adapter
        .find_candidates(&*damaged, Extent::new(0, damaged.size_bytes()), &mut found)
        .unwrap();
    assert_eq!(found.len(), candidates.len());
    let tree = adapter.rebuild_tree(&*damaged, &found).unwrap();
    assert_eq!(tree.nodes.len(), found.len());
    let mut damage = Vec::new();
    adapter.report_damage(&*damaged, &mut damage).unwrap();
    assert!(
        damage
            .iter()
            .any(|item| item.structure == "data block" && item.location == bad),
        "{damage:?}"
    );
    let damaged_big = found
        .iter()
        .find(|candidate| candidate.detail.inode_number == big.inode_number)
        .unwrap();
    let damaged_walk = adapter.walk_extents(&*damaged, damaged_big).unwrap();
    assert_eq!(damaged_walk.claimed, 20_000);
    assert!(adapter
        .read_extent(&*damaged, bad, 0, &mut [0; 16])
        .is_err());
    let mut partial = Vec::new();
    let partial_report = adapter
        .materialize(
            &*damaged,
            &Plan::new(vec![PlanStep::Extent(damaged_walk.extents[0].source)]),
            &mut partial,
        )
        .unwrap();
    assert!(partial_report.unreadable().next().is_some());
    assert_eq!(&partial[4608..], &basic_big_bin()[4608..]);
    let good = found
        .iter()
        .find(|candidate| candidate.detail.inode_number == hello.inode_number)
        .unwrap();
    let walk = adapter.walk_extents(&*damaged, good).unwrap();
    let steps = walk
        .extents
        .iter()
        .map(|extent| match &extent.encoded {
            Some(encoding) => PlanStep::Encoded {
                source: extent.source,
                encoding: encoding.clone(),
            },
            None => PlanStep::Extent(extent.source),
        })
        .collect();
    let mut raw = vec![0; walk.extents[0].source.length as usize];
    assert_eq!(
        adapter
            .read_extent(&*damaged, walk.extents[0].source, 0, &mut raw)
            .unwrap(),
        raw.len()
    );
    let raw_start = walk.extents[0].source.offset as usize;
    assert_eq!(raw, image[raw_start..raw_start + raw.len()]);
    let mut output = Vec::new();
    adapter
        .materialize(&*damaged, &Plan::new(steps), &mut output)
        .unwrap();
    assert_eq!(output, b"hi\n");
}
