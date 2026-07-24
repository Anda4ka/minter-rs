# Build minter-desktop release and copy a safe ship folder into Public/.
# Usage (from repo root):
#   powershell -ExecutionPolicy Bypass -File scripts/package-public.ps1
#   powershell -ExecutionPolicy Bypass -File scripts/package-public.ps1 -MakeZip
#   powershell -ExecutionPolicy Bypass -File scripts/package-public.ps1 -SkipBuild -MakeZip
#
# -MakeZip packs ONLY the allowlist below (or Public/SHIP_MANIFEST.txt if present).
# Live secrets in Public/ (keys.vault, config.json, …) are never included.
# Public/ is gitignored — local packaging only. For GitHub Releases use .github/workflows/release.yml.

param(
    [switch]$MakeZip,
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot
Set-Location $Root

$Out = Join-Path $Root "Public"
New-Item -ItemType Directory -Force -Path $Out | Out-Null

if (-not $SkipBuild) {
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

    $Dest = Join-Path $Out "minter-desktop.exe"
    Copy-Item -Force $Exe $Dest

    $LegacyCli = Join-Path $Out "minter.exe"
    if (Test-Path $LegacyCli) {
        Remove-Item -Force $LegacyCli
        Write-Host "Removed legacy minter.exe from Public/"
    }

    Write-Host ""
    Write-Host "OK: release build complete"
    Write-Host "  built: $Exe"
    Write-Host "  copy:  $Dest"
    Write-Host ""
} else {
    Write-Host "==> SkipBuild: using existing Public\minter-desktop.exe"
    if (-not (Test-Path (Join-Path $Out "minter-desktop.exe"))) {
        Write-Error "Public\minter-desktop.exe missing; run without -SkipBuild first"
        exit 1
    }
}

# Copy docs next to the exe (safe to redistribute)
$DocCopies = @(
    @{ Src = "USER_GUIDE.md"; Dst = "USER_GUIDE.md" },
    @{ Src = "docs\OPERATOR_GUIDE.md"; Dst = "OPERATOR_GUIDE.md" },
    @{ Src = "LICENSE-MIT"; Dst = "LICENSE-MIT" },
    @{ Src = "LICENSE-APACHE"; Dst = "LICENSE-APACHE" },
    @{ Src = "SECURITY.md"; Dst = "SECURITY.md" },
    @{ Src = "README.md"; Dst = "README.md" }
)
foreach ($d in $DocCopies) {
    $src = Join-Path $Root $d.Src
    if (Test-Path -LiteralPath $src) {
        Copy-Item -Force -LiteralPath $src -Destination (Join-Path $Out $d.Dst)
        Write-Host "  doc: $($d.Dst)"
    }
}

# Default allowlist for zip (must match files we actually ship)
$Allowlist = @(
    "minter-desktop.exe",
    "USER_GUIDE.md",
    "OPERATOR_GUIDE.md",
    "LICENSE-MIT",
    "LICENSE-APACHE",
    "SECURITY.md",
    "README.md",
    "SHIP_MANIFEST.txt"
)

$SecretNames = @(
    "keys.vault",
    "config.json",
    "auth_cache.bin",
    "tasks.json",
    "wallet_meta.json",
    "runs_history.json",
    "proxies.txt",
    ".env"
)
$SecretDirs = @("results", "logs")

# Write default manifest if missing
$manifestPath = Join-Path $Out "SHIP_MANIFEST.txt"
if (-not (Test-Path -LiteralPath $manifestPath)) {
    @(
        "# Files included in a safe share zip (one basename per line).",
        "# Secrets listed in package-public.ps1 are never packed even if added here.",
        "minter-desktop.exe",
        "USER_GUIDE.md",
        "OPERATOR_GUIDE.md",
        "LICENSE-MIT",
        "LICENSE-APACHE",
        "SECURITY.md",
        "README.md"
    ) | Set-Content -LiteralPath $manifestPath -Encoding UTF8
}

function Get-ShipAllowlist {
    param([string]$PublicDir)
    $manifest = Join-Path $PublicDir "SHIP_MANIFEST.txt"
    if (Test-Path $manifest) {
        $lines = Get-Content -LiteralPath $manifest -Encoding UTF8 |
            ForEach-Object { $_.Trim() } |
            Where-Object { $_ -and -not $_.StartsWith("#") }
        if ($lines.Count -gt 0) { return @($lines) }
    }
    return $Allowlist
}

if ($MakeZip) {
    Write-Host "==> Making safe share zip (allowlist only)"
    $shipList = Get-ShipAllowlist -PublicDir $Out
    Write-Host "Allowlist:"
    foreach ($n in $shipList) { Write-Host "  + $n" }

    Write-Host "Excluded secrets (present but never packed):"
    $excludedAny = $false
    foreach ($n in $SecretNames) {
        $p = Join-Path $Out $n
        if (Test-Path -LiteralPath $p) {
            Write-Host "  - $n"
            $excludedAny = $true
        }
    }
    foreach ($d in $SecretDirs) {
        $p = Join-Path $Out $d
        if (Test-Path -LiteralPath $p) {
            Write-Host "  - $d\"
            $excludedAny = $true
        }
    }
    if (-not $excludedAny) {
        Write-Host "  (none present)"
    }

    $staging = Join-Path $env:TEMP ("minter-ship-" + [guid]::NewGuid().ToString("n"))
    New-Item -ItemType Directory -Force -Path $staging | Out-Null
    try {
        $packed = @()
        foreach ($name in $shipList) {
            $src = Join-Path $Out $name
            if (-not (Test-Path -LiteralPath $src)) {
                Write-Host "WARN: allowlist file missing, skip: $name"
                continue
            }
            $base = [System.IO.Path]::GetFileName($name)
            if ($SecretNames -contains $base) {
                Write-Host "REFUSE secret on allowlist: $base"
                continue
            }
            Copy-Item -LiteralPath $src -Destination (Join-Path $staging $base) -Force
            $packed += $base
        }
        if ($packed.Count -eq 0) {
            Write-Error "No allowlist files to pack"
            exit 1
        }

        $zipName = "minter-desktop-0.1.0-windows.zip"
        $zipPath = Join-Path $Out $zipName
        if (Test-Path -LiteralPath $zipPath) { Remove-Item -Force -LiteralPath $zipPath }

        Add-Type -AssemblyName System.IO.Compression
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        $zaWrite = [System.IO.Compression.ZipFile]::Open(
            $zipPath,
            [System.IO.Compression.ZipArchiveMode]::Create
        )
        try {
            foreach ($m in $packed) {
                $srcFile = Join-Path $staging $m
                [System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
                    $zaWrite,
                    $srcFile,
                    $m,
                    [System.IO.Compression.CompressionLevel]::Optimal
                ) | Out-Null
            }
        } finally {
            $zaWrite.Dispose()
        }
        Write-Host ""
        Write-Host "OK: safe zip"
        Write-Host "  path: $zipPath"
        Write-Host "  members:"
        foreach ($m in $packed) { Write-Host "    $m" }

        $za = [System.IO.Compression.ZipFile]::OpenRead($zipPath)
        try {
            foreach ($entry in $za.Entries) {
                $en = $entry.FullName.Replace("\", "/").TrimEnd("/")
                $leaf = [System.IO.Path]::GetFileName($en)
                if ($SecretNames -contains $leaf) {
                    Write-Error "ZIP CONTAINS SECRET: $leaf"
                    exit 1
                }
                if ($en -match '^(results|logs)(/|$)') {
                    Write-Error "ZIP CONTAINS SECRET DIR ENTRY: $en"
                    exit 1
                }
                if ($leaf -and ($packed -notcontains $leaf)) {
                    Write-Error "ZIP unexpected member: $en"
                    exit 1
                }
            }
        } finally {
            $za.Dispose()
        }
        Write-Host "  verify: no secrets in zip"
    } finally {
        Remove-Item -Recurse -Force -LiteralPath $staging -ErrorAction SilentlyContinue
    }
}

Write-Host ""
Write-Host "Done. Public\ is local-only (gitignored). For GitHub Releases push a v* tag."
