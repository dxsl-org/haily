#!/usr/bin/env bash
# Phase 0 Track B-undo spike (THROWAWAY). Proves `git checkout -- . && git clean -fd` reverts
# a workspace bit-identically to the clean commit, INCLUDING untracked build artifacts and
# planted untracked files. Exit 0 = undo sound; non-zero = undo UNSOUND (blocks P1 exec).
set -euo pipefail

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
cd "$work"

git init -q
git config user.email "spike@haily.local"
git config user.name  "spike"

mkdir -p src
printf '[package]\nname = "undo-spike"\nversion = "0.1.0"\nedition = "2021"\n' > Cargo.toml
printf 'fn main() { println!("clean"); }\n' > src/main.rs
printf '/target\n' > .gitignore
git add -A
git commit -qm "clean state"

cargo build -q >/dev/null 2>&1
[ -d target ] || { echo "cargo build did not produce target/ — spike setup invalid"; exit 2; }
printf '// MODEL EDIT that must be reverted\n' >> src/main.rs
printf 'planted untracked file\n' > scratch-junk.txt
mkdir -p planted-dir && printf 'nested untracked\n' > planted-dir/nested.txt

# `-x` also removes gitignored artifacts (target/), `-ff` also removes nested git dirs
# (an occasional node_modules/**/.git) — plain `-fd` leaves gitignored output behind.
git checkout -- .
git clean -ffdxq

errs=()
[ -z "$(git status --porcelain)" ] || errs+=("working tree not clean after compensator")
git diff --quiet HEAD || errs+=("tracked files differ from HEAD after compensator")
[ ! -d target ]           || errs+=("target/ (gitignored) NOT removed")
[ ! -f scratch-junk.txt ] || errs+=("planted untracked file NOT removed")
[ ! -d planted-dir ]      || errs+=("planted untracked dir NOT removed")

if [ "${#errs[@]}" -gt 0 ]; then
  echo "UNDO SPIKE: FAIL"
  printf ' - %s\n' "${errs[@]}"
  exit 1
fi
echo "UNDO SPIKE: PASS (compensator: git checkout -- . && git clean -ffdx)"
echo " - working tree clean; tracked bit-identical to HEAD; target/ + planted untracked removed"
