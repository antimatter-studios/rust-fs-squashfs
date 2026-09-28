//! Oracle-validated compression tests.
//!
//! For each standard SquashFS compressor, build a fixture tree with the
//! real `mksquashfs -comp <c>` and read every path back through the
//! pure-Rust driver, asserting exact bytes. This is the ground truth that
//! proves the codec dispatch (gzip / xz / lz4 / zstd / lzo / lzma) decodes real
//! `mksquashfs` output, not just our own synthetic streams.
//!
//! Every tool call here happens in the harness guest, so this file is in
//! the `oracle` tier. Nothing is gated on a tool being present on the
//! host -- there is none to find, and a test that decided that and
//! passed would be the silent green this suite exists to refuse:
//!
//! ```sh
//! chore test:oracle
//! ```

mod common;

use common::*;
use fs_squashfs_test_support::{mksquashfs_from_guest_tree, sha256_hex, ScratchDir};

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
    //
    // UNCONDITIONALLY. This is the only assertion in this function that is
    // not this crate checking its own reading of its own bytes, so making
    // it optional made the strongest check the first one to vanish.
    let viaunsquash = unsquashfs_extract_file(&art.path, "/sub/deep/big.bin");
    assert_eq!(
        viaunsquash, big,
        "[{comp}] driver vs unsquashfs disagree on big.bin"
    );

    // ---- missing path surfaces an error ----
    assert!(fs.lookup_path("/nope").is_err(), "[{comp}] missing path");
}

#[test]
fn oracle_gzip() {
    assert_reads_back("gzip");
}

#[test]
fn oracle_xz() {
    assert_reads_back("xz");
}

#[test]
fn oracle_lz4() {
    assert_reads_back("lz4");
}

#[test]
fn oracle_zstd() {
    assert_reads_back("zstd");
}

#[test]
fn oracle_lzo() {
    assert_reads_back("lzo");
}

/// Legacy lzma (id 2): `is_supported()` claims it and `mksquashfs -comp lzma`
/// still writes it, but nothing read one back (#42). The in-kernel driver
/// has no lzma, so `validate-kernel-mount` cannot cover it; the
/// `unsquashfs` cross-check inside `assert_reads_back` is the external
/// oracle here.
#[test]
fn oracle_lzma() {
    assert_reads_back("lzma");
}
/// Where `scripts/vm-setup.sh` stages the x86-64 ELF the BCJ test needs.
///
/// A FIXTURE, NOT A SCAVENGED FILE. This used to read the host's own
/// `/bin/bash`, which meant the test skipped on every aarch64 machine --
/// including the aarch64 CI job -- and, where it did run, ran against
/// whatever that machine happened to have, so the same test on two x86
/// machines was not the same test (#117).
///
/// Nothing executes the payload; it is only compressed and read back. So
/// the architecture of the MACHINE is irrelevant and only the
/// architecture of the FILE matters, which is what makes this a fixture
/// the guest can stage whatever it is running on.
const X86_MACHINE_CODE: &str = "/var/lib/fs-squashfs-fixtures/x86_64-machine-code.bin";

/// The tree the BCJ test squashes, staged in the guest beside the
/// fixture.
///
/// It carries its own digest: `code.sha256` is what the guest computed
/// for the bytes it put in `code.bin`, so the read-back comparison needs
/// nothing carried across the boundary. A missing fixture FAILS here,
/// naming what provides it, rather than leaving the tree one file short
/// and the assertion below confusing.
fn bcj_tree_stage() -> String {
    format!(
        r#"
fixture={X86_MACHINE_CODE}
[ -f "$fixture" ] || {{
    echo "the x86-64 machine-code fixture is not in this guest." >&2
    echo "scripts/vm-setup.sh stages it; run 'chore vm:provision'." >&2
    exit 1
}}
cp "$fixture" code.bin
sha256sum code.bin | cut -d' ' -f1 > code.sha256
printf 'hi\n' > hello.txt
"#
    )
}

/// `-comp xz -Xbcj x86` DATA BLOCKS READ BACK (#52).
///
/// `mksquashfs` keeps a BCJ-filtered block only when the filter makes it
/// smaller, so the test first proves the filter was kept -- the filtered
/// image is smaller than the unfiltered one -- before reading. Without
/// that step a test over the wrong input passes, which is how this defect
/// survived: the tree listed perfectly and every file read failed with
/// "xz decompression failed".
///
/// That assertion is what makes the test mean anything, and it is why the
/// input cannot be synthesised: the filter only wins on real branch-dense
/// linked code. A synthetic instruction stream made every filter lose on
/// #52, and so did a 32-bit firmware blob. The fixture is a real x86-64
/// executable, and on it the filter wins by 4.2%.
#[test]
fn oracle_xz_bcj_x86_reads_real_machine_code() {
    let scratch = ScratchDir::new("bcj-x86");
    let stage = bcj_tree_stage();
    let plain_path = scratch.join("plain.sqfs");
    let bcj_path = scratch.join("bcj.sqfs");
    let common_args = ["-comp", "xz", "-no-xattrs", "-noappend", "-no-progress"];

    for (out, extra) in [(&plain_path, Vec::new()), (&bcj_path, vec!["-Xbcj", "x86"])] {
        let mut args: Vec<&str> = common_args.to_vec();
        args.extend(extra);
        let made = mksquashfs_from_guest_tree(out, &stage, &args);
        assert!(
            made.status.success(),
            "mksquashfs {args:?} on the BCJ tree failed: {}{}",
            String::from_utf8_lossy(&made.stdout),
            String::from_utf8_lossy(&made.stderr)
        );
    }

    let plain_len = std::fs::metadata(&plain_path).expect("plain image").len();
    let bcj_len = std::fs::metadata(&bcj_path).expect("filtered image").len();
    assert!(
        bcj_len < plain_len,
        "the x86 filter was not kept ({bcj_len} bytes filtered, {plain_len} plain), so \
         this image cannot test the filtered read path. The fixture is supposed to be \
         real x86-64 machine code -- check what scripts/vm-setup.sh staged at \
         {X86_MACHINE_CODE}."
    );

    let fs = open_image(std::fs::read(&bcj_path).expect("read the filtered image"));
    let code = read_whole_file(&fs, "/code.bin");
    // THE GUEST'S OWN DIGEST, read back through the same driver. The
    // bytes never cross the boundary, so the comparison is between what
    // the guest wrote and what this crate read out of the filtered
    // blocks -- which is the whole point of the test.
    let want = String::from_utf8(read_whole_file(&fs, "/code.sha256"))
        .expect("the digest is text")
        .trim()
        .to_string();
    assert_eq!(
        sha256_hex(&code),
        want,
        "the filtered blocks of code.bin did not read back as the guest wrote them \
         ({} bytes read)",
        code.len()
    );
    assert!(
        code.len() > 1 << 20,
        "the fixture is only {} bytes; the filter needs a payload big enough to span \
         many data blocks",
        code.len()
    );
    assert_eq!(read_whole_file(&fs, "/hello.txt"), b"hi\n");
}
