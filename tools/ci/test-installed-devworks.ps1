param(
    [Parameter(Mandatory)][string]$Package,
    [Parameter(Mandatory)][string]$Tests,
    [Parameter(Mandatory)][string]$Output,
    [Parameter(Mandatory)][string]$Source
)

. (Join-Path $PSScriptRoot 'windows-test-installer.ps1')
$msiName = 'DevWorks-Test-windows-x64.msi'
$msi = Join-Path $Package $msiName
$signed = Get-Content -LiteralPath (Join-Path $Package 'signed.json') -Raw | ConvertFrom-Json -AsHashtable
if ($signed.source -cne $Source -or (Get-VerifiedTestSignature $msi).sha256 -cne $signed.installer.sha256) {
    throw 'Signed MSI provenance does not match the approved build'
}

$layout = Get-TestInstallLayout
$profile = $layout.profile
$clean = $layout.machine + $layout.installed + $layout.shortcut + $profile + $layout.currentUser
if ((Test-Path -LiteralPath $clean) -contains $true) {
    throw 'Expected a clean Windows user profile for MSI qualification'
}
$work = Split-Path -Parent $Output
Invoke-TestMsi $msi '/i' (Join-Path $work 'install.log')
$productKey = Confirm-TestInstallScope $layout
foreach ($name in $script:payloadNames) {
    $path = Join-Path $layout.installed $name
    $hash = if ($name.EndsWith('.exe')) { (Get-VerifiedTestSignature $path).sha256 } else { Get-Sha256 $path }
    if ($hash -cne $signed.payload[$name].sha256) { throw "Installed bytes differ for $name" }
}
$desktop = Join-Path $layout.installed 'devworks-desktop.exe'

$png = Join-Path $work 'first-frame.png'
$stderr = Join-Path $work 'app.stderr'
$launchArgs = @('--data-dir', ('"{0}"' -f $profile), '--screenshot', ('"{0}"' -f $png), '--close-after-secs', '40')
$process = Start-Process -FilePath $desktop -ArgumentList $launchArgs -PassThru -RedirectStandardError $stderr
if (-not $process.WaitForExit(120000)) {
    $process.Kill()
    throw 'The installed desktop did not exit cleanly'
}
Get-Content -LiteralPath $stderr | Write-Host
if ($process.ExitCode -ne 0 -or (Get-Item -LiteralPath $png).Length -le 64 -or
    (Get-Content -LiteralPath $stderr -Raw) -notmatch 'devworks: runtime booted') {
    throw 'The installed desktop did not boot the real runtime and render a frame'
}
$evidenceText = & $desktop --release-evidence
if ($LASTEXITCODE -ne 0) { throw 'Cannot read the installed runtime identity' }
$evidence = $evidenceText | ConvertFrom-Json -AsHashtable
if ($evidence.target -cne 'x86_64-pc-windows-msvc' -or $evidence.build -cne $Source -or
    $evidence.migrations.Count -ne 76) { throw 'Unexpected installed build or migration catalogue' }
$releaseJson = Join-Path $work 'release-evidence.json'
$evidenceText | Set-Content -LiteralPath $releaseJson -Encoding utf8NoBOM
@'
import json
import sqlite3
import sys
from pathlib import Path

profile, release = (Path(value) for value in sys.argv[1:])
expected = json.loads(release.read_text(encoding="utf-8"))["migrations"]
database = sqlite3.connect((profile / "workspace.sqlite").resolve().as_uri() + "?mode=ro", uri=True)
assert database.execute("SELECT schema_version, minimum_writer, minimum_reader FROM store_meta").fetchone() == (76, 76, 68)
assert database.execute("PRAGMA user_version").fetchone() == (76,)
assert database.execute("SELECT version, checksum FROM schema_migrations ORDER BY version").fetchall() == [
    (row["version"], row["checksum"]) for row in expected
]
assert database.execute("SELECT shutdown_state FROM app_boots ORDER BY started_at DESC LIMIT 1").fetchone() == ("Clean",)
'@ | python -I - $profile $releaseJson
if ($LASTEXITCODE -ne 0) { throw 'Installed profile or boot evidence did not verify' }

$env:DEVWORKS_PDF_PACKAGED_DESKTOP = $desktop
& (Join-Path $Tests 'pdf_worker.exe') --skip native_crash_timeout_and_cancel_reap_the_worker --nocapture
if ($LASTEXITCODE -ne 0) { throw 'Installed PDF worker qualification failed' }
& (Join-Path $Tests 'document_extraction.exe') --nocapture
if ($LASTEXITCODE -ne 0) { throw 'Installed Documents qualification failed' }

$sentinel = Join-Path $profile 'retain-on-uninstall.txt'
'Retain this test profile' | Set-Content -LiteralPath $sentinel -Encoding utf8NoBOM
$before = Get-Sha256 $sentinel
$profileDatabase = Join-Path $profile 'workspace.sqlite'
$databaseBefore = Get-Sha256 $profileDatabase
Invoke-TestMsi $msi '/x' (Join-Path $work 'uninstall.log')
if (((Test-Path -LiteralPath @($layout.installed, $layout.shortcut, $productKey)) -contains $true) -or
    (Get-Sha256 $sentinel) -cne $before -or (Get-Sha256 $profileDatabase) -cne $databaseBefore) {
    throw 'Uninstall failed to remove the application or preserve its data'
}

New-Item -ItemType Directory -Path $Output | Out-Null
Copy-Item -LiteralPath $msi -Destination $Output
"$($signed.installer.sha256)  $msiName" |
    Set-Content -LiteralPath (Join-Path $Output 'SHA256SUMS.txt') -Encoding ascii
$os = Get-CimInstance Win32_OperatingSystem
$signed.qualification = [ordered]@{
    os = $os.Caption
    osBuild = $os.BuildNumber
    architecture = 'x64'
    scope = 'per-user'
    schemaVersion = 76
    renderedAndClosedCleanly = $true
    installedPdfWorker = 'passed'
    installedDocuments = 'passed'
    uninstallPreservedProfile = $true
}
$signed | ConvertTo-Json -Depth 8 |
    Set-Content -LiteralPath (Join-Path $Output 'qualification.json') -Encoding utf8NoBOM
