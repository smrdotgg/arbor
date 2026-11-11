# Arbor's cold-start probes on Windows: downloads coldstart-probes-windows.zip (win32-hello.exe,
# rune-hello.exe, arbor.exe) into a new folder in %TEMP%, launches each exe 5 times in turn with
# ARBOR_TIMING_FILE set, and prints, per exe, its first launch (a new exe: Windows Defender scans
# it first) and the median of the later ones. Each exe appends "<name> <ms> ..." to the file:
# milliseconds from the process's creation to its first frame (Win32: the end of its first
# WM_PAINT; Rune and Arbor: the first frame submitted), then quits by itself. No admin needed;
# Windows PowerShell 5.1 and PowerShell 7 alike:
#
#   irm https://raw.githubusercontent.com/smrdotgg/arbor/coldstart-probe-1/scripts/coldstart-probe.ps1 | iex
#
# ARBOR_PROBE_ZIP=<url or local path> and ARBOR_PROBE_RUNS=<n> override the zip and the count.

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$url = 'https://github.com/smrdotgg/arbor/releases/download/coldstart-probe-1/coldstart-probes-windows.zip'
if ($env:ARBOR_PROBE_ZIP) { $url = $env:ARBOR_PROBE_ZIP }
$runs = 5
if ($env:ARBOR_PROBE_RUNS) { $runs = [int]$env:ARBOR_PROBE_RUNS }

$dir = Join-Path $env:TEMP ('arbor-coldstart-' + (Get-Date -Format 'yyyyMMdd-HHmmss'))
New-Item -ItemType Directory -Path $dir | Out-Null
$zip = Join-Path $dir 'probes.zip'
if (Test-Path -LiteralPath $url) { Copy-Item -LiteralPath $url $zip } else { Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing }
Expand-Archive -Path $zip -DestinationPath $dir

$log = Join-Path $dir 'timing.txt'
$env:ARBOR_TIMING_FILE = $log
$env:ARBOR_EXIT_AFTER_FIRST_FRAME = '1'
$names = 'win32-hello', 'rune-hello', 'arbor'
foreach ($n in $names) {
    for ($i = 1; $i -le $runs; $i++) {
        $p = Start-Process -FilePath (Join-Path $dir "$n.exe") -WorkingDirectory $HOME -PassThru
        if (-not $p.WaitForExit(30000)) {
            try { $p.Kill() } catch { }
            Add-Content -Path $log -Value "$n timeout"
        }
        Start-Sleep -Milliseconds 300
    }
}
# One more rune-hello, untimed, with Rune's GPU log (stderr, redirected to a file: these exes
# have no console): which adapter it drew with, and why.
$env:ARBOR_TIMING_FILE = Join-Path $dir 'gpu-run.txt'
$env:RUNE_GPU_LOG = '1'
$gpuLog = Join-Path $dir 'rune-gpu.txt'
$p = Start-Process -FilePath (Join-Path $dir 'rune-hello.exe') -WorkingDirectory $HOME -PassThru -RedirectStandardError $gpuLog
if (-not $p.WaitForExit(30000)) { try { $p.Kill() } catch { } }
Remove-Item Env:ARBOR_TIMING_FILE, Env:ARBOR_EXIT_AFTER_FIRST_FRAME, Env:RUNE_GPU_LOG

$lines = @(Get-Content -Path $log)
$rows = foreach ($n in $names) {
    $v = @($lines | Where-Object { $_ -like "$n *" } | ForEach-Object { ($_ -split ' ')[1] })
    $later = @($v | Select-Object -Skip 1 | Where-Object { $_ -match '^[0-9.]+$' } | ForEach-Object { [double]$_ } | Sort-Object)
    $median = 'n/a'
    if ($later.Count % 2) { $median = $later[($later.Count - 1) / 2] }
    elseif ($later.Count) { $median = [math]::Round(($later[$later.Count / 2 - 1] + $later[$later.Count / 2]) / 2, 2) }
    $first = 'n/a'
    if ($v.Count) { $first = $v[0] }
    [pscustomobject]@{ name = $n; first_ms = $first; median_later_ms = $median; runs = $v.Count }
}
Write-Host ''
Write-Host ('GPU: ' + ((Get-CimInstance Win32_VideoController | ForEach-Object { $_.Name }) -join ' + '))
Write-Host ($rows | Format-Table -AutoSize | Out-String)
Write-Host "Arbor's splits (ms from the top of main; before_main_ms is process creation to main):"
$lines | Where-Object { $_ -like 'arbor-timing first-frame*' } | ForEach-Object { Write-Host $_ }
Write-Host "Rune's GPU choice:"
Get-Content -Path $gpuLog -ErrorAction SilentlyContinue | ForEach-Object { Write-Host "  $_" }
Write-Host "Full log: $log"
