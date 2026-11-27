#Requires -Version 7.2
<#
.SYNOPSIS
Verifies translated-cache retention across four compiler timestamps on Windows.

.PARAMETER BuildReceipt
JSON for an exact clean release build of 8b957bec6f82495aec940944c6bf47faa02db590.
Include head, clean, executable, executableSha256, and exit. An executable path
relative to the receipt is accepted.

.PARAMETER CorpusRoot
Directory produced by scripts/generate-split-gate-corpus.mjs.

.PARAMETER OutputDirectory
New directory for the private SDK copies, caches, logs, and results.

.PARAMETER SdkDirectory
Windows SDK x64 directory containing dxcompiler.dll and dxil.dll.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string] $BuildReceipt,
    [Parameter(Mandatory)][string] $CorpusRoot,
    [Parameter(Mandatory)][string] $OutputDirectory,
    [Parameter(Mandatory)][string] $SdkDirectory
)

$ErrorActionPreference = 'Stop'
if (-not $IsWindows) { throw 'This verification requires Windows' }

$expectedMain = '8b957bec6f82495aec940944c6bf47faa02db590'
$receiptFile = Get-Item -LiteralPath $BuildReceipt
$receipt = Get-Content -LiteralPath $receiptFile.FullName -Raw | ConvertFrom-Json
if ($receipt.head -ne $expectedMain -or $receipt.clean -isnot [bool] -or -not $receipt.clean -or
    $receipt.exit -ne 0 -or -not $receipt.executable) {
    throw 'Expected a successful release receipt for the clean main revision'
}
$executable = [IO.Path]::GetFullPath($receipt.executable, $receiptFile.DirectoryName)
if ((Get-FileHash -LiteralPath $executable -Algorithm SHA256).Hash -ne $receipt.executableSha256) {
    throw 'Executable does not match the clean main release receipt'
}
$inputRoot = (Resolve-Path -LiteralPath $CorpusRoot).ProviderPath
$corpus = Join-Path $inputRoot 'corpus'
$inputManifest = Get-Content -LiteralPath (Join-Path $inputRoot 'corpus-manifest.json') -Raw | ConvertFrom-Json
$expectedCorpus = '75b27646d15556b9b6ad44198cfeecf8485184d88f87acadb44a346895d77924'
$entries = @($inputManifest.files | Sort-Object name)
if ($inputManifest.upstream -ne $expectedMain -or $inputManifest.shaderCount -ne 160 -or
    $inputManifest.manifestSha256 -ne $expectedCorpus -or $entries.Count -ne 160 -or
    @(Get-ChildItem -LiteralPath $corpus -Force).Count -ne 160) {
    throw 'Unexpected synthetic corpus'
}
$hashLines = for ($i = 0; $i -lt $entries.Count; $i++) {
    $entry = $entries[$i]
    if ($entry.name -cne ('split-{0:D3}.bin' -f ($i + 1))) {
        throw "Unexpected synthetic input name: $($entry.name)"
    }
    $shader = Join-Path $corpus $entry.name
    $hash = (Get-FileHash -LiteralPath $shader -Algorithm SHA256).Hash.ToLowerInvariant()
    if ((Get-Item -LiteralPath $shader).Length -ne $entry.bytes -or
        $hash -ne $entry.sha256) {
        throw "Synthetic input differs: $($entry.name)"
    }
    "$hash  $($entry.name)`n"
}
$corpusBytes = [Text.Encoding]::UTF8.GetBytes(($hashLines -join ''))
$corpusHash = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($corpusBytes)).ToLowerInvariant()
if ($corpusHash -cne $expectedCorpus) {
    throw 'Synthetic corpus hash does not match the pinned fixture'
}

$installedSdk = (Resolve-Path -LiteralPath $SdkDirectory).ProviderPath
$campaignRoot = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputDirectory)
if (Test-Path -LiteralPath $campaignRoot) { throw "Output directory already exists: $campaignRoot" }
$activeSdk = [IO.Directory]::CreateDirectory((Join-Path $campaignRoot 'private-sdk')).FullName
$cacheRoot = [IO.Directory]::CreateDirectory((Join-Path $campaignRoot 'cache')).FullName
$translatedRoot = Join-Path $cacheRoot 'translated/v2'

function Get-LibraryIdentity([string] $Path) {
    $file = Get-Item -LiteralPath $Path
    return [pscustomobject]@{
        name = $file.Name
        bytes = $file.Length
        sha256 = (Get-FileHash -LiteralPath $Path).Hash.ToLowerInvariant()
        lastWriteTimeUtc = $file.LastWriteTimeUtc.ToString('o')
        version = $file.VersionInfo.FileVersion
    }
}

$libraries = @('dxcompiler.dll', 'dxil.dll')
$sdkRecords = foreach ($library in $libraries) {
    $installed = Join-Path $installedSdk $library
    $private = Join-Path $activeSdk $library
    [IO.File]::Copy($installed, $private)
    [IO.File]::SetLastWriteTimeUtc($private, [IO.File]::GetLastWriteTimeUtc($installed))
    $installedInfo = Get-LibraryIdentity $installed
    $privateInfo = Get-LibraryIdentity $private
    if ($installedInfo.sha256 -ne $privateInfo.sha256 -or $installedInfo.lastWriteTimeUtc -ne $privateInfo.lastWriteTimeUtc) {
        throw "Private SDK copy differs: $library"
    }
    [pscustomobject]@{ installed = $installed; private = $private; identity = $installedInfo }
}
$sdkRecords | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $campaignRoot 'sdk-source.json') -Encoding utf8
$privateCompiler = Join-Path $activeSdk 'dxcompiler.dll'
$initialTime = [IO.File]::GetLastWriteTimeUtc($privateCompiler)

function Get-TranslationManifest {
    if (-not (Test-Path -LiteralPath $translatedRoot)) { return }
    Get-ChildItem -LiteralPath $translatedRoot -Recurse -File | Sort-Object FullName | ForEach-Object {
        $file = $_
        $path = [IO.Path]::GetRelativePath($translatedRoot, $file.FullName).Replace('\', '/')
        if ($path -notmatch '^[a-f0-9]{16}/[^/]+\.bin$') { throw "Unexpected translated file: $path" }
        $bytes = [IO.File]::ReadAllBytes($file.FullName)
        $identity = $path.Split('/')[0]
        if ([Text.Encoding]::ASCII.GetString($bytes, 0, 8) -cne 'XCLTRAN2' -or
            [BitConverter]::ToUInt64($bytes, 8).ToString('x16') -cne $identity) {
            throw "Translated record header differs from its directory: $path"
        }
        $sha256 = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($bytes)).ToLowerInvariant()
        # Ignore only the stored identity when comparing the same record between namespaces.
        [Array]::Clear($bytes, 8, 8)
        [pscustomobject]@{
            path = $path
            bytes = $file.Length
            sha256 = $sha256
            contentsWithoutIdentitySha256 = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($bytes)).ToLowerInvariant()
            lastWriteTimeUtc = $file.LastWriteTimeUtc.ToString('o')
        }
    }
}

$cliArgs = @('shaders', $corpus, '--precompile', '--only', 'cs', '--windows', '1', '--split', '1')
$xclEnvironment = [ordered]@{
    XCL_DXC_DIR = $activeSdk
    XCL_DXIL_CACHE = $cacheRoot
    XCL_DIRECT_DXIL = '1'
    XCL_DIRECT_SPLIT_INSTRUCTIONS = '12'
    XCL_TRANSLATED_CACHE = '1'
    XCL_DXIL_DUMP = 'all'
    XCL_DXC_THREADS = '2'
    XCL_PSO_DISK_CACHE = '0'
    XCL_NETWORK = '0'
}
$phases = @(
    @{ name = 'a-initial'; hours = 0; fresh = $true },
    @{ name = 'b-retimed'; hours = 1; fresh = $true },
    @{ name = 'c-retimed'; hours = 2; fresh = $true },
    @{ name = 'd-retimed'; hours = 3; fresh = $true },
    @{ name = 'd-repeat'; hours = 3; fresh = $false },
    @{ name = 'a-rollback'; hours = 0; fresh = $false }
)
$phaseResults = [Collections.Generic.List[object]]::new()
$previous = @{}
$previousNamespaces = @()
$firstHashes = @{}

try {
    foreach ($phase in $phases) {
        $leg = [IO.Directory]::CreateDirectory((Join-Path $campaignRoot $phase.name)).FullName
        $temp = [IO.Directory]::CreateDirectory((Join-Path $leg 'temp')).FullName
        $data = [IO.Directory]::CreateDirectory((Join-Path $leg 'data')).FullName
        $local = [IO.Directory]::CreateDirectory((Join-Path $leg 'local')).FullName
        $targetTime = $initialTime.AddHours($phase.hours)
        [IO.File]::SetLastWriteTimeUtc($privateCompiler, $targetTime)
        $compiler = Get-LibraryIdentity $privateCompiler
        if ($compiler.sha256 -ne $sdkRecords[0].identity.sha256 -or $compiler.lastWriteTimeUtc -cne $targetTime.ToString('o')) {
            throw "Unexpected compiler metadata for $($phase.name)"
        }

        $launch = [Diagnostics.ProcessStartInfo]::new()
        $launch.FileName = $executable
        $launch.WorkingDirectory = $leg
        $launch.UseShellExecute = $false
        $launch.CreateNoWindow = $true
        $launch.RedirectStandardOutput = $true
        $launch.RedirectStandardError = $true
        foreach ($arg in $cliArgs) { $launch.ArgumentList.Add($arg) }
        foreach ($key in @($launch.Environment.Keys | Where-Object { $_ -like 'XCL_*' })) {
            $null = $launch.Environment.Remove($key)
        }
        foreach ($entry in $xclEnvironment.GetEnumerator()) { $launch.Environment[$entry.Key] = $entry.Value }
        $launch.Environment['TEMP'] = $temp
        $launch.Environment['TMP'] = $temp
        $launch.Environment['LOCALAPPDATA'] = $local
        $launch.Environment['XCL_DATA_ROOT'] = $data
        $process = [Diagnostics.Process]::Start($launch)
        try {
            $asyncOut = $process.StandardOutput.ReadToEndAsync()
            $asyncErr = $process.StandardError.ReadToEndAsync()
            if (-not $process.WaitForExit(90000)) {
                $process.Kill($true)
                throw "$($phase.name) exceeded the process watchdog"
            }
            $exitCode = $process.ExitCode
            $stdout = $asyncOut.GetAwaiter().GetResult()
            $stderr = $asyncErr.GetAwaiter().GetResult()
        } finally {
            $process.Dispose()
        }
        [IO.File]::WriteAllText((Join-Path $leg 'stdout.log'), $stdout)
        [IO.File]::WriteAllText((Join-Path $leg 'stderr.log'), $stderr)
        $summary = [regex]::Match($stdout, '(?m)^compiled 160, already cached 0, errors 0, skipped .+? 0, time [^\r\n]+').Value
        if ($exitCode -ne 0 -or -not $summary -or "$stdout$stderr" -match 'pipelines made on the GPU|Creating D3D12|adapter LUID') {
            throw "Unexpected CPU precompile outcome in $($phase.name)"
        }

        $current = @(Get-TranslationManifest)
        $currentMap = @{}
        foreach ($file in $current) { $currentMap[$file.path] = $file }
        foreach ($old in $previous.Values) {
            $kept = $currentMap[$old.path]
            if (-not $kept -or $kept.sha256 -ne $old.sha256 -or $kept.bytes -ne $old.bytes -or
                $kept.lastWriteTimeUtc -cne $old.lastWriteTimeUtc) {
                throw "Existing translated entry changed during $($phase.name): $($old.path)"
            }
        }
        $namespaces = @(Get-ChildItem -LiteralPath $translatedRoot -Directory | Sort-Object Name | ForEach-Object Name)
        $newFiles = @($current | Where-Object { -not $previous.ContainsKey($_.path) })
        $newNamespaces = @($namespaces | Where-Object { $_ -notin $previousNamespaces })
        $expectedNew = if ($phase.fresh) { 160 } else { 0 }
        $expectedNamespaces = if ($phase.fresh) { 1 } else { 0 }
        $dumpDir = Join-Path $temp 'xcl-dxil-dump'
        $dumps = if (Test-Path -LiteralPath $dumpDir) { @(Get-ChildItem -LiteralPath $dumpDir -File -Filter '*.dxil').Count } else { 0 }
        if ($newFiles.Count -ne $expectedNew -or $newNamespaces.Count -ne $expectedNamespaces -or $dumps -ne $expectedNew) {
            throw "Unexpected translated entries or direct dumps in $($phase.name)"
        }
        $rewrittenIdentities = 0
        foreach ($entry in $newFiles) {
            $fileName = $entry.path.Split('/')[1]
            if ($phase.name -eq 'a-initial') {
                $firstHashes[$fileName] = $entry.contentsWithoutIdentitySha256
            } elseif ($firstHashes[$fileName] -eq $entry.contentsWithoutIdentitySha256) {
                $rewrittenIdentities++
            }
        }
        if ($phase.fresh -and $phase.name -ne 'a-initial' -and $rewrittenIdentities -ne $expectedNew) {
            throw "Translated content changed beyond the identity field in $($phase.name)"
        }
        $current | ConvertTo-Json -AsArray | Set-Content -LiteralPath (Join-Path $leg 'translated-manifest.json') -Encoding utf8
        $record = [pscustomobject]@{
            phase = $phase.name
            compiler = $compiler
            namespaces = $namespaces
            newNamespaces = $newNamespaces
            retainedRecords = $previous.Count
            newRecords = $newFiles.Count
            totalRecords = $current.Count
            totalTranslatedBytes = ($current.bytes | Measure-Object -Sum).Sum
            newRecordsMatchingInitialExceptIdentity = $rewrittenIdentities
            directDumps = $dumps
            cliSummary = $summary
            exit = $exitCode
        }
        $phaseResults.Add($record)
        $record | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $leg 'result.json') -Encoding utf8
        $record | ConvertTo-Json -Depth 5 -Compress | Write-Host
        $previous = $currentMap
        $previousNamespaces = $namespaces
    }
} finally {
    [IO.File]::SetLastWriteTimeUtc($privateCompiler, $initialTime)
}

foreach ($library in $sdkRecords) {
    $original = $library.identity | ConvertTo-Json -Compress
    if ((Get-LibraryIdentity $library.installed | ConvertTo-Json -Compress) -cne $original -or
        (Get-LibraryIdentity $library.private | ConvertTo-Json -Compress) -cne $original) {
        throw "SDK metadata or bytes were not preserved: $($library.identity.name)"
    }
}
if (Test-Path -LiteralPath (Join-Path $cacheRoot 'driver')) { throw 'A driver blob cache was unexpectedly created' }
$windows = Get-ItemProperty -LiteralPath 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion'
[ordered]@{
    build = $receipt
    executable = $executable
    os = "$($windows.DisplayVersion) $($windows.CurrentBuildNumber).$($windows.UBR)"
    cpu = (Get-ItemProperty -LiteralPath 'HKLM:\HARDWARE\DESCRIPTION\System\CentralProcessor\0' -Name ProcessorNameString).ProcessorNameString
    corpusManifestSha256 = $corpusHash
    arguments = $cliArgs
    environment = $xclEnvironment
    sdkRestored = $true
    phases = $phaseResults
} | ConvertTo-Json -Depth 7 | Set-Content -LiteralPath (Join-Path $campaignRoot 'result.json') -Encoding utf8
Write-Host "Full receipt: $(Join-Path $campaignRoot 'result.json')"
