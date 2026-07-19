[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('x86_64-pc-windows-msvc', 'aarch64-pc-windows-msvc')]
    [string]$Target
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
Push-Location $Root
$TemporaryOutput = $null
try {
    # The installer is shipped as a standalone executable. Keep the warnings gate active while
    # forcing Rust and native dependencies to use the static MSVC runtime for both architectures.
    $env:RUSTFLAGS = '-Dwarnings -C target-feature=+crt-static'

    $Metadata = cargo metadata --locked --no-deps --format-version 1 | ConvertFrom-Json
    $Package = $Metadata.packages | Where-Object { $_.name -eq 'snapdog-os-installer' }
    if ($null -eq $Package -or [string]::IsNullOrWhiteSpace($Package.version)) {
        throw 'Could not determine package version.'
    }

    $Architecture = switch ($Target) {
        'x86_64-pc-windows-msvc' { 'x86_64' }
        'aarch64-pc-windows-msvc' { 'aarch64' }
    }

    cargo build --locked --release --target $Target
    if ($LASTEXITCODE -ne 0) {
        throw "Cargo build failed for $Target."
    }

    $Dist = Join-Path $Root 'dist'
    New-Item -ItemType Directory -Force -Path $Dist | Out-Null
    $Source = Join-Path $Root "target/$Target/release/snapdog-os-installer.exe"
    $Output = Join-Path $Dist "snapdog-os-installer-$($Package.version)-windows-$Architecture.exe"
    $TemporaryOutput = Join-Path $Dist ".snapdog-os-installer-$($Package.version)-windows-$Architecture-$PID.tmp.exe"
    Copy-Item -Force -LiteralPath $Source -Destination $TemporaryOutput

    $File = Get-Item -LiteralPath $TemporaryOutput
    if ($File.Length -le 0) {
        throw "Packaged executable is empty: $TemporaryOutput"
    }

    $Dumpbin = @(Get-Command 'dumpbin.exe' -CommandType Application -ErrorAction Stop)[0]
    $DependenciesOutput = & $Dumpbin.Source /nologo /dependents $File.FullName 2>&1 | Out-String
    if ($LASTEXITCODE -ne 0) {
        throw "dumpbin dependency inspection failed for $Output.`n$DependenciesOutput"
    }
    $RedistributablePattern = '(?im)^\s*((?:vcruntime|msvcp|msvcr|concrt|ucrtbase|api-ms-win-crt-)[A-Za-z0-9._-]*\.dll)\s*$'
    $RedistributableDependencies = [regex]::Matches(
        $DependenciesOutput,
        $RedistributablePattern
    ) | ForEach-Object { $_.Groups[1].Value } | Sort-Object -Unique
    if ($RedistributableDependencies.Count -ne 0) {
        throw "Packaged executable depends on a dynamic VC/UCRT runtime: $($RedistributableDependencies -join ', ')"
    }

    # Publish the final filename only after the PE import gate has passed.
    Move-Item -Force -LiteralPath $TemporaryOutput -Destination $Output
    $File = Get-Item -LiteralPath $Output
    Write-Output $File.FullName
}
finally {
    if ($null -ne $TemporaryOutput -and (Test-Path -LiteralPath $TemporaryOutput)) {
        Remove-Item -Force -LiteralPath $TemporaryOutput
    }
    Pop-Location
}
