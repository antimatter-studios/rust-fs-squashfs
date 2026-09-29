//! `fs.squashfs`, the multi-call binary's reader, run as a user runs it,
//! against the committed `test-disks/squashfs-basic.sqfs`.
//!
//! These prove the tool and this crate's reader agree about the fixture,
//! whose contents are written down in `test-disks/squashfs-basic.meta.txt`.
//! Whether squashfs-tools and the kernel agree with both, on images made by
//! `mksquashfs` with every compressor, is the oracle and kernel tiers'
//! question (tests/cli_fs_oracle.rs, tests/cli_fs_kernel.rs).
//!
//! It replaces the tests of `lssquashfs`, whose verbs `fs.squashfs` took
//! over: `info` is `get`, `cat` is `read`, `readlink` is a symlink entry's
//! `target`.

mod cli_support;
mod common;

use cli_support::*;
use common::{basic_big_bin, basic_fixture_path, embed_at_offset, fixture_bytes};
use std::process::Output;

fn fixture() -> String {
    basic_fixture_path().display().to_string()
}

/// `fs.squashfs <fixture> ARGS...`.
fn on_fixture(args: &[&str]) -> Output {
    tool("fs.squashfs")
        .arg(fixture())
        .args(args)
        .output()
        .expect("spawn fs.squashfs")
}

/// The entry named `name` in an `ls` report, as its text.
#[track_caller]
fn entry<'a>(listing: &'a str, name: &str) -> &'a str {
    let at = listing
        .find(&format!("\"name\": \"{name}\""))
        .unwrap_or_else(|| panic!("{name} not listed:\n{listing}"));
    &listing[at..at + listing[at..].find('}').unwrap()]
}

/// A failure: the status, nothing on stdout, and the JSON error.
#[track_caller]
fn refused(out: &Output, code: i32) -> String {
    assert_eq!(out.status.code(), Some(code), "{}", stderr(out));
    assert!(out.stdout.is_empty(), "stdout: {}", stdout(out));
    let err = stderr(out);
    assert!(
        err.starts_with("{\"error\": \"")
            && err.trim_end().ends_with(&format!("\"code\": {code}}}")),
        "{err}"
    );
    err
}

// ---- ls ------------------------------------------------------------------

#[test]
fn ls_lists_the_root_with_every_field_typed() {
    let out = on_fixture(&["ls"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let listing = stdout(&out);
    for name in ["empty.txt", "hello.txt", "link", "sub"] {
        entry(&listing, name);
    }
    let hello = entry(&listing, "hello.txt");
    assert_eq!(json_field(hello, "type"), "file");
    assert_eq!(json_field(hello, "size"), "3");
    assert_eq!(json_field(hello, "mode"), "0644");
    assert!(json_field(hello, "mtime").parse::<u64>().is_ok(), "{hello}");
    assert!(json_field(hello, "inode").parse::<u64>().is_ok(), "{hello}");
    assert_eq!(json_field(entry(&listing, "sub"), "type"), "dir");
    assert_eq!(json_field(entry(&listing, "empty.txt"), "size"), "0");
}

/// `lssquashfs readlink` is gone: the target is a field of the entry.
#[test]
fn a_symlink_entry_carries_its_target() {
    let listing = stdout(&ok(tool("fs.squashfs").args([&fixture(), "ls", "/link"])));
    let link = entry(&listing, "link");
    assert_eq!(json_field(link, "type"), "symlink");
    assert_eq!(json_field(link, "target"), "hello.txt");
    assert!(!entry(&listing, "link").contains("\"name\": \"hello.txt\""));
}

#[test]
fn ls_of_a_file_lists_that_one_entry_and_of_a_directory_its_children() {
    let one = stdout(&ok(tool("fs.squashfs").args([
        &fixture(),
        "ls",
        "/hello.txt",
    ])));
    assert_eq!(one.matches("\"name\"").count(), 1, "{one}");
    entry(&one, "hello.txt");
    let sub = stdout(&ok(tool("fs.squashfs").args([&fixture(), "ls", "/sub"])));
    entry(&sub, "note.md");
    assert_eq!(json_field(entry(&sub, "deep"), "type"), "dir");
}

#[test]
fn ls_text_is_one_line_per_entry_for_a_person() {
    let text = stdout(&ok(tool("fs.squashfs").args([
        &fixture(),
        "ls",
        "--text",
        "/",
    ])));
    assert_eq!(text.lines().count(), 4, "{text}");
    assert!(text.contains("-0644            3 hello.txt"), "{text}");
    assert!(text.contains(" link -> hello.txt"), "{text}");
    assert!(!text.contains('{'), "{text}");
}

// ---- read ----------------------------------------------------------------

#[test]
fn read_writes_each_file_s_exact_bytes() {
    for (path, want) in [
        ("/hello.txt", b"hi\n".to_vec()),
        ("/sub/note.md", b"# note\nsome words here\n".to_vec()),
        ("/sub/deep/big.bin", basic_big_bin()),
        ("/empty.txt", Vec::new()),
    ] {
        let out = ok(tool("fs.squashfs").args([&fixture(), "read", path]));
        assert!(out.stdout == want, "{path}: the bytes read differ");
        assert!(out.stderr.is_empty(), "{path}: {}", stderr(&out));
    }
}

#[test]
fn read_to_a_file_writes_the_file_and_nothing_on_stdout() {
    let dest = image_path("read-o");
    let out = ok(tool("fs.squashfs").args([&fixture(), "read", "/sub/deep/big.bin", "-o", &dest]));
    assert!(out.stdout.is_empty());
    assert_eq!(std::fs::read(&dest).unwrap(), basic_big_bin());
    assert!(!std::path::Path::new(&format!("{dest}.partial")).exists());
}

#[test]
fn read_refuses_what_is_not_a_regular_file() {
    let err = refused(&on_fixture(&["read", "/sub"]), 1);
    assert!(err.contains("is a directory"), "{err}");
    let err = refused(&on_fixture(&["read", "/link"]), 1);
    assert!(err.contains("is a symlink to hello.txt"), "{err}");
    let err = refused(&on_fixture(&["read", "/no/such/file"]), 1);
    assert!(err.contains("not found"), "{err}");
}

// ---- get / info ------------------------------------------------------------

#[test]
fn get_reports_the_canonical_keys_and_squashfs_own() {
    let get = stdout(&ok(tool("fs.squashfs").args([&fixture(), "get"])));
    assert_eq!(json_field(&get, "fs"), "squashfs");
    assert_eq!(json_field(&get, "label"), "null");
    assert_eq!(json_field(&get, "free_bytes"), "0");
    assert_eq!(json_field(&get, "block_size"), "4096");
    assert_eq!(json_field(&get, "dirty"), "false");
    assert_eq!(json_field(&get, "compression"), "gzip");
    assert_eq!(json_field(&get, "version"), "4.0");
    let fs = common::open_image(fixture_bytes());
    assert_eq!(
        json_field(&get, "total_bytes"),
        fs.sb.bytes_used.to_string()
    );
    assert_eq!(
        json_field(&get, "inode_count"),
        fs.sb.inode_count.to_string()
    );
    assert_eq!(
        json_field(&get, "exportable"),
        fs.is_exportable().to_string()
    );
    let info = stdout(&ok(tool("fs.squashfs").args([&fixture(), "info"])));
    assert_eq!(info, get, "info and get differ");
}

#[test]
fn get_one_key_and_a_dotted_key() {
    let label = stdout(&ok(tool("fs.squashfs").args([&fixture(), "get", "label"])));
    assert_eq!(label.trim_end(), "{\n  \"label\": null\n}");
    let comp = ok(tool("fs.squashfs").args([&fixture(), "get", "squashfs.compression", "--text"]));
    assert_eq!(stdout(&comp), "gzip\n");
    let err = refused(&on_fixture(&["get", "no_such_key"]), 2);
    assert!(err.contains("the keys are fs, label"), "{err}");
}

// ---- the verbs a read-only format refuses ----------------------------------

#[test]
fn every_verb_that_would_change_the_image_is_refused_as_read_only() {
    let before = fixture_bytes();
    for verb in [
        &["write", "/new"][..],
        &["mkdir", "/newdir"],
        &["set", "label", "X"],
        &["resize", "1G", "--force"],
    ] {
        let err = refused(&on_fixture(verb), 3);
        assert!(err.contains("SquashFS is read-only"), "{verb:?}: {err}");
    }
    assert!(
        fixture_bytes() == before,
        "a refused verb changed the image"
    );
}

// ---- --offset ------------------------------------------------------------

#[test]
fn offset_reaches_an_image_embedded_in_a_larger_one() {
    let (padded, offset) = embed_at_offset(&fixture_bytes(), 1 << 20);
    let img = image_path("embedded");
    std::fs::write(&img, padded).unwrap();
    let offset = offset.to_string();
    let out = ok(tool("fs.squashfs").args(["--offset", &offset, &img, "read", "/hello.txt"]));
    assert_eq!(out.stdout, b"hi\n");
    // After the verb as well: the flag is global.
    let listing = stdout(&ok(
        tool("fs.squashfs").args([&img, "ls", "/", "--offset", &offset])
    ));
    entry(&listing, "sub");
    // Without it, the zeros at the front are not a filesystem.
    refused(&tool("fs.squashfs").args([&img, "ls"]).output().unwrap(), 1);
    let err = refused(
        &tool("fs.squashfs")
            .args(["--offset", "999999999", &img, "ls"])
            .output()
            .unwrap(),
        1,
    );
    assert!(err.contains("past the end"), "{err}");
}

// ---- images that are not readable ------------------------------------------

#[test]
fn a_missing_or_foreign_or_damaged_image_is_a_structured_failure() {
    let missing = image_path("never-created");
    let err = refused(
        &tool("fs.squashfs").args([&missing, "ls"]).output().unwrap(),
        1,
    );
    assert!(err.contains("open "), "{err}");

    let zeros = image_path("zeros");
    std::fs::write(&zeros, vec![0u8; 8192]).unwrap();
    let err = refused(
        &tool("fs.squashfs").args([&zeros, "get"]).output().unwrap(),
        1,
    );
    assert!(err.contains("is not a readable SquashFS image"), "{err}");

    let mut bad_magic = fixture_bytes();
    bad_magic[0] ^= 0xFF;
    let path = image_path("bad-magic");
    std::fs::write(&path, bad_magic).unwrap();
    refused(
        &tool("fs.squashfs").args([&path, "ls"]).output().unwrap(),
        1,
    );

    let cut = image_path("truncated");
    std::fs::write(&cut, &fixture_bytes()[..4096]).unwrap();
    for verb in ["ls", "get"] {
        refused(&tool("fs.squashfs").args([&cut, verb]).output().unwrap(), 1);
    }
    let out = tool("fs.squashfs")
        .args([&cut, "read", "/sub/deep/big.bin"])
        .output()
        .unwrap();
    refused(&out, 1);
}

#[test]
fn no_target_or_no_verb_is_usage() {
    for args in [&[][..], &[fixture().as_str()][..]] {
        let out = tool("fs.squashfs").args(args).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", stderr(&out));
        assert!(out.stdout.is_empty(), "{args:?}");
    }
    let err = refused(&on_fixture(&["tree"]), 2);
    assert!(err.contains("tree"), "{err}");
}
