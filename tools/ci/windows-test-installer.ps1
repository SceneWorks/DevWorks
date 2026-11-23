Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$script:payloadNames = @('devworks-desktop.exe', 'devworks-test-launcher.exe', 'devworks-pdf-worker.exe', 'pdf-notices.zip')
$script:profileEku = '1.3.6.1.4.1.311.97.840169298.24257481.644551716.202800253'
$script:artifactSigningEku = '1.3.6.1.4.1.311.97.1.0'

function Get-Sha256([string]$Path) {
    (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Assert-ExactFiles([string]$Directory, [string[]]$Names) {
    $items = @(Get-ChildItem -LiteralPath $Directory -Force)
    if ($items.Count -ne $Names.Count) { throw "Unexpected file count in $Directory" }
    foreach ($name in $Names) {
        $item = Get-Item -LiteralPath (Join-Path $Directory $name) -Force
        if ($item.Name -cne $name -or $item.PSIsContainer -or
            ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -or $item.Length -eq 0) {
            throw "Expected a regular nonempty file named $name"
        }
    }
}

function Protect-TestArtifact([string]$Directory, [string]$Output, [string[]]$Names) {
    if ($env:DW421_ARCHIVE_KEY -cnotmatch '^[0-9a-f]{64}$') { throw 'Archive key unavailable' }
    Assert-ExactFiles $Directory $Names
    Push-Location -LiteralPath $Directory
    try {
        & 7z.exe a -t7z -mhe=on -mx=0 -bd "-p$env:DW421_ARCHIVE_KEY" $Output @Names
        if ($LASTEXITCODE -ne 0) { throw 'Action artifact encryption failed' }
    } finally {
        Pop-Location
    }
}

function Expand-TestArtifact([string]$Archive, [string]$ExpectedHash, [string]$Destination, [string[]]$Names) {
    if ($env:DW421_ARCHIVE_KEY -cnotmatch '^[0-9a-f]{64}$' -or
        $ExpectedHash -cnotmatch '^[0-9a-f]{64}$' -or (Get-Sha256 $Archive) -cne $ExpectedHash) {
        throw 'Action artifact identity is invalid'
    }
    New-Item -ItemType Directory -Path $Destination | Out-Null
    # Flat extraction keeps archive member paths out of the filesystem layout.
    & 7z.exe e -bd "-p$env:DW421_ARCHIVE_KEY" "-o$Destination" $Archive
    if ($LASTEXITCODE -ne 0) { throw 'Action artifact decryption failed' }
    Assert-ExactFiles $Destination $Names
}

function Get-TestPeSubsystem([string]$Path, [UInt16]$Machine) {
    $reader = [IO.BinaryReader]::new([IO.File]::OpenRead($Path))
    try {
        if ($reader.ReadUInt16() -ne 0x5a4d) { throw 'Missing MZ signature' }
        $reader.BaseStream.Position = 0x3c
        $header = $reader.ReadUInt32()
        $reader.BaseStream.Position = $header
        if ($reader.ReadUInt32() -ne 0x4550 -or $reader.ReadUInt16() -ne $Machine) {
            throw ("Expected PE machine {0:x4} at {1}" -f $Machine, $Path)
        }
        $optional = $header + 24
        $reader.BaseStream.Position = $optional
        if ($reader.ReadUInt16() -ne 0x20b) { throw "Expected a PE32+ header at $Path" }
        $reader.BaseStream.Position = $optional + 68
        $reader.ReadUInt16()
    } finally {
        $reader.Dispose()
    }
}

function Get-X64PeSubsystem([string]$Path) {
    Get-TestPeSubsystem $Path 0x8664
}

function Assert-UnsignedTestPe([string]$Path, [UInt16]$Machine) {
    $subsystem = Get-TestPeSubsystem $Path $Machine
    if ([IO.Path]::GetFileName($Path) -ceq 'devworks-test-launcher.exe' -and $subsystem -ne 2) {
        throw "Expected a Windows GUI launcher at $Path"
    }
    if ((Get-AuthenticodeSignature -LiteralPath $Path).Status -ne 'NotSigned') {
        throw "Expected unsigned input at $Path"
    }
}

function Assert-UnsignedX64([string]$Path) {
    Assert-UnsignedTestPe $Path 0x8664
}

function New-TestLauncher([string]$Root, [string]$Payload) {
    # MSVC can emit side files. Stage only the executable in the sealed payload.
    $build = New-Item -ItemType Directory -Path (Join-Path $env:RUNNER_TEMP 'launcher-build')
    $launcher = Join-Path $build.FullName 'devworks-test-launcher.exe'
    rustc --edition 2024 --target x86_64-pc-windows-msvc -D warnings -O -C strip=symbols `
        (Join-Path $Root 'packaging/windows/devworks-test-launcher.rs') -o $launcher
    if ($LASTEXITCODE -ne 0) { throw 'Windows Start launcher build failed' }
    Assert-UnsignedX64 $launcher
    Copy-Item -LiteralPath $launcher -Destination $Payload
}

function Get-TestSignTool([ValidateSet('x64','arm64')][string]$Architecture = 'x64') {
    $kit = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
    $versions = @(Get-ChildItem -LiteralPath $kit -Filter signtool.exe -Recurse |
        Where-Object { $_.Directory.Parent.Name -match '^\d+\.\d+\.\d+\.\d+$' } |
        Sort-Object { [version]$_.Directory.Parent.Name } -Descending)
    $signTool = $versions | Where-Object { $_.Directory.Name -eq $Architecture } | Select-Object -First 1
    if (-not $signTool -and $Architecture -eq 'arm64') {
        $signTool = $versions | Where-Object { $_.Directory.Name -eq 'x64' } | Select-Object -First 1
        if ($signTool) { Write-Host 'Using the x64 SDK SignTool to inspect passive ARM64 files' }
    }
    if (-not $signTool) { throw "Windows SDK SignTool unavailable for $Architecture" }
    $signTool.FullName
}

function Get-VerifiedTestSignature([string]$Path, [string]$SignTool) {
    $signature = Get-AuthenticodeSignature -LiteralPath $Path
    if ($signature.Status -ne 'Valid' -or $signature.SignatureType -ne 'Authenticode' -or
        -not $signature.TimeStamperCertificate) {
        throw "Invalid timestamped Authenticode for $Path (status $($signature.Status))"
    }
    $purposes = @($signature.SignerCertificate.Extensions |
        Where-Object { $_ -is [Security.Cryptography.X509Certificates.X509EnhancedKeyUsageExtension] } |
        ForEach-Object { $_.EnhancedKeyUsages | ForEach-Object Value })
    if ($script:profileEku -notin $purposes -or $script:artifactSigningEku -notin $purposes) {
        throw "Unexpected Azure signing profile for $Path"
    }
    if (-not $SignTool) { $SignTool = Get-TestSignTool }
    $report = & $SignTool verify /pa /all /tw /v $Path 2>&1
    $report | Write-Host
    if ($LASTEXITCODE -ne 0) { throw "SignTool verification failed for $Path" }
    [ordered]@{
        profileEku = $script:profileEku
        signer = $signature.SignerCertificate.Subject
        certificate = $signature.SignerCertificate.Thumbprint
        timestampSigner = $signature.TimeStamperCertificate.Subject
        sha256 = Get-Sha256 $Path
    }
}

function New-TestInstaller([string]$Root, [string]$Payload, [string]$Installer) {
    Assert-ExactFiles $Payload $script:payloadNames
    $recipe = Join-Path $Root 'packaging\windows\devworks-test.wxs'
    $object = [IO.Path]::ChangeExtension($Installer, 'wixobj')
    & (Join-Path $env:WIX 'bin\candle.exe') -nologo -arch x64 "-dPayload=$Payload" -out $object $recipe | Write-Host
    if ($LASTEXITCODE -ne 0) { throw 'WiX recipe compilation failed' }
    & (Join-Path $env:WIX 'bin\light.exe') -nologo -spdb -out $Installer $object | Write-Host
    if ($LASTEXITCODE -ne 0) { throw 'WiX installer linking or validation failed' }
    Remove-Item -LiteralPath $object
    $installerApi = New-Object -ComObject WindowsInstaller.Installer
    $database = $installerApi.OpenDatabase($Installer, 0)
    $files = $database.OpenView('SELECT FileName FROM File')
    $null = $files.Execute()
    $names = @()
    while ($record = $files.Fetch()) { $names += ($record.StringData(1) -split '\|')[-1] }
    $null = $files.Close()
    if (Compare-Object ($names | Sort-Object) ($script:payloadNames | Sort-Object) -CaseSensitive) {
        throw 'The MSI carries an unexpected file inventory'
    }
    $streams = $database.OpenView('SELECT Name FROM _Streams')
    $null = $streams.Execute()
    $embedded = @()
    while ($stream = $streams.Fetch()) { $embedded += $stream.StringData(1) }
    $null = $streams.Close()

    $media = $database.OpenView('SELECT Cabinet FROM Media')
    $null = $media.Execute()
    $cabinetCount = 0
    while ($row = $media.Fetch()) {
        $cabinet = $row.StringData(1)
        if (-not $cabinet.StartsWith('#') -or $cabinet.Substring(1) -cnotin $embedded) {
            throw "The MSI media entry '$cabinet' does not reference an embedded cabinet"
        }
        $cabinetCount++
    }
    $null = $media.Close()
    if ($cabinetCount -eq 0) { throw 'The MSI has no embedded cabinets' }
    $cabinetCount
}

function Get-TestInstallLayout {
    @{
        installed = Join-Path $env:LOCALAPPDATA 'DevWorks Test'
        profile = Join-Path $env:LOCALAPPDATA 'DevWorks Test Data'
        shortcut = Join-Path ([Environment]::GetFolderPath('Programs')) 'DevWorks Test.lnk'
        currentUser = 'HKCU:\Software\SceneWorks\DevWorks Test'
        machine = @(
            'HKLM:\Software\SceneWorks\DevWorks Test',
            'HKLM:\Software\WOW6432Node\SceneWorks\DevWorks Test'
        )
    }
}

function Invoke-TestMsi([string]$Installer, [string]$Action, [string]$Log) {
    $process = Start-Process msiexec.exe -Wait -PassThru -ArgumentList @(
        $Action, ('"{0}"' -f $Installer), '/qn', '/norestart', '/l*v', ('"{0}"' -f $Log)
    )
    if ($process.ExitCode -ne 0) {
        Get-Content -LiteralPath $Log -Tail 80 | Write-Host
        throw "msiexec $Action failed with exit code $($process.ExitCode)"
    }
}

function Get-TestProductCode([string]$Installer) {
    $api = New-Object -ComObject WindowsInstaller.Installer
    $database = $api.OpenDatabase($Installer, 0)
    $view = $database.OpenView("SELECT Value FROM Property WHERE Property='ProductCode'")
    $null = $view.Execute()
    $record = $view.Fetch()
    if (-not $record) { throw 'The MSI lacks a product code' }
    $productCode = [string]$record.StringData(1)
    $null = $view.Close()
    $parsed = [guid]::Empty
    if (-not [guid]::TryParseExact($productCode, 'B', [ref]$parsed)) {
        throw 'The MSI product code is invalid'
    }
    $productCode
}

function Get-TestProductContexts([string]$ProductCode) {
    if (-not ('DevWorksInstallerContext' -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;

public static class DevWorksInstallerContext
{
    [DllImport("msi.dll", CharSet = CharSet.Unicode, ExactSpelling = true)]
    private static extern uint MsiGetProductInfoExW(string product, string sid, uint context,
        string property, StringBuilder value, ref uint valueLength);

    public static uint[] Read(string product)
    {
        const uint unknownProduct = 1605;
        var contexts = new List<uint>();
        foreach (uint context in new uint[] { 1, 2, 4 })
        {
            var state = new StringBuilder(16);
            uint length = (uint)state.Capacity;
            uint status = MsiGetProductInfoExW(product, null, context, "State", state, ref length);
            if (status == unknownProduct) continue;
            if (status != 0) throw new Win32Exception((int)status, $"MSI {product} context {context}: {status}");
            if (state.ToString() != "5") throw new InvalidOperationException($"Unexpected MSI state {state}");
            contexts.Add(context);
        }
        return contexts.ToArray();
    }
}
'@
    }
    [DevWorksInstallerContext]::Read($ProductCode)
}

function Confirm-TestInstallScope([string]$Installer, [hashtable]$Layout) {
    Assert-ExactFiles $Layout.installed $script:payloadNames
    $productCode = Get-TestProductCode $Installer
    $contexts = @(Get-TestProductContexts $productCode)
    if ($contexts.Count -ne 1 -or $contexts[0] -ne 2 -or
        (Test-Path -LiteralPath $Layout.machine) -contains $true -or
        (Get-ItemPropertyValue -LiteralPath $Layout.currentUser -Name Installed) -ne 1) {
        throw "Expected just an unmanaged per-user registration for $productCode, found contexts [$contexts]"
    }
    $shell = New-Object -ComObject WScript.Shell
    $link = $shell.CreateShortcut($Layout.shortcut)
    if ($link.TargetPath -ne (Join-Path $Layout.installed 'devworks-test-launcher.exe') -or
        $link.WorkingDirectory.TrimEnd('\') -ne $Layout.installed -or
        $link.Arguments -ne ('--data-dir "{0}"' -f $Layout.profile)) {
        throw 'The Start-menu shortcut does not launch the installed copy with its own profile'
    }
    $productCode
}

function Assert-TestInstallRemoved([hashtable]$Layout, [string]$ProductCode) {
    if ((Test-Path -LiteralPath @($Layout.installed, $Layout.shortcut)) -contains $true -or
        (Get-ItemProperty -LiteralPath $Layout.currentUser -Name Installed,Shortcut -ErrorAction SilentlyContinue) -or
        @(Get-TestProductContexts $ProductCode).Count -ne 0) {
        throw 'The MSI did not remove the test installation and registration'
    }
}
