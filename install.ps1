param(
    [string]$Version = "",
    [string]$To = "$env:USERPROFILE\.cargo\bin",
    [switch]$Force = $false,
    [switch]$Help = $false
)

$ErrorActionPreference = "Stop"

$REPO = "raythurman2386/raven"
$BINARY = "raven"

if ($Help) {
    Write-Host @"
Usage: .\install.ps1 [OPTIONS]

Install raven from a prebuilt GitHub Release binary.

Options:
  -Version VERSION  Install a specific version (default: latest)
  -To DIR           Install to DIR (default: `$env:USERPROFILE\.cargo\bin)
  -Force            Overwrite existing binary without prompting
  -Help             Show this help message

Examples:
  irm https://raw.githubusercontent.com/$REPO/master/install.ps1 | iex
  .\install.ps1 -Version 0.1.6
  .\install.ps1 -To C:\tools
"@
    exit 0
}

function Detect-Platform {
    $arch = $env:PROCESSOR_ARCHITECTURE
    switch ($arch) {
        "AMD64" { return "x86_64-pc-windows-msvc" }
        "ARM64" { return "aarch64-pc-windows-msvc" }
        default {
            throw "Unsupported architecture: $arch"
        }
    }
}

function Get-LatestVersion {
    $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$REPO/releases/latest" -ErrorAction Stop
    return $release.tag_name
}

function Add-ToPath {
    param([string]$Dir)
    $currentPath = [Environment]::GetEnvironmentVariable("PATH", "User")
    if ($currentPath -notlike "*$Dir*") {
        [Environment]::SetEnvironmentVariable("PATH", "$currentPath;$Dir", "User")
        $env:PATH = "$env:PATH;$Dir"
        Write-Host "==> Added $Dir to user PATH"
    }
}

# Wrap the whole install in a try/catch so that when the script is piped into
# `iex` (the documented one-liner), a failure prints the error and pauses
# instead of calling `exit`, which would terminate the host PowerShell session
# and close the window before the user sees what went wrong.
try {
    $triple = Detect-Platform

    if (-not $Version) {
        $versionTag = Get-LatestVersion
    } else {
        $versionTag = $Version
        if ($versionTag -notlike "v*") {
            $versionTag = "v$versionTag"
        }
    }

    $versionNoV = $versionTag -replace "^v", ""
    $artifact = "${BINARY}-${versionNoV}-${triple}.exe"

    $downloadUrl = "https://github.com/$REPO/releases/download/${versionTag}/${artifact}"
    $checksumUrl = "https://github.com/$REPO/releases/download/${versionTag}/checksums.txt"

    Write-Host "==> Platform:  $triple"
    Write-Host "==> Version:   $versionTag"
    Write-Host "==> Artifact:  $artifact"
    Write-Host "==> Install:   $To"

    $destPath = Join-Path $To "$BINARY.exe"

    $oldPath = Join-Path $To $BINARY
    if (Test-Path $oldPath) {
        Write-Host "==> Removing old extensionless $BINARY (left by a previous install)"
        Remove-Item -Force $oldPath
    }

    if ((Test-Path $destPath) -and (-not $Force)) {
        Write-Host "==> $BINARY.exe already exists at $destPath"
        Write-Host "    Use -Force to overwrite."
        exit 0
    }

    $tmpDir = Join-Path $env:TEMP "raven-install-$(Get-Random)"
    New-Item -ItemType Directory -Path $tmpDir -Force | Out-Null

    try {
        Write-Host "==> Downloading $downloadUrl ..."
        $artifactPath = Join-Path $tmpDir $artifact
        try {
            Invoke-WebRequest -Uri $downloadUrl -OutFile $artifactPath -ErrorAction Stop
        } catch {
            throw "Failed to download $downloadUrl : $_`nCheck that the release exists and the artifact name is correct."
        }

        # Fail closed on integrity: the checksum file and matching entry are
        # required. If they're missing, refuse to install rather than silently
        # shipping an unverified binary.
        try {
            $checksumPath = Join-Path $tmpDir "checksums.txt"
            Invoke-WebRequest -Uri $checksumUrl -OutFile $checksumPath -ErrorAction Stop
        } catch {
            throw "Failed to download checksums.txt from $checksumUrl`nRefusing to install without a checksum. Verify the release is complete."
        }

        Write-Host "==> Verifying checksum ..."
        $checksums = Get-Content $checksumPath
        $expected = $null
        foreach ($line in $checksums) {
            if ($line -match "^\s*([a-f0-9]+)\s+$([regex]::Escape($artifact))$") {
                $expected = $Matches[1]
                break
            }
        }
        if (-not $expected) {
            throw "No checksum entry found for $artifact in checksums.txt`nRefusing to install an unverified binary."
        }

        $actual = (Get-FileHash -Path $artifactPath -Algorithm SHA256).Hash.ToLower()
        if ($actual -ne $expected) {
            throw "Checksum mismatch!`n  expected: $expected`n  actual:   $actual"
        }
        Write-Host "==> Checksum OK"

        if (-not (Test-Path $To)) {
            New-Item -ItemType Directory -Path $To -Force | Out-Null
        }

        Move-Item -Path $artifactPath -Destination $destPath -Force

        Write-Host "==> Installed $BINARY $versionTag to $destPath"

        try {
            $ver = & $destPath --version 2>$null
            Write-Host "==> Version: $ver"
        } catch {
        }

        Add-ToPath $To
    } finally {
        Remove-Item -Recurse -Force $tmpDir -ErrorAction SilentlyContinue
    }
} catch {
    Write-Host ""
    Write-Host "Installation failed:" -ForegroundColor Red
    Write-Host "  $($_.Exception.Message)" -ForegroundColor Red
    Write-Host ""
    # Only pause in an interactive session so the error stays visible before
    # the window closes; skip the prompt in non-interactive/CI contexts.
    if ([Environment]::UserInteractive) {
        Write-Host "Press Enter to close..."
        Read-Host
    }
    exit 1
}
