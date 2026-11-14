# Cold start A/B on Windows: launches each exe given, in turn, -Runs times (alternating, after
# one untimed warm-up launch each, so Defender's first scan is left out), each with
# SATCHEL_TIMING_FILE and SATCHEL_EXIT_AFTER_FIRST_FRAME, and prints the median of every number in
# its lines: the probes' `<name> <ms>` total (process creation to first frame) and, for Satchel,
# the first-frame splits (`satchel-timing first-frame …`). Used by the "Cold start (Windows)"
# workflow (BENCH.md).
#
#   ./scripts/coldstart-ab.ps1 -Runs 15 base.exe new.exe

param([int]$Runs = 10, [Parameter(ValueFromRemainingArguments)][string[]]$Exes)

$ErrorActionPreference = 'Stop'
$dir = Join-Path $env:TEMP ('satchel-ab-' + (Get-Date -Format 'yyyyMMdd-HHmmss'))
New-Item -ItemType Directory -Path $dir | Out-Null
# The caller's SATCHEL_TIMING_FILE and SATCHEL_EXIT_AFTER_FIRST_FRAME, put back however this ends
# (environment variables outlive the script in the caller's session, `irm | iex` or not).
$savedEnv = @{}
foreach ($k in 'SATCHEL_TIMING_FILE', 'SATCHEL_EXIT_AFTER_FIRST_FRAME') { $savedEnv[$k] = [Environment]::GetEnvironmentVariable($k) }
function Restore-ProbeEnv {
    foreach ($k in $savedEnv.Keys) {
        if ($null -eq $savedEnv[$k]) { Remove-Item "Env:$k" -ErrorAction SilentlyContinue } else { Set-Item "Env:$k" $savedEnv[$k] }
    }
}
try {
    $env:SATCHEL_EXIT_AFTER_FIRST_FRAME = '1'

    function Launch([string]$exe, [string]$log) {
        $env:SATCHEL_TIMING_FILE = $log
        $p = Start-Process -FilePath $exe -WorkingDirectory $HOME -PassThru
        if (-not $p.WaitForExit(30000)) { try { $p.Kill() } catch { } }
        Start-Sleep -Milliseconds 300
    }

    $logs = @{}
    foreach ($e in $Exes) {
        $logs[$e] = Join-Path $dir ([IO.Path]::GetFileNameWithoutExtension($e) + '-' + $logs.Count + '.txt')
        Launch (Resolve-Path $e).Path (Join-Path $dir 'warmup.txt')
    }
    for ($i = 1; $i -le $Runs; $i++) {
        foreach ($e in $Exes) { Launch (Resolve-Path $e).Path $logs[$e] }
    }

    function Median([double[]]$v) {
        $s = @($v | Sort-Object)
        if ($s.Count -eq 0) { return 'n/a' }
        if ($s.Count % 2) { return $s[($s.Count - 1) / 2] }
        return [math]::Round(($s[$s.Count / 2 - 1] + $s[$s.Count / 2]) / 2, 2)
    }

    foreach ($e in $Exes) {
        $lines = @()
        if (Test-Path -LiteralPath $logs[$e]) { $lines = @(Get-Content -Path $logs[$e]) }
        $totals = @($lines | Where-Object { $_ -match '^[a-z0-9-]+ [0-9.]+ from=' } | ForEach-Object { [double](($_ -split ' ')[1]) })
        $out = "$e  runs=$($totals.Count)  total=$(Median $totals)"
        $splits = @($lines | Where-Object { $_ -like 'satchel-timing first-frame*' -or $_ -like 'rune-hello *' })
        foreach ($k in 'before_main_ms', 'init_at_ms', 'setup_at_ms', 'window_at_ms', 'frame_start_ms', 'uncloaked_at_ms', 'gpu_starting_at_ms', 'gpu_drew_at_ms', 'update_ms', 'encode_ms', 'main_ms', 'gpu_starting_ms', 'window_ms', 'gpu_drew_ms') {
            $v = @($splits | ForEach-Object { if ($_ -match " $k=([0-9.]+)") { [double]$Matches[1] } })
            if ($v.Count) { $out += "  $k=$(Median $v)" }
        }
        # Which renderer drew the first frame (Windows: the CPU, before the GPU exists).
        $drawn = @($splits | ForEach-Object { if ($_ -match ' drawn_by=([a-z]+)') { $Matches[1] } })
        if ($drawn.Count) { $out += "  drawn_by=" + (($drawn | Group-Object | ForEach-Object { "$($_.Name):$($_.Count)" }) -join ',') }
        Write-Host $out
    }
} finally {
    Restore-ProbeEnv
}
