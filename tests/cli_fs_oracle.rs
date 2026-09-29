//! `fs.squashfs`, the tool, held to squashfs-tools on images this crate
//! did not make: `mksquashfs` builds each one in the harness guest, once
//! per compressor and once per layout choice that changes what a reader
//! walks, and then
//!
//! - `unsquashfs -lls` and `fs.squashfs ls` agree entry for entry: name,
//!   type, permissions, size and symlink target, in every directory;
//! - every regular file `fs.squashfs read` returns hashes to what
//!   `unsquashfs -cat` returns for it AND to the source file the guest
//!   hashed before building;
//! - `unsquashfs -s` agrees with `fs.squashfs get` on block size,
//!   compression, inode count and bytes used, and `get` says the
//!   compressor `-comp` asked for;
//! - an image damaged three ways (its magic, its length, a metadata block)
//!   is refused by `unsquashfs` and by the tool, with a structured error
//!   and nothing on stdout.
//!
//! Every tool call runs in the harness VM.

mod cli_support;

use cli_support::*;
use fs_squashfs_test_support::{mksquashfs_from_guest_tree, oracle, ScratchDir};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The source tree, staged in the guest: an empty file, one byte, a block
/// and a block plus one (at `-b 4096`), a file ending in a fragment, a
/// sparse file, a symlink, a hardlink, a long name, deep nesting, a
/// directory large enough to span metadata blocks, and an extended
/// attribute. Its last lines print each regular file's SHA-256, which is
/// the source this test compares the tool's reads with.
const STAGE: &str = r#"
mkdir -p a/b/c/d/e/f wide
: > empty
printf x > one
head -c 4096 /dev/urandom > block
head -c 4097 /dev/urandom > block_plus_one
head -c 1048576 /dev/urandom > big
head -c 5000 /dev/urandom > fragment_tail
truncate -s 3145728 sparse
printf 'end of the hole' >> sparse
ln -s one link
ln -s a/b/c/d/e/f/deep deep_link
ln big hardlink_to_big
printf long > "$(printf 'n%.0s' $(seq 1 255))"
printf deep > a/b/c/d/e/f/deep
for i in $(seq 1 400); do printf '%s' "$i" > "wide/entry-with-a-longish-name-$i"; done
setfattr -n user.cli -v oracle one
find . -type f | sort | while read -r f; do printf 'SRC %s %s\n' "$(sha256sum "$f" | cut -d' ' -f1)" "${f#./}"; done
"#;

/// An image built from [`STAGE`], and the source hashes the guest printed.
struct Built {
    _dir: ScratchDir,
    image: PathBuf,
    source: BTreeMap<String, String>,
}

impl Built {
    fn image(&self) -> String {
        self.image.display().to_string()
    }
}

fn build(label: &str, comp: &str, extra: &[&str]) -> Built {
    let dir = ScratchDir::new(&format!("cli-oracle-{label}"));
    let image = dir.join(&format!("{label}.sqfs"));
    let args = [
        &["-comp", comp, "-b", "4096", "-noappend", "-no-progress"][..],
        extra,
    ]
    .concat();
    let out = mksquashfs_from_guest_tree(&image, STAGE, &args);
    assert!(
        out.status.success(),
        "mksquashfs {args:?} failed:\n{}{}",
        stdout(&out),
        stderr(&out)
    );
    let source: BTreeMap<String, String> = stdout(&out)
        .lines()
        .filter_map(|l| l.strip_prefix("SRC "))
        .filter_map(|l| l.split_once(' '))
        .map(|(sha, path)| (path.to_string(), sha.to_string()))
        .collect();
    assert!(
        source.len() > 400,
        "the guest hashed only {} source files:\n{}",
        source.len(),
        stdout(&out)
    );
    Built {
        _dir: dir,
        image,
        source,
    }
}

/// One entry as `unsquashfs -lls` prints it.
#[derive(Debug)]
struct Listed {
    mode: String,
    size: String,
    target: Option<String>,
}

/// `unsquashfs -lls`, keyed by path (without the `squashfs-root` it
/// prefixes). The root itself is left out: `ls` lists what is in it.
fn unsquashfs_lls(image: &str) -> BTreeMap<String, Listed> {
    let out = oracle("unsquashfs").args(["-lls", image]).output();
    assert!(
        out.status.success(),
        "unsquashfs -lls {image}: {}",
        stderr(&out)
    );
    let mut listed = BTreeMap::new();
    for line in stdout(&out).lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        let is_entry = words.len() >= 6
            && words[0].len() == 10
            && "-dlbcps".contains(&words[0][..1])
            && words[0][1..].chars().all(|c| "rwxsStT-".contains(c));
        if !is_entry {
            continue;
        }
        // mode owner size date time path [-> target]
        let rest = words[5..].join(" ");
        let (path, target) = match rest.split_once(" -> ") {
            Some((p, t)) => (p.to_string(), Some(t.to_string())),
            None => (rest, None),
        };
        let path = path
            .strip_prefix("squashfs-root")
            .unwrap_or(&path)
            .trim_start_matches('/')
            .to_string();
        if path.is_empty() {
            continue;
        }
        listed.insert(
            path,
            Listed {
                mode: words[0].to_string(),
                size: words[2].to_string(),
                target,
            },
        );
    }
    assert!(
        listed.len() > 400,
        "unsquashfs -lls listed {} entries",
        listed.len()
    );
    listed
}

/// `ls -l`'s ten characters, from an `ls` entry's type and octal mode.
fn mode_string(kind: &str, octal: &str) -> String {
    let t = match kind {
        "dir" => 'd',
        "symlink" => 'l',
        "char" => 'c',
        "block" => 'b',
        "fifo" => 'p',
        "socket" => 's',
        _ => '-',
    };
    let bits = u32::from_str_radix(octal, 8).expect("an octal mode");
    let mut s = String::from(t);
    for shift in [6, 3, 0] {
        let b = (bits >> shift) & 7;
        s.push(if b & 4 != 0 { 'r' } else { '-' });
        s.push(if b & 2 != 0 { 'w' } else { '-' });
        s.push(if b & 1 != 0 { 'x' } else { '-' });
    }
    s
}

/// Every entry the tool lists, walking from `/`, keyed by path.
fn tool_tree(image: &str) -> BTreeMap<String, String> {
    let mut all = BTreeMap::new();
    let mut dirs = vec![String::new()];
    while let Some(dir) = dirs.pop() {
        let listing = stdout(&ok(tool("fs.squashfs").args([
            image,
            "ls",
            &format!("/{dir}"),
        ])));
        for entry in listing.split("\n  {").skip(1) {
            let name = json_field(entry, "name");
            let path = if dir.is_empty() {
                name
            } else {
                format!("{dir}/{name}")
            };
            if json_field(entry, "type") == "dir" {
                dirs.push(path.clone());
            }
            all.insert(path, entry.to_string());
        }
    }
    all
}

fn check_image(built: &Built, comp: &str) {
    let image = built.image();

    // ls against unsquashfs -lls, entry for entry.
    let theirs = unsquashfs_lls(&image);
    let ours = tool_tree(&image);
    assert_eq!(
        ours.keys().collect::<Vec<_>>(),
        theirs.keys().collect::<Vec<_>>(),
        "fs.squashfs ls and unsquashfs -lls list different paths"
    );
    for (path, entry) in &ours {
        let listed = &theirs[path];
        let kind = json_field(entry, "type");
        assert_eq!(
            mode_string(&kind, &json_field(entry, "mode")),
            listed.mode,
            "/{path}: type and permissions"
        );
        if kind != "dir" {
            // A directory's size is its listing's, which unsquashfs
            // prints differently; everything else is the file's own.
            assert_eq!(json_field(entry, "size"), listed.size, "/{path}: size");
        }
        let target = entry
            .contains("\"target\": ")
            .then(|| json_field(entry, "target"));
        assert_eq!(target, listed.target, "/{path}: symlink target");
    }

    // read against unsquashfs -cat and the source. Every file against the
    // source; against `-cat`, every file outside wide/ and three inside it,
    // in one guest call, because four hundred one-line files ask the same
    // question four hundred times.
    let catted: Vec<&String> = built
        .source
        .keys()
        .filter(|p| {
            !p.starts_with("wide/")
                || p.ends_with("-1")
                || p.ends_with("-200")
                || p.ends_with("-400")
        })
        .collect();
    // One line of script, the paths as its arguments: the oracle prints
    // each call it makes, and a script of a line per file would print that
    // many lines into the tier's log.
    let script = "set -eu; img=\"$1\"; shift; for p in \"$@\"; do \
                  printf '%s %s\\n' \"$(unsquashfs -cat \"$img\" \"$p\" | sha256sum | cut -d' ' -f1)\" \"$p\"; done";
    let mut args: Vec<&str> = vec!["-c", script, "cat-hash", &image];
    args.extend(catted.iter().map(|p| p.as_str()));
    let out = oracle("bash").args(&args).output();
    assert!(out.status.success(), "unsquashfs -cat: {}", stderr(&out));
    let cat: BTreeMap<String, String> = stdout(&out)
        .lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(sha, path)| (path.to_string(), sha.to_string()))
        .collect();
    assert_eq!(cat.len(), catted.len(), "unsquashfs -cat hashed {cat:?}");
    for (path, source_sha) in &built.source {
        let read = ok(tool("fs.squashfs").args([&image, "read", &format!("/{path}")]));
        let ours = fs_squashfs_test_support::sha256_hex(&read.stdout);
        if let Some(theirs) = cat.get(path) {
            assert_eq!(
                &ours, theirs,
                "/{path}: fs.squashfs read and unsquashfs -cat differ"
            );
        }
        assert_eq!(
            &ours, source_sha,
            "/{path}: fs.squashfs read and the source differ"
        );
    }

    // get against unsquashfs -s.
    let get = stdout(&ok(tool("fs.squashfs").args([&image, "get"])));
    let out = oracle("unsquashfs").args(["-s", &image]).output();
    assert!(out.status.success(), "unsquashfs -s: {}", stderr(&out));
    let stat = stdout(&out);
    let field = |prefix: &str| -> String {
        stat.lines()
            .find_map(|l| l.strip_prefix(prefix))
            .map(|v| v.split_whitespace().next().unwrap_or("").to_string())
            .unwrap_or_else(|| panic!("unsquashfs -s printed no `{prefix}` line:\n{stat}"))
    };
    assert_eq!(field("Block size "), json_field(&get, "block_size"));
    assert_eq!(field("Compression "), json_field(&get, "compression"));
    assert_eq!(json_field(&get, "compression"), comp);
    assert_eq!(field("Number of inodes "), json_field(&get, "inode_count"));
    assert_eq!(field("Filesystem size "), json_field(&get, "bytes_used"));
    assert_eq!(
        json_field(&get, "total_bytes"),
        json_field(&get, "bytes_used")
    );
    assert_eq!(json_field(&get, "label"), "null");
}

#[test]
fn gzip() {
    check_image(&build("gzip", "gzip", &[]), "gzip");
}

#[test]
fn xz() {
    check_image(&build("xz", "xz", &[]), "xz");
}

#[test]
fn lz4() {
    check_image(&build("lz4", "lz4", &[]), "lz4");
}

#[test]
fn zstd() {
    check_image(&build("zstd", "zstd", &[]), "zstd");
}

#[test]
fn lzo() {
    check_image(&build("lzo", "lzo", &[]), "lzo");
}

#[test]
fn without_an_export_table() {
    let built = build("no-exports", "gzip", &["-no-exports"]);
    check_image(&built, "gzip");
    let get = stdout(&ok(tool("fs.squashfs").args([
        &built.image(),
        "get",
        "squashfs.exportable",
    ])));
    assert!(get.contains("\"squashfs.exportable\": false"), "{get}");
}

#[test]
fn without_fragments() {
    let built = build("no-fragments", "gzip", &["-no-fragments"]);
    check_image(&built, "gzip");
    let get = stdout(&ok(tool("fs.squashfs").args([&built.image(), "get"])));
    assert_eq!(json_field(&get, "fragment_count"), "0", "{get}");
}

#[test]
fn with_every_file_ending_in_a_fragment() {
    let built = build("always-fragments", "gzip", &["-always-use-fragments"]);
    check_image(&built, "gzip");
    let get = stdout(&ok(tool("fs.squashfs").args([&built.image(), "get"])));
    assert!(get.contains("\"always_fragments\""), "{get}");
}

#[test]
fn without_extended_attributes() {
    let built = build("no-xattrs", "gzip", &["-no-xattrs"]);
    check_image(&built, "gzip");
    let get = stdout(&ok(tool("fs.squashfs").args([&built.image(), "get"])));
    assert!(get.contains("\"no_xattrs\""), "{get}");
}

/// Copy `image` to `path` with `damage` applied.
fn damaged(image: &Path, path: &Path, damage: impl FnOnce(&mut Vec<u8>)) -> String {
    let mut bytes = std::fs::read(image).unwrap();
    damage(&mut bytes);
    std::fs::write(path, bytes).unwrap();
    path.display().to_string()
}

#[test]
fn a_damaged_image_is_refused_by_unsquashfs_and_by_the_tool() {
    let built = build("damaged", "gzip", &[]);
    let inode_table = {
        let bytes = std::fs::read(&built.image).unwrap();
        u64::from_le_bytes(bytes[0x40..0x48].try_into().unwrap()) as usize
    };
    let cases = [
        (
            "a bad magic",
            damaged(&built.image, &built._dir.join("magic.sqfs"), |b| {
                b[..4].copy_from_slice(b"XXXX")
            }),
        ),
        (
            "a truncated image",
            damaged(&built.image, &built._dir.join("cut.sqfs"), |b| {
                b.truncate(b.len() / 2)
            }),
        ),
        (
            "a damaged metadata block",
            damaged(&built.image, &built._dir.join("meta.sqfs"), |b| {
                b[inode_table + 4..inode_table + 20].fill(0xFF)
            }),
        ),
    ];
    for (what, path) in &cases {
        let theirs = oracle("unsquashfs").args(["-lls", path]).output();
        assert!(
            !theirs.status.success(),
            "unsquashfs accepted {what}:\n{}",
            stdout(&theirs)
        );
        let ours = tool("fs.squashfs")
            .args([path, "ls", "/"])
            .output()
            .unwrap();
        assert_eq!(ours.status.code(), Some(1), "{what}: {}", stderr(&ours));
        assert!(ours.stdout.is_empty(), "{what}: {}", stdout(&ours));
        assert!(
            stderr(&ours).starts_with("{\"error\": "),
            "{what}: {}",
            stderr(&ours)
        );
    }
}
