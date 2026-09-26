//! THE KERNEL ORACLE: the real in-kernel SquashFS driver, reading back
//! images `mksquashfs` wrote, compared against what this driver reports.
//!
//! # Why this exists, and why it is not `unsquashfs`
//!
//! `unsquashfs` is a second opinion on the same file format, written by
//! the same project as the writer. The kernel is the thing these images
//! are actually for — SquashFS exists to be mounted, from a distribution
//! image, a container layer or a firmware payload — and it is a third
//! implementation with its own decompressors and its own idea of the
//! layout. An image `unsquashfs` reads happily can still be one the
//! kernel refuses, or reads differently.
//!
//! # What this replaces
//!
//! `validate-kernel-mount` in `ci.yml` did this in shell: for each
//! compressor, `sudo mount -t squashfs -o loop`, then `diff -r` against
//! the source tree, then `unsquashfs -d` and `diff -r` again, then
//! `lssquashfs cat | sha256sum` against `sudo cat | sha256sum`, then
//! `ls -1` and `readlink` comparisons.
//!
//! Every one of those needed **root on the host**, so the check existed
//! only on a Linux CI runner: a developer could not run it, a Mac could
//! not run it, and nothing said so. It also compared trees with `diff`,
//! which answers "are these the same" and not "what differs" — and it
//! compared our driver against the kernel one `sha256sum` at a time,
//! naming three files out of the tree.
//!
//! Here the mount happens in the harness guest, so it runs anywhere the
//! VM runs, and the guest reports the whole tree AS DATA — type, mode,
//! owner, size, SHA-256, symlink target, device number and extended
//! attributes for every path. A mismatch names the path and the field.

mod common;

use common::{dir, file, pattern, symlink};
use fs_squashfs::Filesystem;
use fs_squashfs_test_support::{guest_kernel_refusal, guest_kernel_report, sha256_hex};
use std::collections::BTreeMap;

/// The tree every compressor is asked to carry.
///
/// Shaped like the one the shell job built, for the same reasons: a file
/// small enough to land in a fragment, a nested directory so the
/// directory table is more than one entry deep, a symlink, and a payload
/// big enough to span many data blocks AND leave a tail fragment — which
/// is the combination that exercises the block path and the fragment path
/// in one image.
///
/// `pattern` rather than `/dev/urandom`: the shell job used random bytes,
/// so the image differed on every run and a failure could not be compared
/// with yesterday's. This is deterministic and just as incompressible in
/// the ways that matter here.
const BIG: usize = 1_572_864;

fn sample_tree() -> common::Node {
    dir(vec![
        ("file.txt", file(b"hello world\n")),
        ("link", symlink("file.txt")),
        (
            "sub",
            dir(vec![
                ("another.txt", file(b"another\n")),
                ("nested", dir(vec![("big.bin", file(&pattern(BIG)))])),
            ]),
        ),
    ])
}

/// What this driver says about every path, in the same shape the guest
/// reports: `(field, path) -> value`.
fn ours(fs: &Filesystem) -> BTreeMap<(String, String), String> {
    let mut out = BTreeMap::new();
    let mut stack = vec![String::new()];
    while let Some(prefix) = stack.pop() {
        let dir_path = if prefix.is_empty() {
            "/".to_string()
        } else {
            format!("/{prefix}")
        };
        let inode = fs
            .lookup_path(&dir_path)
            .unwrap_or_else(|e| panic!("lookup {dir_path}: {e:?}"));
        for entry in fs
            .read_dir(&inode)
            .unwrap_or_else(|e| panic!("read_dir {dir_path}: {e:?}"))
        {
            let name = String::from_utf8_lossy(&entry.name).into_owned();
            if name == "." || name == ".." {
                continue;
            }
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let child = fs
                .lookup_path(&format!("/{path}"))
                .unwrap_or_else(|e| panic!("lookup /{path}: {e:?}"));
            // THE KERNEL'S VOCABULARY, NOT OURS. The guest reports what
            // `stat -c '%F'` says with spaces turned into dashes —
            // `directory`, `symbolic-link`, `regular-file`, and
            // `regular-empty-file` for a zero-length one. Inventing a
            // second set of words here would make every comparison fail on
            // spelling and say nothing about the filesystem.
            if child.is_dir() {
                out.insert(("type".into(), path.clone()), "directory".into());
                stack.push(path);
            } else if child.is_symlink() {
                out.insert(("type".into(), path.clone()), "symbolic-link".into());
                let target = fs
                    .read_symlink_target(&child)
                    .unwrap_or_else(|e| panic!("readlink /{path}: {e:?}"));
                out.insert(
                    ("target".into(), path),
                    String::from_utf8_lossy(&target).into_owned(),
                );
            } else {
                let bytes = common::read_whole_file(fs, &format!("/{path}"));
                let kind = if bytes.is_empty() {
                    "regular-empty-file"
                } else {
                    "regular-file"
                };
                out.insert(("type".into(), path.clone()), kind.into());
                out.insert(("size".into(), path.clone()), bytes.len().to_string());
                out.insert(("sha256".into(), path), sha256_hex(&bytes));
            }
        }
    }
    out
}

/// EVERY COMPRESSOR THE FORMAT ALLOWS, because a codec claimed as
/// supported and never put in front of the kernel is the gap #42 was
/// opened for. `lzma` is deliberately absent: the in-kernel driver has no
/// legacy-lzma support, so a mount of one would fail for a reason that is
/// nothing to do with this crate. `unsquashfs` covers it instead, in
/// tests/oracle_compat.rs.
const KERNEL_CODECS: &[&str] = &["gzip", "xz", "lz4", "zstd", "lzo"];

#[test]
fn every_compressor_the_kernel_has_reads_back_exactly_what_we_do() {
    let tree = sample_tree();
    for comp in KERNEL_CODECS {
        let art = common::build_with_mksquashfs(comp, &tree);
        let image = art.path.to_str().expect("utf-8 image path");
        let theirs = guest_kernel_report(image, comp);
        let fs = common::open_image_path(&art.path);
        let mine = ours(&fs);

        // The kernel reports mode, uid/gid and xattrs too. This crate has
        // no opinion about the first two here — mksquashfs sets them from
        // the staging tree — so the comparison is over the fields BOTH
        // sides claim to know: what each path is, how big the files are,
        // their contents, and where the symlinks point.
        for (key, want) in &theirs {
            if !matches!(key.0.as_str(), "type" | "size" | "sha256" | "target") {
                continue;
            }
            let got = mine.get(key).unwrap_or_else(|| {
                panic!(
                    "[{comp}] the kernel reports {} for {} and this driver reports nothing",
                    key.0, key.1
                )
            });
            assert_eq!(
                got, want,
                "[{comp}] {} of {}: this driver and the kernel disagree",
                key.0, key.1
            );
        }

        // AND THE OTHER DIRECTION, because the loop above would pass a
        // driver that reported a subset: an image whose directory table we
        // read short would agree about everything we did find.
        for key in mine.keys() {
            assert!(
                theirs.contains_key(key),
                "[{comp}] this driver reports {} for {} and the kernel does not see it at all",
                key.0,
                key.1
            );
        }

        // A FLOOR ON THE COMPARISON ITSELF. Two empty maps are equal, and
        // an image the kernel mounted but found nothing in would satisfy
        // every assertion above.
        let files = theirs.keys().filter(|k| k.0 == "sha256").count();
        assert!(
            files >= 3,
            "[{comp}] the kernel found only {files} files to compare — the tree has three, \
             so the comparison is of almost nothing"
        );
    }
}

#[test]
fn a_damaged_superblock_is_refused_by_the_kernel() {
    let art = common::build_with_mksquashfs("gzip", &sample_tree());
    let mut bytes = art.bytes.clone();
    // The superblock starts at offset 0 and begins with the magic,
    // "hsqs" little-endian.
    bytes[0..4].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
    let damaged = fs_squashfs_test_support::ScratchDir::new("kernel-refusal");
    let path = damaged.join("broken.sqfs");
    std::fs::write(&path, &bytes).expect("write the damaged image");

    let said = guest_kernel_refusal(
        path.to_str().expect("utf-8 image path"),
        "a superblock with the magic overwritten",
    );
    assert!(
        !said.trim().is_empty(),
        "the kernel refused the image but said nothing about it"
    );
}
