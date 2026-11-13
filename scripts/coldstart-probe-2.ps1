# Arbor's cold-start probes, round 2 (Windows): where Rune's GPU setup spends its time on this
# machine. Downloads coldstart-probes-2-windows.zip into a new folder in %TEMP% and:
#   1. runs gpu_probe.exe (a console program) in each mode, twice (cold, then again): Rune's own
#      window path phase by phase, then plain wgpu on DX12 and on Vulkan, per GPU adapter;
#   2. launches arbor-base.exe (coldstart-probe-1's: v0.1.0-alpha.4's startup), arbor.exe (this
#      round's fixes) and rune-hello.exe in turn, 5 times each after one untimed launch, and
#      prints the median from process creation to the first frame, with Arbor's splits.
# Nothing is installed; no admin needed; Windows PowerShell 5.1 and PowerShell 7 alike:
#
#   irm https://raw.githubusercontent.com/smrdotgg/arbor/coldstart-probe-2/scripts/coldstart-probe-2.ps1 | iex
#
# ARBOR_PROBE_ZIP=<url or local path> and ARBOR_PROBE_RUNS=<n> override the zip and the count.

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$url = 'https://github.com/smrdotgg/arbor/releases/download/coldstart-probe-2/coldstart-probes-2-windows.zip'
if ($env:ARBOR_PROBE_ZIP) { $url = $env:ARBOR_PROBE_ZIP }
$runs = 5
if ($env:ARBOR_PROBE_RUNS) { $runs = [int]$env:ARBOR_PROBE_RUNS }

$dir = Join-Path $env:TEMP ('arbor-coldstart2-' + (Get-Date -Format 'yyyyMMdd-HHmmss'))
New-Item -ItemType Directory -Path $dir | Out-Null
$zip = Join-Path $dir 'probes.zip'
if (Test-Path -LiteralPath $url) { Copy-Item -LiteralPath $url $zip } else { Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing }
Expand-Archive -Path $zip -DestinationPath $dir

try {
    Write-Host ('GPU: ' + ((Get-CimInstance Win32_VideoController | ForEach-Object { $_.Name + ' (' + $_.DriverVersion + ')' }) -join ' + '))
} catch {
    Write-Host 'GPU: (unknown)'
}

# 1. gpu_probe, each mode in its own process, twice.
$modes = @('rune', 'dx12-fxc', 'vulkan')
if (Test-Path -LiteralPath (Join-Path $dir 'dxcompiler.dll')) { $modes += 'dx12-dxc' }
foreach ($m in $modes) {
    foreach ($pass in 'cold', 'again') {
        $out = Join-Path $dir "gpu-$m-$pass.txt"
        Write-Host ''
        Write-Host "== gpu_probe $m ($pass)"
        try {
            $p = Start-Process -FilePath (Join-Path $dir 'gpu_probe.exe') -ArgumentList $m -WorkingDirectory $dir -PassThru -NoNewWindow -RedirectStandardOutput $out -RedirectStandardError "$out.err"
            if (-not $p.WaitForExit(60000)) { try { $p.Kill() } catch { }; Write-Host 'timed out after 60 s' }
        } catch {
            Write-Host "failed: $($_.Exception.Message)"
        }
        foreach ($f in $out, "$out.err") {
            if (Test-Path -LiteralPath $f) { Get-Content -Path $f | ForEach-Object { Write-Host "  $_" } }
        }
    }
}

# 2. Process creation to first frame, alternating; one log per exe (arbor-base.exe and arbor.exe
# both call their lines "arbor").
$names = 'arbor-base', 'arbor', 'rune-hello'
# The caller's ARBOR_TIMING_FILE and ARBOR_EXIT_AFTER_FIRST_FRAME, put back however this ends
# (environment variables outlive the script in the caller's session, `irm | iex` or not).
$savedEnv = @{}
foreach ($k in 'ARBOR_TIMING_FILE', 'ARBOR_EXIT_AFTER_FIRST_FRAME') { $savedEnv[$k] = [Environment]::GetEnvironmentVariable($k) }
function Restore-ProbeEnv {
    foreach ($k in $savedEnv.Keys) {
        if ($null -eq $savedEnv[$k]) { Remove-Item "Env:$k" -ErrorAction SilentlyContinue } else { Set-Item "Env:$k" $savedEnv[$k] }
    }
}
try {
    $env:ARBOR_EXIT_AFTER_FIRST_FRAME = '1'
    function Launch([string]$n, [string]$file) {
        $env:ARBOR_TIMING_FILE = $file
        try {
            $p = Start-Process -FilePath (Join-Path $dir "$n.exe") -WorkingDirectory $HOME -PassThru
            if (-not $p.WaitForExit(30000)) { try { $p.Kill() } catch { }; Add-Content -Path $file -Value "timeout" }
        } catch {
            Add-Content -Path $file -Value "failed: $($_.Exception.Message)"
        }
        Start-Sleep -Milliseconds 300
    }
    foreach ($n in $names) { Launch $n (Join-Path $dir "first-$n.txt") }
    for ($i = 1; $i -le $runs; $i++) { foreach ($n in $names) { Launch $n (Join-Path $dir "timing-$n.txt") } }
} finally {
    Restore-ProbeEnv
}

function Median($v) {
    $s = @($v | Sort-Object)
    if ($s.Count -eq 0) { return 'n/a' }
    if ($s.Count % 2) { return $s[($s.Count - 1) / 2] }
    return [math]::Round(($s[$s.Count / 2 - 1] + $s[$s.Count / 2]) / 2, 2)
}
function Lines([string]$file) {
    if (Test-Path -LiteralPath $file) { @(Get-Content -Path $file) } else { @() }
}
# The probes' line: "<name> <ms> from=...".
function Totals($from) {
    @($from | Where-Object { $_ -match '^[a-z0-9-]+ [0-9.]+ from=' } | ForEach-Object { [double](($_ -split ' ')[1]) })
}
$rows = foreach ($n in $names) {
    $f = Totals (Lines (Join-Path $dir "first-$n.txt"))
    $t = Totals (Lines (Join-Path $dir "timing-$n.txt"))
    $firstMs = 'n/a'
    if ($f.Count) { $firstMs = $f[0] }
    [pscustomobject]@{ name = $n; first_ms = $firstMs; median_ms = (Median $t); runs = $t.Count }
}
Write-Host ''
Write-Host ($rows | Format-Table -AutoSize | Out-String)
Write-Host "Splits (ms from the top of main; before_main_ms: process creation to main):"
foreach ($n in $names) {
    Lines (Join-Path $dir "timing-$n.txt") | Where-Object { $_ -like 'arbor-timing first-frame*' -or $_ -like 'rune-hello *' -or $_ -like 'timeout*' -or $_ -like 'failed*' } | ForEach-Object { Write-Host "$n  $_" }
}
Write-Host "Folder: $dir"
