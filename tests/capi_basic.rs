//! C-ABI smoke tests — invoke the `fs_squashfs_*` functions directly via
//! the rlib.
//!
//! Staticlibs don't re-export unmangled C symbols to integration tests, so
//! instead of `extern "C" { fs_squashfs_mount ... }` we call the public
//! items in `fs_squashfs::capi` directly. This verifies the *logic* behind
//! the exports; the actual ABI symbol surface is verified by downstream
//! consumers linking `libfs_squashfs.a`.
//!
//! Everything here reads the committed `test-disks/squashfs-basic.sqfs`
//! fixture, so the whole file runs under a plain `cargo test` — no
//! squashfs-tools required.

mod common;

use std::ffi::{c_char, c_int, CStr, CString};

use common::basic_fixture_path;
use fs_squashfs::capi::*;

/// The committed fixture as a NUL-terminated path.
fn fixture_cstr() -> CString {
    CString::new(basic_fixture_path().to_str().unwrap()).unwrap()
}

fn last_err_str() -> String {
    unsafe {
        let p = fs_squashfs_last_error();
        if p.is_null() {
            return "<null>".into();
        }
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

/// Mount the committed fixture or panic with the last error.
fn mount_fixture() -> *mut fs_squashfs_fs_t {
    let path = fixture_cstr();
    let fs = unsafe { fs_squashfs_mount(path.as_ptr()) };
    assert!(!fs.is_null(), "mount returned NULL: {}", last_err_str());
    fs
}

#[test]
fn mount_and_umount_basic_image() {
    let fs = mount_fixture();
    unsafe { fs_squashfs_umount(fs) };
}

#[test]
fn umount_null_is_safe() {
    // Documented contract: safe to call with NULL.
    unsafe { fs_squashfs_umount(std::ptr::null_mut()) };
}

#[test]
fn mount_rejects_missing_file() {
    let path = CString::new("/tmp/definitely-does-not-exist-sqfs-xyz").unwrap();
    let fs = unsafe { fs_squashfs_mount(path.as_ptr()) };
    assert!(fs.is_null(), "mount should have failed");
    let err = last_err_str();
    assert!(
        err.contains("open") || err.contains("No such") || err.contains("mount"),
        "err was: {err}"
    );
    assert_eq!(fs_squashfs_last_errno(), 5 /* EIO */);
}

#[test]
fn mount_rejects_null_path() {
    let fs = unsafe { fs_squashfs_mount(std::ptr::null()) };
    assert!(fs.is_null());
    assert_eq!(fs_squashfs_last_errno(), 22 /* EINVAL */);
}

#[test]
fn mount_rejects_non_squashfs_bytes() {
    // A real file that isn't a SquashFS image: the crate's own Cargo.toml.
    let path = CString::new(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
    let fs = unsafe { fs_squashfs_mount(path.as_ptr()) };
    assert!(fs.is_null(), "mounting a non-squashfs file should fail");
    let err = last_err_str();
    assert!(
        err.contains("SquashFS") || err.contains("magic") || err.contains("superblock"),
        "err was: {err}"
    );
}

#[test]
fn volume_info_reports_expected_fields() {
    let fs = mount_fixture();
    let mut info = unsafe { std::mem::zeroed::<fs_squashfs_volume_info_t>() };
    let rc = unsafe { fs_squashfs_get_volume_info(fs, &mut info) };
    assert_eq!(rc, 0, "get_volume_info failed: {}", last_err_str());

    assert_eq!(info.block_size, 4096, "fixture built with -b 4096");
    assert_eq!(info.compression_id, 1, "gzip");
    assert_eq!(info.version_major, 4);
    assert!(info.inode_count >= 5, "at least 5 inodes");
    assert!(info.bytes_used > 0);
    // compression_name is a NUL-terminated "gzip".
    let name = unsafe { CStr::from_ptr(info.compression_name.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    assert_eq!(name, "gzip", "compression_name");

    unsafe { fs_squashfs_umount(fs) };
}

#[test]
fn volume_info_null_args_error_not_crash() {
    let fs = mount_fixture();
    unsafe {
        let rc = fs_squashfs_get_volume_info(std::ptr::null_mut(), std::ptr::null_mut());
        assert_eq!(rc, -1);
        let mut info = std::mem::zeroed::<fs_squashfs_volume_info_t>();
        let rc = fs_squashfs_get_volume_info(std::ptr::null_mut(), &mut info);
        assert_eq!(rc, -1);
        let rc = fs_squashfs_get_volume_info(fs, std::ptr::null_mut());
        assert_eq!(rc, -1);
        fs_squashfs_umount(fs);
    }
}

#[test]
fn stat_root_is_directory() {
    let fs = mount_fixture();
    let root = CString::new("/").unwrap();
    let mut attr = unsafe { std::mem::zeroed::<fs_squashfs_attr_t>() };
    let rc = unsafe { fs_squashfs_stat(fs, root.as_ptr(), &mut attr) };
    assert_eq!(rc, 0, "stat / failed: {}", last_err_str());
    // file_type 2 == DIR in the C ABI.
    assert_eq!(attr.file_type, 2, "root file_type != DIR");
    assert!(attr.link_count >= 2, "dir link_count >= 2");
    unsafe { fs_squashfs_umount(fs) };
}

#[test]
fn stat_regular_file_reports_size() {
    let fs = mount_fixture();
    let p = CString::new("/hello.txt").unwrap();
    let mut attr = unsafe { std::mem::zeroed::<fs_squashfs_attr_t>() };
    let rc = unsafe { fs_squashfs_stat(fs, p.as_ptr(), &mut attr) };
    assert_eq!(rc, 0, "stat /hello.txt failed: {}", last_err_str());
    assert_eq!(attr.file_type, 1, "REG_FILE");
    assert_eq!(attr.size, 3, "\"hi\\n\"");
    unsafe { fs_squashfs_umount(fs) };
}

#[test]
fn stat_empty_file_size_zero() {
    let fs = mount_fixture();
    let p = CString::new("/empty.txt").unwrap();
    let mut attr = unsafe { std::mem::zeroed::<fs_squashfs_attr_t>() };
    let rc = unsafe { fs_squashfs_stat(fs, p.as_ptr(), &mut attr) };
    assert_eq!(rc, 0, "stat /empty.txt failed: {}", last_err_str());
    assert_eq!(attr.file_type, 1, "REG_FILE");
    assert_eq!(attr.size, 0, "empty file");
    unsafe { fs_squashfs_umount(fs) };
}

#[test]
fn stat_symlink_classified_as_symlink() {
    let fs = mount_fixture();
    let p = CString::new("/link").unwrap();
    let mut attr = unsafe { std::mem::zeroed::<fs_squashfs_attr_t>() };
    let rc = unsafe { fs_squashfs_stat(fs, p.as_ptr(), &mut attr) };
    assert_eq!(rc, 0, "stat /link failed: {}", last_err_str());
    assert_eq!(attr.file_type, 7, "SYMLINK");
    unsafe { fs_squashfs_umount(fs) };
}

#[test]
fn stat_missing_path_returns_enoent() {
    let fs = mount_fixture();
    let missing = CString::new("/definitely-not-there-987").unwrap();
    let mut attr = unsafe { std::mem::zeroed::<fs_squashfs_attr_t>() };
    let rc = unsafe { fs_squashfs_stat(fs, missing.as_ptr(), &mut attr) };
    assert_eq!(rc, -1);
    assert_eq!(fs_squashfs_last_errno(), 2 /* ENOENT */);
    assert!(last_err_str().contains("not found") || last_err_str().contains("stat"));
    unsafe { fs_squashfs_umount(fs) };
}

#[test]
fn stat_null_args_error_not_crash() {
    unsafe {
        let mut attr = std::mem::zeroed::<fs_squashfs_attr_t>();
        let rc = fs_squashfs_stat(std::ptr::null_mut(), std::ptr::null(), &mut attr);
        assert_eq!(rc, -1);
    }
}

#[test]
fn dir_open_root_lists_all_entries() {
    let fs = mount_fixture();
    let root = CString::new("/").unwrap();
    let iter = unsafe { fs_squashfs_dir_open(fs, root.as_ptr()) };
    assert!(!iter.is_null(), "dir_open / failed: {}", last_err_str());

    let mut names = Vec::new();
    loop {
        let de = unsafe { fs_squashfs_dir_next(iter) };
        if de.is_null() {
            break;
        }
        let name_ptr = unsafe { (*de).name.as_ptr() };
        let name = unsafe { CStr::from_ptr(name_ptr).to_string_lossy().into_owned() };
        // name_len must match the C-string length.
        assert_eq!(
            unsafe { (*de).name_len } as usize,
            name.len(),
            "name_len mismatch for {name}"
        );
        names.push(name);
    }
    unsafe { fs_squashfs_dir_close(iter) };

    names.sort();
    assert_eq!(
        names,
        vec!["empty.txt", "hello.txt", "link", "sub"],
        "root listing"
    );
    unsafe { fs_squashfs_umount(fs) };
}

#[test]
fn dir_next_returns_correct_file_types() {
    let fs = mount_fixture();
    let root = CString::new("/").unwrap();
    let iter = unsafe { fs_squashfs_dir_open(fs, root.as_ptr()) };
    assert!(!iter.is_null());

    let mut by_name = std::collections::BTreeMap::new();
    loop {
        let de = unsafe { fs_squashfs_dir_next(iter) };
        if de.is_null() {
            break;
        }
        let name = unsafe { CStr::from_ptr((*de).name.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        by_name.insert(name, unsafe { (*de).file_type });
    }
    unsafe { fs_squashfs_dir_close(iter) };

    assert_eq!(by_name.get("hello.txt"), Some(&1u8), "REG_FILE");
    assert_eq!(by_name.get("sub"), Some(&2u8), "DIR");
    assert_eq!(by_name.get("link"), Some(&7u8), "SYMLINK");
    unsafe { fs_squashfs_umount(fs) };
}

#[test]
fn dir_open_on_file_is_enotdir() {
    let fs = mount_fixture();
    let p = CString::new("/hello.txt").unwrap();
    let iter = unsafe { fs_squashfs_dir_open(fs, p.as_ptr()) };
    assert!(iter.is_null(), "dir_open on a file must fail");
    assert_eq!(fs_squashfs_last_errno(), 20 /* ENOTDIR */);
    unsafe { fs_squashfs_umount(fs) };
}

#[test]
fn dir_open_missing_is_enoent() {
    let fs = mount_fixture();
    let p = CString::new("/no-such-dir").unwrap();
    let iter = unsafe { fs_squashfs_dir_open(fs, p.as_ptr()) };
    assert!(iter.is_null());
    assert_eq!(fs_squashfs_last_errno(), 2 /* ENOENT */);
    unsafe { fs_squashfs_umount(fs) };
}

#[test]
fn dir_next_on_null_iter_returns_null() {
    let de = unsafe { fs_squashfs_dir_next(std::ptr::null_mut()) };
    assert!(de.is_null());
}

#[test]
fn dir_close_null_is_safe() {
    unsafe { fs_squashfs_dir_close(std::ptr::null_mut()) };
}

#[test]
fn dir_open_null_args_error_not_crash() {
    let iter = unsafe { fs_squashfs_dir_open(std::ptr::null_mut(), std::ptr::null()) };
    assert!(iter.is_null());
}

/// The fixture's `/link` points at `hello.txt`: 9 bytes.
const LINK_TARGET: &[u8] = b"hello.txt";

/// A byte no readlink ever writes into these buffers, so "untouched" is
/// checkable: the target is ASCII and the terminator is 0.
const SENTINEL: c_char = 0x5A;

/// Call readlink on `/link` of the committed fixture with a buffer of
/// `bufsize` bytes pre-filled with [`SENTINEL`], returning the result,
/// errno, last error and the buffer as it was left.
fn readlink_link(bufsize: usize) -> (c_int, c_int, String, Vec<c_char>) {
    let fs = mount_fixture();
    let p = CString::new("/link").unwrap();
    // One spare byte past `bufsize`, so a write beyond the size the
    // caller declared shows up too.
    let mut buf = vec![SENTINEL; bufsize + 1];
    let rc = unsafe { fs_squashfs_readlink(fs, p.as_ptr(), buf.as_mut_ptr(), bufsize) };
    let (errno, msg) = (fs_squashfs_last_errno(), last_err_str());
    unsafe { fs_squashfs_umount(fs) };
    (rc, errno, msg, buf)
}

#[test]
fn readlink_returns_the_target_length_and_writes_it_nul_terminated() {
    let (rc, _, msg, buf) = readlink_link(256);
    assert_eq!(
        rc,
        LINK_TARGET.len() as c_int,
        "readlink /link must return the target length, like readlink(2): {msg}"
    );
    let written: Vec<u8> = buf[..LINK_TARGET.len()]
        .iter()
        .map(|&b| b.to_ne_bytes()[0])
        .collect();
    assert_eq!(written, LINK_TARGET);
    assert_eq!(buf[LINK_TARGET.len()], 0, "the target is NUL-terminated");
    assert_eq!(
        buf[LINK_TARGET.len() + 1],
        SENTINEL,
        "nothing is written past the NUL"
    );
}

#[test]
fn readlink_fits_exactly_when_bufsize_is_length_plus_one() {
    let (rc, errno, msg, buf) = readlink_link(LINK_TARGET.len() + 1);
    assert_eq!(
        rc,
        LINK_TARGET.len() as c_int,
        "exact fit must succeed: {msg}"
    );
    assert_eq!(errno, 0);
    let written: Vec<u8> = buf[..LINK_TARGET.len()]
        .iter()
        .map(|&b| b.to_ne_bytes()[0])
        .collect();
    assert_eq!(written, LINK_TARGET);
    assert_eq!(buf[LINK_TARGET.len()], 0);
    assert_eq!(buf[LINK_TARGET.len() + 1], SENTINEL);
}

#[test]
fn readlink_one_byte_short_is_erange_and_writes_nothing() {
    // Room for the target but not its NUL: Linux would truncate; this
    // contract refuses instead.
    let (rc, errno, msg, buf) = readlink_link(LINK_TARGET.len());
    assert_eq!(rc, -1);
    assert_eq!(errno, 34 /* ERANGE */);
    assert!(
        msg.contains(&(LINK_TARGET.len() + 1).to_string()),
        "the error must name the size needed: {msg:?}"
    );
    assert!(
        buf.iter().all(|&b| b == SENTINEL),
        "a refused readlink must leave the buffer untouched: {buf:?}"
    );
}

#[test]
fn readlink_buffer_too_small_is_erange() {
    let (rc, errno, _, buf) = readlink_link(4);
    assert_eq!(rc, -1);
    assert_eq!(errno, 34 /* ERANGE */);
    assert!(
        buf.iter().all(|&b| b == SENTINEL),
        "buffer written: {buf:?}"
    );
}

#[test]
fn readlink_zero_bufsize_is_erange() {
    // Zero is just the smallest buffer too small to hold the target.
    let (rc, errno, msg, buf) = readlink_link(0);
    assert_eq!(rc, -1);
    assert_eq!(errno, 34 /* ERANGE */);
    assert!(
        msg.contains(&(LINK_TARGET.len() + 1).to_string()),
        "the error must name the size needed: {msg:?}"
    );
    assert!(
        buf.iter().all(|&b| b == SENTINEL),
        "buffer written: {buf:?}"
    );
}

#[test]
fn readlink_on_regular_file_is_einval() {
    let fs = mount_fixture();
    let p = CString::new("/hello.txt").unwrap();
    let mut buf: [c_char; 256] = [SENTINEL; 256];
    let rc = unsafe { fs_squashfs_readlink(fs, p.as_ptr(), buf.as_mut_ptr(), buf.len()) };
    assert_eq!(rc, -1, "readlink on a file must fail");
    assert_eq!(fs_squashfs_last_errno(), 22 /* EINVAL */);
    assert!(buf.iter().all(|&b| b == SENTINEL), "buffer written");
    unsafe { fs_squashfs_umount(fs) };
}

#[test]
fn readlink_on_a_missing_path_is_enoent() {
    let fs = mount_fixture();
    let p = CString::new("/no-such-link").unwrap();
    let mut buf: [c_char; 256] = [SENTINEL; 256];
    let rc = unsafe { fs_squashfs_readlink(fs, p.as_ptr(), buf.as_mut_ptr(), buf.len()) };
    assert_eq!(rc, -1, "readlink on a missing path must fail");
    assert_eq!(fs_squashfs_last_errno(), 2 /* ENOENT */);
    assert!(buf.iter().all(|&b| b == SENTINEL), "buffer written");
    unsafe { fs_squashfs_umount(fs) };
}

#[test]
fn readlink_null_buf_is_einval_even_when_it_would_be_too_small() {
    let fs = mount_fixture();
    let p = CString::new("/link").unwrap();
    for bufsize in [0, 256] {
        let rc = unsafe { fs_squashfs_readlink(fs, p.as_ptr(), std::ptr::null_mut(), bufsize) };
        assert_eq!(rc, -1);
        assert_eq!(
            fs_squashfs_last_errno(),
            22, /* EINVAL */
            "bufsize {bufsize}"
        );
    }
    unsafe { fs_squashfs_umount(fs) };
}

#[test]
fn readlink_null_args_error_not_crash() {
    unsafe {
        let rc = fs_squashfs_readlink(
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null_mut(),
            0,
        );
        assert_eq!(rc, -1);
        assert_eq!(fs_squashfs_last_errno(), 22 /* EINVAL */);
    }
}

/// The C ABI refuses the stat rather than reporting root, and leaves
/// `attr` alone when it does.
///
/// `fill_attr` resolves the owner before it writes anything, so a
/// caller handed -1 has a struct it never had a reason to read rather
/// than one filled in as far as the failure. The image is the committed
/// fixture with `id_count` patched from 2 to 1, which puts the gid
/// index its inodes carry past the end of the table.
#[test]
fn stat_refuses_an_id_index_past_the_table_without_touching_attr() {
    let mut bytes = std::fs::read(basic_fixture_path()).unwrap();
    bytes[0x1A..0x1C].copy_from_slice(&1u16.to_le_bytes());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("short-id-table.sqfs");
    std::fs::write(&path, &bytes).unwrap();

    let cpath = CString::new(path.to_str().unwrap()).unwrap();
    let fs = unsafe { fs_squashfs_mount(cpath.as_ptr()) };
    assert!(!fs.is_null(), "mount returned NULL: {}", last_err_str());

    let p = CString::new("/hello.txt").unwrap();
    let mut attr = unsafe { std::mem::zeroed::<fs_squashfs_attr_t>() };
    let rc = unsafe { fs_squashfs_stat(fs, p.as_ptr(), &mut attr) };
    assert_eq!(rc, -1, "stat reported success on an unresolvable owner");
    assert_eq!(fs_squashfs_last_errno(), 5 /* EIO */);
    assert_eq!(attr.gid, 0, "gid was written");
    assert_eq!(attr.uid, 0, "uid was written");
    assert_eq!(
        attr.size, 0,
        "attr was filled in as far as the failure, so a caller ignoring \
         the return code reads a half-built struct"
    );
    unsafe { fs_squashfs_umount(fs) };
}

// ---------------------------------------------------------------------------
// A path the C ABI cannot decode
// ---------------------------------------------------------------------------

/// A path whose bytes are not UTF-8, as a C string.
fn undecodable_path() -> Vec<c_char> {
    // "/caf\xe9.txt" — latin-1 for "café.txt", which is what a name
    // written on a Linux box with a non-UTF-8 locale looks like.
    let mut bytes: Vec<u8> = b"/caf".to_vec();
    bytes.push(0xE9);
    bytes.extend_from_slice(b".txt\0");
    // `c_char` is `i8` on x86_64 and Apple targets and `u8` on
    // aarch64-linux, so neither `as i8` nor `as c_char` is right on all of
    // them: the first does not type-check there and the second is a
    // no-op cast clippy refuses. `from_ne_bytes` exists on both (#78).
    bytes
        .into_iter()
        .map(|b| c_char::from_ne_bytes([b]))
        .collect()
}

/// A non-UTF-8 path that names no file is `ENOENT`, and never the root.
///
/// TWO CONTRACTS, ONE TEST. The first is #43's, and it has not changed:
/// `cstr_to_str` used to return `""` for anything undecodable, and the
/// empty string is not an error downstream — `lookup_path` splits it
/// into one empty component, drops it, and returns the root inode as a
/// successful lookup. So the call answered 0 and filled `attr` with the
/// root directory's inode number, mode, size and mtime, indistinguishable
/// from a real hit. That must never come back.
///
/// The second is #67's, and it replaces the `EINVAL` this test used to
/// assert. These bytes are not "undecodable" any more: names are bytes,
/// the path is compared byte for byte, and this one simply names no file
/// in the fixture. `ENOENT` is the honest answer — the caller's argument
/// was fine, the file is not there. `EINVAL` would now be wrong, and
/// would send a caller looking at its own string handling.
#[test]
fn a_non_utf8_path_that_names_no_file_is_not_found_and_is_not_the_root() {
    let fs = mount_fixture();
    let path = undecodable_path();
    let mut attr = unsafe { std::mem::zeroed::<fs_squashfs_attr_t>() };

    // What the root actually is, so the assertion can name it.
    let root = CString::new("/").unwrap();
    let mut root_attr = unsafe { std::mem::zeroed::<fs_squashfs_attr_t>() };
    assert_eq!(
        unsafe { fs_squashfs_stat(fs, root.as_ptr(), &mut root_attr) },
        0
    );

    let rc = unsafe { fs_squashfs_stat(fs, path.as_ptr(), &mut attr) };
    assert_eq!(
        rc, -1,
        "a path naming no file was answered as a successful stat"
    );
    assert_eq!(
        fs_squashfs_last_errno(),
        2, /* ENOENT */
        "refused, but as a bad argument rather than a missing file: {}",
        last_err_str()
    );
    assert_ne!(
        attr.inode, root_attr.inode,
        "the root inode was reported for a path that names no file"
    );
    assert_eq!(attr.inode, 0, "attr was written for a refused call");
    unsafe { fs_squashfs_umount(fs) };
}

/// The directory iterator likewise: a path naming no directory is not
/// the root listing.
///
/// This is the one that hurts most in practice — a caller walking a
/// tree, composing paths from names this driver handed back, gets the
/// root's entries again and walks in a circle.
#[test]
fn dir_open_on_a_non_utf8_path_that_names_nothing_is_not_the_root_listing() {
    let fs = mount_fixture();
    let path = undecodable_path();
    let iter = unsafe { fs_squashfs_dir_open(fs, path.as_ptr()) };
    assert!(
        iter.is_null(),
        "a path naming no directory opened an iterator"
    );
    assert_eq!(fs_squashfs_last_errno(), 2 /* ENOENT */);
    unsafe { fs_squashfs_umount(fs) };
}

/// And reading a file, which fails for the reason it should: the file
/// is not there.
#[test]
fn read_file_on_a_non_utf8_path_that_names_nothing_is_not_found() {
    let fs = mount_fixture();
    let path = undecodable_path();
    let mut buf = [0u8; 16];
    let n = unsafe {
        fs_squashfs_read_file(
            fs,
            path.as_ptr(),
            buf.as_mut_ptr() as *mut std::ffi::c_void,
            0,
            buf.len() as u64,
        )
    };
    assert!(n < 0, "a path naming no file read {n} bytes");
    assert_eq!(
        fs_squashfs_last_errno(),
        2, /* ENOENT */
        "refused, but as something other than a missing file: {}",
        last_err_str()
    );
    unsafe { fs_squashfs_umount(fs) };
}

/// A path that decodes is still a path.
///
/// The control for the three above: a decoder that refused everything
/// would satisfy them all, and would be a worse driver than the one
/// with the defect.
#[test]
fn a_decodable_path_still_resolves() {
    let fs = mount_fixture();
    let p = CString::new("/hello.txt").unwrap();
    let mut attr = unsafe { std::mem::zeroed::<fs_squashfs_attr_t>() };
    assert_eq!(
        unsafe { fs_squashfs_stat(fs, p.as_ptr(), &mut attr) },
        0,
        "an ordinary path was refused: {}",
        last_err_str()
    );
    assert_eq!(attr.size, 3);
    unsafe { fs_squashfs_umount(fs) };
}
