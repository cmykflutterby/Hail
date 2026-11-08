#Requires -Version 7.4
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$root = Split-Path $PSScriptRoot -Parent
$project = Get-Content (Join-Path $PSScriptRoot 'project.json') -Raw | ConvertFrom-Json
$local = Join-Path $root '_local'
$env:CARGO_HOME = Join-Path $local 'cargo-home'
$env:RUSTUP_HOME = Join-Path $local 'rustup'
$env:CARGO_TARGET_DIR = Join-Path $local 'target'
$env:ZIG_GLOBAL_CACHE_DIR = Join-Path $local 'zig-cache'
$env:ZIG_LOCAL_CACHE_DIR = Join-Path $local 'zig-local-cache'
$env:CARGO_ZIGBUILD_CACHE_DIR = Join-Path $local 'zigbuild-cache'
$env:TEMP = Join-Path $local 'tmp'
$env:TMP = $env:TEMP
$env:RUSTUP_TOOLCHAIN = "$($project.toolchain)-x86_64-pc-windows-gnu"

foreach ($directory in @(
        (Join-Path $local 'cargo-home'),
        (Join-Path $local 'rustup'),
        (Join-Path $local 'downloads'),
        (Join-Path $local 'tools'),
        (Join-Path $local 'tmp'),
        (Join-Path $root 'vendor\crates'))) {
    New-Item -ItemType Directory -Path $directory -Force | Out-Null
}

function Invoke-Native {
    param([string]$Program, [string[]]$Arguments)
    & $Program @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$Program failed with exit code $LASTEXITCODE." }
}

function Get-ToolDirectory {
    param([string]$Name)
    Join-Path $local "tools\$($project.tools.$Name.directory)"
}

function Get-ToolExecutable {
    param([string]$Name)
    Join-Path (Get-ToolDirectory $Name) $project.tools.$Name.executable
}

function Install-ProjectTool {
    param([string]$Name)
    $tool = $project.tools.$Name
    $executable = Get-ToolExecutable $Name
    if (Test-Path $executable) { return }

    $archive = Join-Path $local "downloads\$Name-$([IO.Path]::GetFileName($tool.url))"
    if (!(Test-Path $archive)) {
        Invoke-WebRequest -Uri $tool.url -OutFile $archive
    }
    $actualHash = (Get-FileHash $archive -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actualHash -ne $tool.sha256) {
        Remove-Item -LiteralPath $archive -Force
        throw "$Name archive checksum mismatch."
    }

    $destination = Join-Path $local 'tools'
    if ($tool.PSObject.Properties.Name -contains 'flatArchive' -and $tool.flatArchive) {
        $destination = Get-ToolDirectory $Name
        New-Item -ItemType Directory -Path $destination -Force | Out-Null
    }
    Invoke-Native (Join-Path $env:SystemRoot 'System32\tar.exe') @('-xf', $archive, '-C', $destination)
    if (!(Test-Path $executable)) { throw "$Name extraction did not provide its required executable." }
}

$rustupInit = Join-Path $local 'downloads\rustup-init.exe'
if (!(Test-Path (Join-Path $env:CARGO_HOME 'bin\rustup.exe'))) {
    if (!(Test-Path $rustupInit)) {
        Invoke-WebRequest -Uri 'https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-gnu/rustup-init.exe' -OutFile $rustupInit
    }
    $checksumText = [string](Invoke-WebRequest -Uri 'https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-gnu/rustup-init.exe.sha256').Content
    $expectedHash = [regex]::Match($checksumText, '[a-fA-F0-9]{64}').Value.ToLowerInvariant()
    $actualHash = (Get-FileHash $rustupInit -Algorithm SHA256).Hash.ToLowerInvariant()
    if (!$expectedHash -or $actualHash -ne $expectedHash) {
        Remove-Item -LiteralPath $rustupInit -Force
        throw "rustup-init checksum mismatch (expected $expectedHash, got $actualHash)."
    }
    $env:RUSTUP_INIT_SKIP_PATH_CHECK = 'yes'
    Invoke-Native $rustupInit @('-y', '--no-modify-path', '--default-toolchain', 'none', '--profile', 'minimal')
}

$rustup = Join-Path $env:CARGO_HOME 'bin\rustup.exe'
$cargo = Join-Path $env:CARGO_HOME 'bin\cargo.exe'
$toolchain = "$($project.toolchain)-x86_64-pc-windows-gnu"
Invoke-Native $rustup @('toolchain', 'install', $toolchain, '--profile', 'minimal', '--component', 'rustfmt,clippy', '--no-self-update')
Invoke-Native $rustup @('target', 'add', 'x86_64-pc-windows-gnu', 'aarch64-pc-windows-gnullvm', 'x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu', '--toolchain', $toolchain)

foreach ($name in @('gcc', 'llvm-mingw', 'zig', 'cargo-zigbuild')) {
    Install-ProjectTool $name
}

$env:PATH = "$(Join-Path $env:CARGO_HOME 'bin');$(Join-Path (Get-ToolDirectory 'gcc') 'bin');$(Join-Path (Get-ToolDirectory 'llvm-mingw') 'bin');$(Get-ToolDirectory 'cargo-zigbuild');$(Get-ToolDirectory 'zig');$env:PATH"

Push-Location $root
try {
    Remove-Item -LiteralPath (Join-Path $env:CARGO_HOME 'config.toml') -Force -ErrorAction SilentlyContinue
    $vendorConfiguration = & $cargo vendor '--versioned-dirs' (Join-Path $root 'vendor\crates')
    if ($LASTEXITCODE -ne 0) { throw 'Cargo dependency vendoring failed.' }
    $vendorConfiguration | Set-Content -Path (Join-Path $env:CARGO_HOME 'config.toml') -Encoding utf8NoBOM
}
finally {
    Pop-Location
}

Write-Host 'GitHub Actions build prerequisites initialized; snow was intentionally not fetched.'
