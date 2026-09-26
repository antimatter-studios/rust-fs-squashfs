//! Device nodes, against images `mksquashfs` wrote.
//!
//! A block or character device's identity is its major and minor
//! number; its type alone says nothing about what it is. The parser read
//! the on-disk `rdev` and dropped it, so every device in an image came
//! out as 0:0 (#57) -- a `/dev` full of nodes that point at nothing.
//!
//! # The oracle
//!
//! `mksquashfs -p` writes device nodes from pseudo definitions, so no
//! privilege is needed to create them, and `unsquashfs -lln` reports
//! the major and minor it reads back. Nothing in that comparison came
//! from this repository.
//!
//! Only majors below 4096 and minors below 256 are used. Above that the
//! reference tools disagree with each other: squashfs-tools 4.5.1's
//! `unsquashfs -lln` decodes the `u32` as `major << 8 | minor`, while the
//! kernel (and this crate) use the Linux `new_encode_dev` layout, which
//! agrees with it only in that range.
//!
//! Needs `mksquashfs` and `unsquashfs`, and skips without them -- except
//! in CI, where `common::tool_available` fails instead.

mod common;
use common::{dir, file, ImageArtifact};

use fs_squashfs_test_support::oracle;
use std::collections::BTreeMap;

/// `(name, kind, major, minor)` for each device the image carries.
const DEVICES: &[(&str, char, u32, u32)] = &[
    ("loop0", 'b', 7, 0),
    ("null", 'c', 1, 3),
    // A major above 255 needs the high bits of the 12-bit major field.
    ("nvme0n1", 'b', 259, 1),
];

fn build(extra: &[&str]) -> ImageArtifact {
    let mut args: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
    for (name, kind, major, minor) in DEVICES {
        args.push("-p".into());
        args.push(format!("{name} {kind} 660 0 0 {major} {minor}"));
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    common::build_with_mksquashfs_args("gzip", &dir(vec![("readme.txt", file(b"hi\n"))]), &args)
}

/// `name -> (kind, major, minor)` as `unsquashfs -lln` reports it.
fn reference(image: &ImageArtifact) -> BTreeMap<String, (char, u32, u32)> {
    let out = oracle("unsquashfs").arg("-lln").arg(&image.path).output();
    assert!(
        out.status.success(),
        "unsquashfs -lln failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    parse_device_lines(&String::from_utf8_lossy(&out.stdout))
}

/// Pull `kind major, minor ... name` out of `ls -l`-shaped lines, where
/// the device number sits in the size column as `7,  0`.
fn parse_device_lines(listing: &str) -> BTreeMap<String, (char, u32, u32)> {
    let mut found = BTreeMap::new();
    for line in listing.lines() {
        let kind = match line.chars().next() {
            Some(k @ ('b' | 'c')) => k,
            _ => continue,
        };
        let Some((before, after)) = line.split_once(',') else {
            continue;
        };
        let major = before
            .split_whitespace()
            .last()
            .and_then(|m| m.parse().ok())
            .unwrap_or_else(|| panic!("no major in {line:?}"));
        let minor = after
            .split_whitespace()
            .next()
            .and_then(|m| m.parse().ok())
            .unwrap_or_else(|| panic!("no minor in {line:?}"));
        let name = line
            .rsplit('/')
            .next()
            .and_then(|n| n.split_whitespace().last())
            .unwrap_or_else(|| panic!("no name in {line:?}"))
            .to_string();
        found.insert(name, (kind, major, minor));
    }
    found
}

// `tools_ready()` used to live here, answering "are mksquashfs and
// unsquashfs on PATH" so its callers could return early. The tools are in
// the harness guest now, so the answer is always yes or the run fails
// saying why — there is nothing left for it to report.

/// `lssquashfs ls /` prints the major and minor where `ls -l` does, and
/// they are the ones `unsquashfs -lln` reads out of the same image.
#[test]
fn lssquashfs_reports_the_device_numbers_unsquashfs_reads() {
    let image = build(&["-no-xattrs"]);
    let expected = reference(&image);
    // Control: the reference saw every device the image was built with,
    // so an empty parse cannot make the comparison vacuous.
    for (name, kind, major, minor) in DEVICES {
        assert_eq!(
            expected.get(*name),
            Some(&(*kind, *major, *minor)),
            "unsquashfs -lln does not report {name} as built: {expected:?}"
        );
    }

    let out = assert_cmd::Command::cargo_bin("lssquashfs")
        .unwrap()
        .arg(&image.path)
        .args(["ls", "/"])
        .output()
        .expect("spawn lssquashfs");
    assert!(out.status.success(), "lssquashfs ls / failed");
    let ours = parse_device_lines(&String::from_utf8_lossy(&out.stdout));
    assert_eq!(
        ours,
        expected,
        "lssquashfs and unsquashfs disagree on the device nodes:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// The same numbers through the Rust API and through `fs_squashfs_stat`,
/// with the raw value checked against the kernel's packing.
#[test]
fn the_api_and_the_c_abi_report_the_device_numbers_unsquashfs_reads() {
    let image = build(&["-no-xattrs"]);
    let expected = reference(&image);
    check_api_and_abi(&image, &expected);
}

/// Extended device inodes are not reached here: squashfs-tools 4.6.1
/// writes the basic form for pseudo devices even with `-xattrs-add` or a
/// pseudo `x` attribute, measured. `src/inode.rs`'s
/// `an_extended_device_inode_keeps_rdev_and_its_xattr_index` covers that
/// arm from bytes laid out per `squashfs_fs.h`.
fn check_api_and_abi(image: &ImageArtifact, expected: &BTreeMap<String, (char, u32, u32)>) {
    use fs_squashfs::capi::*;
    use fs_squashfs::inode::{TYPE_BASIC_BLKDEV, TYPE_BASIC_CHRDEV};

    let fs = common::open_image_path(&image.path);
    let img = std::ffi::CString::new(image.path.to_str().unwrap()).unwrap();
    let cfs = unsafe { fs_squashfs_mount(img.as_ptr()) };
    assert!(!cfs.is_null(), "fs_squashfs_mount failed");

    for (name, kind, major, minor) in DEVICES {
        assert_eq!(expected.get(*name), Some(&(*kind, *major, *minor)));
        let path = format!("/{name}");
        let inode = fs.lookup_path(&path).expect("lookup device");
        let want_type = match kind {
            'b' => TYPE_BASIC_BLKDEV,
            _ => TYPE_BASIC_CHRDEV,
        };
        assert_eq!(inode.inode_type, want_type, "{path}: inode form");
        // Linux new_encode_dev, which for these ranges is major << 8 | minor.
        let raw = (major << 8) | minor;
        assert_eq!(inode.rdev, raw, "{path}: raw rdev");
        assert_eq!(
            (inode.rdev_major(), inode.rdev_minor()),
            (*major, *minor),
            "{path}: decoded"
        );
        let c = std::ffi::CString::new(path.clone()).unwrap();
        let mut attr: fs_squashfs_attr_t = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { fs_squashfs_stat(cfs, c.as_ptr(), &mut attr) }, 0);
        assert_eq!(attr.rdev, raw, "{path}: fs_squashfs_stat rdev");
    }
    // A regular file reports no device number.
    let c = std::ffi::CString::new("/readme.txt").unwrap();
    let mut attr: fs_squashfs_attr_t = unsafe { std::mem::zeroed() };
    attr.rdev = 0xdead;
    assert_eq!(unsafe { fs_squashfs_stat(cfs, c.as_ptr(), &mut attr) }, 0);
    assert_eq!(attr.rdev, 0, "a regular file carried a device number");
    unsafe { fs_squashfs_umount(cfs) };
}
