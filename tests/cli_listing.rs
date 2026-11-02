mod cli_support;

use cli_support::*;

#[test]
fn recursive_listing_help_uses_the_shared_arguments_and_defaults() {
    let output = ok(tool("fs.squashfs").args(["disk.img", "ls", "--help"]));
    let help = stdout(&output)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    for expected in [
        "Walk depth first, names sorted as bytes; never follow a symlink",
        "How deep --recursive may go (default: 64); a directory's entries are depth 1",
        "How many entries --recursive may list (default: 100000)",
        "-R, --recursive",
        "--max-depth <N>",
        "--limit <N>",
    ] {
        assert!(help.contains(expected), "missing {expected:?}:\n{help}");
    }
    assert!(output.stderr.is_empty(), "{}", stderr(&output));
}

#[test]
fn recursive_listing_bounds_require_recursion_and_unsigned_values() {
    for arguments in [
        vec!["--max-depth", "1"],
        vec!["--limit", "1"],
        vec!["-R", "--max-depth", "-1"],
        vec!["--recursive", "--limit", "nope"],
    ] {
        let output = tool("fs.squashfs")
            .args(["disk.img", "ls"])
            .args(&arguments)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{arguments:?}");
        assert!(output.stdout.is_empty(), "{}", stdout(&output));
        assert_eq!(json_field(&stderr(&output), "code"), "2");
    }
}
