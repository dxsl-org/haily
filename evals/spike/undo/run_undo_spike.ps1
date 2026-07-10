# Phase 0 Track B-undo spike (THROWAWAY). Proves the chosen compensator
# `git checkout -- . && git clean -fd` reverts a workspace bit-identically to the clean
# commit, INCLUDING untracked build artifacts (target/) and planted untracked files.
# Exit 0 = undo sound; non-zero = undo UNSOUND (blocks P1 exec).

$ErrorActionPreference = "Stop"
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("haily-undo-spike-" + [System.Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $work | Out-Null
Push-Location $work
try {
    git init -q
    git config user.email "spike@haily.local"
    git config user.name  "spike"

    # Minimal buildable crate (no deps → offline, fast).
    New-Item -ItemType Directory -Path (Join-Path $work "src") | Out-Null
    Set-Content -Path "Cargo.toml" -Value "[package]`nname = `"undo-spike`"`nversion = `"0.1.0`"`nedition = `"2021`"`n"
    Set-Content -Path "src/main.rs" -Value "fn main() { println!(`"clean`"); }`n"
    Set-Content -Path ".gitignore"  -Value "/target`n"
    git add -A
    git commit -qm "clean state"

    # Produce untracked build artifacts + mutate tracked file + plant untracked junk.
    cargo build -q 2>&1 | Out-Null
    if (-not (Test-Path "target")) { throw "cargo build did not produce target/ — spike setup invalid" }
    Add-Content -Path "src/main.rs" -Value "// MODEL EDIT that must be reverted"
    Set-Content -Path "scratch-junk.txt" -Value "planted untracked file"
    New-Item -ItemType Directory -Path "planted-dir" | Out-Null
    Set-Content -Path "planted-dir/nested.txt" -Value "nested untracked"

    # The compensator under test. `-x` also removes gitignored artifacts (target/), `-ff`
    # also removes nested git dirs (an occasional node_modules/**/.git) — plain `-fd` (the
    # originally-specified form) leaves gitignored build output behind (spike finding).
    git checkout -- .
    git clean -ffdxq

    # Assertions — use git's own view (autocrlf-robust), not raw byte compares.
    $errors = @()
    $status = (git status --porcelain)
    if ($status) { $errors += "working tree not clean after compensator:`n$status" }
    git diff --quiet HEAD; if ($LASTEXITCODE -ne 0) { $errors += "tracked files differ from HEAD after compensator" }
    if (Test-Path "target")            { $errors += "target/ (gitignored build artifacts) NOT removed" }
    if (Test-Path "scratch-junk.txt")  { $errors += "planted untracked file NOT removed" }
    if (Test-Path "planted-dir")       { $errors += "planted untracked dir NOT removed" }

    if ($errors.Count -gt 0) {
        Write-Host "UNDO SPIKE: FAIL"
        $errors | ForEach-Object { Write-Host " - $_" }
        exit 1
    }
    Write-Host "UNDO SPIKE: PASS (compensator: git checkout -- . && git clean -ffdx)"
    Write-Host " - working tree clean; tracked files bit-identical to HEAD (git diff empty)"
    Write-Host " - target/ (gitignored) removed; planted untracked file + dir removed"
    exit 0
}
finally {
    Pop-Location
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}
