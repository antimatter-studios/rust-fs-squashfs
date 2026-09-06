//! Resolving an inode number, against images `mksquashfs` wrote.
//!
//! # What makes this an oracle rather than a round trip
//!
//! The driver already reports an inode number for every file it stats or
//! lists. So the table can be checked against the driver's *other*
//! answer: walk the tree, note the number reported for each path, then
//! resolve each number through the export table and require the inode
//! that comes back to be the same file.
//!
//! That is a real check because the two answers come from different
//! places on disk. The walk reads a directory entry and follows its
//! reference into the inode table; the resolution reads the export
//! table's entry for that number and follows a reference it stores
//! independently. Nothing but the image itself makes them agree.
//!
//! The failure this guards against is specific: inode numbers are
//! 1-based, so an off-by-one returns a real inode that is simply the
//! wrong one. A caller resolving a file identifier has no way at all to
//! notice that, which is what makes it worth a test that compares
//! identities rather than merely checking a lookup succeeds.
//!
//! Needs `mksquashfs`, and skips without it.

mod common;
use common::{dir, file, mksquashfs_available, symlink, ImageArtifact, Node};

use fs_squashfs::Filesystem;
use std::process::Command;

/// A tree with one of everything the export table has to cover: several
/// directories, several files, and a symlink — the inode types are
/// written to different arms of the parser, and the table indexes all of
/// them alike.
fn tree() -> Node {
    dir(vec![
        ("a.txt", file(b"first\n")),
        ("b.txt", file(b"second\n")),
        ("link", symlink("a.txt")),
        (
            "sub",
            dir(vec![
                ("c.txt", file(b"third\n")),
                ("deep", dir(vec![("d.txt", file(b"fourth\n"))])),
            ]),
        ),
    ])
}

/// Every path in the image, by walking from the root.
fn walk(fs: &Filesystem, at: &str, out: &mut Vec<String>) {
    let Ok(inode) = fs.lookup_path(at) else {
        return;
    };
    if !inode.is_dir() {
        return;
    }
    for e in fs.read_dir(&inode).expect("read_dir") {
        let name = String::from_utf8_lossy(&e.name).to_string();
        let child = if at == "/" {
            format!("/{name}")
        } else {
            format!("{at}/{name}")
        };
        out.push(child.clone());
        walk(fs, &child, out);
    }
}

/// Build the tree above with the given extra `mksquashfs` arguments.
/// `-no-exports` is one of them, so this file needs the spelling that
/// lets it pass either.
fn build(extra: &[&str]) -> ImageArtifact {
    common::build_with_mksquashfs_args("gzip", &tree(), extra)
}

#[test]
fn every_inode_number_resolves_to_the_file_it_names() {
    if !mksquashfs_available() {
        eprintln!("mksquashfs not on PATH — skipping");
        return;
    }
    let image = build(&[]);
    let fs = common::open_image_path(&image.path);
    assert!(
        fs.is_exportable(),
        "mksquashfs builds an export table unless told not to, and this image has none"
    );

    let mut paths = vec!["/".to_string()];
    walk(&fs, "/", &mut paths);
    assert!(
        paths.len() >= 7,
        "only {} paths — the comparison would prove little",
        paths.len()
    );

    for path in &paths {
        let by_path = fs.lookup_path(path).expect("lookup by path");
        let by_number = fs
            .read_inode_by_number(by_path.inode_number)
            .unwrap_or_else(|e| {
                panic!(
                    "{path}: inode {} did not resolve: {e}",
                    by_path.inode_number
                )
            });

        // Identity, not merely success. An off-by-one in the table's
        // indexing returns a real inode, and comparing only that the
        // lookup succeeded would pass.
        assert_eq!(
            by_number.inode_number, by_path.inode_number,
            "{path}: resolving inode {} produced inode {}",
            by_path.inode_number, by_number.inode_number
        );
        assert_eq!(by_number.inode_type, by_path.inode_type, "{path}: type");
        assert_eq!(by_number.file_size, by_path.file_size, "{path}: size");
        assert_eq!(by_number.permissions, by_path.permissions, "{path}: mode");
        assert_eq!(by_number.mtime, by_path.mtime, "{path}: mtime");
        assert_eq!(
            by_number.symlink_target, by_path.symlink_target,
            "{path}: symlink target"
        );
    }
}

/// A resolved regular file must read back as the same bytes the path
/// does. Metadata agreeing is not enough: the inode carries where the
/// data lives, and two inodes of the same size and mode are easy to
/// confuse.
#[test]
fn a_file_resolved_by_number_reads_the_same_bytes() {
    if !mksquashfs_available() {
        eprintln!("mksquashfs not on PATH — skipping");
        return;
    }
    let image = build(&[]);
    let fs = common::open_image_path(&image.path);
    for (path, want) in [
        ("/a.txt", &b"first\n"[..]),
        ("/b.txt", &b"second\n"[..]),
        ("/sub/c.txt", &b"third\n"[..]),
        ("/sub/deep/d.txt", &b"fourth\n"[..]),
    ] {
        let by_path = fs.lookup_path(path).expect(path);
        let inode = fs
            .read_inode_by_number(by_path.inode_number)
            .expect("resolve");
        let mut buf = vec![0u8; inode.file_size as usize];
        let n = fs.read_file(&inode, 0, &mut buf).expect("read");
        assert_eq!(&buf[..n], want, "{path} read differently by number");
    }
}

/// Inode numbers are 1-based and bounded by the superblock's count.
/// Zero is not an inode, and neither is one past the end.
#[test]
fn numbers_outside_the_image_are_refused() {
    if !mksquashfs_available() {
        eprintln!("mksquashfs not on PATH — skipping");
        return;
    }
    let image = build(&[]);
    let fs = common::open_image_path(&image.path);
    let count = fs.sb.inode_count;
    assert!(count > 0);

    assert!(
        fs.read_inode_by_number(0).is_err(),
        "inode 0 does not exist, and returning inode 1 for it would be the \
         off-by-one this table is most likely to have"
    );
    assert!(fs.read_inode_by_number(count + 1).is_err());
    assert!(fs.read_inode_by_number(u32::MAX).is_err());
    // The last real one does resolve, so the bound is not off the other
    // way either.
    assert!(fs.read_inode_by_number(count).is_ok());
}

/// `mksquashfs -no-exports` builds an image with no such map. Asking is
/// then a refusal that says so, and it has to be distinguishable from
/// "no such inode" — the first will never succeed for this image, the
/// second might for a different number.
#[test]
fn an_image_built_without_exports_says_so_rather_than_guessing() {
    if !mksquashfs_available() {
        eprintln!("mksquashfs not on PATH — skipping");
        return;
    }
    let image = build(&["-no-exports"]);
    let fs = common::open_image_path(&image.path);
    assert!(!fs.is_exportable());
    assert!(matches!(
        fs.read_inode_by_number(1),
        Err(fs_squashfs::Error::NotExportable)
    ));

    // And the same image still resolves paths perfectly well, so the
    // absence costs only this one question.
    assert_eq!(
        common::read_whole_file(&fs, "/a.txt"),
        b"first\n".to_vec(),
        "an image without an export table stopped reading"
    );
}

/// The export flag in the superblock and the table's presence are two
/// separate statements on disk, and they must agree. If they ever did
/// not, a caller would be told one thing by the flag and another by the
/// lookup.
#[test]
fn the_superblock_flag_agrees_with_whether_the_table_is_there() {
    if !mksquashfs_available() {
        eprintln!("mksquashfs not on PATH — skipping");
        return;
    }
    /// `SQUASHFS_EXPORTABLE`, bit 7 of the superblock's flags word.
    const EXPORTABLE: u16 = 0x0080;
    for (extra, expect) in [(&[][..], true), (&["-no-exports"][..], false)] {
        let image = build(extra);
        let fs = common::open_image_path(&image.path);
        assert_eq!(
            fs.sb.flags & EXPORTABLE != 0,
            expect,
            "the flag disagrees with what mksquashfs {extra:?} was asked for"
        );
        assert_eq!(
            fs.is_exportable(),
            expect,
            "the flag and the table disagree for mksquashfs {extra:?}"
        );
    }
}

/// The same check through the C surface, which is where a consumer holding
/// a file identifier actually lives.
#[test]
fn the_c_surface_resolves_a_number_the_same_way() {
    if !mksquashfs_available() {
        eprintln!("mksquashfs not on PATH — skipping");
        return;
    }
    use fs_squashfs::capi::*;
    use std::ffi::CString;

    let image = build(&[]);
    let c_path = CString::new(image.path.to_str().unwrap()).unwrap();
    let fs = unsafe { fs_squashfs_mount(c_path.as_ptr()) };
    assert!(!fs.is_null());
    assert_eq!(unsafe { fs_squashfs_is_exportable(fs) }, 1);

    let mut by_path: fs_squashfs_attr_t = unsafe { std::mem::zeroed() };
    let inner = CString::new("/sub/c.txt").unwrap();
    assert_eq!(
        unsafe { fs_squashfs_stat(fs, inner.as_ptr(), &mut by_path) },
        0
    );

    let mut by_ino: fs_squashfs_attr_t = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { fs_squashfs_stat_ino(fs, by_path.inode, &mut by_ino) },
        0
    );
    assert_eq!(by_ino.inode, by_path.inode);
    assert_eq!(by_ino.size, by_path.size);
    assert_eq!(by_ino.file_type, by_path.file_type);
    assert_eq!(by_ino.mtime, by_path.mtime);

    // Out of range fails as ENOENT rather than as a hardware fault.
    assert_eq!(
        unsafe { fs_squashfs_stat_ino(fs, u32::MAX, &mut by_ino) },
        -1
    );
    assert_eq!(fs_squashfs_last_errno(), 2, "expected ENOENT");
    // NULL tolerance: a failure, not a crash inside the caller.
    assert_eq!(
        unsafe { fs_squashfs_stat_ino(std::ptr::null_mut(), 1, &mut by_ino) },
        -1
    );
    assert_eq!(
        unsafe { fs_squashfs_stat_ino(fs, 1, std::ptr::null_mut()) },
        -1
    );
    assert_eq!(
        unsafe { fs_squashfs_is_exportable(std::ptr::null_mut()) },
        -1
    );
    unsafe { fs_squashfs_umount(fs) };

    // An image with no table answers ENOTSUP, which a caller can tell
    // apart from ENOENT and act on.
    let no_exports = build(&["-no-exports"]);
    let c_path = CString::new(no_exports.path.to_str().unwrap()).unwrap();
    let fs = unsafe { fs_squashfs_mount(c_path.as_ptr()) };
    assert!(!fs.is_null());
    assert_eq!(unsafe { fs_squashfs_is_exportable(fs) }, 0);
    assert_eq!(unsafe { fs_squashfs_stat_ino(fs, 1, &mut by_ino) }, -1);
    assert_eq!(fs_squashfs_last_errno(), 45, "expected ENOTSUP");
    unsafe { fs_squashfs_umount(fs) };
}

/// `unsquashfs` is not asked to resolve an inode number — it has no such
/// mode — so this checks the one thing it can say: the image really does
/// hold the number of inodes the export table is sized from.
#[test]
fn the_inode_count_the_table_is_sized_from_is_the_one_unsquashfs_counts() {
    if !mksquashfs_available() {
        eprintln!("mksquashfs not on PATH — skipping");
        return;
    }
    let image = build(&[]);
    let fs = common::open_image_path(&image.path);
    let out = Command::new("unsquashfs")
        .args(["-s"])
        .arg(&image.path)
        .output()
        .expect("spawn unsquashfs");
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    let reported: u32 = text
        .lines()
        .find_map(|l| l.strip_prefix("Number of inodes "))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or_else(|| panic!("unsquashfs -s did not report an inode count:\n{text}"));
    assert_eq!(
        fs.sb.inode_count, reported,
        "the count the export table is sized from is not the one unsquashfs sees"
    );
    // And every one of them resolves, so the table really does cover the
    // whole range rather than only the part a walk happens to reach.
    for n in 1..=reported {
        fs.read_inode_by_number(n)
            .unwrap_or_else(|e| panic!("inode {n} of {reported} did not resolve: {e}"));
    }
}
