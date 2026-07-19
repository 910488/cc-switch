param(
    [string]$OutputDirectory = (Join-Path $PSScriptRoot "..\local-installers"),
    [switch]$SkipInstall
)

$ErrorActionPreference = "Stop"

$repository = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$resolvedOutputDirectory = if ([System.IO.Path]::IsPathRooted($OutputDirectory)) {
    [System.IO.Path]::GetFullPath($OutputDirectory)
} else {
    [System.IO.Path]::GetFullPath((Join-Path $repository $OutputDirectory))
}
$vswhere = "C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe"
if (-not (Test-Path -LiteralPath $vswhere)) {
    throw "Visual Studio Installer (vswhere.exe) was not found."
}

$visualStudio = & $vswhere `
    -latest `
    -products * `
    -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
    -property installationPath
if (-not $visualStudio) {
    throw "Install Visual Studio Build Tools with the Desktop development with C++ workload."
}

$vsDevCmd = Join-Path $visualStudio "Common7\Tools\VsDevCmd.bat"
if (-not (Test-Path -LiteralPath $vsDevCmd)) {
    throw "VsDevCmd.bat was not found under $visualStudio."
}

# Import the complete MSVC x64 environment into this PowerShell process.
cmd.exe /d /c "`"$vsDevCmd`" -arch=x64 -host_arch=x64 >nul && set" |
    ForEach-Object {
        if ($_ -match '^([^=]+)=(.*)$') {
            [Environment]::SetEnvironmentVariable($matches[1], $matches[2], "Process")
        }
    }

# WiX 3 fails to launch light.exe reliably from paths containing non-ASCII
# characters. Build through a stable ASCII junction while retaining one target
# directory for fast incremental release builds.
$buildRoot = "C:\cc-switch-local"
if (Test-Path -LiteralPath $buildRoot) {
    $item = Get-Item -LiteralPath $buildRoot -Force
    $targets = @($item.Target | ForEach-Object { [string]$_ })
    if ($item.LinkType -ne "Junction" -or $targets -notcontains $repository) {
        throw "$buildRoot already exists and is not a junction to $repository."
    }
} else {
    New-Item -ItemType Junction -Path $buildRoot -Target $repository | Out-Null
}

$pnpmVersion = "10.12.3"
& corepack "pnpm@$pnpmVersion" --version | Out-Null
if ($LASTEXITCODE -ne 0) {
    throw "Unable to prepare pnpm $pnpmVersion through Corepack."
}

$temporaryConfig = Join-Path $env:TEMP "cc-switch-local-msi-$PID.json"
@{
    build = @{
        beforeBuildCommand = "corepack pnpm@$pnpmVersion run build:renderer"
    }
    bundle = @{
        createUpdaterArtifacts = $false
    }
} | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $temporaryConfig -Encoding ascii

$locationPushed = $false
try {
    Push-Location $buildRoot
    $locationPushed = $true
    if (-not $SkipInstall) {
        & corepack "pnpm@$pnpmVersion" install --frozen-lockfile
        if ($LASTEXITCODE -ne 0) {
            throw "pnpm install failed with exit code $LASTEXITCODE."
        }
    } elseif (-not (Test-Path -LiteralPath ".\node_modules\.bin\tauri.cmd")) {
        throw "-SkipInstall requires an existing node_modules directory. Run once without it."
    }

    & .\node_modules\.bin\tauri.cmd build --ci --bundles msi --config $temporaryConfig
    if ($LASTEXITCODE -ne 0) {
        throw "Tauri MSI build failed with exit code $LASTEXITCODE."
    }

    $msi = Get-ChildItem `
        -Path "src-tauri\target\release\bundle\msi" `
        -Filter "*.msi" `
        -File |
        Select-Object -First 1
    if ($null -eq $msi) {
        throw "Tauri completed without producing an MSI."
    }

    New-Item -ItemType Directory -Force -Path $resolvedOutputDirectory | Out-Null
    $shortSha = (& git rev-parse --short=7 HEAD).Trim()
    $destination = Join-Path $resolvedOutputDirectory "CC-Switch-Continuity-$shortSha-Windows-x64-local.msi"
    Copy-Item -LiteralPath $msi.FullName -Destination $destination -Force

    $hash = Get-FileHash -Algorithm SHA256 -LiteralPath $destination
    Write-Host "MSI: $destination"
    Write-Host "SHA256: $($hash.Hash)"
} finally {
    if ($locationPushed) {
        Pop-Location
    }
    Remove-Item -LiteralPath $temporaryConfig -Force -ErrorAction SilentlyContinue
}
