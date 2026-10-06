//! `rust-fs-squashfs`: the command-line tools for SquashFS, one multi-call
//! binary.
//!
//! Installed as `rust-fs-squashfs` and linked as each dotted name. The
//! dispatch and the output contract every tool shares are `fs_core::cli`
//! (rust-fs-core's `cli` feature); `squashfs` is the tools themselves.

mod squashfs;

use fs_core::cli;
use std::process::ExitCode;

static FAMILY: cli::Family = cli::Family {
    repo: "rust-fs-squashfs",
    crate_name: env!("CARGO_PKG_NAME"),
    version: env!("CARGO_PKG_VERSION"),
    about: "SquashFS tools: work on a SquashFS image directly, without mounting it",
    install_hints: &[
        "`chore cli:install` from a checkout of this repository",
        "`brew install antimatter-studios/tap/rust-fs-squashfs`",
    ],
    tools: &[squashfs::fs::TOOL],
};

fn main() -> ExitCode {
    cli::main(&FAMILY)
}
