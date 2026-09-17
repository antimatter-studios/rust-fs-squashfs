//! Oracle-validated compression tests.
//!
//! For each standard SquashFS compressor, build a fixture tree with the
//! real `mksquashfs -comp <c>` and read every path back through the
//! pure-Rust driver, asserting exact bytes. This is the ground truth that
//! proves the codec dispatch (gzip / xz / lz4 / zstd / lzo / lzma) decodes real
//! `mksquashfs` output, not just our own synthetic streams.
//!
//! Every test here is `#[ignore]`-gated so `cargo test` stays green on a
//! host without `squashfs-tools`. Run them with:
//!
//! ```sh
//! cargo test --release -- --ignored
//! ```

mod common;

use common::*;

/// The fixture tree, shared across every compressor:
///   /hello.txt          small file (lands in a tail fragment)
///   /sub/note.md        nested small file
///   /sub/deep/big.bin   ~300 KiB pseudo-random -> multiple data blocks
///   /link               symlink -> hello.txt
fn fixture_tree() -> Node {
    dir(vec![
        ("hello.txt", file(b"hi\n")),
        (
            "sub",
            dir(vec![
                ("note.md", file(b"# note\nsome words here\n")),
                ("deep", dir(vec![("big.bin", file(&pattern(300_000)))])),
            ]),
        ),
        ("link", symlink("hello.txt")),
    ])
}

/// Run the full read-back assertion suite against an image built with the
/// given compressor.
fn assert_reads_back(comp: &str) {
    if !mksquashfs_available() {
        eprintln!("skipping {comp}: mksquashfs not on PATH");
        return;
    }
    let art = build_with_mksquashfs(comp, &fixture_tree());
    let fs = open_image(art.bytes.clone());

    // Report the compressor the driver detected matches what we asked for.
    assert_eq!(
        fs.compressor().name(),
        comp,
        "driver detected wrong compressor for -comp {comp}"
    );

    // ---- root listing ----
    assert_eq!(
        sorted_dir_names(&fs, "/"),
        vec!["hello.txt", "link", "sub"],
        "[{comp}] root listing"
    );
    assert_eq!(
        sorted_dir_names(&fs, "/sub"),
        vec!["deep", "note.md"],
        "[{comp}] /sub listing"
    );

    // ---- small (fragment) file ----
    assert_eq!(
        read_whole_file(&fs, "/hello.txt"),
        b"hi\n",
        "[{comp}] fragment file"
    );

    // ---- nested small file ----
    assert_eq!(
        read_whole_file(&fs, "/sub/note.md"),
        b"# note\nsome words here\n",
        "[{comp}] nested file"
    );

    // ---- multi-block large file: full content + mid-file offset read ----
    let expect = pattern(300_000);
    let big = read_whole_file(&fs, "/sub/deep/big.bin");
    assert_eq!(big.len(), 300_000, "[{comp}] big.bin length");
    assert_eq!(big, expect, "[{comp}] big.bin full content");

    let big_inode = fs.lookup_path("/sub/deep/big.bin").unwrap();
    let mut mid = vec![0u8; 1000];
    fs.read_file(&big_inode, 200_000, &mut mid).unwrap();
    assert_eq!(
        &mid[..],
        &expect[200_000..201_000],
        "[{comp}] big.bin mid-file offset read"
    );

    // ---- symlink target ----
    let link = fs.lookup_path("/link").unwrap();
    assert!(link.is_symlink(), "[{comp}] /link is a symlink");
    assert_eq!(
        fs.read_symlink_target(&link).unwrap(),
        b"hello.txt",
        "[{comp}] symlink target"
    );

    // ---- cross-check the multi-block file against unsquashfs itself ----
    if unsquashfs_available() {
        let viaunsquash = unsquashfs_extract_file(&art.path, "/sub/deep/big.bin");
        assert_eq!(
            viaunsquash, big,
            "[{comp}] driver vs unsquashfs disagree on big.bin"
        );
    }

    // ---- missing path surfaces an error ----
    assert!(fs.lookup_path("/nope").is_err(), "[{comp}] missing path");
}

#[test]
#[ignore = "requires squashfs-tools (mksquashfs); run with -- --ignored"]
fn oracle_gzip() {
    assert_reads_back("gzip");
}

#[test]
#[ignore = "requires squashfs-tools (mksquashfs); run with -- --ignored"]
fn oracle_xz() {
    assert_reads_back("xz");
}

#[test]
#[ignore = "requires squashfs-tools (mksquashfs); run with -- --ignored"]
fn oracle_lz4() {
    assert_reads_back("lz4");
}

#[test]
#[ignore = "requires squashfs-tools (mksquashfs); run with -- --ignored"]
fn oracle_zstd() {
    assert_reads_back("zstd");
}

#[test]
#[ignore = "requires squashfs-tools (mksquashfs); run with -- --ignored"]
fn oracle_lzo() {
    assert_reads_back("lzo");
}

/// Legacy lzma (id 2): `is_supported()` claims it and `mksquashfs -comp lzma`
/// still writes it, but nothing read one back (#42). The in-kernel driver
/// has no lzma, so `validate-kernel-mount` cannot cover it; the
/// `unsquashfs` cross-check inside `assert_reads_back` is the external
/// oracle here.
#[test]
#[ignore = "requires squashfs-tools (mksquashfs); run with -- --ignored"]
fn oracle_lzma() {
    assert_reads_back("lzma");
}

/// Real x86 machine code from this host's own executables, or `None` on
/// a host whose executables are not x86 (an aarch64 one skips). The x86
/// BCJ filter only wins -- and so is only kept -- on real branch-dense
/// code: a synthetic imitation made every filter lose on #52, and so did
/// a 32-bit firmware blob tried here, so it is the host's programs, which
/// on CI's x86-64 runner are exactly that, and the test then requires the
/// filter to have been kept.
fn host_x86_code(want: usize) -> Option<Vec<u8>> {
    let candidates = [
        "/bin/bash",
        "/usr/bin/bash",
        "/usr/bin/perl",
        "/usr/bin/python3",
        "/usr/bin/git",
    ];
    let mut code = Vec::new();
    for path in candidates {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let machine = bytes.get(18..20).map(|m| u16::from_le_bytes([m[0], m[1]]));
        if bytes.starts_with(b"\x7fELF") && matches!(machine, Some(0x03 | 0x3E)) {
            code.extend_from_slice(&bytes);
            if code.len() >= want {
                code.truncate(want);
                return Some(code);
            }
        }
    }
    (code.len() >= 256 * 1024).then_some(code)
}

/// `-comp xz -Xbcj x86` DATA BLOCKS READ BACK (#52).
///
/// `mksquashfs` keeps a BCJ-filtered block only when the filter makes it
/// smaller, so the test first proves the filter was kept -- the filtered
/// image is smaller than the unfiltered one -- before reading. Without
/// that step a test over the wrong input passes, which is how this defect
/// survived: the tree listed perfectly and every file read failed with
/// "xz decompression failed".
#[test]
#[ignore = "requires squashfs-tools (mksquashfs); run with -- --ignored"]
fn oracle_xz_bcj_x86_reads_real_machine_code() {
    if !mksquashfs_available() {
        eprintln!("mksquashfs not available -- skipping");
        return;
    }
    let Some(code) = host_x86_code(2 << 20) else {
        eprintln!("no x86 machine code on this host -- skipping");
        return;
    };
    let tree = dir(vec![
        ("code.bin", file(&code)),
        ("hello.txt", file(b"hi\n")),
    ]);
    let plain = build_with_mksquashfs_args("xz", &tree, &["-no-xattrs"]);
    let bcj = build_with_mksquashfs_args("xz", &tree, &["-no-xattrs", "-Xbcj", "x86"]);
    assert!(
        bcj.bytes.len() < plain.bytes.len(),
        "the x86 filter was not kept ({} bytes filtered, {} plain), so this image cannot \
         test the filtered read path",
        bcj.bytes.len(),
        plain.bytes.len()
    );
    let fs = open_image(bcj.bytes.clone());
    assert_eq!(
        read_whole_file(&fs, "/code.bin"),
        code,
        "code.bin through BCJ"
    );
    assert_eq!(read_whole_file(&fs, "/hello.txt"), b"hi\n");
}
