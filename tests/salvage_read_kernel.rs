//! Materializing from a damaged image, against the kernel.
//!
//! One data block and one fragment block are corrupted. The kernel reads
//! each data block on its own, so a `dd` of each block of each file says
//! which bytes survive and what they are. A materialized file must hold
//! exactly those bytes, its report must call missing exactly the blocks
//! the kernel cannot read, and the damage audit must name both corrupted
//! blocks.

mod common;

use common::{dir, file};
use fs_squashfs::data::BlockSource;
use fs_squashfs::salvage::Salvage;
use fs_squashfs::salvage_read::{MissingPolicy, RangeSource};
use fs_squashfs::Volume;
use fs_squashfs_test_support::{guest_kernel_read, sha256_hex, ScratchDir};
use std::collections::BTreeMap;

const BS: usize = 4096;

fn text_blocks(n: usize, tail: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for b in 0..n {
        let mut block = String::new();
        let mut line = 0;
        while block.len() < BS {
            block.push_str(&format!(
                "block {b:03} line {line:04} of a file to materialize\n"
            ));
            line += 1;
        }
        out.extend_from_slice(&block.as_bytes()[..BS]);
    }
    out.extend(std::iter::repeat_n(b'T', tail));
    out
}

#[test]
fn a_materialized_file_holds_exactly_the_bytes_the_kernel_reads() {
    let tree = dir(vec![
        ("big.bin", file(&text_blocks(12, 1000))),
        ("other.bin", file(&text_blocks(3, 200))),
    ]);
    let art = common::build_with_mksquashfs_args(
        "gzip",
        &tree,
        &["-no-xattrs", "-b", "4096", "-always-use-fragments"],
    );
    let fs = common::open_image(art.bytes.clone());
    let volume = Volume::open(common::MemDev::arc(art.bytes.clone())).unwrap();
    let big = fs.lookup_path("/big.bin").unwrap();
    let blocks = volume.file_blocks(&big);
    let (data_at, data_len) = blocks[5].source.extent().unwrap();
    let BlockSource::Fragment { index, .. } = blocks.last().unwrap().source else {
        panic!("big.bin's tail is not in a fragment");
    };
    let frag =
        fs_squashfs::indirect::fragment_entry(&volume.fragment_table(), u64::from(index)).unwrap();
    assert_eq!(big.block_sizes[5] & (1 << 24), 0);
    assert_eq!(
        frag.size & (1 << 24),
        0,
        "the fragment block must be compressed"
    );

    let mut bytes = art.bytes.clone();
    // Inside each stored stream, past the zlib header and short of its
    // checksum: a small fragment block is only a few dozen bytes.
    let frag_len = u64::from(frag.size & 0x00FF_FFFF);
    for (at, len) in [(data_at, data_len), (frag.start, frag_len)] {
        assert!(
            len >= 12,
            "a {len}-byte stream is too small to corrupt inside"
        );
        for b in &mut bytes[(at + 2) as usize..(at + len - 4) as usize] {
            *b ^= 0x66;
        }
    }
    let damaged = Volume::open(common::MemDev::arc(bytes.clone())).unwrap();
    let salvage = Salvage::new(&damaged);
    let scan = salvage.find_candidates();

    let audit = salvage.report_damage(&scan.candidates, true);
    for at in [data_at, frag.start] {
        assert!(
            audit.damage.iter().any(|d| d.offset == at),
            "the audit misses the block at {at}: {:?}",
            audit.damage
        );
    }

    // Ours, per block: the hash of what materialize wrote there, or missing.
    let mut ours = BTreeMap::new();
    let mut prefixes = BTreeMap::new();
    for name in ["big.bin", "other.bin"] {
        let inode = fs.lookup_path(&format!("/{name}")).unwrap();
        let c = scan
            .candidates
            .iter()
            .find(|c| c.inode_number() == inode.inode_number)
            .unwrap();
        let mut out = Vec::new();
        let report = salvage
            .materialize(c, &mut out, MissingPolicy::Zeros)
            .unwrap();
        assert_eq!(
            out.len() as u64,
            inode.file_size,
            "{name}: zeros keep every range in place"
        );
        for r in &report.ranges {
            let key = format!("{name} {}", r.file_offset / BS as u64);
            let v = match r.source {
                RangeSource::Missing(_) => "missing".to_string(),
                _ => sha256_hex(&out[r.file_offset as usize..(r.file_offset + r.length) as usize]),
            };
            ours.insert(key, v);
        }
        let mut prefix = Vec::new();
        let stop = salvage
            .materialize(c, &mut prefix, MissingPolicy::Stop)
            .unwrap();
        assert_eq!(prefix.len() as u64, stop.written);
        let first_missing = stop.missing().next().map(|r| r.file_offset);
        assert_eq!(
            prefix.len() as u64,
            first_missing.unwrap_or(inode.file_size),
            "{name}"
        );
        assert_eq!(&prefix[..], &out[..prefix.len()]);
        prefixes.insert(name, (prefix.len(), sha256_hex(&prefix)));
    }
    assert!(ours.values().filter(|v| *v == "missing").count() >= 2);

    let scratch = ScratchDir::new("salvage-read-kernel");
    let image = scratch.join("damaged.sqfs");
    std::fs::write(&image, &bytes).unwrap();
    let mut script = String::from("cd \"$MNT\"\n");
    for key in ours.keys() {
        let (name, index) = key.split_once(' ').unwrap();
        script.push_str(&format!(
            "if h=$(dd if={name} bs={BS} skip={index} count=1 status=none | sha256sum); then echo \"{key} ${{h%% *}}\"; else echo \"{key} missing\"; fi\n"
        ));
    }
    for (name, (len, _)) in &prefixes {
        script.push_str(&format!(
            "echo \"prefix {name} $(head -c {len} {name} | sha256sum | cut -d' ' -f1)\"\n"
        ));
    }
    let out = guest_kernel_read(image.to_str().unwrap(), &script);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let theirs: BTreeMap<String, String> = text
        .lines()
        .filter(|l| !l.starts_with("prefix "))
        .filter_map(|l| l.rsplit_once(' '))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    assert_eq!(theirs, ours, "per block, the kernel's bytes and ours");
    for line in text.lines().filter_map(|l| l.strip_prefix("prefix ")) {
        let (name, hash) = line.split_once(' ').unwrap();
        assert_eq!(prefixes[name].1, hash, "{name}: the stop-at-missing prefix");
    }
}
