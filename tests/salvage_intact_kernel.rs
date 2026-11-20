//! Salvage on intact images, against the kernel's whole listing.
//!
//! For every compressor the kernel has, an image of full blocks, tail
//! fragments, a sparse file, an empty file, symlinks and nested
//! directories. Salvage finds its candidates, rebuilds the tree, and
//! materializes every file and symlink; every path the kernel lists must
//! be one salvage places, with the same type, size and SHA-256, or the
//! same symlink target -- and salvage must place nothing the kernel does
//! not list.

mod common;

use common::{dir, file, pattern, symlink};
use fs_squashfs::salvage::{ClaimSource, Parent, Salvage};
use fs_squashfs::salvage_read::MissingPolicy;
use fs_squashfs::Volume;
use fs_squashfs_test_support::{guest_kernel_report, sha256_hex};
use std::collections::BTreeMap;

fn tree() -> common::Node {
    let mut sparse = pattern(5000);
    sparse.extend(std::iter::repeat_n(0u8, 3 * 131_072));
    sparse.extend(pattern(777));
    dir(vec![
        ("hello.txt", file(b"hello\n")),
        ("empty", file(b"")),
        ("link", symlink("hello.txt")),
        (
            "sub",
            dir(vec![
                ("big.bin", file(&pattern(700_000))),
                ("sparse.bin", file(&sparse)),
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
fn every_path_the_kernel_lists_is_one_salvage_recovers_with_the_same_bytes() {
    for comp in ["gzip", "xz", "lz4", "zstd", "lzo"] {
        let art = common::build_with_mksquashfs(comp, &tree());
        let volume = Volume::open(common::MemDev::arc(art.bytes.clone())).unwrap();
        let salvage = Salvage::new(&volume);
        let scan = salvage.find_candidates();
        let tree = salvage.rebuild_tree(&scan.candidates);

        // Each candidate's path, from its listing claims up to the root.
        let path_of = |mut r: u64| -> Option<String> {
            let mut parts = Vec::new();
            for _ in 0..64 {
                let node = &tree.nodes[&r];
                if node.parents().contains(&Parent::Root) {
                    parts.reverse();
                    return Some(parts.join("/"));
                }
                let claim = node
                    .claims
                    .iter()
                    .find(|c| c.source == ClaimSource::Listing)?;
                parts.push(String::from_utf8_lossy(claim.name.as_ref()?).into_owned());
                let Parent::Candidate(p) = claim.parent else {
                    return None;
                };
                r = p;
            }
            None
        };

        let mut ours = BTreeMap::new();
        for c in &scan.candidates {
            let Some(path) = path_of(c.inode_ref) else {
                continue;
            };
            if path.is_empty() {
                continue; // the root
            }
            let inode = &c.record.inode;
            if inode.is_dir() {
                ours.insert(("type".to_string(), path), "directory".to_string());
                continue;
            }
            let mut out = Vec::new();
            let report = salvage
                .materialize(c, &mut out, MissingPolicy::Stop)
                .unwrap();
            assert!(
                report.complete(),
                "[{comp}] {path}: {:?}",
                report.missing().collect::<Vec<_>>()
            );
            if inode.is_symlink() {
                ours.insert(
                    ("type".to_string(), path.clone()),
                    "symbolic-link".to_string(),
                );
                ours.insert(
                    ("target".to_string(), path),
                    String::from_utf8(out).unwrap(),
                );
            } else {
                // `stat -c %F`'s own words, which the report uses.
                let kind = if out.is_empty() {
                    "regular-empty-file"
                } else {
                    "regular-file"
                };
                ours.insert(("type".to_string(), path.clone()), kind.to_string());
                ours.insert(("size".to_string(), path.clone()), out.len().to_string());
                ours.insert(("sha256".to_string(), path), sha256_hex(&out));
            }
        }

        let theirs: BTreeMap<_, _> = guest_kernel_report(art.path.to_str().unwrap(), comp)
            .into_iter()
            .filter(|((kind, _), _)| matches!(kind.as_str(), "type" | "size" | "sha256" | "target"))
            .collect();
        assert!(
            theirs.keys().filter(|k| k.0 == "sha256").count() >= 5,
            "[{comp}] {theirs:?}"
        );
        assert_eq!(ours, theirs, "[{comp}] salvage and the kernel");
    }
}
