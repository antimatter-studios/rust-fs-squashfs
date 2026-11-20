//! The salvage candidates against the kernel, on an image whose inode
//! table has lost the block holding a directory's inode.
//!
//! The kernel reaches a file only through its directories, so with the
//! inode of `b` lost it can stat nothing under `b/`. The files' own inodes
//! are intact, and salvage finds them by scanning the inode table and
//! through the export table: every file the kernel stats must be a
//! candidate, every file under `b/` whose inode survives must be one too,
//! and those must be orphans in the rebuilt tree -- found, but placed by
//! nothing that survives.

mod common;

use common::{dir, file, Node};
use fs_squashfs::records::inode_at;
use fs_squashfs::salvage::Salvage;
use fs_squashfs::Volume;
use fs_squashfs_test_support::{guest_kernel_read, ScratchDir};
use std::collections::{BTreeMap, BTreeSet};

fn many_files() -> Node {
    let sub = |base: usize| {
        let entries: Vec<(String, Node)> = (0..500)
            .map(|i| {
                (
                    format!("file-{:05}.txt", base + i),
                    file(format!("contents of file {}\n", base + i).as_bytes()),
                )
            })
            .collect();
        Node::Dir(entries.into_iter().collect())
    };
    dir(vec![("a", sub(0)), ("b", sub(500)), ("c", sub(1000))])
}

#[test]
fn salvage_finds_every_file_the_kernel_reaches_and_the_ones_it_cannot() {
    let art = common::build_with_mksquashfs("gzip", &many_files());
    let fs = common::open_image(art.bytes.clone());
    let root = fs.root_inode().unwrap();
    let mut files: BTreeMap<String, (u64, u32)> = BTreeMap::new();
    let mut b_ref = 0;
    for top in fs.read_dir(&root).unwrap() {
        if top.name == b"b" {
            b_ref = top.inode_ref;
        }
        let d = fs.read_inode(top.inode_ref).unwrap();
        for e in fs.read_dir(&d).unwrap() {
            let path = format!(
                "{}/{}",
                String::from_utf8_lossy(&top.name),
                String::from_utf8_lossy(&e.name)
            );
            files.insert(path, (e.inode_ref, e.inode_number));
        }
    }

    // Corrupt the inode-table block holding b's inode.
    let intact = Volume::open(common::MemDev::arc(art.bytes.clone())).unwrap();
    let walk = intact.inode_table();
    let target = walk.block_at(b_ref >> 16).expect("b's block");
    assert_ne!(
        b_ref >> 16,
        fs.sb.root_inode_ref >> 16,
        "the root must survive for the kernel to mount"
    );
    assert!(target.compressed());
    let at = target.offset as usize + 2;
    let mut bytes = art.bytes.clone();
    for x in &mut bytes[at + 8..at + 40] {
        *x ^= 0x5A;
    }

    let damaged = Volume::open(common::MemDev::arc(bytes.clone())).unwrap();
    let salvage = Salvage::new(&damaged);
    let scan = salvage.find_candidates();
    let numbers: BTreeSet<u32> = scan.candidates.iter().map(|c| c.inode_number()).collect();
    let by_ref: BTreeMap<u64, _> = scan.candidates.iter().map(|c| (c.inode_ref, c)).collect();
    let tree = salvage.rebuild_tree(&scan.candidates);
    let orphans: BTreeSet<u64> = tree.orphans().map(|n| n.inode_ref).collect();

    // Every file whose own inode decodes is a candidate; those under b/
    // were found without any directory, and nothing places them.
    let bs = damaged.superblock().block_size;
    let mut under_b = 0;
    for (path, &(r, _)) in &files {
        let survives = inode_at(salvage.inode_table(), r, bs).is_ok();
        assert_eq!(by_ref.contains_key(&r), survives, "{path}");
        if survives && path.starts_with("b/") {
            under_b += 1;
            let c = by_ref[&r];
            assert!(
                c.found_by.scan && c.found_by.export && !c.found_by.directory,
                "{path}: {:?}",
                c.found_by
            );
            assert!(
                orphans.contains(&r),
                "{path} is placed by nothing that survives"
            );
        }
    }
    assert!(
        under_b > 0,
        "no file under b/ survived: the test proves nothing"
    );

    // The kernel: which paths it can stat, by inode number.
    let scratch = ScratchDir::new("salvage-kernel");
    let image = scratch.join("damaged.sqfs");
    std::fs::write(&image, &bytes).unwrap();
    let mut script = String::from("cd \"$MNT\"\nwhile IFS= read -r p; do\n  if n=$(stat -c %i \"$p\" 2>/dev/null); then echo \"$n $p\"; fi\ndone <<'PATHS'\n");
    for p in files.keys() {
        script.push_str(p);
        script.push('\n');
    }
    script.push_str("PATHS\n");
    let out = guest_kernel_read(image.to_str().unwrap(), &script);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let reached: Vec<(u32, String)> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(n, p)| (n.parse().unwrap(), p.to_string()))
        .collect();
    assert!(
        reached.len() > 500,
        "the kernel reached only {} paths",
        reached.len()
    );
    assert!(
        reached.iter().all(|(_, p)| !p.starts_with("b/")),
        "the kernel reached under b/"
    );
    for (n, p) in &reached {
        assert_eq!(files[p].1, *n, "{p}: the kernel's inode number");
        assert!(
            numbers.contains(n),
            "the kernel stats {p} (inode {n}), and salvage did not find it"
        );
    }
}
