//! Recursive paths, types, sizes and identities checked against GNU find
//! on Linux's mount of a reference-made image, inside the harness guest.
mod cli_support;

use cli_support::*;
use fs_squashfs::{Filesystem, Inode};
use fs_squashfs_test_support::{guest_kernel_read, mksquashfs_from_guest_tree, ScratchDir};
use std::collections::BTreeMap;

#[test]
fn recursive_listing_matches_find_and_stops_on_damaged_directory_graphs() {
    let dir = ScratchDir::new("recursive-kernel");
    let image = dir.join("tree.sqfs");
    let built = mksquashfs_from_guest_tree(
        &image,
        r#"
mkdir -p a/deep empty z
printf payload > a/deep/file
printf space > 'two words'
ln a/deep/file hard
ln -s a dirlink
ln -s ../.. a/deep/up
ln -s absent dangling
printf raw > $'a/deep/raw-\xff'
"#,
        &["-noappend", "-no-progress", "-noI"],
    );
    assert!(built.status.success(), "{}", stderr(&built));
    let image_arg = image.display().to_string();
    let oracle = guest_kernel_read(
        &image_arg,
        "find \"$MNT\" -mindepth 1 -printf '%P\\0%y\\0%s\\0%i\\0'",
    );
    assert!(oracle.status.success(), "{}", stderr(&oracle));
    let mut expected = BTreeMap::new();
    let fields: Vec<_> = oracle.stdout.split(|b| *b == 0).collect();
    assert_eq!(fields.last(), Some(&b"".as_slice()));
    assert_eq!((fields.len() - 1) % 4, 0);
    for row in fields[..fields.len() - 1].chunks_exact(4) {
        let mut path = b"/".to_vec();
        path.extend_from_slice(row[0]);
        let number = |bytes: &[u8]| std::str::from_utf8(bytes).unwrap().parse::<u64>().unwrap();
        let kind = match row[1] {
            b"d" => "dir",
            b"f" => "file",
            b"l" => "symlink",
            other => panic!("unexpected find type {other:?}"),
        };
        expected.insert(path, (kind.to_string(), number(row[2]), number(row[3])));
    }
    assert_eq!(expected.len(), 11, "the oracle must walk the whole tree");
    let output = stdout(&ok(tool("fs.squashfs").args([&image_arg, "ls", "-R", "/"])));
    let mut actual = BTreeMap::new();
    for row in output.split("\n  {").skip(1) {
        let path = if row.contains("\"path_hex\"") {
            let hex = json_field(row, "path_hex");
            (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect()
        } else {
            json_field(row, "path").into_bytes()
        };
        assert_eq!(
            json_field(row, "depth").parse::<usize>().unwrap(),
            path.iter().filter(|b| **b == b'/').count()
        );
        assert!(!row.contains("\"revisited\""));
        let value = (
            json_field(row, "type"),
            json_field(row, "size").parse::<u64>().unwrap(),
            json_field(row, "inode").parse::<u64>().unwrap(),
        );
        assert!(actual.insert(path, value).is_none(), "duplicate path");
    }
    assert_eq!(actual, expected);

    // Mutate only the reference image's uncompressed basic directory
    // inodes. These faults exercise CLI termination, not format validity.
    let bytes = std::fs::read(&image).unwrap();
    let fs = Filesystem::open(std::sync::Arc::new(
        fs_core::FileDevice::open(&image).unwrap(),
    ))
    .unwrap();
    let root = fs.root_inode().unwrap();
    let child = fs
        .read_dir(&root)
        .unwrap()
        .into_iter()
        .find(|e| e.name == b"a")
        .unwrap();
    let inode_at = |reference: u64| {
        let block = fs.sb.inode_table_start as usize + (reference >> 16) as usize;
        assert_ne!(
            u16::from_le_bytes(bytes[block..block + 2].try_into().unwrap()) & 0x8000,
            0
        );
        block + 2 + (reference & 0xffff) as usize
    };
    let root_at = inode_at(fs.sb.root_inode_ref);
    let child_at = inode_at(child.inode_ref);
    let node: Inode = fs.read_inode(child.inode_ref).unwrap();
    assert_eq!((root.inode_type, node.inode_type), (1, 1));
    for fault in ["identity", "cycle", "unreadable"] {
        let mut damaged = bytes.clone();
        match fault {
            "identity" => damaged[child_at + 12..child_at + 16]
                .copy_from_slice(&root.inode_number.to_le_bytes()),
            "cycle" => {
                damaged[child_at + 16..child_at + 20]
                    .copy_from_slice(&bytes[root_at + 16..root_at + 20]);
                damaged[child_at + 24..child_at + 28]
                    .copy_from_slice(&bytes[root_at + 24..root_at + 28]);
            }
            _ => damaged[child_at + 16..child_at + 20].copy_from_slice(&u32::MAX.to_le_bytes()),
        }
        let bad = dir.join(&format!("{fault}.sqfs"));
        std::fs::write(&bad, &damaged).unwrap();
        let out = tool("fs.squashfs")
            .arg(&bad)
            .args(["ls", "-R", "--limit", "30", "--max-depth", "10", "/"])
            .output()
            .unwrap();
        if fault == "unreadable" {
            assert_eq!(out.status.code(), Some(1), "{out:?}");
            assert_eq!(json_field(&stderr(&out), "code"), "1");
            assert!(stderr(&out).contains("/a"));
            assert_eq!(stdout(&out).matches("\"path\":").count(), 1);
        } else {
            assert!(out.status.success(), "{fault}: {}", stderr(&out));
            assert!(stdout(&out).contains("\"revisited\": true"));
            assert!(!stdout(&out).contains("/a/deep"));
        }
        assert_eq!(
            std::fs::read(&bad).unwrap(),
            damaged,
            "the walk is read-only"
        );
    }
    assert_eq!(std::fs::read(&image).unwrap(), bytes);
}
