#Requires -Version 7.4
[CmdletBinding(SupportsShouldProcess)]
param(
    [switch]$Init,
    [ValidateSet('Windows', 'Linux', 'All')][string]$Platform = 'Windows',
    [ValidateSet('x64', 'intel64', 'amd64', 'arm64', 'All')][string]$Architecture = 'x64',
    [ValidateSet('Debug', 'Release', 'All')][string]$Configuration = 'Debug',
    [switch]$Build,
    [switch]$Test,
    [ValidateSet('Drops', 'Build', 'Cache', 'Local', 'Vendor', 'All')][string[]]$Clean
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
try {
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
    $buildRequested = Resolve-BuildRequest -Bound $PSBoundParameters -Script 'build.ps1'
    if (!($buildRequested -or $Init -or $Clean)) { exit 0 }
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
    Write-Error $_ -ErrorAction Continue
    exit 1
}
