param(
    [Parameter(Mandatory)] [string] $MainExe,
    [Parameter(Mandatory)] [string] $CandidateExe,
    [Parameter(Mandatory)] [string] $EvidenceRoot
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
$bin = @{
    main = (Resolve-Path -LiteralPath $MainExe).Path
    candidate = (Resolve-Path -LiteralPath $CandidateExe).Path
}
$inheritedXclSettings = @((Get-ChildItem Env: | Where-Object Name -like 'XCL_*').Name)
$root = Join-Path $EvidenceRoot (Get-Date -Format 'yyyyMMdd-HHmmss-fff')
New-Item -ItemType Directory -Path $root | Out-Null
$env:XCL_TEST_TWO_PASS_RAM_PROFILE = '1'
$env:XCL_ADAPTER = '4080'
$env:XCL_NETWORK = '0'
$env:XCL_DXC_DIR = 'C:/Program Files (x86)/Windows Kits/10/bin/10.0.26100.0/x64'
$env:XCL_PSO_DISK_CACHE = '1'
$env:XCL_PSO_PRELOAD = '0'
$env:XCL_ASYNC_PSO = '0'
$testArgs = @('--exact', 'two_pass_compute_ram_hit_profile', '--nocapture', '--test-threads=1')
$hardware = @(nvidia-smi --query-gpu=name,driver_version --format=csv,noheader)
if ($LASTEXITCODE -ne 0 -or ($hardware -join "`n") -notmatch 'GeForce RTX 4080,') {
    throw 'NVIDIA driver or RTX 4080 could not be verified'
}
$caseNumber = 0
$results = foreach ($variant in @('main', 'candidate', 'candidate', 'main', 'main', 'candidate')) {
    $caseNumber++
    $caseRoot = Join-Path $root "$caseNumber-$variant"
    New-Item -ItemType Directory -Path $caseRoot | Out-Null
    $env:XCL_DXIL_CACHE = Join-Path $caseRoot 'dxil'
    $env:XCL_DATA_ROOT = Join-Path $caseRoot 'data'
    $stdout = Join-Path $caseRoot 'stdout.log'
    $stderr = Join-Path $caseRoot 'stderr.log'
    & $bin[$variant] @testArgs >$stdout 2>$stderr
    $exit = $LASTEXITCODE
    $out = Get-Content -LiteralPath $stdout -Raw
    $err = Get-Content -LiteralPath $stderr -Raw
    if ($exit -ne 0 -or $out -notmatch 'profile adapter: .*GeForce RTX 4080\b' -or
        $out -notmatch 'test result: ok\. 1 passed; 0 failed; 0 ignored;' -or
        $out -notmatch 'profile readback: step=0 MS=red GS=green' -or
        $out -notmatch 'profile readback: step=32 MS=red GS=green' -or
        "$out`n$err" -match '(?im)^SKIPPED\b|skipping') {
        throw "Unexpected test outcome in $caseRoot (exit $exit)"
    }
    $rows = foreach ($match in [regex]::Matches($err, '(?m)^two-pass-ram kind=(MS|GS) dxil=(\d+) input_blob=(\d+) source=(cold|ram|disk) reused=(true|false) disk_path=(true|false) path_ns=(\d+) pre_driver_ns=(\d+) hr=(0x[\da-f]+)\r?$')) {
        [pscustomobject]@{
            Kind = $match.Groups[1].Value
            Dxil = [int]$match.Groups[2].Value
            InputBlob = [int]$match.Groups[3].Value
            Source = $match.Groups[4].Value
            Reused = $match.Groups[5].Value -eq 'true'
            DiskPath = $match.Groups[6].Value -eq 'true'
            PathNs = [long]$match.Groups[7].Value
            PreDriverNs = [long]$match.Groups[8].Value
            Hr = $match.Groups[9].Value
        }
    }
    if ($rows.Count -ne 66 -or @($rows | Where-Object Hr -ne '0x0').Count) {
        throw "Expected 66 successful compute-pass creates in $caseRoot"
    }
    $summary = foreach ($kind in @('MS', 'GS')) {
        $kindRows = @($rows | Where-Object Kind -eq $kind)
        $cold = $kindRows[0]
        $warm = @($kindRows | Select-Object -Skip 1)
        $warmWithPath = @($warm | Where-Object DiskPath)
        $warmDxil = @($warm.Dxil | Sort-Object -Unique)
        if ($warm.Count -ne 32 -or $cold.Source -ne 'cold' -or $cold.Reused -or !$cold.DiskPath -or $cold.InputBlob -ne 0 -or
            @($warm | Where-Object { $_.Source -ne 'ram' -or !$_.Reused -or $_.InputBlob -le 0 }).Count -or
            $warmDxil.Count -ne 1 -or $warmDxil[0] -ne $cold.Dxil -or
            $warmWithPath.Count -ne $(if ($variant -eq 'main') { 32 } else { 0 })) {
            throw "Missing cold/RAM-hit comparison for $kind in $caseRoot"
        }
        [pscustomobject]@{
            Kind = $kind
            DxilBytesPerCreate = $cold.Dxil
            RamHits = $warm.Count
            BytesAcceptedFromRam = ($warm.InputBlob | Measure-Object -Sum).Sum
            WarmDiskPaths = $warmWithPath.Count
            WarmDxilBytesHashedForDisk = $warmWithPath.Count * $cold.Dxil
            WarmPathNs = @($warm.PathNs)
            WarmPreDriverNs = @($warm.PreDriverNs)
        }
    }
    $files = @(Get-ChildItem -LiteralPath (Join-Path $env:XCL_DXIL_CACHE 'driver') -Filter '*.pso' -File | Sort-Object Name)
    if ($files.Count -ne 4 -or @($files | Where-Object Length -le 0).Count) {
        throw "Expected two compute and two graphics blobs in $caseRoot"
    }
    $rows | Export-Csv -NoTypeInformation -LiteralPath (Join-Path $caseRoot 'trace.csv')
    [pscustomobject]@{
        Case = $caseNumber
        Variant = $variant
        ExeSha256 = (Get-FileHash -LiteralPath $bin[$variant]).Hash
        Status = '1 passed, 0 failed, 0 ignored, 0 skipped'
        ReadbackChecks = 4
        DriverFiles = @($files.Name)
        Summary = @($summary)
    }
    Write-Host "Verified $caseNumber-$variant (64 RAM hits, 4 readbacks)"
}
if (@($results | ForEach-Object { $_.DriverFiles -join ' ' } | Sort-Object -Unique).Count -ne 1) {
    throw 'Cold driver filenames differ between runs'
}
[ordered]@{
    GPU = $hardware
    InheritedXclSettings = $inheritedXclSettings
    TestArgs = $testArgs
    Environment = [ordered]@{
        XCL_TEST_TWO_PASS_RAM_PROFILE = '1'
        XCL_ADAPTER = '4080'
        XCL_NETWORK = '0'
        XCL_DXC_DIR = $env:XCL_DXC_DIR
        XCL_PSO_DISK_CACHE = '1'
        XCL_PSO_PRELOAD = '0'
        XCL_ASYNC_PSO = '0'
        XCL_DXIL_CACHE = '<fresh case>/dxil'
        XCL_DATA_ROOT = '<fresh case>/data'
    }
    Runs = @($results)
} | ConvertTo-Json -Depth 7 | Set-Content -LiteralPath (Join-Path $root 'receipt.json') -Encoding utf8
Write-Host "Receipt $root/receipt.json"
