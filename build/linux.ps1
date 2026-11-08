#Requires -Version 7.4
[CmdletBinding(SupportsShouldProcess)]
param(
    [switch]$Init,
    [ValidateSet('Windows', 'Linux', 'All')][string]$Platform = 'Linux',
    [ValidateSet('x64', 'intel64', 'amd64', 'arm64', 'All')][string]$Architecture = 'x64',
    [ValidateSet('Debug', 'Release', 'All')][string]$Configuration = 'Debug',
    [switch]$Build,
    [switch]$Test,
    [ValidateSet('Drops', 'Build', 'Cache', 'Local', 'Vendor', 'All')][string[]]$Clean
)
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'Common.psm1') -Force
$buildRequested = Resolve-BuildRequest -Bound $PSBoundParameters -Script 'linux.ps1'
if (!($buildRequested -or $Init -or $Clean)) { return }
if ($buildRequested -and $Platform -ne 'Linux') { throw 'linux.ps1 requires -Platform Linux; use build.ps1 for other families.' }
if ($WhatIfPreference -and !$Clean) { throw '-WhatIf only previews -Clean.' }
if ($Clean) {
    Invoke-WorkspaceClean -Mode $Clean -WhatIf:$WhatIfPreference
    if (!($buildRequested -or $Init) -or $WhatIfPreference) { return }
}
Initialize-Environment | Out-Null
if ($Init) { Initialize-Workspace }
Assert-Initialized
if (!$buildRequested) { Write-Host 'Initialization complete; nothing built. Add -Build (or -Test) to build.'; return }
Invoke-PlatformBuild -Platform $Platform -Architecture $Architecture -Configuration $Configuration -Test:$Test
