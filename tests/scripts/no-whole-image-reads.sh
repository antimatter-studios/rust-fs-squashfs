#!/usr/bin/env bash
set -euo pipefail

# Image comparisons must use common::snapshot_image/assert_images_equal so
# test memory stays bounded regardless of image size. Keep the search narrow
# to path variables conventionally holding complete images; ordinary small
# exported files and fuzz inputs are intentionally read as bytes.
if rg -n 'fs::read\s*\(\s*&?(image|img|disk)\b' \
  tests/cli_export_kernel.rs tests/cli_export_tree_kernel.rs \
  tests/cli_ls_recursive_kernel.rs tests/cli_xattr_kernel.rs; then
  echo 'whole-image fs::read found; use common::snapshot_image and assert_images_equal' >&2
  exit 1
fi
