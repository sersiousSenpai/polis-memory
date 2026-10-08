# SPDX-License-Identifier: Apache-2.0
# Polis Memory - one-line install on Windows:
#
#   irm https://redline.dev/polis/install.ps1 | iex
#
# Downloads polis.exe from the GitHub release, verifies its SHA-256, installs
# it to %LOCALAPPDATA%\Programs\polis (added to your user PATH), then runs
# `polis setup`, which shows its plan and asks before changing anything.
#
# Environment: POLIS_VERSION (a tag; default latest), POLIS_BIN_DIR,
# POLIS_NO_SETUP=1 (install only), POLIS_SETUP_ARGS (e.g. "--yes").
# Undo: `polis uninstall`.
$ErrorActionPreference = 'Stop'

$repo = if ($env:POLIS_INSTALL_REPO) { $env:POLIS_INSTALL_REPO } else { 'sersiousSenpai/polis-memory' }
$version = if ($env:POLIS_VERSION) { $env:POLIS_VERSION } else { 'latest' }
$binDir = if ($env:POLIS_BIN_DIR) { $env:POLIS_BIN_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\polis' }

if (-not [Environment]::Is64BitOperatingSystem) { throw 'polis-install: Polis needs 64-bit Windows.' }
$target = 'x86_64-pc-windows-msvc'
$archive = "polis-memory-$target.zip"
$base = if ($env:POLIS_INSTALL_BASE) { $env:POLIS_INSTALL_BASE }
        elseif ($version -eq 'latest') { "https://github.com/$repo/releases/latest/download" }
        else { "https://github.com/$repo/releases/download/$version" }

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("polis-install-" + [Guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    Write-Host "polis-install: downloading $archive ($version)"
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    Invoke-WebRequest -UseBasicParsing -Uri "$base/$archive" -OutFile (Join-Path $tmp $archive)
    Invoke-WebRequest -UseBasicParsing -Uri "$base/$archive.sha256" -OutFile (Join-Path $tmp "$archive.sha256")

    $want = ((Get-Content (Join-Path $tmp "$archive.sha256") | Where-Object { $_.Trim() } | Select-Object -First 1) -split '\s+')[0].ToLower()
    $got = (Get-FileHash -Algorithm SHA256 (Join-Path $tmp $archive)).Hash.ToLower()
    if (-not $want -or $want -ne $got) { throw "polis-install: checksum mismatch for $archive (want $want, got $got) - nothing installed" }

    Expand-Archive -Path (Join-Path $tmp $archive) -DestinationPath (Join-Path $tmp 'x') -Force
    $src = Get-ChildItem -Path (Join-Path $tmp 'x') -Recurse -Filter 'polis.exe' | Select-Object -First 1
    if (-not $src) { throw "polis-install: $archive has no polis.exe" }
    New-Item -ItemType Directory -Force -Path $binDir | Out-Null
    $dest = Join-Path $binDir 'polis.exe'
    Copy-Item $src.FullName $dest -Force
    Write-Host "polis-install: installed $dest ($(& $dest --version))"

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not (($userPath -split ';') -contains $binDir)) {
        [Environment]::SetEnvironmentVariable('Path', (($userPath, $binDir) | Where-Object { $_ }) -join ';', 'User')
        $env:Path = "$env:Path;$binDir"
        Write-Host "polis-install: added $binDir to your user PATH (new terminals pick it up)"
    }
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}

if ($env:POLIS_NO_SETUP -eq '1') {
    Write-Host "polis-install: skipping setup (POLIS_NO_SETUP=1) - run 'polis setup' when ready"
    return
}
$setupArgs = @('setup', '--polis', $dest)
if ($env:POLIS_SETUP_ARGS) { $setupArgs += ($env:POLIS_SETUP_ARGS -split '\s+' | Where-Object { $_ }) }
& $dest @setupArgs
