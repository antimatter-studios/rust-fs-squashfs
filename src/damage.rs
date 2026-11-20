//! Damage, as the primitives report it.
//!
//! A primitive returns what the bytes say and reports what is wrong with
//! them; it does not refuse a structure for being damaged. Each report is
//! a [`Damage`]: what kind of damage, how bad, the source bytes it is in,
//! and the structure those bytes were meant to be.
//!
//! The vocabulary is the one the family's salvage convention uses
//! (rust-fs-exfat `docs/convention.md`, section 4), so that moving to the
//! shared types in rust-fs-core is a renaming and not a reinterpretation.

use std::fmt;

/// What kind of damage was seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DamageKind {
    /// A signature is wrong: the bytes are not the structure they should
    /// be, such as a superblock without the SquashFS magic.
    Signature,
    /// The structure's integrity check failed. SquashFS has no checksums;
    /// its check is the compressed stream itself, which must decode to the
    /// length the geometry fixes.
    Checksum,
    /// Two things that must agree do not, such as a table that starts
    /// after the one meant to follow it.
    Mismatch,
    /// Something the structure needs is absent.
    Missing,
    /// A field holds a value the format does not allow.
    Malformed,
    /// A chain of structures stops before its end.
    Broken,
    /// The source bytes could not be read.
    Unreadable,
}

impl fmt::Display for DamageKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DamageKind::Signature => "signature",
            DamageKind::Checksum => "checksum",
            DamageKind::Mismatch => "mismatch",
            DamageKind::Missing => "missing",
            DamageKind::Malformed => "malformed",
            DamageKind::Broken => "broken",
            DamageKind::Unreadable => "unreadable",
        })
    }
}

/// How much the damage costs a reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Severity {
    /// Worth knowing, but nothing is lost.
    Notice,
    /// Something is lost or uncertain, but the structure is still usable.
    Warning,
    /// The structure cannot be used as it stands.
    Error,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Severity::Notice => "notice",
            Severity::Warning => "warning",
            Severity::Error => "error",
        })
    }
}

/// One piece of observed damage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Damage {
    /// What kind of damage it is.
    pub kind: DamageKind,
    /// How much it costs.
    pub severity: Severity,
    /// Byte offset of the damaged bytes in the image.
    pub offset: u64,
    /// How many bytes are damaged, or are the structure the damage is in.
    pub length: u64,
    /// The structure the bytes were meant to be, such as `"superblock"`.
    pub structure: &'static str,
    /// What is wrong, in words: the field and the rule it breaks.
    pub detail: String,
}

impl fmt::Display for Damage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} in the {} at byte {} (+{}): {}",
            self.severity, self.kind, self.structure, self.offset, self.length, self.detail
        )
    }
}
