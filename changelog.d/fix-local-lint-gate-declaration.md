### Fixed
- Plain `chore lint` passes outside GitHub Actions: the pinned rust-fs-core ci-gate reads the workflow's own declaration of its profile-conditional jobs, and a shell guard holds the local run to it.
