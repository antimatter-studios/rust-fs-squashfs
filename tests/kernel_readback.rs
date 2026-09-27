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
use fs_squashfs_test_support::{
    guest_kernel_refusal, guest_kernel_report, mksquashfs_from_guest_tree, sha256_hex, ScratchDir,
};
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

// ===========================================================================
// Hardlinks (#111)
// ===========================================================================
//
// SquashFS is deduplicated at build time and read-only, so shared inodes
// are ORDINARY rather than exotic: `mksquashfs` emits them for every
// hardlink in the source tree, and distribution and container images are
// full of them. Nothing here tested a link count or inode identity, and
// the kernel report did not carry either field -- so the one oracle that
// could have caught a wrong `nlink` was not being asked.
//
// Two facts, and they fail separately. The COUNT says how many names an
// inode has; the IDENTITY says which names are the same inode. A driver
// that reported `nlink` correctly and resolved two links to different
// inodes would break `find -samefile`, package verification and every
// backup tool that deduplicates by inode, while agreeing about the count.

/// A tree staged in the guest, because hardlinks cannot be expressed in
/// the `Node` model the other tests use -- a `Node` is a tree and a
/// hardlink is what makes it a graph.
///
/// It covers what the issue asks for: two links in ONE directory, a
/// third of the same inode in ANOTHER (so the resolution is not an
/// accident of sharing a parent), a file with one link as the control,
/// and a non-regular type that also carries a count.
const HARDLINK_TREE: &str = r#"
mkdir -p dir_a dir_b
printf 'shared payload
' > dir_a/original.txt
ln dir_a/original.txt dir_a/same_dir_link.txt
ln dir_a/original.txt dir_b/other_dir_link.txt
printf 'alone
' > dir_a/solo.txt
mkfifo dir_a/pipe
ln dir_a/pipe dir_a/pipe_link
"#;

/// This driver's `(nlink, inode number)` for every path in the image.
fn ours_links(fs: &Filesystem) -> BTreeMap<String, (u32, u32)> {
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
            out.insert(path.clone(), (child.nlink, child.inode_number));
            if child.is_dir() {
                stack.push(path);
            }
        }
    }
    out
}

/// Group paths by whichever number identifies their inode, so two sides
/// that number inodes differently can still be compared on WHICH paths
/// share one.
fn classes<K: Ord + Clone, V: Ord + Clone>(of: &BTreeMap<K, V>) -> Vec<Vec<K>> {
    let mut by_value: BTreeMap<V, Vec<K>> = BTreeMap::new();
    for (key, value) in of {
        by_value.entry(value.clone()).or_default().push(key.clone());
    }
    let mut out: Vec<Vec<K>> = by_value.into_values().collect();
    out.sort();
    out
}

#[test]
fn hardlinks_carry_the_count_and_the_identity_the_kernel_reports() {
    let scratch = ScratchDir::new("kernel-hardlinks");
    let image = scratch.join("hardlinks.sqfs");
    let out = mksquashfs_from_guest_tree(
        &image,
        HARDLINK_TREE,
        &["-comp", "gzip", "-noappend", "-no-progress"],
    );
    assert!(
        out.status.success(),
        "mksquashfs on the hardlink tree failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let path = image.to_str().expect("utf-8 image path");
    let theirs = guest_kernel_report(path, "hardlinks");
    let fs = common::open_image_path(&image);
    let mine = ours_links(&fs);

    // A FLOOR ON THE COMPARISON. Two empty maps agree about everything.
    // The tree has six entries under two directories.
    assert!(
        mine.len() >= 8,
        "this driver found only {} paths in the hardlink image; the tree has eight",
        mine.len()
    );

    // 1. THE COUNT, for every path the kernel reported one for.
    let mut checked = 0;
    for ((field, path), want) in &theirs {
        if field != "nlink" {
            continue;
        }
        let (got, _) = mine.get(path).unwrap_or_else(|| {
            panic!("the kernel sees {path} and this driver reports nothing for it")
        });
        assert_eq!(
            &got.to_string(),
            want,
            "link count of {path}: this driver says {got}, the kernel says {want}"
        );
        checked += 1;
    }
    assert!(
        checked >= 8,
        "only {checked} link counts were compared; the kernel should report one per path"
    );

    // AND THE COUNT IS NOT UNIFORMLY 1, which is what a driver that never
    // read the field would report, and which every assertion above would
    // accept on a tree of ordinary files.
    let shared = mine.values().filter(|(nlink, _)| *nlink > 1).count();
    assert!(
        shared >= 4,
        "only {shared} paths have more than one link, so this proves nothing about \
         hardlinks: the tree has three names for one file and two for one pipe"
    );

    // 2. THE IDENTITY. The kernel's inode numbers are its own, so what is
    // compared is which paths share one.
    let theirs_ino: BTreeMap<String, String> = theirs
        .iter()
        .filter(|((field, _), _)| field == "ino")
        .map(|((_, path), value)| (path.clone(), value.clone()))
        .collect();
    let mine_ino: BTreeMap<String, u32> = mine
        .iter()
        .map(|(path, (_, ino))| (path.clone(), *ino))
        .collect();
    assert_eq!(
        classes(&mine_ino),
        classes(&theirs_ino),
        "this driver and the kernel disagree about WHICH paths are the same inode"
    );
}

// ===========================================================================
// POSIX ACLs (#112)
// ===========================================================================

/// A tree carrying both kinds of ACL, and a `user.` attribute as the
/// control that proves extended attributes survive at all.
const ACL_TREE: &str = r#"
mkdir -p d
printf 'x\n' > d/f.txt
setfattr -n user.colour -v blue d/f.txt
setfacl -m u:1234:rwx,u:1235:r-x,g:1237:r-- d
setfacl -d -m u:1234:rwx d
setfacl -m u:1234:rwx d/f.txt
"#;

/// The extended attributes this driver reports for a path, as
/// `name=value`, sorted.
fn ours_xattrs(fs: &Filesystem, path: &str) -> Vec<String> {
    let inode = fs
        .lookup_path(path)
        .unwrap_or_else(|e| panic!("lookup {path}: {e:?}"));
    let mut out: Vec<String> = fs
        .list_xattrs(&inode)
        .unwrap_or_else(|e| panic!("list_xattrs {path}: {e:?}"))
        .into_iter()
        .map(|entry| {
            format!(
                "{}={}",
                String::from_utf8_lossy(&entry.name),
                String::from_utf8_lossy(&entry.value)
            )
        })
        .collect();
    out.sort();
    out
}

/// POSIX ACLs do not survive into a SquashFS image, and this driver
/// agrees with the kernel about that and about what is left.
///
/// #112 asks for ACLs to be read back "value bytes and all". They
/// cannot be: **the format has no `system.` namespace.** A SquashFS
/// xattr carries its namespace as an index into three prefixes --
/// `user.`, `trusted.`, `security.` (`xattr::PREFIXES`) -- and
/// `system.posix_acl_access` is in none of them, so `mksquashfs` drops
/// it. Measured on a staged tree that definitely had them:
///
/// ```text
/// # before mksquashfs
/// # file: d
/// user:1234:rwx  group:1237:r--  mask::rwx  default:user:1234:rwx
/// system.posix_acl_access=0sAgAAAAEABwD/////AgAHANIEAAAC...
///
/// # the same image, mounted by Linux
/// # file: d
/// user::rwx  group::rwx  other::r-x
/// getfattr: d: Operation not supported
/// ```
///
/// So the test that can exist is the one that matters: the driver must
/// report exactly what the kernel reports, INCLUDING the permission bits
/// the dropped ACL left behind. Note `group::r-x` becoming `group::rwx`
/// above -- the ACL mask was written into the group bits, so an image
/// built from a tree with ACLs has different effective group permissions
/// from the tree. A driver that disagreed with the kernel here would be
/// the "mounts fine and enforces the wrong permissions" failure the issue
/// is about, and it is the half of it a read-only driver can have.
#[test]
fn acls_do_not_survive_the_format_and_the_driver_agrees_with_the_kernel() {
    let scratch = ScratchDir::new("kernel-acls");
    let image = scratch.join("acls.sqfs");
    let out = mksquashfs_from_guest_tree(
        &image,
        ACL_TREE,
        &["-comp", "gzip", "-noappend", "-no-progress", "-xattrs"],
    );
    assert!(
        out.status.success(),
        "mksquashfs on the ACL tree failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let path = image.to_str().expect("utf-8 image path");
    let theirs = guest_kernel_report(path, "acls");
    let fs = common::open_image_path(&image);

    // 1. THE CONTROL. A `user.` attribute is in a namespace the format
    //    has, so it must be there on both sides -- otherwise the
    //    assertions below would pass on an image with no xattrs at all,
    //    or on a `-xattrs` flag that stopped working.
    assert_eq!(
        ours_xattrs(&fs, "/d/f.txt"),
        vec!["user.colour=blue".to_string()],
        "the control attribute is missing, so this proves nothing about ACLs"
    );
    assert_eq!(
        theirs.get(&("xattrs".to_string(), "d/f.txt".to_string())),
        Some(&"user.colour=blue".to_string()),
        "the kernel does not see the control attribute either"
    );

    // 2. AND NO ACL, on either side, on either path.
    for path in ["/d", "/d/f.txt"] {
        for attribute in ours_xattrs(&fs, path) {
            assert!(
                !attribute.starts_with("system."),
                "this driver reports {attribute} on {path}, but the SquashFS xattr \
                 namespace index has three values and `system.` is not one of them"
            );
        }
    }
    for ((field, path), value) in &theirs {
        if field == "xattrs" {
            assert!(
                !value.contains("system.posix_acl"),
                "the kernel reports {value} on {path}: the image carries an ACL after all, \
                 and this test's premise is wrong"
            );
        }
    }

    // 3. THE PERMISSIONS THE DROPPED ACL LEFT BEHIND. This is what a
    //    consumer acts on, and where a disagreement would be the failure
    //    that matters.
    for ((field, path), want) in &theirs {
        if field != "mode" {
            continue;
        }
        let inode = fs
            .lookup_path(&format!("/{path}"))
            .unwrap_or_else(|e| panic!("lookup /{path}: {e:?}"));
        assert_eq!(
            &format!("{:o}", inode.permissions & 0o7777),
            want,
            "mode of {path}: this driver and the kernel disagree about the permissions \
             an image built from a tree with ACLs ended up with"
        );
    }
}
