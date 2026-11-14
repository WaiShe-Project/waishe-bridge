# Builds the release installer:
#   1. the Dashboard from the sibling `waishe` checkout (npm ci + build),
#   2. copies it into dashboard-dist (embedded into the executable),
#   3. cargo build --release,
#   4. the Inno Setup installer into target\installer.
# Requires Node.js, Rust (MSVC) and Inno Setup 7 (ISCC.exe on PATH or in its default per-user location).
$ErrorActionPreference = 'Stop'
$bridge = Split-Path -Parent $PSScriptRoot
$dashboard = Join-Path (Split-Path -Parent $bridge) 'waishe\dashboard'
if (-not (Test-Path (Join-Path $dashboard 'package.json'))) { throw "Dashboard not found at $dashboard (check out waishe next to waishe-bridge)" }

Push-Location $dashboard
try {
    npm ci --no-audit --no-fund
    if ($LASTEXITCODE) { throw 'npm ci failed' }
    npm run build
    if ($LASTEXITCODE) { throw 'Dashboard build failed' }
} finally { Pop-Location }

$dist = Join-Path $bridge 'dashboard-dist'
if (Test-Path $dist) { Remove-Item -Recurse -Force $dist }
Copy-Item -Recurse (Join-Path $dashboard 'dist') $dist

Push-Location $bridge
try {
    cargo build --release
    if ($LASTEXITCODE) { throw 'cargo build failed' }
    $version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
    $iscc = (Get-Command ISCC.exe -ErrorAction SilentlyContinue).Source
    if (-not $iscc) { $iscc = Join-Path $env:LOCALAPPDATA 'Programs\Inno Setup 7\ISCC.exe' }
    & $iscc /Q "/DAppVersion=$version" 'packaging\windows\waishe-bridge.iss'
    if ($LASTEXITCODE) { throw 'ISCC failed' }
    $out = Join-Path $bridge "target\installer\WaiShe-Setup-$version-x64.exe"
    $hash = (Get-FileHash -Algorithm SHA256 $out).Hash.ToLower()
    "$hash  $(Split-Path -Leaf $out)" | Set-Content -Encoding ascii "$out.sha256"
    Write-Host "Built $out ($([math]::Round((Get-Item $out).Length / 1MB, 2)) MiB)"
    Write-Host "SHA-256 $hash"
} finally { Pop-Location }
