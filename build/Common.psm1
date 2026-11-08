#Requires -Version 7.4
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$script:Root = Split-Path $PSScriptRoot -Parent
$script:Config = Get-Content (Join-Path $PSScriptRoot 'project.json') -Raw | ConvertFrom-Json

function Invoke-Native {
    param([string]$Program, [string[]]$Arguments)
    & $Program @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$Program failed with exit code $LASTEXITCODE." }
}

function Set-GeneratedText {
    param([string]$Path, [string]$Text)
    if ((Test-Path $Path) -and (Get-Content $Path -Raw).TrimEnd() -eq $Text.TrimEnd()) { return }
    Set-Content -Path $Path -Value $Text -Encoding ascii -NoNewline
}

function Initialize-Environment {
    if (!$IsWindows -or [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -ne 'X64') {
        throw 'This build orchestrator requires x64 Windows and PowerShell 7.4+. It cross-compiles all four target families.'
    }
    foreach ($folder in @('build', 'tests', 'vendor', 'drops', '_local', '_local\logs', '_local\tools', '_local\downloads', '_local\linkers', '_local\tmp')) {
        New-Item -ItemType Directory -Path (Join-Path $script:Root $folder) -Force | Out-Null
    }
    $env:CARGO_HOME = Join-Path $script:Root '_local\cargo-home'
    $env:RUSTUP_HOME = Join-Path $script:Root '_local\rustup'
    $env:CARGO_TARGET_DIR = Join-Path $script:Root '_local\target'
    $env:ZIG_GLOBAL_CACHE_DIR = Join-Path $script:Root '_local\zig-cache'
    $env:ZIG_LOCAL_CACHE_DIR = Join-Path $script:Root '_local\zig-local-cache'
    $env:CARGO_ZIGBUILD_CACHE_DIR = Join-Path $script:Root '_local\zigbuild-cache'
    $env:CARGO_ZIGBUILD_ZIG_PATH = Get-ToolExecutable 'zig'
    $env:CARGO_ZIGBUILD_ZIG_COMMAND = Get-ToolExecutable 'zig'
    $env:TEMP = Join-Path $script:Root '_local\tmp'
    $env:TMP = $env:TEMP
    $env:RUSTUP_TOOLCHAIN = "$($script:Config.toolchain)-x86_64-pc-windows-gnu"
    $env:CARGO_NET_GIT_FETCH_WITH_CLI = 'true'
    $env:CARGO_INCREMENTAL = '0'
    $localBin = Join-Path $env:CARGO_HOME 'bin'
    $toolBin = Join-Path (Get-ToolDirectory 'gcc') 'bin'
    $llvmBin = Join-Path (Get-ToolDirectory 'llvm-mingw') 'bin'
    $zigbuildBin = Get-ToolDirectory 'cargo-zigbuild'
    $zigBin = Get-ToolDirectory 'zig'
    $env:PATH = "$localBin;$toolBin;$llvmBin;$zigbuildBin;$zigBin;$env:PATH"
    return $script:Root
}

function Get-ToolDirectory {
    param([string]$Name)
    return Join-Path $script:Root "_local\tools\$($script:Config.tools.$Name.directory)"
}

function Get-ToolExecutable {
    param([string]$Name)
    return Join-Path (Get-ToolDirectory $Name) $script:Config.tools.$Name.executable
}

function Get-Cargo { return Join-Path $env:CARGO_HOME 'bin\cargo.exe' }
function Get-Rustup { return Join-Path $env:CARGO_HOME 'bin\rustup.exe' }

function Install-ProjectTool {
    param([string]$Name)
    $tool = $script:Config.tools.$Name
    if (Test-Path (Get-ToolExecutable $Name)) { return }
    $archive = Join-Path $script:Root "_local\downloads\$Name-$([IO.Path]::GetFileName($tool.url))"
    if (!(Test-Path $archive)) {
        Write-Host "Downloading pinned $Name..."
        Invoke-WebRequest -Uri $tool.url -OutFile $archive
    }
    if ((Get-FileHash $archive -Algorithm SHA256).Hash.ToLowerInvariant() -ne $tool.sha256) {
        Remove-Item -LiteralPath $archive
        throw "$Name archive checksum mismatch; deleted the invalid download. Run -Init again."
    }
    $destination = Join-Path $script:Root '_local\tools'
    if ($tool.PSObject.Properties.Name -contains 'flatArchive' -and $tool.flatArchive) {
        $destination = Get-ToolDirectory $Name
        New-Item -ItemType Directory -Path $destination -Force | Out-Null
    }
    Invoke-Native (Join-Path $env:SystemRoot 'System32\tar.exe') @('-xf', $archive, '-C', $destination)
    if (!(Test-Path (Get-ToolExecutable $Name))) { throw "$Name extraction did not provide its required executable." }
}

function Initialize-Workspace {
    if (!(Test-Path (Join-Path $script:Root '.git'))) {
        Invoke-Native 'git' @('-C', $script:Root, 'init', '-b', 'main')
    }
    if (!(Test-Path (Get-Rustup))) {
        $installer = Join-Path $script:Root '_local\downloads\rustup-init.exe'
        Invoke-WebRequest -Uri 'https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-gnu/rustup-init.exe' -OutFile $installer
        $checksumResponse = (Invoke-WebRequest -Uri 'https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-gnu/rustup-init.exe.sha256').Content
        $checksumText = if ($checksumResponse -is [byte[]]) {
            [Text.Encoding]::UTF8.GetString($checksumResponse)
        } else { [string]$checksumResponse }
        $expected = $checksumText.Split(' ')[0].Trim()
        if ((Get-FileHash $installer -Algorithm SHA256).Hash.ToLowerInvariant() -ne $expected) {
            throw 'rustup-init checksum mismatch.'
        }
        $env:RUSTUP_INIT_SKIP_PATH_CHECK = 'yes'
        Invoke-Native $installer @('-y', '--no-modify-path', '--default-toolchain', 'none', '--profile', 'minimal')
    }
    Invoke-Native (Get-Rustup) @('toolchain', 'install', "$($script:Config.toolchain)-x86_64-pc-windows-gnu", '--profile', 'minimal', '--component', 'rustfmt,clippy', '--no-self-update')
    Invoke-Native (Get-Rustup) @('target', 'add', 'x86_64-pc-windows-gnu', 'aarch64-pc-windows-gnullvm', 'x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu')
    foreach ($name in @('gcc', 'llvm-mingw', 'zig', 'cargo-zigbuild')) { Install-ProjectTool $name }

    foreach ($source in $script:Config.externalSources) {
        $path = Join-Path $script:Root "vendor\$($source.name)"
        if (!(Test-Path $path)) {
            New-Item -ItemType Directory -Path $path | Out-Null
            Invoke-Native 'git' @('-C', $path, 'init')
            Invoke-Native 'git' @('-C', $path, 'remote', 'add', 'origin', $source.url)
        }
        $revision = & git -C $path rev-parse HEAD 2>$null
        if ($LASTEXITCODE -ne 0 -or $revision -ne $source.revision) {
            $dirty = & git -C $path status --porcelain
            if ($LASTEXITCODE -ne 0) { throw "Cannot inspect $($source.name) source checkout." }
            if ($dirty) { throw "Vendored $($source.name) has local changes; refusing to overwrite them." }
            Invoke-Native 'git' @('-C', $path, 'fetch', '--depth', '1', 'origin', $source.revision)
            Invoke-Native 'git' @('-C', $path, 'checkout', '--detach', $source.revision)
        }
        if ($source.submodules) {
            Invoke-Native 'git' @('-C', $path, 'submodule', 'update', '--init', '--recursive')
        }
    }
    $snowToolchain = Get-Content (Join-Path $script:Root 'vendor\snow\rust-toolchain.toml') -Raw
    if ($snowToolchain -notmatch ('channel\s*=\s*"' + [regex]::Escape($script:Config.toolchain) + '"')) {
        throw 'Pinned Snow source and project Rust toolchain do not match.'
    }
    $configFile = Join-Path $env:CARGO_HOME 'config.toml'
    if (Test-Path $configFile) { Remove-Item -LiteralPath $configFile }
    Push-Location $script:Root
    try {
        # Not --locked: existing Cargo.lock pins are kept; only newly declared dependencies are resolved.
        $vendorArguments = @('vendor', '--versioned-dirs', (Join-Path $script:Root 'vendor\crates'))
        $vendorConfiguration = & (Get-Cargo) @vendorArguments
        if ($LASTEXITCODE -ne 0) { throw 'Cargo dependency vendoring failed.' }
        $vendorConfiguration | Set-Content -Path $configFile -Encoding utf8NoBOM
    }
    finally { Pop-Location }
    Assert-Initialized
}

function Assert-Initialized {
    foreach ($path in @((Get-Rustup), (Get-Cargo), (Join-Path $env:CARGO_HOME 'config.toml'), (Join-Path $script:Root 'Cargo.lock'), (Join-Path $script:Root 'vendor\crates'))) {
        if (!(Test-Path $path)) { throw "Missing $path. Run .\build.ps1 -Init." }
    }
    foreach ($name in @('gcc', 'llvm-mingw', 'zig', 'cargo-zigbuild')) {
        if (!(Test-Path (Get-ToolExecutable $name))) { throw "Missing $name. Run .\build.ps1 -Init." }
    }
    $version = & (Get-Cargo) --version
    if ($LASTEXITCODE -ne 0 -or $version -notlike "cargo $($script:Config.toolchain) *") {
        throw 'The initialized Cargo version does not match the project toolchain. Run -Init.'
    }
    foreach ($target in @('x86_64-pc-windows-gnu', 'aarch64-pc-windows-gnullvm', 'x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu')) {
        Set-TargetLinker $target | Out-Null
    }
}

function Set-TargetLinker {
    param([string]$Target)
    $key = $Target.ToUpperInvariant().Replace('-', '_')
    if ($Target -eq 'x86_64-pc-windows-gnu') {
        $gcc = Get-ToolExecutable 'gcc'
        $specifications = (& $gcc -dumpspecs) -join "`n"
        if ($LASTEXITCODE -ne 0) { throw 'Cannot inspect GCC linker specifications.' }
        $endfile = [regex]::Match($specifications, '(?m)^\*endfile:\r?\n([^\r\n]+)').Groups[1].Value
        $manifestObject = & $gcc '-print-file-name=default-manifest.o'
        if ($LASTEXITCODE -ne 0 -or !(Test-Path $manifestObject)) { throw 'GCC default manifest object is missing.' }
        # GCC's if-exists specs expansion does not quote paths containing spaces.
        $endfile = $endfile.Replace('%:if-exists(default-manifest.o%s)', '-l:default-manifest.o')
        if (!$endfile) { throw 'GCC endfile specifications are missing.' }
        $specsPath = Join-Path $script:Root '_local\linkers\gcc-paths.specs'
        Set-GeneratedText $specsPath "*endfile:`n$endfile`n"
        $linker = Join-Path $script:Root "_local\linkers\$Target.cmd"
        Set-GeneratedText $linker "@echo off`r`n`"$gcc`" -specs=`"$specsPath`" %*`r`n"
        $flags = '-C target-feature=+crt-static'
    }
    elseif ($Target -eq 'aarch64-pc-windows-gnullvm') {
        $linker = Get-ToolExecutable 'llvm-mingw'
        $flags = '-C target-feature=+crt-static'
    }
    else {
        $zigTarget = if ($Target.StartsWith('aarch64')) { 'aarch64' } else { 'x86_64' }
        $linker = Join-Path $script:Root "_local\linkers\$Target.cmd"
        Set-GeneratedText $linker "@echo off`r`n`"$(Get-ToolExecutable 'cargo-zigbuild')`" zig cc -- -g -fno-sanitize=all -target $zigTarget-linux-gnu.$($script:Config.glibc) %*`r`n"
        $flags = ''
    }
    # C compiler and archiver for crates that build C/assembly (e.g. ring for HTTPS).
    $ccKey = $Target.Replace('-', '_')
    if ($Target -eq 'x86_64-pc-windows-gnu') {
        $cc = Get-ToolExecutable 'gcc'
        $ar = Join-Path (Split-Path $cc) 'ar.exe'
    }
    elseif ($Target -eq 'aarch64-pc-windows-gnullvm') {
        $cc = Get-ToolExecutable 'llvm-mingw'
        $ar = Join-Path (Split-Path $cc) 'llvm-ar.exe'
    }
    else {
        $zig = Get-ToolExecutable 'zig'
        $cc = Join-Path $script:Root "_local\linkers\$Target-cc.cmd"
        Set-GeneratedText $cc "@echo off`r`n`"$(Get-ToolExecutable 'cargo-zigbuild')`" zig cc -- -fno-sanitize=all -target $zigTarget-linux-gnu.$($script:Config.glibc) %*`r`n"
        $ar = Join-Path $script:Root "_local\linkers\$Target-ar.cmd"
        Set-GeneratedText $ar "@echo off`r`n`"$zig`" ar %*`r`n"
    }
    [Environment]::SetEnvironmentVariable("CC_$ccKey", $cc, 'Process')
    [Environment]::SetEnvironmentVariable("AR_$ccKey", $ar, 'Process')
    [Environment]::SetEnvironmentVariable("CARGO_TARGET_${key}_LINKER", $linker, 'Process')
    [Environment]::SetEnvironmentVariable("CARGO_TARGET_${key}_RUSTFLAGS", $flags, 'Process')
    return $linker
}

function Get-PackageName {
    param([string]$Platform, [string]$Arch, [string]$Configuration)
    $metadata = & (Get-Cargo) metadata --offline --locked --no-deps --format-version 1 --manifest-path (Join-Path $script:Root 'Cargo.toml') | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) { throw 'Cannot read package metadata.' }
    $version = ($metadata.packages | Where-Object name -eq $script:Config.binary).version
    return "$($script:Config.package.name)-$version-$($Platform.ToLowerInvariant())-$Arch-$($Configuration.ToLowerInvariant())"
}

function Invoke-StandaloneTests {
    param([string]$App, [string]$TestBinary, [string]$Name)
    $id = [guid]::NewGuid().ToString('N')
    $env:HAIL_TEST_ROOT = Join-Path $script:Root "_local\test-runs\$id"
    $env:HAIL_APP = $App
    $env:HAIL_DEFAULT_CONFIG = Join-Path $script:Root $script:Config.package.config
    $log = Join-Path $script:Root "_local\logs\tests-$Name-$id.log"
    & $TestBinary 2>&1 | Tee-Object -FilePath $log | Out-Host
    if ($LASTEXITCODE -ne 0) { throw "Standalone tests failed; see $log." }
}

function Write-TarGz {
    param([string]$Source, [string]$Destination, [string[]]$Executables)
    $fileStream = [IO.File]::Create($Destination)
    $gzip = [IO.Compression.GZipStream]::new($fileStream, [IO.Compression.CompressionLevel]::Optimal)
    $writer = [System.Formats.Tar.TarWriter]::new($gzip, $true)
    try {
        $base = Split-Path $Source -Parent
        foreach ($file in Get-ChildItem $Source -File -Recurse) {
            $relative = [IO.Path]::GetRelativePath($base, $file.FullName).Replace('\', '/')
            $entry = [System.Formats.Tar.PaxTarEntry]::new([System.Formats.Tar.TarEntryType]::RegularFile, $relative)
            $entry.Mode = if ($file.Name -in $Executables) { [IO.UnixFileMode]493 } else { [IO.UnixFileMode]420 }
            $input = [IO.File]::OpenRead($file.FullName)
            try { $entry.DataStream = $input; $writer.WriteEntry($entry) }
            finally { $input.Dispose() }
        }
    }
    finally { $writer.Dispose(); $gzip.Dispose(); $fileStream.Dispose() }
}

function New-Package {
    param([string]$Platform, [string]$Target, [string]$Name, [string]$App, [string]$Linker, [string]$TestStatus)
    $stage = Join-Path $script:Root "_local\packages\$([guid]::NewGuid().ToString('N'))"
    $packageRoot = Join-Path $stage $Name
    New-Item -ItemType Directory -Path $packageRoot -Force | Out-Null
    # Release payload only: binary, standard config, and README.md when one exists.
    Copy-Item -LiteralPath $App -Destination $packageRoot
    Copy-Item -LiteralPath (Join-Path $script:Root $script:Config.package.config) -Destination $packageRoot
    $readme = Join-Path $script:Root $script:Config.package.readme
    if (Test-Path $readme) { Copy-Item -LiteralPath $readme -Destination (Join-Path $packageRoot 'README.md') }

    $compiler = & (Join-Path $env:CARGO_HOME 'bin\rustc.exe') -Vv
    if ($LASTEXITCODE -ne 0) { throw 'Cannot read compiler identity.' }
    $linkerVersion = if ($Target.Contains('linux')) {
        & (Get-ToolExecutable 'zig') version
    } elseif ($Target -eq 'x86_64-pc-windows-gnu') {
        & (Get-ToolExecutable 'gcc') --version
    } else { & $Linker --version }
    if ($LASTEXITCODE -ne 0) { throw 'Cannot read linker identity.' }
    $commit = & git -C $script:Root rev-parse --verify HEAD 2>$null
    if ($LASTEXITCODE -ne 0) { $commit = 'uncommitted' }
    $provenance = [ordered]@{
        package = $Name; target = $Target
        rust = @($compiler); linker = [IO.Path]::GetFileName($Linker); linkerVersion = @($linkerVersion)
        glibcMinimum = $(if ($Platform -eq 'Linux') { $script:Config.glibc } else { $null })
        snowRevision = $script:Config.externalSources[0].revision
        projectRevision = $commit; builtUtc = [DateTime]::UtcNow.ToString('o'); tests = $TestStatus
        files = @(Get-ChildItem $packageRoot -File | ForEach-Object { @{ path = $_.Name; sha256 = (Get-FileHash $_.FullName).Hash.ToLowerInvariant() } })
    }
    $provenance | ConvertTo-Json -Depth 6 | Set-Content (Join-Path $script:Root "_local\logs\package-$Name.json") -Encoding utf8NoBOM

    $extension = if ($Platform -eq 'Windows') { 'zip' } else { 'tar.gz' }
    $temporaryArchive = Join-Path $stage "$Name.$extension"
    if ($Platform -eq 'Windows') { Compress-Archive -LiteralPath $packageRoot -DestinationPath $temporaryArchive }
    else { Write-TarGz -Source $packageRoot -Destination $temporaryArchive -Executables @($script:Config.binary) }
    $destination = Join-Path $script:Root "drops\$Name.$extension"
    Move-Item -LiteralPath $temporaryArchive -Destination $destination -Force
    $hash = (Get-FileHash $destination -Algorithm SHA256).Hash.ToLowerInvariant()
    "$hash  $Name.$extension" | Set-Content "$destination.sha256" -Encoding ascii
    Remove-Item -LiteralPath $stage -Recurse -Force
    Write-Host "Packaged: $destination"
}

function New-TestBundle {
    param([string]$Name, [string]$App, [string]$TestBinary)
    $bundle = Join-Path $script:Root "_local\test-bundles\$Name"
    if (Test-Path $bundle) { Remove-Item -LiteralPath $bundle -Recurse -Force }
    New-Item -ItemType Directory -Path $bundle -Force | Out-Null
    foreach ($file in @($App, $TestBinary, (Join-Path $script:Root $script:Config.package.config))) {
        Copy-Item -LiteralPath $file -Destination $bundle
    }
    Write-Host "Test bundle (not a release package): $bundle"
}
function Invoke-PlatformBuild {
    param(
        [string]$Platform,
        [string]$Architecture,
        [string]$Configuration,
        [switch]$Test
    )
    $architectures = if ($Architecture -eq 'All') { @('x64', 'arm64') }
        elseif ($Architecture -in @('intel64', 'amd64')) { @('x64') } else { @($Architecture) }
    $configurations = if ($Configuration -eq 'All') { @('Debug', 'Release') } else { @($Configuration) }
    Push-Location $script:Root
    try {
        foreach ($arch in $architectures) {
            $target = if ($Platform -eq 'Windows') {
                if ($arch -eq 'x64') { 'x86_64-pc-windows-gnu' } else { 'aarch64-pc-windows-gnullvm' }
            } else {
                if ($arch -eq 'x64') { 'x86_64-unknown-linux-gnu' } else { 'aarch64-unknown-linux-gnu' }
            }
            $linker = Set-TargetLinker $target
            foreach ($mode in $configurations) {
                Write-Host "Building $Platform $arch $mode ($target)..."
                $arguments = @('build', '--offline', '--locked', '--workspace', '--bins', '--target', $target)
                if ($mode -eq 'Release') { $arguments += '--release' }
                Invoke-Native (Get-Cargo) $arguments
                $profile = $mode.ToLowerInvariant()
                $suffix = if ($Platform -eq 'Windows') { '.exe' } else { '' }
                $app = Join-Path $env:CARGO_TARGET_DIR "$target\$profile\$($script:Config.binary)$suffix"
                $testBinary = Join-Path $env:CARGO_TARGET_DIR "$target\$profile\$($script:Config.testBinary)$suffix"
                $name = Get-PackageName -Platform $Platform -Arch $arch -Configuration $mode
                $testStatus = 'not requested'
                if ($Test) {
                    New-TestBundle -Name $name -App $app -TestBinary $testBinary
                    if ($Platform -eq 'Windows' -and $arch -eq 'x64') {
                        Invoke-StandaloneTests -App $app -TestBinary $testBinary -Name $name
                        $testStatus = 'passed on Windows x64'
                    } else {
                        $testStatus = 'compiled; not executed (requires target OS/architecture)'
                        Write-Warning "$name tests compiled but cannot execute on this Windows x64 host. Run the test bundle on the target."
                    }
                }
                New-Package -Platform $Platform -Target $target -Name $name -App $app -Linker $linker -TestStatus $testStatus
            }
        }
    }
    finally { Pop-Location }
}

$script:CleanModes = [ordered]@{
    Drops = @('drops\*')
    Build = @('_local\target', '_local\packages', '_local\package-validation', '_local\test-bundles', '_local\test-runs', '_local\tmp', 'target')
    Cache = @('_local\downloads', '_local\logs', '_local\zig-cache', '_local\zig-local-cache', '_local\zigbuild-cache', '_local\cargo-home\registry', '_local\cargo-home\git')
    Local = @('_local\*', 'target')
    Vendor = @('vendor\*')
}

function Get-CleanPaths {
    param([string[]]$Mode)
    $selected = [Collections.Generic.List[string]]::new()
    foreach ($name in $Mode) {
        $names = switch ($name) {
            'Cache' { @('Build', 'Cache') }
            'All' { @('Local', 'Vendor', 'Drops') }
            default { @($name) }
        }
        foreach ($entry in $names) { $selected.AddRange([string[]]$script:CleanModes[$entry]) }
    }
    $protected = @('source', 'tests', 'build', '.git', '.cargo', 'design-docs') |
        ForEach-Object { [IO.Path]::GetFullPath((Join-Path $script:Root $_)) }
    $paths = foreach ($pattern in $selected | Select-Object -Unique) {
        foreach ($item in @(Get-Item -Path (Join-Path $script:Root $pattern) -Force -ErrorAction SilentlyContinue)) {
            $full = [IO.Path]::GetFullPath($item.FullName)
            $relative = [IO.Path]::GetRelativePath($script:Root, $full)
            if ($relative.StartsWith('..') -or [IO.Path]::IsPathRooted($relative) -or $relative -eq '.') {
                throw "Refusing to clean $full; it is outside the workspace."
            }
            if ($protected | Where-Object { $full -eq $_ -or $full.StartsWith("$_\", [StringComparison]::OrdinalIgnoreCase) }) {
                throw "Refusing to clean protected path $full."
            }
            $full
        }
    }
    # Drop children of folders that are already being removed.
    $sorted = @($paths | Sort-Object -Unique)
    return @($sorted | Where-Object {
            $candidate = $_
            -not ($sorted | Where-Object { $candidate.StartsWith("$_\", [StringComparison]::OrdinalIgnoreCase) })
        })
}

function Invoke-WorkspaceClean {
    [CmdletBinding(SupportsShouldProcess)]
    param([Parameter(Mandatory)][ValidateSet('Drops', 'Build', 'Cache', 'Local', 'Vendor', 'All')][string[]]$Mode)
    $paths = Get-CleanPaths -Mode $Mode
    $failures = [Collections.Generic.List[string]]::new()
    [long]$freed = 0
    foreach ($path in $paths) {
        [long]$bytes = 0
        foreach ($file in @(Get-ChildItem -LiteralPath $path -Recurse -Force -File -ErrorAction SilentlyContinue)) { $bytes += $file.Length }
        $label = '{0} ({1:N1} MB)' -f [IO.Path]::GetRelativePath($script:Root, $path), ($bytes / 1MB)
        if (!$PSCmdlet.ShouldProcess($label, 'Remove')) { continue }
        try {
            Remove-Item -LiteralPath $path -Recurse -Force -ErrorAction Stop
            $freed += $bytes
            Write-Host "Removed $label"
        }
        catch { $failures.Add("$label`: $($_.Exception.Message)") }
    }
    foreach ($folder in @('vendor', 'drops', '_local')) {
        $path = Join-Path $script:Root $folder
        if (!(Test-Path -LiteralPath $path) -and $PSCmdlet.ShouldProcess($folder, 'Create empty folder')) {
            New-Item -ItemType Directory -Path $path | Out-Null
        }
    }
    if ($paths.Count -eq 0) { Write-Host "Clean ($($Mode -join ', ')): nothing to remove." }
    elseif (!$WhatIfPreference) { Write-Host ('Clean ({0}): freed {1:N1} MB.' -f ($Mode -join ', '), ($freed / 1MB)) }
    if ($failures.Count -gt 0) {
        throw "Clean incomplete; close programs using these paths (IDE, rust-analyzer, running hail/tests) and retry:`n$($failures -join "`n")"
    }
}

function Resolve-BuildRequest {
    # Shared flag contract for build.ps1 and the child scripts. Only -Build or -Test
    # builds; -Platform/-Architecture/-Configuration merely select what to build.
    param(
        [Parameter(Mandatory)][System.Collections.IDictionary]$Bound,
        [string]$Script = 'build.ps1'
    )
    $build = $Bound.ContainsKey('Build') -and [bool]$Bound['Build']
    $test = $Bound.ContainsKey('Test') -and [bool]$Bound['Test']
    $selection = @(@('Platform', 'Architecture', 'Configuration').Where({ $Bound.ContainsKey($_) }))
    if (@($selection).Count -gt 0 -and !($build -or $test)) {
        throw "-$($selection -join ', -') only choose(s) targets and never build(s) on their own; add -Build (or -Test) to build."
    }
    $requested = $build -or $test
    $actions = @(@('Init', 'Clean', 'Build', 'Test').Where({ $Bound.ContainsKey($_) }))
    if (@($actions).Count -eq 0) {
        Write-Host @"
Nothing to do. $Script runs only the actions you ask for:
  -Init                      prepare toolchain, linkers and vendored sources (no build)
  -Build                     build and package (default selection: Windows x64 Debug)
  -Test                      build, package and run the standalone tests
  -Clean <modes>             clean (Drops, Build, Cache, Local, Vendor, All); -WhatIf previews
  -Platform/-Architecture/-Configuration select targets for -Build/-Test
See BUILD.md.
"@
    }
    return $requested
}

Export-ModuleMember -Function Initialize-Environment, Initialize-Workspace, Assert-Initialized, Invoke-PlatformBuild, Invoke-WorkspaceClean, Resolve-BuildRequest
