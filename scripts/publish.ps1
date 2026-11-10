param(
    [string]$Token,
    [switch]$DryRun,
    [switch]$AllowDirty
)

$ErrorActionPreference = "Stop"
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RootDir = Split-Path -Parent $ScriptDir
$OrderFile = Join-Path $ScriptDir "publish-order.txt"

# Read crates from publish-order.txt
$crates = Get-Content $OrderFile | Where-Object { $_ -match '\S' -and -not $_.Trim().StartsWith('#') } | ForEach-Object { $_.Trim() }

if (-not $crates) {
    Write-Error "No crates found in $OrderFile"
    exit 1
}

$tokenToUse = if ($Token) { $Token } else { $env:CARGO_REGISTRY_TOKEN }

if (-not $DryRun -and -not $tokenToUse) {
    Write-Error "A crates.io token is required. Pass -Token 'cio_xxx' or set `$env:CARGO_REGISTRY_TOKEN."
    exit 1
}

if ($tokenToUse) {
    $env:CARGO_REGISTRY_TOKEN = $tokenToUse
}

Set-Location $RootDir

$versionMatch = Select-String -Path (Join-Path $RootDir "Cargo.toml") -Pattern 'version\s*=\s*"([^"]+)"'
$version = if ($versionMatch) { $versionMatch.Matches[0].Groups[1].Value } else { "0.1.0" }

if ($DryRun) {
    Write-Host "Running publish dry-run across all crates..." -ForegroundColor Cyan
    $publishArgs = @("publish", "--dry-run", "--locked")
    foreach ($c in $crates) {
        $publishArgs += @("-p", $c)
    }
    if ($AllowDirty) { $publishArgs += "--allow-dirty" }
    & cargo @publishArgs
    exit 0
}

function Test-CratePublished($crate, $ver) {
    try {
        $resp = Invoke-WebRequest -Uri "https://crates.io/api/v1/crates/$crate/$ver" -Headers @{ "User-Agent" = "xstate-statemachine-release" } -Method Head -TimeoutSec 10 -UseBasicParsing -ErrorAction SilentlyContinue
        return ($resp.StatusCode -eq 200)
    } catch {
        return $false
    }
}

Write-Host "Publishing $($crates.Count) workspace crates at version $version to crates.io..." -ForegroundColor Green

foreach ($crate in $crates) {
    if (Test-CratePublished $crate $version) {
        Write-Host "$crate $version is already published on crates.io, skipping." -ForegroundColor Yellow
        continue
    }

    $publishArgs = @("publish", "--locked", "-p", $crate)
    if ($AllowDirty) { $publishArgs += "--allow-dirty" }

    $published = $false
    while (-not $published) {
        Write-Host "`n==> Publishing $crate $version..." -ForegroundColor Cyan
        
        $prevEAP = $ErrorActionPreference
        $ErrorActionPreference = "Continue"
        & cargo @publishArgs
        $code = $LASTEXITCODE
        $ErrorActionPreference = $prevEAP

        if ($code -eq 0) {
            $published = $true
        } else {
            if (Test-CratePublished $crate $version) {
                $published = $true
                break
            }
            Write-Host "`nPublish attempt failed for $crate (exit code $code)." -ForegroundColor Yellow
            Write-Host "If crates.io new-crate rate limit (429) was hit, waiting 5 minutes to auto-retry..." -ForegroundColor Yellow
            Write-Host "Press Ctrl+C to abort if this is an unexpected failure." -ForegroundColor Gray
            Start-Sleep -Seconds 300
        }
    }

    Write-Host "Waiting for $crate $version to appear in crates.io index..." -ForegroundColor Gray
    $appeared = $false
    for ($i = 0; $i -lt 30; $i++) {
        Start-Sleep -Seconds 10
        if (Test-CratePublished $crate $version) {
            $appeared = $true
            Write-Host "$crate $version is live on crates.io!" -ForegroundColor Green
            break
        }
        Write-Host "Still waiting for index ($(($i + 1) * 10)s)..." -ForegroundColor Gray
    }

    if (-not $appeared) {
        Write-Warning "$crate was published, but didn't appear in the HTTP API within 5 minutes. Proceeding to next crate..."
    }
}

Write-Host "`nAll crates published successfully at version $version!" -ForegroundColor Green
