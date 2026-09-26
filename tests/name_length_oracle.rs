//! The longest names SquashFS allows, through the C ABI, against images
//! `mksquashfs` wrote.
//!
//! The format and the kernel allow 256-byte names (`SQUASHFS_NAME_LEN`),
//! and the parser accepts them. `fs_squashfs_dirent_t` held
//! `char name[256]` with a `uint8_t name_len`, so a 256-byte name lost
//! its last byte to the NUL and was reported as 255 -- a name that then
//! does not open, beside a name that opens and is never listed (#56).
//!
//! 255 and 256 are the two lengths that matter; the existing stress test
//! covers 255 through the Rust API only. A 256-byte name cannot be
//! created on most host filesystems (ext4 and APFS stop at 255), so the
//! file is made with a `mksquashfs -p` pseudo definition instead.
//!
//! The oracle is `unsquashfs -lln`, which must list both names at full
//! length before the driver's listing is compared with it.
//!
//! Needs `mksquashfs` and `unsquashfs`; skips without them, fails in CI.

mod common;
use common::{dir, file};

use fs_squashfs::capi::*;
use std::collections::BTreeSet;
use std::ffi::{CStr, CString};
use std::process::Command;

#[test]
fn names_of_255_and_256_bytes_list_at_full_length_and_open() {
    let n255 = "a".repeat(255);
    let n256 = "b".repeat(256);
    let p255 = format!("{n255} f 644 0 0 echo hi");
    let p256 = format!("{n256} f 644 0 0 echo hi");
    let image = common::build_with_mksquashfs_args(
        "gzip",
        &dir(vec![("short.txt", file(b"hi\n"))]),
        &["-no-xattrs", "-p", &p255, "-p", &p256],
    );

    // The reference lists both at full length.
    let out = Command::new("unsquashfs")
        .arg("-lln")
        .arg(&image.path)
        .output()
        .expect("spawn unsquashfs");
    assert!(out.status.success());
    let listing = String::from_utf8_lossy(&out.stdout);
    let reference: BTreeSet<String> = listing
        .lines()
        .filter_map(|l| l.split_whitespace().last())
        .filter_map(|p| p.strip_prefix("squashfs-root/"))
        .map(str::to_owned)
        .collect();
    let want: BTreeSet<String> = [n255.clone(), n256.clone(), "short.txt".into()].into();
    assert_eq!(
        reference, want,
        "unsquashfs -lln does not list the names as built"
    );

    let img = CString::new(image.path.to_str().unwrap()).unwrap();
    let fs = unsafe { fs_squashfs_mount(img.as_ptr()) };
    assert!(!fs.is_null(), "mount failed");
    let root = CString::new("/").unwrap();
    let iter = unsafe { fs_squashfs_dir_open(fs, root.as_ptr()) };
    assert!(!iter.is_null(), "dir_open / failed");

    let mut listed = BTreeSet::new();
    loop {
        let de = unsafe { fs_squashfs_dir_next(iter) };
        if de.is_null() {
            break;
        }
        let de = unsafe { &*de };
        let name = unsafe { CStr::from_ptr(de.name.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            usize::from(de.name_len),
            name.len(),
            "name_len disagrees with the NUL-terminated name {name:?}"
        );
        listed.insert(name);
    }
    unsafe { fs_squashfs_dir_close(iter) };
    assert_eq!(
        listed.iter().map(String::len).collect::<Vec<_>>(),
        reference.iter().map(String::len).collect::<Vec<_>>(),
        "the C ABI listed names at different lengths from unsquashfs"
    );
    assert_eq!(
        listed, reference,
        "the C ABI listing differs from unsquashfs"
    );

    // Every name the listing handed back opens.
    for name in &listed {
        let path = CString::new(format!("/{name}")).unwrap();
        let mut attr: fs_squashfs_attr_t = unsafe { std::mem::zeroed() };
        let rc = unsafe { fs_squashfs_stat(fs, path.as_ptr(), &mut attr) };
        assert_eq!(rc, 0, "a listed name ({} bytes) does not open", name.len());
        assert_eq!(attr.size, 3, "{} bytes: wrong file", name.len());
    }
    unsafe { fs_squashfs_umount(fs) };
}
