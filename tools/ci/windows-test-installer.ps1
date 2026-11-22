Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$script:payloadNames = @('devworks-desktop.exe', 'devworks-pdf-worker.exe', 'pdf-notices.zip')
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
    if ($env:DW416_ARCHIVE_KEY -cnotmatch '^[0-9a-f]{64}$') { throw 'Archive key unavailable' }
    Assert-ExactFiles $Directory $Names
    Push-Location -LiteralPath $Directory
    try {
        & 7z.exe a -t7z -mhe=on -mx=0 -bd "-p$env:DW416_ARCHIVE_KEY" $Output @Names
        if ($LASTEXITCODE -ne 0) { throw 'Action artifact encryption failed' }
    } finally {
        Pop-Location
    }
}

function Expand-TestArtifact([string]$Archive, [string]$ExpectedHash, [string]$Destination, [string[]]$Names) {
    if ($env:DW416_ARCHIVE_KEY -cnotmatch '^[0-9a-f]{64}$' -or
        $ExpectedHash -cnotmatch '^[0-9a-f]{64}$' -or (Get-Sha256 $Archive) -cne $ExpectedHash) {
        throw 'Action artifact identity is invalid'
    }
    New-Item -ItemType Directory -Path $Destination | Out-Null
    # Flat extraction keeps archive member paths out of the filesystem layout.
    & 7z.exe e -bd "-p$env:DW416_ARCHIVE_KEY" "-o$Destination" $Archive
    if ($LASTEXITCODE -ne 0) { throw 'Action artifact decryption failed' }
    Assert-ExactFiles $Destination $Names
}

function Assert-UnsignedX64([string]$Path) {
    $reader = [IO.BinaryReader]::new([IO.File]::OpenRead($Path))
    try {
        if ($reader.ReadUInt16() -ne 0x5a4d) { throw 'Missing MZ signature' }
        $reader.BaseStream.Position = 0x3c
        $reader.BaseStream.Position = $reader.ReadUInt32()
        if ($reader.ReadUInt32() -ne 0x4550 -or $reader.ReadUInt16() -ne 0x8664) {
            throw "Expected an x64 PE file at $Path"
        }
    } finally {
        $reader.Dispose()
    }
    if ((Get-AuthenticodeSignature -LiteralPath $Path).Status -ne 'NotSigned') {
        throw "Expected unsigned input at $Path"
    }
}

function Get-VerifiedTestSignature([string]$Path) {
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
    $kit = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
    $signTool = Get-ChildItem -LiteralPath $kit -Filter signtool.exe -Recurse |
        Where-Object { $_.Directory.Name -eq 'x64' -and $_.Directory.Parent.Name -match '^\d+(\.\d+){3}$' } |
        Sort-Object { [version]$_.Directory.Parent.Name } -Descending |
        Select-Object -First 1
    if (-not $signTool) { throw 'Windows SDK SignTool unavailable' }
    $report = & $signTool.FullName verify /pa /all /tw /v $Path 2>&1
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
    & (Join-Path $env:WIX 'bin\candle.exe') -nologo -arch x64 "-dPayload=$Payload" -out $object $recipe
    if ($LASTEXITCODE -ne 0) { throw 'WiX recipe compilation failed' }
    & (Join-Path $env:WIX 'bin\light.exe') -nologo -spdb -out $Installer $object
    if ($LASTEXITCODE -ne 0) { throw 'WiX installer linking or validation failed' }
    Remove-Item -LiteralPath $object
    $installerApi = New-Object -ComObject WindowsInstaller.Installer
    $database = $installerApi.OpenDatabase($Installer, 0)
    $files = $database.OpenView('SELECT FileName FROM File')
    $files.Execute()
    $names = @()
    while ($record = $files.Fetch()) { $names += ($record.StringData(1) -split '\|')[-1] }
    $files.Close()
    if (Compare-Object ($names | Sort-Object) ($script:payloadNames | Sort-Object) -CaseSensitive) {
        throw 'The MSI carries an unexpected file inventory'
    }
    $upgrade = $database.OpenView('SELECT Cabinet FROM Media')
    $upgrade.Execute()
    $media = $upgrade.Fetch()
    if (-not $media -or $media.StringData(1) -notlike '#*' -or $upgrade.Fetch()) {
        throw 'The MSI must have one embedded CAB'
    }
    $upgrade.Close()
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
    if ($link.TargetPath -ne (Join-Path $Layout.installed 'devworks-desktop.exe') -or
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
