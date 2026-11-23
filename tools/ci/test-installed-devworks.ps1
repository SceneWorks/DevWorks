param(
    [Parameter(Mandatory)][string]$Package,
    [Parameter(Mandatory)][string]$Tests,
    [Parameter(Mandatory)][string]$Output,
    [Parameter(Mandatory)][string]$Source,
    [Parameter(Mandatory)][ValidateSet('76', '78')][int]$ExpectedSchema,
    [switch]$ProbeIncomingMcp
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
$productCode = Confirm-TestInstallScope $msi $layout
foreach ($name in $script:payloadNames) {
    $path = Join-Path $layout.installed $name
    $hash = if ($name.EndsWith('.exe')) { (Get-VerifiedTestSignature $path).sha256 } else { Get-Sha256 $path }
    if ($hash -cne $signed.payload[$name].sha256) { throw "Installed bytes differ for $name" }
}
$desktop = Join-Path $layout.installed 'devworks-desktop.exe'
$launcher = Join-Path $layout.installed 'devworks-test-launcher.exe'
if ((Get-X64PeSubsystem $launcher) -ne 2 -or (Get-X64PeSubsystem $desktop) -ne 3) {
    throw 'Expected a GUI Start launcher and a console-capable desktop for diagnostics'
}

Start-Process -FilePath $layout.shortcut
$clock = [Diagnostics.Stopwatch]::StartNew()
$shortcutDesktop = $null
while ($clock.Elapsed.TotalSeconds -lt 90) {
    $shortcutDesktop = Get-Process -Name 'devworks-desktop' -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -eq $desktop -and $_.MainWindowTitle -eq 'DevWorks' } |
        Select-Object -First 1
    if ($shortcutDesktop) { break }
    Start-Sleep -Milliseconds 250
}
if (-not $shortcutDesktop) { throw 'The installed Start shortcut did not open DevWorks' }
$null = $shortcutDesktop.Handle
if (-not $shortcutDesktop.CloseMainWindow() -or -not $shortcutDesktop.WaitForExit(30000) -or
    $shortcutDesktop.ExitCode -ne 0 -or -not (Test-Path -LiteralPath (Join-Path $profile 'workspace.sqlite'))) {
    throw 'The Start shortcut did not start and close cleanly in the isolated test profile'
}

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
    (Get-Content -LiteralPath $stderr -Raw) -notmatch 'devworks: runtime booted' -or
    (Get-Content -LiteralPath $stderr -Raw) -notmatch 'devworks: window closed') {
    throw 'The installed desktop did not boot the real runtime and render a frame'
}
$evidenceText = & $desktop --release-evidence
if ($LASTEXITCODE -ne 0) { throw 'Cannot read the installed runtime identity' }
$evidence = $evidenceText | ConvertFrom-Json -AsHashtable
if ($evidence.target -cne 'x86_64-pc-windows-msvc' -or $evidence.build -cne $Source -or
    $evidence.migrations.Count -ne $ExpectedSchema) { throw 'Unexpected installed build or migration catalogue' }
$releaseJson = Join-Path $work 'release-evidence.json'
$evidenceText | Set-Content -LiteralPath $releaseJson -Encoding utf8NoBOM
@'
import json
import sqlite3
import sys
from pathlib import Path

profile, release = (Path(value) for value in sys.argv[1:3])
schema = int(sys.argv[3])
expected = json.loads(release.read_text(encoding="utf-8"))["migrations"]
database = sqlite3.connect((profile / "workspace.sqlite").resolve().as_uri() + "?mode=ro", uri=True)
assert database.execute("SELECT schema_version, minimum_writer, minimum_reader FROM store_meta").fetchone() == (schema, schema, 68)
assert database.execute("PRAGMA user_version").fetchone() == (schema,)
assert database.execute("SELECT version, checksum FROM schema_migrations ORDER BY version").fetchall() == [
    (row["version"], row["checksum"]) for row in expected
]
assert database.execute("SELECT shutdown_state FROM app_boots ORDER BY started_at DESC LIMIT 2").fetchall() == [
    ("Clean",), ("Clean",)
]
'@ | python -I - $profile $releaseJson $ExpectedSchema
if ($LASTEXITCODE -ne 0) { throw 'Installed profile or boot evidence did not verify' }

if ($ProbeIncomingMcp) {
    $database = Join-Path $profile 'workspace.sqlite'
    $databaseHash = Get-Sha256 $database
    $probe = [Diagnostics.ProcessStartInfo]::new($desktop)
    $probe.UseShellExecute = $false
    $probe.CreateNoWindow = $true
    $probe.RedirectStandardOutput = $true
    $probe.RedirectStandardError = $true
    $probe.ArgumentList.Add('mcp')
    $probe.ArgumentList.Add('--data-dir')
    $probe.ArgumentList.Add($profile)
    $probe.Environment['DEVWORKS_MCP_TOKEN'] = 'unregistered-package-probe'
    $bridge = [Diagnostics.Process]::Start($probe)
    try {
        $stdout = $bridge.StandardOutput.ReadToEndAsync()
        $stderr = $bridge.StandardError.ReadToEndAsync()
        if (-not $bridge.WaitForExit(15000)) {
            $bridge.Kill()
            $bridge.WaitForExit()
            throw 'Installed MCP bridge survived without the desktop'
        }
        if ($bridge.ExitCode -eq 0 -or $stdout.Result.Length -ne 0 -or
            $stderr.Result -notmatch 'devworks mcp:' -or (Get-Sha256 $database) -cne $databaseHash) {
            throw 'Installed MCP bridge did not fail closed without changing the store'
        }
    } finally {
        $bridge.Dispose()
    }
}

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
Assert-TestInstallRemoved $layout $productCode
if ((Get-Sha256 $sentinel) -cne $before -or (Get-Sha256 $profileDatabase) -cne $databaseBefore) {
    throw 'Uninstall changed the test data'
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
    schemaVersion = $ExpectedSchema
    startShortcutOpenedAndClosedCleanly = $true
    startLauncherSubsystem = 'Windows GUI'
    renderedAndClosedCleanly = $true
    installedPdfWorker = 'passed'
    installedDocuments = 'passed'
    uninstallPreservedProfile = $true
}
if ($ProbeIncomingMcp) {
    $signed.qualification.closedDesktopMcpBridge = 'passed'
}
$signed | ConvertTo-Json -Depth 8 |
    Set-Content -LiteralPath (Join-Path $Output 'qualification.json') -Encoding utf8NoBOM
