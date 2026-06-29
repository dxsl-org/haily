# build.ps1 — Build Haily portable exe and stage it at the project root.
#
# Usage:
#   .\build.ps1           # release build (default)
#   .\build.ps1 -Debug    # debug build (faster, no optimisations)
#
# After running:
#   d:\haily\
#     haily.exe        <- fresh build
#     data\            <- created if missing; haily.db appears on first run
#     models\          <- put GGUF files here; configure path in Settings

param(
    [switch]$Debug,
    # GPU backend for embedded llama.cpp. Requires the matching toolchain installed:
    #   cuda   → NVIDIA CUDA Toolkit (nvcc on PATH)
    #   vulkan → Vulkan SDK (VULKAN_SDK set)
    # Omit for a CPU-only build. GPU layers in Settings only take effect with a GPU build.
    [ValidateSet("", "cuda", "vulkan")]
    [string]$Gpu = ""
)

$ErrorActionPreference = "Stop"
$Root = $PSScriptRoot

$cfg = if ($Debug) { "debug" } else { "release" }
$gpuLabel = if ($Gpu) { $Gpu } else { "CPU-only" }

Write-Host ""
Write-Host "  Haily Portable Build" -ForegroundColor Cyan
Write-Host "  mode   : $cfg" -ForegroundColor DarkGray
Write-Host "  gpu    : $gpuLabel" -ForegroundColor DarkGray
Write-Host "  output : $Root\haily.exe" -ForegroundColor DarkGray
Write-Host ""

# ── 0. Verify GPU toolchain when requested ────────────────────────────────────
if ($Gpu -eq "cuda" -and -not (Get-Command nvcc -ErrorAction SilentlyContinue)) {
    throw "CUDA build requested but 'nvcc' is not on PATH. Install the NVIDIA CUDA Toolkit first."
}
if ($Gpu -eq "vulkan" -and -not $env:VULKAN_SDK) {
    throw "Vulkan build requested but VULKAN_SDK is not set. Install the Vulkan SDK first."
}

# ── 1. Tauri build (no installer) ─────────────────────────────────────────────
# --no-bundle  → skip NSIS/MSI; produce only the raw exe.
# tauri CLI also runs beforeBuildCommand (npm run build) and sets TAURI_ENV_*
# vars that embed the frontend correctly — plain `cargo build` doesn't do this.
$tauriArgs = @("tauri", "build", "--no-bundle")
if ($Debug) { $tauriArgs += "--debug" }
if ($Gpu)   { $tauriArgs += @("--features", $Gpu) }

Write-Host "[1/2] cargo $($tauriArgs -join ' ')..." -ForegroundColor Yellow
Push-Location "$Root\src-tauri"
try {
    & cargo @tauriArgs
    if ($LASTEXITCODE -ne 0) { throw "cargo tauri build failed (exit $LASTEXITCODE)" }
} finally {
    Pop-Location
}

# ── 2. Locate the built exe ───────────────────────────────────────────────────
# src-tauri is a workspace member → shared target at $Root\target\.
# Fallback: src-tauri\target\ (standalone build outside workspace).
$candidates = @(
    "$Root\target\$cfg\haily.exe",
    "$Root\src-tauri\target\$cfg\haily.exe"
)
$exeSrc = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $exeSrc) {
    throw "Cannot find haily.exe in:`n  $($candidates -join "`n  ")"
}

# ── 3. Stage portable layout ──────────────────────────────────────────────────
Write-Host "[2/2] Staging portable layout..." -ForegroundColor Yellow

Copy-Item $exeSrc "$Root\haily.exe" -Force
New-Item -ItemType Directory -Force -Path "$Root\data"   | Out-Null
New-Item -ItemType Directory -Force -Path "$Root\models" | Out-Null

# ── Done ──────────────────────────────────────────────────────────────────────
$exeMB = [math]::Round((Get-Item "$Root\haily.exe").Length / 1MB, 1)

Write-Host ""
Write-Host "  Build complete!" -ForegroundColor Green
Write-Host ""
Write-Host "  Portable layout:" -ForegroundColor Cyan
Write-Host "    haily.exe   ($exeMB MB)" -ForegroundColor White

$dataFiles = (Get-ChildItem "$Root\data" -ErrorAction SilentlyContinue).Count
Write-Host "    data\       ($dataFiles files — haily.db created on first run)" -ForegroundColor DarkGray

$modelFiles = (Get-ChildItem "$Root\models" -Filter "*.gguf" -ErrorAction SilentlyContinue).Count
Write-Host "    models\     ($modelFiles GGUF files)" -ForegroundColor DarkGray

Write-Host ""
Write-Host "  Run: .\haily.exe" -ForegroundColor White
Write-Host ""
