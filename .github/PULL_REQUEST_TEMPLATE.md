<!--
  PR template for rust-fs-squashfs. Keep PRs scoped: one logical change per
  PR unless the changes are genuinely interlocked. Delete sections that don't
  apply to your change rather than leaving them empty.
-->

## Summary

<!-- One-paragraph description of what this PR changes. Lead with the WHY, not the WHAT — readers can see the diff. -->

## Motivation

<!-- What problem prompted this change? Bug report, audit finding, perf measurement, missing capability for a downstream consumer? Link issues if relevant. -->

## Change shape

<!-- Bullet list of the discrete edits. Helps a reviewer (human or AI) follow the diff. Group by file or by concern, whichever reads cleaner. -->

- 

## Behaviour change

<!-- What did the crate do before? What does it do now? Especially important if any public API, on-disk format support, or C ABI surface moved. -->

- Before:
- After:

## Testing

<!-- New tests added? Existing tests that exercise the change? Manual reproduction steps if no automated coverage exists. `cargo test --release` output snippet is fine. Oracle tests (`-- --ignored`) need `squashfs-tools` on PATH. -->

- [ ] `cargo test` passes locally (no external tools needed)
- [ ] `cargo test --release -- --ignored` passes (with `squashfs-tools` installed)
- [ ] `cargo clippy --all-targets -- -D warnings` clean
- [ ] New tests cover the new code path (or "N/A" with rationale)

## ABI / on-disk compatibility

<!-- Tick the boxes that apply, or strike through and explain. -->

- [ ] No change to public Rust API
- [ ] No change to C ABI (`include/fs_squashfs.h` shape, struct layouts, function signatures)
- [ ] No change to which on-disk SquashFS variants/compressors are accepted
- [ ] If any of the above DID change, the change is binary-compatible (new fields appended, sentinel values reserved, etc.) — explained below.

## Risk

<!-- This is a READ-ONLY driver: the worst plausible failure mode is returning wrong bytes / a wrong listing for a crafted or unusual image. Be honest about which path a reviewer must not miss. -->

## Checklist before merge

- [ ] Commit messages describe the WHY, not just the WHAT
- [ ] No unrelated changes mixed in (formatting drift, unrelated TODOs)
- [ ] Public docs / module headers updated if behaviour changed
- [ ] CHANGELOG / README status table updated if user-visible
