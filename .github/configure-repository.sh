#!/bin/sh
# Run once with an authenticated maintainer's gh CLI after main exists.
set -eu
sharebox_repo='ossmalaysia/sharebox'
sharebox_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
command -v gh >/dev/null 2>&1 || { echo 'Install GitHub CLI first.' >&2; exit 1; }
gh auth status
gh api --method PATCH "repos/$sharebox_repo" \
  -F allow_squash_merge=true \
  -F allow_merge_commit=false \
  -F allow_rebase_merge=false \
  -F delete_branch_on_merge=true \
  --silent
gh api --method PUT "repos/$sharebox_repo/branches/main/protection" \
  --input "$sharebox_root/.github/branch-protection.json" --silent
gh api --method PUT "repos/$sharebox_repo/private-vulnerability-reporting" --silent
echo 'Repository settings applied. Required check: CI passed; solo-maintainer mode with resolved discussions; no force push or deletion.'
echo 'Review branch protection in GitHub before accepting contributions.'
