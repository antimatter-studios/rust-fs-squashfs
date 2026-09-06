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
use common::{mksquashfs_available, open_image_path, unsquashfs_available};

use fs_squashfs::Filesystem;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A value long enough that `mksquashfs` stores it out of line rather
/// than inline. Measured against 4.7.5: 300 bytes goes out of line, a
/// four-byte value repeated across three sets does not, so length is the
/// trigger rather than repetition alone.
const LONG_VALUE_LEN: usize = 300;

fn long_value() -> Vec<u8> {
    b"y".repeat(LONG_VALUE_LEN)
}

/// How to set an extended attribute on a file, on this machine.
///
/// `setfattr` on Linux and `xattr` on macOS take different arguments and
/// neither exists on both, so the test finds one and remembers which.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Setter {
    SetFattr,
    MacXattr,
}

impl Setter {
    fn detect() -> Option<Setter> {
        let dir = tempfile::tempdir().ok()?;
        let probe = dir.path().join("probe");
        std::fs::write(&probe, b"x").ok()?;
        [Setter::SetFattr, Setter::MacXattr]
            .into_iter()
            .find(|s| s.set(&probe, "user.probe", b"x").is_ok())
    }

    fn set(self, path: &Path, name: &str, value: &[u8]) -> Result<(), String> {
        let value = String::from_utf8(value.to_vec()).expect("test values are text");
        let out = match self {
            Setter::SetFattr => Command::new("setfattr")
                .args(["-n", name, "-v", &value])
                .arg(path)
                .output(),
            Setter::MacXattr => Command::new("xattr")
                .args(["-w", name, &value])
                .arg(path)
                .output(),
        };
        match out {
            Ok(o) if o.status.success() => Ok(()),
            Ok(o) => Err(String::from_utf8_lossy(&o.stderr).into_owned()),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Every attribute on a path, as the operating system reports it.
    ///
    /// `com.apple.*` names are dropped: macOS adds `com.apple.provenance`
    /// to files it writes, so it appears on an extracted file without
    /// ever having been in the image.
    fn list(self, path: &Path) -> BTreeMap<String, Vec<u8>> {
        let mut out = BTreeMap::new();
        let names = match self {
            Setter::SetFattr => {
                let o = Command::new("getfattr")
                    .args(["--absolute-names", "-d", "-m", "-"])
                    .arg(path)
                    .output()
                    .expect("spawn getfattr");
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
                    .filter_map(|l| l.split_once('=').map(|(n, _)| n.to_string()))
                    .collect::<Vec<_>>()
            }
            Setter::MacXattr => {
                let o = Command::new("xattr")
                    .arg(path)
                    .output()
                    .expect("spawn xattr");
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty())
                    .collect::<Vec<_>>()
            }
        };
        for name in names {
            if name.starts_with("com.apple.") {
                continue;
            }
            let value = match self {
                Setter::SetFattr => {
                    let o = Command::new("getfattr")
                        .args(["--absolute-names", "--only-values", "-n", &name])
                        .arg(path)
                        .output()
                        .expect("spawn getfattr");
                    o.stdout
                }
                Setter::MacXattr => {
                    let o = Command::new("xattr")
                        .args(["-p", &name])
                        .arg(path)
                        .output()
                        .expect("spawn xattr");
                    // `xattr -p` prints the value with a trailing newline
                    // it did not read from the file.
                    let mut v = o.stdout;
                    if v.last() == Some(&b'\n') {
                        v.pop();
                    }
                    v
                }
            };
            out.insert(name, value);
        }
        out
    }
}

/// The source tree every image below is built from.
///
/// The shapes are chosen so a failure cannot hide:
///
/// - `two.txt` carries two attributes, so the walk over a set has to
///   advance correctly from the first record to the second;
/// - `shared-a.txt` and `shared-b.txt` carry the *same* attribute, which
///   is what makes them share one id-table entry — the indirection the
///   format exists for;
/// - `big.txt` carries a value long enough to be stored out of line,
///   which is a different code path from an inline one;
/// - `big-too.txt` carries the same long value, so the out-of-line
///   record is referenced from two sets and reading it must not depend
///   on which one asked;
/// - `bare.txt` carries none, so an empty list is distinguished from a
///   failure to look;
/// - `sub/` is a directory with an attribute, since directories carry
///   them and nothing else here would show it.
struct Fixture {
    dir: tempfile::TempDir,
    setter: Setter,
}

impl Fixture {
    fn build() -> Option<Fixture> {
        let setter = Setter::detect()?;
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("src");
        std::fs::create_dir_all(src.join("sub")).expect("create tree");
        for name in [
            "two.txt",
            "shared-a.txt",
            "shared-b.txt",
            "big.txt",
            "big-too.txt",
            "bare.txt",
            "sub/inner.txt",
        ] {
            std::fs::write(src.join(name), format!("contents of {name}\n")).expect("write");
        }
        let long = String::from_utf8(long_value()).unwrap();
        for (path, name, value) in [
            ("two.txt", "user.colour", "blue"),
            ("two.txt", "user.tag", "alpha"),
            ("shared-a.txt", "user.label", "same"),
            ("shared-b.txt", "user.label", "same"),
            ("big.txt", "user.big", long.as_str()),
            ("big-too.txt", "user.big", long.as_str()),
            ("sub", "user.on-dir", "yes"),
        ] {
            setter
                .set(&src.join(path), name, value.as_bytes())
                .unwrap_or_else(|e| panic!("setting {name} on {path}: {e}"));
        }
        Some(Fixture { dir, setter })
    }

    fn src(&self) -> PathBuf {
        self.dir.path().join("src")
    }

    /// Build an image from the tree with the given extra arguments.
    fn image(&self, label: &str, extra: &[&str]) -> PathBuf {
        let img = self.dir.path().join(format!("{label}.sqfs"));
        let _ = std::fs::remove_file(&img);
        let out = Command::new("mksquashfs")
            .arg(self.src())
            .arg(&img)
            .args(["-comp", "gzip", "-noappend", "-no-progress"])
            .args(extra)
            .output()
            .expect("spawn mksquashfs");
        assert!(
            out.status.success(),
            "mksquashfs {extra:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        img
    }
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

fn tools_ready() -> bool {
    if !mksquashfs_available() || !unsquashfs_available() {
        eprintln!("squashfs-tools not on PATH — skipping");
        return false;
    }
    true
}

/// The round trip: build an image with attributes, extract it with
/// `unsquashfs -x`, and require this driver to report exactly what the
/// operating system reads off the extracted files.
///
/// This is the check the issue asked for, and it is the strongest one
/// available: nothing in the comparison came from this repository.
#[test]
fn what_unsquashfs_restores_is_what_this_driver_reports() {
    if !tools_ready() {
        return;
    }
    let Some(fx) = Fixture::build() else {
        eprintln!("no way to set an extended attribute on this machine — skipping");
        return;
    };
    let img = fx.image("roundtrip", &["-xattrs"]);
    let fs = open_image_path(&img);

    let dest = tempfile::tempdir().expect("tempdir");
    let out = Command::new("unsquashfs")
        .args(["-f", "-x", "-d"])
        .arg(dest.path())
        .arg(&img)
        .output()
        .expect("spawn unsquashfs");
    assert!(
        out.status.success(),
        "unsquashfs failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

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
        let reference = fx.setter.list(&dest.path().join(path));
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
    if !tools_ready() {
        return;
    }
    let Some(fx) = Fixture::build() else {
        eprintln!("no way to set an extended attribute — skipping");
        return;
    };
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
    if !tools_ready() {
        return;
    }
    let Some(fx) = Fixture::build() else {
        eprintln!("no way to set an extended attribute — skipping");
        return;
    };
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
    if !tools_ready() {
        return;
    }
    let Some(fx) = Fixture::build() else {
        eprintln!("no way to set an extended attribute — skipping");
        return;
    };
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
    if !tools_ready() {
        return;
    }
    let Some(fx) = Fixture::build() else {
        eprintln!("no way to set an extended attribute — skipping");
        return;
    };
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
    if !tools_ready() {
        return;
    }
    let Some(fx) = Fixture::build() else {
        eprintln!("no way to set an extended attribute — skipping");
        return;
    };
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
    if !tools_ready() {
        return;
    }
    let Some(fx) = Fixture::build() else {
        eprintln!("no way to set an extended attribute — skipping");
        return;
    };
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
    if !tools_ready() {
        return;
    }
    let Some(fx) = Fixture::build() else {
        eprintln!("no way to set an extended attribute — skipping");
        return;
    };
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

    if !tools_ready() {
        return;
    }
    let Some(fx) = Fixture::build() else {
        eprintln!("no way to set an extended attribute — skipping the rest");
        return;
    };
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
