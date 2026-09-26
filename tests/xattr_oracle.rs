//! Extended attributes, against images `mksquashfs` wrote.
//!
//! # What is an oracle here and what is not
//!
//! Two independent references, because neither covers everything:
//!
//! - **`unsquashfs -x`** extracts the image and restores the attributes
//!   onto the extracted files, and the operating system's own tool then
//!   reads them back. That is a full round-trip through a tool this
//!   repository did not write. It only covers the `user.` namespace,
//!   though: restoring a `trusted.` or `security.` attribute needs
//!   privilege the test does not have, and unsquashfs drops them
//!   silently rather than failing.
//!
//! - **`mksquashfs -xattrs-add name=value`** puts an attribute on every
//!   file in the image, whatever the host filesystem supports. That
//!   reaches the two namespaces the round-trip cannot, and the reference
//!   is what mksquashfs was told to add.
//!
//! The unit tests in `src/xattr.rs` build tables with the offsets the
//! parser reads them from, so they cannot tell whether the layout is
//! right at all. This file can.
//!
//! Needs `mksquashfs`, `unsquashfs`, and a way to set an extended
//! attribute on a file — `setfattr` on Linux, `xattr` on macOS — and
//! skips without them.

mod common;
use common::open_image_path;

use fs_squashfs::Filesystem;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A value long enough that `mksquashfs` stores it out of line rather
/// than inline. Measured against 4.7.5: 300 bytes goes out of line, a
/// four-byte value repeated across three sets does not, so length is the
/// trigger rather than repetition alone.
const LONG_VALUE_LEN: usize = 300;

fn long_value() -> Vec<u8> {
    b"y".repeat(LONG_VALUE_LEN)
}

// THE `Setter` ENUM IS GONE, AND SO IS EVERYTHING IT NEGOTIATED.
//
// It existed to answer three host questions: is this Linux or macOS, is
// `setfattr` (or `xattr`) installed, and does setting a `user.*` attribute
// actually work in this TMPDIR — a filesystem, a mount option or a sandbox
// could all say no. Each answer could be "no", and "no" meant nine tests
// skipped and a suite that passed having compared nothing (#80), which is
// why it had grown an assertion to turn the skip into a failure under CI
// and nowhere else.
//
// The staging now happens INSIDE the harness guest, so all three questions
// have one answer and it is not in doubt: it is Linux, `scripts/vm-setup.sh`
// installs `attr`, and the guest's own disk carries extended attributes. A
// thing that cannot vary does not need detecting, and nothing here can skip.
//
// It also removes a question this file should never have been asked. The
// repository reaches the guest as a 9p mount, so whether a `user.*`
// attribute set on one side is visible on the other is a property of the
// host's filesystem and of the transport — not of SquashFS. Staging on the
// guest's own disk is the only place where `setfattr` means what it says.

struct Fixture {
    dir: ScratchDir,
}

/// The tree, built in the guest. `$SRC` is an empty directory and the
/// working directory; everything below is the guest's own disk, so
/// `setfattr` is simply `setfattr`.
///
/// Kept as one script rather than a call per attribute: it is one guest
/// round trip instead of eight, and the tree and its attributes are a
/// single fact about the fixture rather than a sequence that could half
/// happen.
const STAGE: &str = r#"
mkdir -p sub
for n in two.txt shared-a.txt shared-b.txt big.txt big-too.txt bare.txt sub/inner.txt; do
    printf 'contents of %s\n' "$n" > "$n"
done
long="$(printf 'y%.0s' $(seq 300))"
setfattr -n user.colour -v blue     two.txt
setfattr -n user.tag    -v alpha    two.txt
setfattr -n user.label  -v same     shared-a.txt
setfattr -n user.label  -v same     shared-b.txt
setfattr -n user.big    -v "$long"  big.txt
setfattr -n user.big    -v "$long"  big-too.txt
setfattr -n user.on-dir -v yes      sub
"#;

impl Fixture {
    /// NOT `Option`. There is nothing left to detect, so there is no way
    /// for this to decline — a guest that cannot set an attribute fails the
    /// provision in scripts/vm-setup.sh, naming itself.
    fn build() -> Fixture {
        Fixture {
            dir: ScratchDir::new("xattr"),
        }
    }

    /// Build an image from the staged tree with the given extra arguments.
    ///
    /// The image lands inside the repository, because the HOST opens it with
    /// this crate's reader; the tree it is built from never leaves the guest.
    fn image(&self, label: &str, extra: &[&str]) -> PathBuf {
        let img = self.dir.join(&format!("{label}.sqfs"));
        let _ = std::fs::remove_file(&img);
        let out = mksquashfs_from_guest_tree(
            &img,
            STAGE,
            &[&["-comp", "gzip", "-noappend", "-no-progress"], extra].concat(),
        );
        assert!(
            out.status.success(),
            "mksquashfs {extra:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        img
    }
}

/// Every extended attribute the GUEST'S OWN TOOLS see, for each path, after
/// `unsquashfs -x` has restored them onto extracted files.
///
/// One guest call: extract, then `getfattr -R` over the tree. `-e hex`
/// rather than base64 because decoding hex needs nothing but this function,
/// and a `user.big` value of 300 bytes has to survive the comparison
/// exactly.
///
/// A path with no attributes simply does not appear, which is what
/// `list_xattrs` reports for it too — so `bare.txt` compares equal as an
/// empty map rather than as a special case.
fn restored_xattrs(image: &Path) -> BTreeMap<String, BTreeMap<String, Vec<u8>>> {
    let script = format!(
        r#"set -euo pipefail
dest="$(mktemp -d /var/tmp/fs-squashfs-restore.XXXXXX)"
unsquashfs -f -x -d "$dest" {img} >/dev/null
cd "$dest"
getfattr -R -d -e hex . 2>/dev/null || true
rm -rf "$dest""#,
        img = guest_quote(&image.to_string_lossy()),
    );
    let out = oracle("bash").args(["-c", &script]).output();
    assert!(
        out.status.success(),
        "restoring the attributes in the guest failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let text = String::from_utf8_lossy(&out.stdout);
    let mut all: BTreeMap<String, BTreeMap<String, Vec<u8>>> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("# file: ") {
            // `getfattr -R .` reports `./two.txt`; the tests ask by `two.txt`.
            let path = rest.trim().trim_start_matches("./").to_owned();
            all.entry(path.clone()).or_default();
            current = Some(path);
        } else if let Some((name, value)) = line.split_once('=') {
            let Some(path) = current.as_ref() else {
                continue;
            };
            let hex = value.trim().trim_start_matches("0x");
            let bytes = (0..hex.len())
                .step_by(2)
                .map(|i| {
                    u8::from_str_radix(&hex[i..i + 2], 16)
                        .unwrap_or_else(|e| panic!("getfattr hex {value:?}: {e}"))
                })
                .collect();
            all.entry(path.clone())
                .or_default()
                .insert(name.trim().to_owned(), bytes);
        }
    }
    all
}

/// What this driver says, for one path.
fn ours(fs: &Filesystem, path: &str) -> BTreeMap<String, Vec<u8>> {
    let inode = fs
        .lookup_path(path)
        .unwrap_or_else(|e| panic!("lookup {path}: {e:?}"));
    fs.list_xattrs(&inode)
        .unwrap_or_else(|e| panic!("list_xattrs {path}: {e:?}"))
        .into_iter()
        .map(|e| (String::from_utf8_lossy(&e.name).into_owned(), e.value))
        .collect()
}

/// The round trip: build an image with attributes, extract it with
/// `unsquashfs -x`, and require this driver to report exactly what the
/// operating system reads off the extracted files.
///
/// This is the check the issue asked for, and it is the strongest one
/// available: nothing in the comparison came from this repository.
#[test]
fn what_unsquashfs_restores_is_what_this_driver_reports() {
    let fx = Fixture::build();
    let img = fx.image("roundtrip", &["-xattrs"]);
    let fs = open_image_path(&img);

    // unsquashfs extracts and restores, and the guest's own getfattr reads
    // the result back. Nothing in this comparison came from this repository.
    let restored = restored_xattrs(&img);

    let mut checked = 0;
    for path in [
        "two.txt",
        "shared-a.txt",
        "shared-b.txt",
        "big.txt",
        "big-too.txt",
        "bare.txt",
        "sub",
        "sub/inner.txt",
    ] {
        let reference = restored.get(path).cloned().unwrap_or_default();
        let got = ours(&fs, &format!("/{path}"));
        assert_eq!(
            got, reference,
            "/{path}: this driver and unsquashfs disagree"
        );
        checked += got.len();
    }
    assert!(
        checked >= 7,
        "only {checked} attributes survived the round trip — the comparison is \
         of almost nothing, so it proves almost nothing"
    );
}

/// Two files carrying the same attribute share one id-table entry: that
/// indirection is the whole reason the table exists, and a driver that
/// resolved the index wrongly would report one file's attributes on
/// another. Both must come back, and identically.
#[test]
fn two_files_sharing_a_set_both_read_it() {
    let fx = Fixture::build();
    let fs = open_image_path(&fx.image("shared", &["-xattrs"]));
    let a = ours(&fs, "/shared-a.txt");
    let b = ours(&fs, "/shared-b.txt");
    assert_eq!(
        a.get("user.label").map(|v| v.as_slice()),
        Some(&b"same"[..])
    );
    assert_eq!(a, b, "two files sharing a set read differently");
}

/// A value long enough to be stored out of line, referenced from two
/// different sets. The out-of-line record is a bare length and its
/// bytes, reached through a second packed reference, and reading it must
/// not depend on which set asked.
#[test]
fn an_out_of_line_value_reads_in_full_from_either_set() {
    let fx = Fixture::build();
    let fs = open_image_path(&fx.image("ool", &["-xattrs"]));
    for path in ["/big.txt", "/big-too.txt"] {
        let got = ours(&fs, path);
        let value = got
            .get("user.big")
            .unwrap_or_else(|| panic!("{path} lost user.big: {:?}", got.keys()));
        assert_eq!(value.len(), LONG_VALUE_LEN, "{path}: value truncated");
        assert_eq!(value, &long_value(), "{path}: value differs");
    }
}

/// The two namespaces the round trip cannot reach.
///
/// Restoring a `trusted.` or `security.` attribute needs privilege this
/// test does not have, so `unsquashfs` drops them and the extracted file
/// cannot be the reference. `mksquashfs -xattrs-add` puts them in the
/// image regardless of what the host filesystem supports, and what it
/// was told to add is the reference instead.
///
/// The prefix is stored as a small integer rather than spelled out, so
/// getting the table wrong swaps one namespace for another — and a
/// `security.` attribute reported as `user.` is not a cosmetic error.
#[test]
fn the_trusted_and_security_namespaces_are_assembled_correctly() {
    let fx = Fixture::build();
    let fs = open_image_path(&fx.image(
        "namespaces",
        &[
            "-xattrs",
            "-xattrs-add",
            "trusted.level=high",
            "-xattrs-add",
            "security.demo=labelled",
        ],
    ));
    // `-xattrs-add` puts a non-user attribute on every file, so any of
    // them will do — including the one that carries nothing else.
    let got = ours(&fs, "/bare.txt");
    assert_eq!(
        got.get("trusted.level").map(|v| v.as_slice()),
        Some(&b"high"[..]),
        "trusted.level missing or misread: {:?}",
        got.keys().collect::<Vec<_>>()
    );
    assert_eq!(
        got.get("security.demo").map(|v| v.as_slice()),
        Some(&b"labelled"[..]),
        "security.demo missing or misread: {:?}",
        got.keys().collect::<Vec<_>>()
    );
    // And the user namespace still resolves alongside them, so a table
    // read off by one would not pass by relabelling everything.
    let two = ours(&fs, "/two.txt");
    assert_eq!(
        two.get("user.colour").map(|v| v.as_slice()),
        Some(&b"blue"[..])
    );
}

/// A file with no attributes in an image that has them, and every file
/// in an image built with `-no-xattrs`. Both are empty lists rather than
/// errors: a caller cannot act on the difference between "none" and
/// "could not look".
#[test]
fn absence_is_an_empty_list_and_not_a_failure() {
    let fx = Fixture::build();
    let with = open_image_path(&fx.image("with", &["-xattrs"]));
    let bare = with.lookup_path("/bare.txt").expect("bare.txt");
    assert!(with.list_xattrs(&bare).unwrap().is_empty());
    assert_eq!(with.get_xattr(&bare, b"user.colour").unwrap(), None);

    let without = open_image_path(&fx.image("without", &["-no-xattrs"]));
    for path in ["/two.txt", "/bare.txt", "/sub"] {
        let inode = without.lookup_path(path).expect(path);
        assert!(
            without.list_xattrs(&inode).unwrap().is_empty(),
            "{path} reported attributes from an image built without them"
        );
    }
}

/// `get_xattr` must agree with `list_xattrs` name for name, since one is
/// implemented over the other and a caller may use either.
#[test]
fn getting_one_attribute_agrees_with_listing_them_all() {
    let fx = Fixture::build();
    let fs = open_image_path(&fx.image("get", &["-xattrs"]));
    for path in ["/two.txt", "/big.txt", "/sub"] {
        let inode = fs.lookup_path(path).expect(path);
        let listed = fs.list_xattrs(&inode).unwrap();
        assert!(!listed.is_empty(), "{path} has no attributes to compare");
        for e in &listed {
            assert_eq!(
                fs.get_xattr(&inode, &e.name).unwrap().as_ref(),
                Some(&e.value),
                "{path}: get_xattr disagrees with list_xattrs"
            );
        }
        assert_eq!(fs.get_xattr(&inode, b"user.never-set").unwrap(), None);
    }
}

// ---------------------------------------------------------------------
// The C surface
//
// A staticlib does not re-export unmangled C symbols to an integration
// test, so these call the public items in `fs_squashfs::capi` directly.
// That verifies the logic behind the exports, which is where the buffer
// arithmetic and the error mapping live.
// ---------------------------------------------------------------------

use fs_squashfs::capi::*;
use fs_squashfs_test_support::{guest_quote, mksquashfs_from_guest_tree, oracle, ScratchDir};
use std::ffi::{c_char, c_void, CStr, CString};

/// `ENOENT`, as the header documents it. Spelled out rather than
/// imported, so the test asserts the contract rather than mirroring the
/// source.
const ENOENT: i32 = 2;

fn cstr(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn last_error() -> String {
    unsafe { CStr::from_ptr(fs_squashfs_last_error()) }
        .to_string_lossy()
        .into_owned()
}

fn mount(img: &Path) -> *mut fs_squashfs_fs_t {
    let c = cstr(img.to_str().unwrap());
    let fs = unsafe { fs_squashfs_mount(c.as_ptr()) };
    assert!(!fs.is_null(), "mount failed: {}", last_error());
    fs
}

/// The probe form reports the size the real call needs, and the names
/// come back NUL-separated. A caller allocates on the strength of that
/// number, so the two must agree exactly.
#[test]
fn the_c_listxattr_probe_agrees_with_the_real_call() {
    let fx = Fixture::build();
    let img = fx.image("capi-list", &["-xattrs"]);
    let fs = mount(&img);
    let path = cstr("/two.txt");

    let needed = unsafe { fs_squashfs_listxattr(fs, path.as_ptr(), std::ptr::null_mut(), 0) };
    assert!(needed > 0, "{}", last_error());

    let mut buf = vec![0u8; needed as usize];
    let wrote = unsafe {
        fs_squashfs_listxattr(
            fs,
            path.as_ptr(),
            buf.as_mut_ptr().cast::<c_char>(),
            buf.len(),
        )
    };
    assert_eq!(wrote, needed, "the probe and the real call disagree");

    let mut names: Vec<&[u8]> = buf.split(|&b| b == 0).collect();
    // The buffer ends with a terminator, so the split leaves an empty
    // tail; a name is never empty, so this is unambiguous.
    assert_eq!(names.pop(), Some(&b""[..]));
    names.sort_unstable();
    assert_eq!(names, vec![&b"user.colour"[..], &b"user.tag"[..]]);

    // A buffer too small takes whole names only: a half-written name is
    // not a name, and a caller that parsed one would act on a name that
    // does not exist.
    let mut small = vec![0xAAu8; 12];
    let got = unsafe {
        fs_squashfs_listxattr(
            fs,
            path.as_ptr(),
            small.as_mut_ptr().cast::<c_char>(),
            small.len(),
        )
    };
    assert_eq!(got, needed, "a short write must still report the full size");
    let written = &small[..small.iter().position(|&b| b == 0xAA).unwrap_or(small.len())];
    assert!(written.is_empty() || written.last() == Some(&0));

    unsafe { fs_squashfs_umount(fs) };
}

/// The value comes back at its full length through the C surface too,
/// including the out-of-line case, and the probe reports it without
/// writing.
#[test]
fn the_c_getxattr_returns_the_value_and_its_length() {
    let fx = Fixture::build();
    let img = fx.image("capi-get", &["-xattrs"]);
    let fs = mount(&img);
    let path = cstr("/big.txt");
    let name = cstr("user.big");

    let size =
        unsafe { fs_squashfs_getxattr(fs, path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0) };
    assert_eq!(size, LONG_VALUE_LEN as i64, "{}", last_error());

    let mut buf = vec![0u8; LONG_VALUE_LEN];
    let n = unsafe {
        fs_squashfs_getxattr(
            fs,
            path.as_ptr(),
            name.as_ptr(),
            buf.as_mut_ptr().cast::<c_void>(),
            buf.len(),
        )
    };
    assert_eq!(n, LONG_VALUE_LEN as i64);
    assert_eq!(buf, long_value());

    // Absent is -1/ENOENT, and a file with none lists zero — a success,
    // not a failure. A caller testing `<= 0` would merge the two.
    let missing = unsafe {
        fs_squashfs_getxattr(
            fs,
            path.as_ptr(),
            cstr("user.never-set").as_ptr(),
            std::ptr::null_mut(),
            0,
        )
    };
    assert_eq!(missing, -1);
    assert_eq!(fs_squashfs_last_errno(), ENOENT, "{}", last_error());
    assert_eq!(
        unsafe { fs_squashfs_listxattr(fs, cstr("/bare.txt").as_ptr(), std::ptr::null_mut(), 0) },
        0,
        "{}",
        last_error()
    );

    unsafe { fs_squashfs_umount(fs) };
}

/// NULL tolerance. A caller passing NULL should get a failure, not a
/// crash inside its own process.
#[test]
fn the_c_xattr_entry_points_tolerate_nulls() {
    let mut buf = [0u8; 8];
    assert_eq!(
        unsafe {
            fs_squashfs_listxattr(
                std::ptr::null_mut(),
                cstr("/x").as_ptr(),
                std::ptr::null_mut(),
                0,
            )
        },
        -1
    );
    assert_eq!(
        unsafe {
            fs_squashfs_getxattr(
                std::ptr::null_mut(),
                cstr("/x").as_ptr(),
                cstr("user.x").as_ptr(),
                buf.as_mut_ptr().cast::<c_void>(),
                buf.len(),
            )
        },
        -1
    );

    let fx = Fixture::build();
    let img = fx.image("capi-null", &["-xattrs"]);
    let fs = mount(&img);
    assert_eq!(
        unsafe { fs_squashfs_listxattr(fs, std::ptr::null(), std::ptr::null_mut(), 0) },
        -1
    );
    assert_eq!(
        unsafe {
            fs_squashfs_getxattr(
                fs,
                cstr("/two.txt").as_ptr(),
                std::ptr::null(),
                std::ptr::null_mut(),
                0,
            )
        },
        -1
    );
    // A path that is not there fails as ENOENT rather than as EIO, so a
    // user is not sent looking for a hardware fault.
    assert_eq!(
        unsafe { fs_squashfs_listxattr(fs, cstr("/nope.txt").as_ptr(), std::ptr::null_mut(), 0) },
        -1
    );
    assert_eq!(fs_squashfs_last_errno(), ENOENT, "{}", last_error());
    unsafe { fs_squashfs_umount(fs) };
}
