# Test the production CLI and LocalSystem pipe on a disposable Windows CI guest.
$ErrorActionPreference = 'Stop'
if ($env:RUNNER_ENVIRONMENT -ne 'github-hosted') {
    throw 'This fixture needs an isolated GitHub-hosted Windows guest.'
}
$key = 'HKLM:\SOFTWARE\SceneWorks\DevHost'
if (Test-Path $key) { throw 'The guest already has a dev-host disk policy.' }

$cargo = (Get-Command cargo).Source
& $cargo build -p dev-host --locked
if ($LASTEXITCODE) { throw 'Could not build dev-host.' }
$metadata = & $cargo metadata --locked --no-deps --format-version 1 | ConvertFrom-Json
if ($LASTEXITCODE) { throw 'Could not find the dev-host build output.' }

$id = [guid]::NewGuid().ToString('N')
$root = Join-Path $env:RUNNER_TEMP "dev-host-budget-$id"
$output = Join-Path $root 'client-output'
$exe = Join-Path $root 'dev-host.exe'
$systemTask = "DevHostBudgetSystem-$id"
$clientTask = "DevHostBudgetClient-$id"
$env:DEV_HOST_ROOT = $root
$env:DEV_HOST_REAL_CARGO = $cargo

function Invoke-DevHost([string[]] $Command) {
    & $exe @Command
    if ($LASTEXITCODE) { throw "dev-host $Command failed with $LASTEXITCODE." }
}

function Wait-Fixture([scriptblock] $Ready, [string] $Failure) {
    $until = [DateTime]::UtcNow.AddSeconds(25)
    while (-not (& $Ready)) {
        if ([DateTime]::UtcNow -ge $until) { throw $Failure }
        Start-Sleep -Milliseconds 250
    }
}

function Start-FixtureTask([string] $Name, [string] $Account, [string] $ScriptPath) {
    $powerShell = Join-Path $PSHOME 'powershell.exe'
    $arguments = '-NoProfile -NonInteractive -ExecutionPolicy Bypass -File "{0}"' -f $ScriptPath
    $action = New-ScheduledTaskAction -Execute $powerShell -Argument $arguments
    $principal = New-ScheduledTaskPrincipal -UserId $Account -LogonType ServiceAccount
    Register-ScheduledTask -TaskName $Name -Action $action -Principal $principal | Out-Null
    Start-ScheduledTask -TaskName $Name
}

try {
    New-Item -ItemType Directory -Path $output -Force | Out-Null
    Set-Content -Path (Join-Path $root '.dev-host-root') -Value 'dev-host-root v1'
    Copy-Item -LiteralPath (Join-Path $metadata.target_directory 'debug/dev-host.exe') -Destination $exe
    & icacls.exe $root /grant '*S-1-5-19:(OI)(CI)RX' | Out-Null
    if ($LASTEXITCODE) { throw 'Could not grant fixture read access to LocalService.' }
    & icacls.exe $output /grant '*S-1-5-19:(OI)(CI)M' | Out-Null
    if ($LASTEXITCODE) { throw 'Could not grant fixture output access to LocalService.' }
    $settings = Join-Path $root 'settings.json'
    @{
        generation = $id
        minimum_free_bytes = 1GB
        wait_seconds = 10
        profiles = @{ local = 1MB; runner = 1MB }
        worker_images = @()
    } | ConvertTo-Json -Depth 3 | Set-Content -Encoding Ascii -Path $settings
    Invoke-DevHost @('init-host')
    Invoke-DevHost @('disk-budget', 'provision', $settings, '--drained')

    $broker = Join-Path $root 'broker.ps1'
    @'
$env:DEV_HOST_ROOT = $PSScriptRoot
& (Join-Path $PSScriptRoot 'dev-host.exe') disk-budget serve *> (Join-Path $PSScriptRoot 'broker.log')
exit $LASTEXITCODE
'@ | Set-Content -Path $broker
    Start-FixtureTask $systemTask 'NT AUTHORITY\SYSTEM' $broker
    Wait-Fixture {
        $ErrorActionPreference = 'Continue'
        & $exe disk-budget status 2>$null | Out-Null
        $LASTEXITCODE -eq 0
    } 'LocalSystem broker did not answer.'

    $client = Join-Path $root 'client.ps1'
    @'
$ErrorActionPreference = 'Stop'
$env:DEV_HOST_ROOT = $PSScriptRoot
$exe = Join-Path $PSScriptRoot 'dev-host.exe'
$lockDir = & $exe lock-dir
if ($LASTEXITCODE) { exit $LASTEXITCODE }
$anchor = Join-Path $lockDir 'disk-budget/disk-budget.lock'
try {
    [IO.File]::OpenWrite($anchor).Dispose()
    throw 'LocalService could write to the ledger lock.'
} catch [UnauthorizedAccessException] {
    # Expected. This identity communicates with the broker instead.
}
& $exe disk-budget run local -- (Join-Path $PSHOME 'powershell.exe') -NoProfile -NonInteractive -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot 'writer.ps1')
exit $LASTEXITCODE
'@ | Set-Content -Path $client
    $writer = Join-Path $root 'writer.ps1'
    @'
param([string] $Marker = 'admitted')
$ErrorActionPreference = 'Stop'
$output = Join-Path $PSScriptRoot 'client-output'
New-Item -ItemType File -Path (Join-Path $output $Marker) | Out-Null
$until = [DateTime]::UtcNow.AddSeconds(45)
while (-not (Test-Path (Join-Path $output 'finish'))) {
    if ([DateTime]::UtcNow -ge $until) { throw 'Timed out waiting for fixture release.' }
    Start-Sleep -Milliseconds 100
}
'@ | Set-Content -Path $writer
    Start-FixtureTask $clientTask 'NT AUTHORITY\LOCAL SERVICE' $client
    $marker = Join-Path $output 'admitted'
    Wait-Fixture { Test-Path $marker } 'LocalService build was never admitted.'
    $status = Invoke-DevHost @('disk-budget', 'status') | ConvertFrom-Json
    if ($status.Status.leases -ne 1) { throw 'The LocalService writer is not charged.' }

    $buildArgs = 'disk-budget run local -- "{0}" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "{1}" second' -f (Join-Path $PSHOME 'powershell.exe'), $writer
    $overlap = Start-Process -FilePath $exe -ArgumentList $buildArgs -PassThru
    Wait-Fixture { Test-Path (Join-Path $output 'second') } 'The concurrent build was never admitted.'
    $status = Invoke-DevHost @('disk-budget', 'status') | ConvertFrom-Json
    if ($status.Status.leases -ne 2) { throw 'Both overlapping writers must hold reservations.' }

    $shim = Join-Path $root 'cargo.exe'
    Copy-Item -LiteralPath $exe -Destination $shim
    & $shim --version
    if ($LASTEXITCODE) { throw 'The cargo.exe shim did not admit a concurrent local invocation.' }
    New-Item -ItemType File -Path (Join-Path $output 'finish') | Out-Null
    if (-not $overlap.WaitForExit(20000) -or $overlap.ExitCode -ne 0) {
        throw 'The overlapping local test build did not exit normally.'
    }
    Wait-Fixture {
        $status = Invoke-DevHost @('disk-budget', 'status') | ConvertFrom-Json
        $status.Status.leases -eq 0
    } 'The broker did not release both finished jobs.'
    Write-Host 'Verified LocalSystem broker, standard-account ledger denial, concurrent cargo shim admission, and release.'
} finally {
    foreach ($task in @($clientTask, $systemTask)) {
        if (Get-ScheduledTask -TaskName $task -ErrorAction SilentlyContinue) {
            Stop-ScheduledTask -TaskName $task -ErrorAction SilentlyContinue
            Unregister-ScheduledTask -TaskName $task -Confirm:$false
        }
    }
    $brokerLog = Join-Path $root 'broker.log'
    if (Test-Path $brokerLog) { Get-Content $brokerLog }
}
