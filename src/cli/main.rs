//! `rust-fs-squashfs`: the command-line tools for SquashFS, one multi-call
//! binary.
//!
//! Installed as `rust-fs-squashfs` and linked as each dotted name; see
//! `common` for the dispatch and the output contract every tool shares,
//! and `squashfs` for the tools themselves.

// The shared plumbing is a library in waiting (see its module docs): its
// API is whole, and a piece SquashFS does not call yet is not dead, it is
// the part another driver's tools will.
#[allow(dead_code)]
mod common;
mod squashfs;

use std::process::ExitCode;

static FAMILY: common::Family = common::Family {
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
    common::main(&FAMILY)
}
