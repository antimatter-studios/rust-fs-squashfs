//! `fs_squashfs_readlink` against `unsquashfs`, for links `mksquashfs`
//! built from a real source tree.
//!
//! The C-ABI tests in `capi_basic.rs` read the committed fixture, whose
//! one link this crate's own reader already agrees with. This file asks a
//! third implementation: `unsquashfs -lls` lists every symlink's size and
//! target, and the C ABI must return that size as its result and write
//! that target, NUL-terminated, into the buffer.
//!
//! The targets cover a short relative link, an absolute one, one with a
//! space, and one longer than 256 bytes so a length held in a byte, or a
//! fixed-size buffer somewhere in the path, cannot pass.

mod common;
use common::{dir, file, symlink};

use fs_squashfs::capi::*;
use fs_squashfs_test_support::oracle;
use std::collections::BTreeMap;
use std::ffi::{c_char, c_int, CString};

/// `name -> (size, target)` for every symlink `unsquashfs -lls` lists.
fn unsquashfs_links(image: &std::path::Path) -> BTreeMap<String, (usize, String)> {
    let out = oracle("unsquashfs").arg("-lls").arg(image).output();
    assert!(
        out.status.success(),
        "unsquashfs -lls failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let listing = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut links = BTreeMap::new();
    for line in listing.lines().filter(|l| l.starts_with('l')) {
        // lrwxrwxrwx root/root  9 2024-01-01 00:00 squashfs-root/NAME -> TARGET
        let size: usize = line
            .split_whitespace()
            .nth(2)
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| panic!("no size in {line:?}"));
        let (_, rest) = line
            .split_once(" squashfs-root/")
            .unwrap_or_else(|| panic!("no path in {line:?}"));
        let (name, target) = rest
            .split_once(" -> ")
            .unwrap_or_else(|| panic!("no target in {line:?}"));
        links.insert(name.to_owned(), (size, target.to_owned()));
    }
    links
}

#[test]
fn readlink_returns_the_length_and_target_unsquashfs_reports() {
    let long = format!("{}/end", "segment-".repeat(40)); // 324 bytes
    let targets = [
        ("short", "hello.txt".to_owned()),
        ("absolute", "/usr/share/doc/README".to_owned()),
        ("spaced", "a dir/with space.txt".to_owned()),
        ("long", long),
    ];
    let mut entries = vec![("hello.txt", file(b"hi\n"))];
    entries.extend(targets.iter().map(|(n, t)| (*n, symlink(t))));
    let image = common::build_with_mksquashfs("gzip", &dir(entries));

    let reference = unsquashfs_links(&image.path);
    assert_eq!(
        reference.len(),
        targets.len(),
        "unsquashfs does not list the links as built: {reference:?}"
    );

    let img = CString::new(image.path.to_str().unwrap()).unwrap();
    let fs = unsafe { fs_squashfs_mount(img.as_ptr()) };
    assert!(!fs.is_null(), "mount failed");
    for (name, (size, target)) in &reference {
        assert_eq!(
            *size,
            target.len(),
            "{name}: unsquashfs disagrees with itself"
        );
        let path = CString::new(format!("/{name}")).unwrap();

        // Exactly length + 1: the smallest buffer the contract accepts.
        let mut buf: Vec<c_char> = vec![0x5A; size + 1];
        let rc = unsafe { fs_squashfs_readlink(fs, path.as_ptr(), buf.as_mut_ptr(), buf.len()) };
        assert_eq!(
            rc, *size as c_int,
            "{name}: readlink must return the length"
        );
        let got: Vec<u8> = buf[..*size].iter().map(|&b| b as u8).collect();
        assert_eq!(
            String::from_utf8_lossy(&got),
            target.as_str(),
            "{name}: target differs from unsquashfs"
        );
        assert_eq!(buf[*size], 0, "{name}: not NUL-terminated");

        // One byte fewer is refused, and the buffer is left alone.
        let mut short: Vec<c_char> = vec![0x5A; *size];
        let rc =
            unsafe { fs_squashfs_readlink(fs, path.as_ptr(), short.as_mut_ptr(), short.len()) };
        assert_eq!(
            rc, -1,
            "{name}: a buffer without room for the NUL must fail"
        );
        assert_eq!(fs_squashfs_last_errno(), 34 /* ERANGE */, "{name}");
        assert!(short.iter().all(|&b| b == 0x5A), "{name}: buffer written");
    }
    unsafe { fs_squashfs_umount(fs) };
}
