mod cli_support;
mod common;

use cli_support::*;
use common::basic_fixture_path;
use std::process::Output;

fn listing_paths(output: &Output) -> Vec<String> {
    stdout(output)
        .split("\n  {")
        .skip(1)
        .map(|row| json_field(row, "path"))
        .collect()
}

#[test]
fn recursive_listing_reached_bounds_keep_the_prefix_and_exact_fits_succeed() {
    let image = basic_fixture_path();
    let complete = ok(tool("fs.squashfs").arg(&image).args(["ls", "--recursive"]));
    let expected = [
        "/empty.txt",
        "/hello.txt",
        "/link",
        "/sub",
        "/sub/deep",
        "/sub/deep/big.bin",
        "/sub/note.md",
    ];
    assert_eq!(listing_paths(&complete), expected);
    for arguments in [
        vec!["ls", "-R"],
        vec!["ls", "-R", "--max-depth", "3", "--limit", "7"],
        vec!["ls", "-R", "--max-depth", "64", "--limit", "100000"],
    ] {
        let output = ok(tool("fs.squashfs").arg(&image).args(arguments));
        assert_eq!(output.stdout, complete.stdout);
        assert!(output.stderr.is_empty(), "{}", stderr(&output));
    }

    for (bound, value, count, stopped) in [
        ("--max-depth", "0", 0, "/empty.txt"),
        ("--max-depth", "1", 4, "/sub/deep"),
        ("--max-depth", "2", 5, "/sub/deep/big.bin"),
        ("--limit", "0", 0, "/empty.txt"),
        ("--limit", "1", 1, "/hello.txt"),
        ("--limit", "4", 4, "/sub/deep"),
    ] {
        for recursive in ["--recursive", "-R"] {
            let output = tool("fs.squashfs")
                .arg(&image)
                .args(["ls", recursive, bound, value])
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(1), "{bound} {value}");
            assert_eq!(listing_paths(&output), expected[..count]);
            assert_eq!(
                stderr(&output),
                format!("{{\"error\": \"{bound} {value} reached at {stopped}\", \"code\": 1}}\n")
            );
        }
    }

    let text = tool("fs.squashfs")
        .arg(&image)
        .args(["ls", "-R", "--text", "--limit", "1"])
        .output()
        .unwrap();
    assert_eq!(text.status.code(), Some(1));
    assert_eq!(stdout(&text).lines().count(), 1);
    assert_eq!(
        stderr(&text),
        "fs.squashfs: --limit 1 reached at /hello.txt\n"
    );
}

#[test]
fn recursive_listing_of_a_file_or_symlink_starts_at_depth_zero() {
    let image = basic_fixture_path();
    for path in ["/hello.txt", "/link"] {
        let zero = tool("fs.squashfs")
            .arg(&image)
            .args(["ls", "-R", "--limit", "0", path])
            .output()
            .unwrap();
        assert_eq!(zero.status.code(), Some(1));
        assert_eq!(stdout(&zero).trim(), "[]");
        assert_eq!(
            stderr(&zero),
            format!("{{\"error\": \"--limit 0 reached at {path}\", \"code\": 1}}\n")
        );

        let output = ok(tool("fs.squashfs").arg(&image).args([
            "ls",
            "-R",
            "--max-depth",
            "0",
            "--limit",
            "1",
            path,
        ]));
        assert_eq!(listing_paths(&output), [path]);
        assert_eq!(json_field(&stdout(&output), "depth"), "0");
        assert!(output.stderr.is_empty(), "{}", stderr(&output));
    }
}
