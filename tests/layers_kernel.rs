//! The shared layers on images `mksquashfs` made, against the kernel.
//!
//! For each compressor the kernel has: rust-fs-core's conformance helpers
//! pass over the volume, and the shared `FileAPI` lists exactly the paths
//! the kernel lists, with the same type, size, SHA-256 and symlink
//! target.

mod common;

use common::{dir, file, pattern, symlink};
use fs_core::layers::conformance::{self, Expected, Limits};
use fs_core::layers::{FileAPI, NodeKind, OpenVolume};
use fs_squashfs::layers::SquashfsVolume;
use fs_squashfs_test_support::{guest_kernel_report, sha256_hex};
use std::collections::BTreeMap;

fn sparse() -> Vec<u8> {
    let mut v = pattern(5000);
    v.extend(std::iter::repeat_n(0u8, 3 * 131_072));
    v.extend(pattern(777));
    v
}

fn tree() -> common::Node {
    dir(vec![
        ("hello.txt", file(b"hello\n")),
        ("empty", file(b"")),
        ("link", symlink("hello.txt")),
        (
            "sub",
            dir(vec![
                ("big.bin", file(&pattern(700_000))),
                ("sparse.bin", file(&sparse())),
                (
                    "deeper",
                    dir(vec![
                        ("note.md", file(b"# a note\n")),
                        ("up", symlink("../big.bin")),
                    ]),
                ),
            ]),
        ),
    ])
}

#[test]
fn the_layers_conform_and_list_what_the_kernel_lists() {
    for comp in ["gzip", "xz", "lz4", "zstd", "lzo"] {
        let art = common::build_with_mksquashfs(comp, &tree());
        let volume = SquashfsVolume::open(common::MemDev::arc(art.bytes.clone())).expect("open");
        let expected = [
            Expected::file("/hello.txt", b"hello\n"),
            Expected::file("/sub/big.bin", &pattern(700_000)),
            Expected::file("/sub/sparse.bin", &sparse()),
            Expected::file("/sub/deeper/note.md", b"# a note\n"),
            Expected::directory("/sub/deeper"),
            Expected::absent("/sub/nope"),
        ];
        conformance::check_healthy(&volume, &expected, &Limits::default()).assert_passed();
        conformance::check_files_against_candidates(&volume, &Limits::default()).assert_passed();

        let mut ours = BTreeMap::new();
        let mut stack = vec![(String::new(), volume.root().unwrap())];
        while let Some((prefix, node)) = stack.pop() {
            for e in volume.read_dir(&node).unwrap() {
                let path = if prefix.is_empty() {
                    e.name.clone()
                } else {
                    format!("{prefix}/{}", e.name)
                };
                match e.kind {
                    NodeKind::Directory => {
                        ours.insert(("type".to_string(), path.clone()), "directory".to_string());
                        stack.push((path, e.node));
                    }
                    NodeKind::Symlink => {
                        ours.insert(
                            ("type".to_string(), path.clone()),
                            "symbolic-link".to_string(),
                        );
                        let target = volume.read_link(&e.node).unwrap();
                        ours.insert(
                            ("target".to_string(), path),
                            String::from_utf8(target).unwrap(),
                        );
                    }
                    NodeKind::File => {
                        let mut bytes = Vec::new();
                        volume.copy_to(&e.node, &mut bytes).unwrap();
                        let kind = if bytes.is_empty() {
                            "regular-empty-file"
                        } else {
                            "regular-file"
                        };
                        ours.insert(("type".to_string(), path.clone()), kind.to_string());
                        ours.insert(("size".to_string(), path.clone()), bytes.len().to_string());
                        ours.insert(("sha256".to_string(), path), sha256_hex(&bytes));
                    }
                    NodeKind::Special => {}
                    _ => {}
                }
            }
        }
        let theirs: BTreeMap<_, _> = guest_kernel_report(art.path.to_str().unwrap(), comp)
            .into_iter()
            .filter(|((kind, _), _)| matches!(kind.as_str(), "type" | "size" | "sha256" | "target"))
            .collect();
        assert!(theirs.keys().filter(|k| k.0 == "sha256").count() >= 5);
        assert_eq!(ours, theirs, "[{comp}] the shared FileAPI and the kernel");
    }
}
