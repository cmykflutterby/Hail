#Requires -Version 7.4
[CmdletBinding(SupportsShouldProcess)]
param(
    [switch]$Init,
    [string]$Platform = 'Windows',
    [string]$Architecture = 'x64',
    [string]$Configuration = 'Debug',
    [switch]$Build,
    [switch]$Test,
    [string[]]$Clean,
    [switch]$Help
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Show-BuildUsage {
    Write-Host @'
Usage:
  .\build.ps1 [options]

Actions:
  -Init                         Prepare the toolchain, linkers and vendored sources.
  -Build                        Build and package the selected target.
  -Test                         Build, package and run the standalone tests.
  -Clean <mode[,mode...]>       Remove generated files (Drops, Build, Cache, Local,
                                Vendor or All). Use -WhatIf to preview cleanup.
  -Help                         Show this usage information.

Target selection:
  -Platform <Windows|Linux|All> Select the platform for -Build or -Test. Default: Windows.
  -Architecture <x64|intel64|amd64|arm64|All>
                                Select the architecture. Default: x64.
  -Configuration <Debug|Release|All>
                                Select the build configuration. Default: Debug.

Examples:
  .\build.ps1 -Init
  .\build.ps1 -Build
  .\build.ps1 -Test -Platform All -Architecture All -Configuration All
  .\build.ps1 -Build -Platform Linux -Architecture arm64 -Configuration Release
  .\build.ps1 -Clean Build -WhatIf
  .\build.ps1 -Clean Drops,Build
'@
}

$showUsageOnError = $true
if ($Help) {
    Show-BuildUsage
    exit 0
}

try {
    $validPlatforms = @('Windows', 'Linux', 'All')
    $validArchitectures = @('x64', 'intel64', 'amd64', 'arm64', 'All')
    $validConfigurations = @('Debug', 'Release', 'All')
    $validCleanModes = @('Drops', 'Build', 'Cache', 'Local', 'Vendor', 'All')
    if ($Platform -notin $validPlatforms) { throw "-Platform '$Platform' is invalid." }
    if ($Architecture -notin $validArchitectures) { throw "-Architecture '$Architecture' is invalid." }
    if ($Configuration -notin $validConfigurations) { throw "-Configuration '$Configuration' is invalid." }
    if ($Clean) {
        $invalidCleanModes = @($Clean | Where-Object { $_ -notin $validCleanModes })
        if ($invalidCleanModes) { throw "-Clean mode '$($invalidCleanModes -join "', '")' is invalid." }
    }
    if (!$Init -and !$Build -and !$Test -and !$Clean) {
        Show-BuildUsage
        $showUsageOnError = $false
        exit 1
    }

    if ($Init) {
        foreach ($folder in @('build', 'tests', '.cargo')) {
            $path = Join-Path $PSScriptRoot $folder
            if (!(Test-Path $path) -and (Test-Path (Join-Path $PSScriptRoot '.git'))) {
                & git -C $PSScriptRoot restore --worktree -- $folder
                if ($LASTEXITCODE -ne 0) { throw "Cannot restore tracked $folder; restore it from your checkout." }
            }
        }
    }
    Import-Module (Join-Path $PSScriptRoot 'build\Common.psm1') -Force
    try {
        $buildRequested = Resolve-BuildRequest -Bound $PSBoundParameters -Script 'build.ps1'
    }
    catch {
        Show-BuildUsage
        $showUsageOnError = $false
        throw
    }
    if (!($buildRequested -or $Init -or $Clean)) {
        Show-BuildUsage
        $showUsageOnError = $false
        exit 1
    }
    $showUsageOnError = $false
    if ($WhatIfPreference -and !$Clean) { throw '-WhatIf only previews -Clean.' }
    if ($Clean) {
        Invoke-WorkspaceClean -Mode $Clean -WhatIf:$WhatIfPreference
        if (!($buildRequested -or $Init) -or $WhatIfPreference) { exit 0 }
    }
    $root = Initialize-Environment
    $log = Join-Path $root "_local\logs\build-$([DateTime]::UtcNow.ToString('yyyyMMdd-HHmmss'))-$PID.log"
    Start-Transcript -Path $log | Out-Null
    try {
        if ($Init) { Initialize-Workspace }
        Assert-Initialized
        if (!$buildRequested) { Write-Host 'Initialization complete; nothing built. Add -Build (or -Test) to build.'; return }
        $platforms = if ($Platform -eq 'All') { @('Windows', 'Linux') } else { @($Platform) }
        foreach ($family in $platforms) {
            $arguments = @{
                Platform = $family
                Architecture = $Architecture
                Configuration = $Configuration
                Build = $true
                Test = $Test
            }
            & (Join-Path $root "build\$($family.ToLowerInvariant()).ps1") @arguments
        }
    }
    finally { Stop-Transcript | Out-Null }
}
catch {
    if ($showUsageOnError) { Show-BuildUsage }
    Write-Error $_ -ErrorAction Continue
    exit 1
}
