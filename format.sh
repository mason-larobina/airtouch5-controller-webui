#!/usr/bin/env bash
#
# format.sh - Format the code base with prettier.
#
# Prettier handles web/config/documentation files (Markdown, JSON, YAML,
# CSS, JS). It does NOT format Rust (use `cargo fmt` for that) and it does
# NOT safely handle the Askama/Jinja HTML templates under templates/
# (prettier's HTML parser errors on the `{% %}` / `{{ }}` syntax, and the
# Jinja plugin corrupts whitespace inside attribute strings), so those are
# intentionally excluded.
#
# Usage:
#   ./format.sh          # write formatted files in place (default)
#   ./format.sh --check  # exit non-zero if any file would change (CI)

set -euo pipefail

# Allow `**` to recurse and unmatched globs to expand to nothing so we can
# detect which candidate types actually have matching files.
shopt -s globstar nullglob

# Resolve the repository root regardless of where the script is run from.
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$repo_root"

check_mode=0
for arg in "$@"; do
  case "$arg" in
    --check|-c) check_mode=1 ;;
    -h|--help)
      sed -n '2,15p' "$0"
      exit 0
      ;;
    *)
      echo "error: unknown argument '$arg'" >&2
      echo "usage: $0 [--check]" >&2
      exit 2
      ;;
  esac
done

# Run prettier via npx so this works without a local install. Pinned to a
# recent 3.x release for deterministic formatting.
prettier=(npx --yes prettier@3.8.1)

# Candidate file types prettier knows how to format safely in this repo.
# Only globs that actually match at least one file are passed to prettier,
# so types that are not present yet do not cause spurious "No files matching"
# errors.
candidate_globs=(
  "**/*.json"
  "**/*.yml"
  "**/*.yaml"
  "**/*.css"
  "**/*.js"
  "**/*.md"
)

globs=()
for g in "${candidate_globs[@]}"; do
  matches=( $g )
  if [ "${#matches[@]}" -gt 0 ]; then
    globs+=( "$g" )
  fi
  unset matches
done

# .prettierignore is honored automatically when running from the repo root
# and excludes /target, /static/vendor, and .git.

if [ "${#globs[@]}" -eq 0 ]; then
  echo "No files to format."
  exit 0
fi

if [ "$check_mode" -eq 1 ]; then
  echo "Checking formatting with prettier..."
  "${prettier[@]}" --check "${globs[@]}"
else
  echo "Formatting with prettier..."
  "${prettier[@]}" --write "${globs[@]}"
fi
