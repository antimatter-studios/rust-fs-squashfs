//! Stress + edge-case tests: many files in one directory, deep directory
//! chains, max-length names, empty files/dirs, and malformed-image refusal.
//!
//! The oracle-built cases (many files, deep trees, long names, all five
//! compressors) need `mksquashfs` and are `#[ignore]`-gated. Run them with:
//!
//! ```sh
//! cargo test --release --test stress -- --ignored
//! ```
//!
//! The committed-fixture and malformed-image cases run under a plain
//! `cargo test` (no external tools).

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::*;
use fs_squashfs::{Error, Filesystem};

// ===========================================================================
// Committed-fixture edge cases (no external tools)
// ===========================================================================

#[test]
fn empty_file_reads_as_zero_length() {
    let fs = open_image_path(&basic_fixture_path());
    let inode = fs.lookup_path("/empty.txt").unwrap();
    assert!(inode.is_regular_file());
    assert_eq!(inode.file_size, 0);
    let mut buf = [0u8; 8];
    assert_eq!(fs.read_file(&inode, 0, &mut buf).unwrap(), 0);
}

#[test]
fn deep_path_lookup_on_committed_fixture() {
    // /sub/deep/big.bin is the deepest committed path.
    let fs = open_image_path(&basic_fixture_path());
    assert!(fs
        .lookup_path("/sub/deep/big.bin")
        .unwrap()
        .is_regular_file());
    // A path that descends through a regular file must be rejected.
    assert!(fs.lookup_path("/hello.txt/nope").is_err());
}

// ===========================================================================
// Malformed-image refusal — the reader must return Err(_), never panic.
// ===========================================================================

/// `Filesystem::open` wrapped in `catch_unwind`: `Ok(Some(e))` on a clean
/// error, `Ok(None)` if it surprisingly succeeded, `Err(_)` on a panic.
fn open_classify(bytes: Vec<u8>) -> std::thread::Result<Option<Error>> {
    std::panic::catch_unwind(move || {
        let dev: Arc<dyn fs_core::BlockRead> = Arc::new(MemDev::new(bytes));
        Filesystem::open(dev).err()
    })
}

#[test]
fn flipped_magic_is_rejected_no_panic() {
    let mut img = fixture_bytes();
    img[0] ^= 0xFF; // magic at byte 0
    let err = open_classify(img)
        .expect("must not panic")
        .expect("must error");
    assert!(
        matches!(err, Error::NotSquashfs | Error::BadSuperblock(_)),
        "expected NotSquashfs/BadSuperblock, got {err:?}",
    );
}

#[test]
fn truncated_image_is_rejected_no_panic() {
    let img = vec![0u8; 32]; // far too short for a 96-byte superblock
    assert!(open_classify(img).expect("must not panic").is_some());
}

#[test]
fn all_zeros_image_is_rejected_no_panic() {
    let img = vec![0u8; 4096];
    let err = open_classify(img)
        .expect("must not panic")
        .expect("must error");
    assert!(
        matches!(err, Error::NotSquashfs | Error::BadSuperblock(_)),
        "expected NotSquashfs/BadSuperblock for zeros, got {err:?}",
    );
}

#[test]
fn corrupt_block_log_is_rejected_no_panic() {
    let mut img = fixture_bytes();
    img[0x16] = 99; // block_log out of the 12..=20 range
    let err = open_classify(img)
        .expect("must not panic")
        .expect("must error");
    assert!(
        matches!(err, Error::BadSuperblock(_)),
        "expected BadSuperblock, got {err:?}",
    );
}

#[test]
fn unsupported_compression_id_is_rejected_no_panic() {
    let mut img = fixture_bytes();
    // compression_id at 0x14: 0xFFFF is not a known codec.
    img[0x14..0x16].copy_from_slice(&0xFFFFu16.to_le_bytes());
    let err = open_classify(img)
        .expect("must not panic")
        .expect("must error");
    assert!(
        matches!(
            err,
            Error::UnsupportedCompression(_) | Error::BadSuperblock(_)
        ),
        "expected UnsupportedCompression, got {err:?}",
    );
}

#[test]
fn corrupt_root_inode_ref_is_handled_no_panic() {
    let mut img = fixture_bytes();
    // root_inode_ref at 0x20: a wildly out-of-range value must error on
    // the first root-inode read, not panic.
    img[0x20..0x28].copy_from_slice(&0xFFFF_FFFF_FFFFu64.to_le_bytes());
    let res: std::thread::Result<fs_squashfs::Result<()>> = std::panic::catch_unwind(move || {
        let dev: Arc<dyn fs_core::BlockRead> = Arc::new(MemDev::new(img));
        let fs = Filesystem::open(dev)?;
        fs.root_inode().map(|_| ())
    });
    assert!(
        res.expect("must not panic").is_err(),
        "bad root ref must error"
    );
}

// ===========================================================================
// A superblock that lies about the image it describes (#48)
// ===========================================================================
//
// Each case patches ONE field of the committed fixture, which mksquashfs
// wrote, so every other field is one the reference tool produced. Offsets
// are `struct squashfs_super_block`'s. The committed image's tables run
// inode 0x4e9a < directory 0x4f10 < fragment 0x4f89 < export 0x4fb1 <
// id 0x4fc3 < bytes_used 0x4fcb, inside a 20480-byte file.

fn patch_u64(img: &mut [u8], at: usize, v: u64) {
    img[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

fn rd_u64(img: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(img[at..at + 8].try_into().unwrap())
}

/// Open must refuse with `BadSuperblock` naming `needle`.
fn assert_refused(img: Vec<u8>, needle: &str) {
    match open_classify(img).expect("must not panic") {
        Some(Error::BadSuperblock(why)) => assert!(
            why.contains(needle),
            "refused, but for {why:?} rather than for {needle:?}"
        ),
        Some(other) => panic!("expected BadSuperblock naming {needle:?}, got {other:?}"),
        None => panic!("a superblock whose {needle} is a lie opened cleanly"),
    }
}

/// `bytes_used` is published to the OS as the volume's size, so it
/// cannot exceed the device it was read from.
#[test]
fn bytes_used_beyond_the_device_is_refused() {
    let mut img = fixture_bytes();
    patch_u64(&mut img, 0x28, 1 << 40);
    assert_refused(img, "bytes_used");
}

/// The kernel refuses a minor version it does not know; so does this.
#[test]
fn an_unknown_minor_version_is_refused() {
    let mut img = fixture_bytes();
    img[0x1E..0x20].copy_from_slice(&1u16.to_le_bytes());
    assert_refused(img, "minor");
}

/// A SquashFS 3.x image has `block_log` somewhere else, so it used to be
/// refused as a bad block size rather than as the wrong major version.
#[test]
fn a_version_3_superblock_is_refused_as_version_3() {
    let mut img = fixture_bytes();
    img[0x1C..0x1E].copy_from_slice(&3u16.to_le_bytes());
    img[0x16] = 99;
    assert_refused(img, "4.x");
}

#[test]
fn a_table_starting_past_bytes_used_is_refused() {
    let mut img = fixture_bytes();
    patch_u64(&mut img, 0x48, 1 << 40); // directory_table_start
    assert_refused(img, "table");
}

#[test]
fn a_table_starting_inside_the_superblock_is_refused() {
    let mut img = fixture_bytes();
    patch_u64(&mut img, 0x40, 10); // inode_table_start
    assert_refused(img, "table");
}

/// In bounds individually, out of order together: the directory table
/// before the inode table.
#[test]
fn tables_out_of_order_are_refused() {
    let mut img = fixture_bytes();
    let inode_start = rd_u64(&img, 0x40);
    patch_u64(&mut img, 0x48, inode_start - 1); // directory_table_start
    assert_refused(img, "order");
}

/// The root inode's block has to lie inside the inode table.
#[test]
fn a_root_inode_outside_the_inode_table_is_refused() {
    let mut img = fixture_bytes();
    let inode_start = rd_u64(&img, 0x40);
    let dir_start = rd_u64(&img, 0x48);
    let past = (dir_start - inode_start) << 16;
    patch_u64(&mut img, 0x20, past); // root_inode_ref
    assert_refused(img, "root");
}

/// The control for all of the above: the fixture as mksquashfs wrote it
/// still opens, so none of them passes by refusing everything.
#[test]
fn the_unpatched_fixture_still_opens() {
    assert!(open_classify(fixture_bytes())
        .expect("must not panic")
        .is_none());
}

// ===========================================================================
// Oracle-built stress trees (require mksquashfs)
// ===========================================================================

/// Many files in one directory: proves directory listings that span
/// multiple metadata blocks parse and every entry resolves.
#[test]
#[ignore = "requires squashfs-tools (mksquashfs); run with -- --ignored"]
fn many_files_in_one_directory_gzip() {
    if !mksquashfs_available() {
        eprintln!("skipping: mksquashfs not on PATH");
        return;
    }
    let n = 1000;
    let mut entries = Vec::with_capacity(n);
    let names: Vec<String> = (0..n).map(|i| format!("file_{i:04}")).collect();
    for (i, name) in names.iter().enumerate() {
        entries.push((name.as_str(), file(format!("contents-{i}\n").as_bytes())));
    }
    let art = build_with_mksquashfs("gzip", &dir(entries));
    let fs = open_image(art.bytes);

    // Every entry is listed.
    let listed = sorted_dir_names(&fs, "/");
    assert_eq!(listed.len(), n, "directory entry count");

    // Spot-check a scatter of files resolve + read back correctly.
    for &i in &[0usize, 1, 42, 500, 999] {
        let got = read_whole_file(&fs, &format!("/file_{i:04}"));
        assert_eq!(got, format!("contents-{i}\n").as_bytes(), "file_{i:04}");
    }
}

/// A deep directory chain: proves path traversal recurses arbitrarily.
#[test]
#[ignore = "requires squashfs-tools (mksquashfs); run with -- --ignored"]
fn deep_directory_chain_gzip() {
    if !mksquashfs_available() {
        eprintln!("skipping: mksquashfs not on PATH");
        return;
    }
    // Build /d0/d1/.../d19/leaf.txt nested 20 deep.
    const DEPTH: usize = 20;
    let mut node = dir(vec![("leaf.txt", file(b"bottom\n"))]);
    for level in (0..DEPTH).rev() {
        node = dir(vec![(
            Box::leak(format!("d{level}").into_boxed_str()) as &str,
            node,
        )]);
    }
    let art = build_with_mksquashfs("gzip", &node);
    let fs = open_image(art.bytes);

    let mut path = String::new();
    for level in 0..DEPTH {
        path.push_str(&format!("/d{level}"));
    }
    path.push_str("/leaf.txt");
    assert_eq!(read_whole_file(&fs, &path), b"bottom\n", "deep leaf");
}

/// Max-length (255-byte) and unicode filenames round-trip.
#[test]
#[ignore = "requires squashfs-tools (mksquashfs); run with -- --ignored"]
fn long_and_unicode_names_gzip() {
    if !mksquashfs_available() {
        eprintln!("skipping: mksquashfs not on PATH");
        return;
    }
    let long_name = "x".repeat(255);
    let tree = dir(vec![
        (long_name.as_str(), file(b"long\n")),
        ("h\u{e9}llo-\u{4e16}\u{754c}.txt", file(b"unicode\n")),
        ("file with spaces.txt", file(b"spaces\n")),
    ]);
    let art = build_with_mksquashfs("gzip", &tree);
    let fs = open_image(art.bytes);

    assert_eq!(read_whole_file(&fs, &format!("/{long_name}")), b"long\n");
    assert_eq!(
        read_whole_file(&fs, "/h\u{e9}llo-\u{4e16}\u{754c}.txt"),
        b"unicode\n"
    );
    assert_eq!(read_whole_file(&fs, "/file with spaces.txt"), b"spaces\n");
}

/// Empty files and empty directories survive a round-trip.
#[test]
#[ignore = "requires squashfs-tools (mksquashfs); run with -- --ignored"]
fn empty_files_and_dirs_gzip() {
    if !mksquashfs_available() {
        eprintln!("skipping: mksquashfs not on PATH");
        return;
    }
    let tree = dir(vec![
        ("zero.bin", file(b"")),
        ("empty_dir", dir(vec![])),
        ("nonempty", dir(vec![("x", file(b"x\n"))])),
    ]);
    let art = build_with_mksquashfs("gzip", &tree);
    let fs = open_image(art.bytes);

    let zero = fs.lookup_path("/zero.bin").unwrap();
    assert_eq!(zero.file_size, 0);
    assert_eq!(read_whole_file(&fs, "/zero.bin"), b"");

    let empty_dir = fs.lookup_path("/empty_dir").unwrap();
    assert!(empty_dir.is_dir());
    assert!(
        fs.read_dir(&empty_dir).unwrap().is_empty(),
        "empty dir listing"
    );

    assert_eq!(sorted_dir_names(&fs, "/nonempty"), vec!["x"]);
}

/// A moderately varied tree built once per compressor; read every leaf
/// back and confirm the bytes match. Proves the stress shape decodes from
/// every codec's real `mksquashfs` output.
#[test]
#[ignore = "requires squashfs-tools (mksquashfs); run with -- --ignored"]
fn varied_tree_all_compressors() {
    if !mksquashfs_available() {
        eprintln!("skipping: mksquashfs not on PATH");
        return;
    }
    // (path, contents) pairs the tree below contains.
    let mut expected: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    expected.insert("/a.txt".into(), b"alpha\n".to_vec());
    expected.insert("/dir/b.bin".into(), pattern(50_000)); // multi-block
    expected.insert("/dir/sub/c.txt".into(), b"charlie\n".to_vec());
    expected.insert("/dir/sub/empty".into(), Vec::new());

    let tree = dir(vec![
        ("a.txt", file(b"alpha\n")),
        (
            "dir",
            dir(vec![
                ("b.bin", file(&pattern(50_000))),
                (
                    "sub",
                    dir(vec![("c.txt", file(b"charlie\n")), ("empty", file(b""))]),
                ),
            ]),
        ),
    ]);

    for comp in ["gzip", "xz", "lz4", "zstd", "lzo"] {
        let art = build_with_mksquashfs(comp, &tree);
        let fs = open_image(art.bytes);
        for (path, want) in &expected {
            let got = read_whole_file(&fs, path);
            assert_eq!(&got, want, "[{comp}] {path}");
        }
    }
}

// ===========================================================================
// Sums over image-supplied offsets
//
// Three places add a raw `u64` off the superblock to a value out of an
// inode. One of them saturated; the other two used a plain `+`, which
// panics in debug — the profile these tests run in — and wraps in
// release, which is how the crate ships. A wrapped offset reads some
// unrelated part of the image as the structure that was asked for.
//
// Both profiles matter and they used to disagree, so these run under
// `cargo test` and `cargo test --release` alike and assert the same
// thing in each: an error, not a panic and not a plausible answer.
// ===========================================================================

/// Set one little-endian `u64` field of the superblock.
fn with_sb_u64(mut img: Vec<u8>, off: usize, value: u64) -> Vec<u8> {
    img[off..off + 8].copy_from_slice(&value.to_le_bytes());
    img
}

/// Open the image and try to reach its root directory, reporting a
/// panic as a panic rather than letting it end the test.
///
/// THE CACHE IS OFF, and that is not incidental. `Filesystem::open`
/// wraps the device in `fs_core::CachingDevice`, and at the
/// `am-fs-core` version this crate pins — v0.2.10 — its `read_at`
/// computes `(offset + buf.len() as u64 - 1) / bs` with a plain `+`.
/// The saturated offset these tests produce is `u64::MAX`, so that line
/// panics in debug before this crate's own refusal is reached:
///
/// ```text
/// panicked at rust-fs-core/src/caching_device.rs:167: attempt to add with overflow
/// ```
///
/// It is fixed on `rust-fs-core` main — the same sum now goes through
/// `checked_add`, tracked as rust-fs-core#34 — but there is no tag past
/// v0.2.10, so it is not something this repository can pick up yet.
/// Testing the uncached path asks about this crate's rule rather than
/// about a dependency's.
///
/// WHEN am-fs-core MOVES PAST v0.2.10, change this back to
/// `Filesystem::open` and delete this paragraph. The cached path is the
/// one every consumer takes, so leaving the workaround here after its
/// cause is gone would quietly stop testing the default.
fn walk_root(bytes: Vec<u8>) -> std::thread::Result<Result<(), Error>> {
    std::panic::catch_unwind(move || {
        let dev: Arc<dyn fs_core::BlockRead> = Arc::new(MemDev::new(bytes));
        let fs = Filesystem::open_with_cache(dev, 0)?;
        let root = fs.root_inode()?;
        fs.read_dir(&root)?;
        Ok(())
    })
}

/// An absurd `inode_table_start` plus a large block offset must fail,
/// not overflow.
///
/// `block_offset` is `root_inode_ref >> 16`, so `1 << 56` in the
/// reference makes `1 << 40` in the offset — the shape the sum cannot
/// hold. Before this: `panicked at src/metablock.rs: attempt to add
/// with overflow` in debug, and in release a read from the wrapped
/// offset.
#[test]
fn an_absurd_inode_table_start_errors_rather_than_overflowing() {
    let img = with_sb_u64(fixture_bytes(), 0x40, 0xffff_ff00_0000_0060);
    let img = with_sb_u64(img, 0x20, 1u64 << 56); // root_inode_ref
    let err = walk_root(img)
        .expect("must not panic")
        .expect_err("must not resolve a root inode from an offset nothing points at");
    // Since #48 the mount refuses this table start before any sum is
    // taken, so the refusal is the assertion here. The sum itself --
    // saturating to u64::MAX, which no device reaches, rather than
    // wrapping to a small offset that names real bytes -- is still
    // pinned by `metablock::tests::start_abs_saturates_rather_than_wrapping`,
    // which this test can no longer reach through `open`.
    assert!(
        matches!(err, Error::BadSuperblock(_)),
        "expected the superblock to be refused, got {err:?}"
    );
}

/// An absurd `directory_table_start` fails at the offset it names,
/// rather than at some other offset.
///
/// This one cannot demonstrate the overflow, and it is worth being
/// explicit about why: the committed fixture's root inode has
/// `dir_start_block == 0`, so the sum is `directory_table_start + 0`
/// and nothing this test can set in the superblock makes it wrap. A
/// non-zero `dir_start_block` lives inside a compressed metablock.
///
/// What it does establish is that the directory path reaches the device
/// with the offset it was given, which is what makes the unit test on
/// `MetadataRef::start_abs` cover this site: `read_dir` now goes
/// through that function instead of writing the sum out by hand, and
/// writing it out by hand is how the two came to disagree.
#[test]
fn an_absurd_directory_table_start_errors_at_the_offset_it_names() {
    let absurd = u64::MAX - 8;
    let img = with_sb_u64(fixture_bytes(), 0x48, absurd);
    let err = walk_root(img)
        .expect("must not panic")
        .expect_err("must not read a directory listing from an offset nothing points at");
    // Since #48 the mount refuses a directory table starting past
    // `bytes_used` before `read_dir` can name any offset; the offset
    // arithmetic is covered by the `MetadataRef::start_abs` unit tests.
    assert!(
        matches!(err, Error::BadSuperblock(_)),
        "expected the superblock to be refused, got {err:?}"
    );
}

/// The positive control for both: the untouched fixture still mounts and
/// its root still lists.
///
/// Without it, a change that refused every image would pass the two
/// above.
#[test]
fn the_untouched_fixture_still_mounts_and_lists() {
    walk_root(fixture_bytes())
        .expect("must not panic")
        .expect("the committed fixture is sound");
}

// ===========================================================================
// An id index the image's table cannot answer
// ===========================================================================

/// Byte offset of `id_count` in the superblock.
const ID_COUNT_OFF: usize = 0x1A;

/// An index past the id table is refused rather than answered as root.
///
/// `resolve_uid` / `resolve_gid` used to return 0 for an index they
/// could not resolve. Zero is not a sentinel: uid 0 is `root` and gid 0
/// is `wheel`, the most privileged answer either function can give, and
/// nothing distinguished it from an image that really says root. The
/// index comes off the inode and the table length comes off the
/// superblock, so a truncated or patched image reaches it without
/// anything else about the image looking wrong.
///
/// Measured on the committed fixture, whose table is `[501, 20]`, with
/// `id_count` patched from 2 to 1:
///
/// ```text
/// id_count=2   /hello.txt  uid_idx=0 gid_idx=1 -> uid=501 gid=20
/// id_count=1   /hello.txt  uid_idx=0 gid_idx=1 -> uid=501 gid=0   (silently wheel)
/// ```
#[test]
fn an_id_index_past_the_table_is_refused_rather_than_answered_as_root() {
    let good = open_image(fixture_bytes());
    let inode = good.lookup_path("/hello.txt").unwrap();
    assert_eq!(
        inode.gid_idx, 1,
        "the fixture stopped using the second id table slot, so this test \
         no longer reaches a truncated table"
    );
    assert_eq!(good.resolve_uid(inode.uid_idx).unwrap(), 501);
    assert_eq!(good.resolve_gid(inode.gid_idx).unwrap(), 20);

    let mut bytes = fixture_bytes();
    bytes[ID_COUNT_OFF..ID_COUNT_OFF + 2].copy_from_slice(&1u16.to_le_bytes());
    let short = open_image(bytes);
    let inode = short.lookup_path("/hello.txt").unwrap();
    assert_eq!(
        short.resolve_uid(inode.uid_idx).unwrap(),
        501,
        "the index that is still in range stopped resolving"
    );
    match short.resolve_gid(inode.gid_idx) {
        Err(Error::BadInode(_)) => {}
        other => panic!("a gid index past the table gave {other:?}"),
    }
}

/// Every table layout `mksquashfs` writes still opens under the
/// superblock checks (#48): optional tables absent in each combination,
/// an empty root, and each compressor. The checks are only right if the
/// reference writer's own output satisfies them.
#[test]
#[ignore = "requires squashfs-tools (mksquashfs); run with -- --ignored"]
fn every_table_layout_mksquashfs_writes_passes_the_superblock_checks() {
    if !mksquashfs_available() {
        eprintln!("skipping: mksquashfs not on PATH");
        return;
    }
    let full = dir(vec![
        ("small.txt", file(b"tail\n")),
        ("big.bin", file(&pattern(70_000))),
        ("sub", dir(vec![("inner.txt", file(b"inner\n"))])),
    ]);
    let empty = dir(vec![]);
    let layouts: &[&[&str]] = &[
        &[],
        &["-no-xattrs"],
        &["-no-exports"],
        &["-no-fragments"],
        &["-no-exports", "-no-fragments", "-no-xattrs"],
        &["-always-use-fragments"],
    ];
    let mut opened = 0;
    for tree in [&full, &empty] {
        for extra in layouts {
            for comp in ["gzip", "xz", "lz4", "zstd", "lzo"] {
                let art = build_with_mksquashfs_args(comp, tree, extra);
                let dev: Arc<dyn fs_core::BlockRead> = Arc::new(MemDev::new(art.bytes));
                let fs = Filesystem::open(dev).unwrap_or_else(|e| {
                    panic!("mksquashfs -comp {comp} {extra:?} wrote an image this refuses: {e:?}")
                });
                fs.root_inode()
                    .unwrap_or_else(|e| panic!("-comp {comp} {extra:?}: root inode: {e:?}"));
                opened += 1;
            }
        }
    }
    assert_eq!(opened, 2 * layouts.len() * 5);
}
