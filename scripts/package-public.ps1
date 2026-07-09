# Build minter-desktop release and stage a desktop-only Public package + zip.
# Usage (from repo root):
#   powershell -ExecutionPolicy Bypass -File scripts/package-public.ps1

$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot
Set-Location $Root

Write-Host "==> cargo build -p minter-desktop --release"
cargo build -p minter-desktop --release
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$ExeCandidates = @(
    (Join-Path $Root "target\release\minter-desktop.exe"),
    (Join-Path $Root "crates\minter-desktop\src-tauri\target\release\minter-desktop.exe")
)
$Exe = $ExeCandidates | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $Exe) {
    Write-Error "minter-desktop.exe not found after build"
    exit 1
}

$Out = Join-Path $Root "Public"
New-Item -ItemType Directory -Force -Path $Out | Out-Null

Copy-Item -Force $Exe (Join-Path $Out "minter-desktop.exe")

$LegacyCli = Join-Path $Out "minter.exe"
if (Test-Path $LegacyCli) {
    Remove-Item -Force $LegacyCli
    Write-Host "Removed legacy minter.exe from Public/"
}

# Expect example docs in Public/ (ASCII + any locale-named MD)
$expected = @("config.example.json", "proxies.example.txt", "README.txt")
foreach ($f in $expected) {
    $src = Join-Path $Out $f
    if (-not (Test-Path $src)) {
        Write-Warning "Missing $f in Public/ - add manually"
    }
}
Get-ChildItem -Path $Out -Filter "*.md" -ErrorAction SilentlyContinue | ForEach-Object {
    Write-Host "  doc: $($_.Name)"
}

# Zip Public package
$Ver = "0.1.0"
$CargoToml = Join-Path $Root "crates\minter-desktop\src-tauri\Cargo.toml"
if (Test-Path $CargoToml) {
    $m = Select-String -Path $CargoToml -Pattern '^\s*version\s*=\s*"([^"]+)"' | Select-Object -First 1
    if ($m) { $Ver = $m.Matches.Groups[1].Value }
}
$ZipName = "minter-desktop-$Ver-windows.zip"
$ZipPath = Join-Path $Root $ZipName
if (Test-Path $ZipPath) { Remove-Item -Force $ZipPath }
Compress-Archive -Path (Join-Path $Out "*") -DestinationPath $ZipPath -Force

Write-Host ""
Write-Host "OK: Public/ staged with minter-desktop.exe"
Write-Host "  exe: $Exe"
Write-Host "  out: $Out"
Write-Host "  zip: $ZipPath"
Write-Host ""
Write-Host "NSIS installer (optional, requires tauri-cli):"
Write-Host "  cargo tauri build --bundles nsis"
Write-Host "  (cwd: crates/minter-desktop)"
