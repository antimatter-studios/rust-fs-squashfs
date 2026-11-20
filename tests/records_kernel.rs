//! Records decoded past damage, against the kernel.
//!
//! The in-kernel SquashFS driver reads metadata blocks on demand, one at a
//! time, so on an image with one inode-table block corrupted it still
//! stats every file whose inode -- and whose directories' inodes -- lie
//! outside that block, and fails the rest with an I/O error. That is the
//! same line [`records::inode_at`] draws, drawn by an implementation that
//! is not ours: for every path, our verdict and the kernel's must agree.
//!
//! [`records::inode_at`]: fs_squashfs::records::inode_at

mod common;

use common::{dir, file, Node};
use fs_squashfs::records::inode_at;
use fs_squashfs::Volume;
use fs_squashfs_test_support::{guest_kernel_read, ScratchDir};
use std::collections::BTreeMap;

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
fn the_kernel_fails_exactly_the_paths_whose_inodes_are_lost() {
    let art = common::build_with_mksquashfs("gzip", &many_files());
    let fs = common::open_image(art.bytes.clone());

    // Every file: its inode reference and its directory's.
    let mut files: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    let root = fs.root_inode().unwrap();
    for top in fs.read_dir(&root).unwrap() {
        let d = fs.read_inode(top.inode_ref).unwrap();
        for entry in fs.read_dir(&d).unwrap() {
            let name = format!(
                "{}/{}",
                String::from_utf8_lossy(&top.name),
                String::from_utf8_lossy(&entry.name)
            );
            files.insert(name, (entry.inode_ref, top.inode_ref));
        }
    }
    assert_eq!(files.len(), 1500);

    let intact = Volume::open(common::MemDev::arc(art.bytes.clone())).unwrap();
    let walk = intact.inode_table();
    assert!(walk.blocks.len() >= 4, "{} inode blocks", walk.blocks.len());
    let target = &walk.blocks[1];
    assert!(
        target.compressed(),
        "the target block must hold a stream to break"
    );
    let at = target.offset as usize + 2;
    let mut bytes = art.bytes.clone();
    for b in &mut bytes[at + 8..at + 40] {
        *b ^= 0x5A;
    }

    let damaged = Volume::open(common::MemDev::arc(bytes.clone())).unwrap();
    let walk = damaged.inode_table();
    let block_size = damaged.superblock().block_size;
    let root_ok = inode_at(&walk, damaged.superblock().root_inode_ref, block_size).is_ok();
    assert!(
        root_ok,
        "the root inode must survive for the kernel to mount"
    );
    let ours: BTreeMap<&str, bool> = files
        .iter()
        .map(|(path, &(file_ref, dir_ref))| {
            let ok = inode_at(&walk, file_ref, block_size).is_ok()
                && inode_at(&walk, dir_ref, block_size).is_ok();
            (path.as_str(), ok)
        })
        .collect();
    let lost = ours.values().filter(|ok| !**ok).count();
    assert!(
        lost > 0 && lost < ours.len(),
        "{lost} of {} lost: the damage must split the tree",
        ours.len()
    );

    let scratch = ScratchDir::new("records-kernel");
    let image = scratch.join("damaged.sqfs");
    std::fs::write(&image, &bytes).unwrap();
    let mut script = String::from("cd \"$MNT\"\nwhile IFS= read -r p; do\n  if stat -c %s \"$p\" > /dev/null 2>&1; then echo \"ok $p\"; else echo \"bad $p\"; fi\ndone <<'PATHS'\n");
    for path in files.keys() {
        script.push_str(path);
        script.push('\n');
    }
    script.push_str("PATHS\n");
    let out = guest_kernel_read(image.to_str().unwrap(), &script);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let theirs: BTreeMap<String, bool> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(verdict, path)| (path.to_string(), verdict == "ok"))
        .collect();
    assert_eq!(
        theirs.len(),
        ours.len(),
        "the kernel reported {} paths",
        theirs.len()
    );
    let disagree: Vec<_> = ours
        .iter()
        .filter(|(path, ok)| theirs.get(**path) != Some(*ok))
        .map(|(path, ok)| format!("{path}: ours {ok}, kernel {:?}", theirs.get(*path)))
        .collect();
    assert!(
        disagree.is_empty(),
        "{} disagreements: {:?}",
        disagree.len(),
        &disagree[..disagree.len().min(10)]
    );
}
