#!/usr/bin/env bash
# Bump the version (single source of truth: Cargo.toml). Usage: scripts/bump.sh patch|minor|major
set -euo pipefail
cd "$(dirname "$0")/.."
part="${1:-patch}"
cur=$(sed -n 's/^version = "\([0-9.]*\)"/\1/p' Cargo.toml | head -1)
IFS=. read -r ma mi pa <<<"$cur"
case "$part" in
    patch) pa=$((pa + 1)) ;;
    minor) mi=$((mi + 1)); pa=0 ;;
    major) ma=$((ma + 1)); mi=0; pa=0 ;;
    *) echo "usage: $0 patch|minor|major" >&2; exit 1 ;;
esac
new="$ma.$mi.$pa"
sed -i "0,/^version = \"$cur\"/s//version = \"$new\"/" Cargo.toml
# Keep Cargo.lock in step without touching dependencies.
cargo update -p encoder --offline >/dev/null 2>&1 || cargo metadata --format-version 1 >/dev/null
echo "$cur → $new"
