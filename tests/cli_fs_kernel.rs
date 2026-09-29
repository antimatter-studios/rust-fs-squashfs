//! `fs.squashfs`, the tool, held to THE KERNEL: an image `mksquashfs` builds
//! in the harness guest, once per compressor the kernel supports, is
//! loop-mounted by Linux there, and every file the kernel reads hashes to
//! what `fs.squashfs read` returns; every symlink target and every type
//! the kernel reports is the one `fs.squashfs ls` lists.

mod cli_support;

use cli_support::*;
use fs_squashfs_test_support::{
    guest_kernel_report, mksquashfs_from_guest_tree, sha256_hex, ScratchDir,
};

const STAGE: &str = r#"
mkdir -p d/e/f
: > empty
printf x > one
head -c 4097 /dev/urandom > block_plus_one
head -c 300000 /dev/urandom > big
head -c 5000 /dev/urandom > fragment_tail
truncate -s 1048576 sparse
printf 'end' >> sparse
head -c 3000 /dev/urandom > d/e/f/deep
ln -s one link
ln -s d/e/f/deep deep_link
"#;

fn check(comp: &str) {
    let dir = ScratchDir::new(&format!("cli-kernel-{comp}"));
    let image = dir.join(&format!("{comp}.sqfs"));
    let out = mksquashfs_from_guest_tree(
        &image,
        STAGE,
        &["-comp", comp, "-b", "4096", "-noappend", "-no-progress"],
    );
    assert!(
        out.status.success(),
        "mksquashfs -comp {comp}: {}",
        stderr(&out)
    );
    let image = image.display().to_string();

    let kernel = guest_kernel_report(&image, comp);
    let mut files = 0;
    let mut links = 0;
    for ((kind, path), value) in &kernel {
        match kind.as_str() {
            "sha256" => {
                let read = ok(tool("fs.squashfs").args([&image, "read", &format!("/{path}")]));
                assert_eq!(&sha256_hex(&read.stdout), value, "-comp {comp} /{path}");
                files += 1;
            }
            "target" => {
                let listing = stdout(&ok(tool("fs.squashfs").args([
                    &image,
                    "ls",
                    &format!("/{path}"),
                ])));
                assert_eq!(
                    &json_field(&listing, "target"),
                    value,
                    "-comp {comp} /{path}"
                );
                links += 1;
            }
            "type" => {
                let listing = stdout(&ok(tool("fs.squashfs").args([
                    &image,
                    "ls",
                    &format!("/{path}"),
                ])));
                let ours = match json_field(&listing, "type").as_str() {
                    "file" if value == "regular-empty-file" => "regular-empty-file".to_string(),
                    "file" => "regular-file".to_string(),
                    "dir" => "directory".to_string(),
                    "symlink" => "symbolic-link".to_string(),
                    other => other.to_string(),
                };
                if value == "directory" {
                    // `ls` of a directory lists its children; its own type
                    // is in its parent's listing.
                    assert!(listing.starts_with('['), "-comp {comp} /{path}");
                } else {
                    assert_eq!(&ours, value, "-comp {comp} /{path}");
                }
            }
            _ => {}
        }
    }
    // Seven regular files and two symlinks: two empty maps agree about
    // everything, so the comparison must have compared something.
    assert_eq!((files, links), (7, 2), "-comp {comp}: {kernel:?}");
}

#[test]
fn gzip() {
    check("gzip");
}

#[test]
fn xz() {
    check("xz");
}

#[test]
fn lz4() {
    check("lz4");
}

#[test]
fn zstd() {
    check("zstd");
}

#[test]
fn lzo() {
    check("lzo");
}
