# Agent Show one-line installer for Windows (PowerShell 5+).
#
# Usage:
#   irm https://raw.githubusercontent.com/benjamin7007/Agent Show/master/install.ps1 | iex
#
# Optional environment variables:
#   $env:AGENT_SHOW_VERSION   pin a specific tag (e.g. v1.9.1). Default: latest.
#   $env:AGENT_SHOW_PREFIX    install dir. Default: $env:LOCALAPPDATA\agent-show.
#
# Requires: PowerShell 5+ (built-in on Windows 10/11).

& {
$ErrorActionPreference = 'Stop'

$Repo = "benjamin7007/Agent Show"
$Version = if ($env:AGENT_SHOW_VERSION) { $env:AGENT_SHOW_VERSION } else { "latest" }
$Target = "x86_64-pc-windows-msvc"
$Asset = "agent-show-${Target}.zip"

# Resolve install prefix
$Prefix = if ($env:AGENT_SHOW_PREFIX) { $env:AGENT_SHOW_PREFIX } else { Join-Path $env:LOCALAPPDATA "agent-show" }
if (-not (Test-Path $Prefix)) { New-Item -ItemType Directory -Path $Prefix -Force | Out-Null }

# Build download URL
if ($Version -eq "latest") {
    $Url = "https://github.com/${Repo}/releases/latest/download/${Asset}"
    $ShaUrl = "${Url}.sha256"
} else {
    $Url = "https://github.com/${Repo}/releases/download/${Version}/${Asset}"
    $ShaUrl = "${Url}.sha256"
}

Write-Host "==> Target:  $Target" -ForegroundColor Cyan
Write-Host "==> Version: $Version" -ForegroundColor Cyan
Write-Host "==> Prefix:  $Prefix" -ForegroundColor Cyan
Write-Host "==> Asset:   $Url" -ForegroundColor Cyan

# Download
$TmpDir = Join-Path $env:TEMP "agent-show-install-$(Get-Random)"
New-Item -ItemType Directory -Path $TmpDir -Force | Out-Null
$ZipPath = Join-Path $TmpDir $Asset
$ShaPath = Join-Path $TmpDir "${Asset}.sha256"

try {
    Write-Host "==> Downloading..." -ForegroundColor Cyan
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    Invoke-WebRequest -Uri $Url -OutFile $ZipPath -UseBasicParsing
    Invoke-WebRequest -Uri $ShaUrl -OutFile $ShaPath -UseBasicParsing
} catch {
    Write-Host "error: download failed - $_" -ForegroundColor Red
    exit 1
}

# Verify checksum
$Expected = (Get-Content $ShaPath -Raw).Trim().Split(' ')[0]
$Actual = (Get-FileHash -Path $ZipPath -Algorithm SHA256).Hash.ToLower()
if ($Expected -ne $Actual) {
    Write-Host "error: checksum mismatch (expected $Expected, got $Actual)" -ForegroundColor Red
    exit 1
}
Write-Host "==> Checksum OK" -ForegroundColor Cyan

# Extract
Expand-Archive -Path $ZipPath -DestinationPath $TmpDir -Force
$BinSrc = Join-Path (Join-Path $TmpDir "agent-show-${Target}") "agent-show.exe"
if (-not (Test-Path $BinSrc)) {
    Write-Host "error: agent-show.exe not found in archive" -ForegroundColor Red
    exit 1
}

# Install
$BinDest = Join-Path $Prefix "agent-show.exe"
Copy-Item -Path $BinSrc -Destination $BinDest -Force
Write-Host "==> Installed: $BinDest" -ForegroundColor Cyan

# Cleanup temp
Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue

# Add to PATH if needed
$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($UserPath -notlike "*$Prefix*") {
    [Environment]::SetEnvironmentVariable("Path", "$Prefix;$UserPath", "User")
    $env:Path = "$Prefix;$env:Path"
    Write-Host "==> Added $Prefix to user PATH (restart terminal to take effect)" -ForegroundColor Yellow
}

# Show version
$ver = (& $BinDest --version 2>&1) | Out-String
Write-Host "==> $($ver.Trim())" -ForegroundColor Cyan

# Auto-start: stop any existing agent-show, then start new version
$ServerUrl = "http://127.0.0.1:7777"

# Kill existing agent-show processes so the new binary takes effect immediately
$existing = Get-Process -Name "agent-show" -ErrorAction SilentlyContinue
if ($existing) {
    Write-Host "==> Stopping existing agent-show (PID: $($existing.Id -join ', '))..." -ForegroundColor Yellow
    $existing | Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Seconds 2
}

Write-Host "==> Starting agent-show server..." -ForegroundColor Cyan
Start-Process -FilePath $BinDest -ArgumentList @("serve", "--no-open") -WindowStyle Hidden
Start-Sleep -Seconds 3
try {
    $null = Invoke-WebRequest -Uri $ServerUrl -UseBasicParsing -TimeoutSec 2 -ErrorAction Stop
    Write-Host "==> Server is up: $ServerUrl" -ForegroundColor Cyan
    Start-Process -FilePath $ServerUrl
} catch {
    Write-Host "==> Could not auto-start. Run manually: agent-show serve" -ForegroundColor Yellow
}

Write-Host "==> Done!" -ForegroundColor Green
}
