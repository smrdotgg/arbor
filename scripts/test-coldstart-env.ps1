# Checks that the cold-start scripts put back the caller's ARBOR_TIMING_FILE and
# ARBOR_EXIT_AFTER_FIRST_FRAME, after a normal run and after one that fails. Needs only
# PowerShell 7 (any OS; the "exes" are stand-in scripts) or Windows PowerShell 5.1:
#
#   pwsh -NoProfile -File scripts/test-coldstart-env.ps1

$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$tmp = Join-Path ([IO.Path]::GetTempPath()) ('arbor-env-test-' + [guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
if (-not $env:TEMP) { $env:TEMP = $tmp }
$failures = 0

# A stand-in for an exe: writes a probe line to $ARBOR_TIMING_FILE and exits.
$fake = Join-Path $tmp 'fake.exe'
if ($IsWindows -or $PSVersionTable.PSEdition -eq 'Desktop') {
    $fake = Join-Path $tmp 'fake.cmd'
    Set-Content -Path $fake -Value '@echo arbor 1.00 from=process epoch_us=1>> "%ARBOR_TIMING_FILE%"'
} else {
    Set-Content -Path $fake -Value "#!/bin/sh`necho 'arbor 1.00 from=process epoch_us=1' >> `"`$ARBOR_TIMING_FILE`""
    chmod +x $fake
}

function Check([string]$name, [scriptblock]$run) {
    $env:ARBOR_TIMING_FILE = 'caller-file'
    Remove-Item Env:ARBOR_EXIT_AFTER_FIRST_FRAME -ErrorAction SilentlyContinue
    try { & $run *>$null } catch { }
    $ok = $env:ARBOR_TIMING_FILE -eq 'caller-file' -and $null -eq $env:ARBOR_EXIT_AFTER_FIRST_FRAME
    Write-Host ("{0}: {1}" -f $(if ($ok) { 'ok  ' } else { 'FAIL' }), $name)
    if (-not $ok) { $script:failures++ }
}

Check 'coldstart-ab, a normal run' { & (Join-Path $here 'coldstart-ab.ps1') -Runs 1 $fake }
Check 'coldstart-ab, an exe that is not there' { & (Join-Path $here 'coldstart-ab.ps1') -Runs 1 (Join-Path $tmp 'missing.exe') }

# coldstart-probe-2 on a zip of stand-ins named as its exes (gpu_probe's output is ignored).
$zipDir = Join-Path $tmp 'zip'
New-Item -ItemType Directory -Path $zipDir | Out-Null
foreach ($n in 'gpu_probe', 'arbor-base', 'arbor', 'rune-hello') { Copy-Item $fake (Join-Path $zipDir ("$n.exe")) }
$zip = Join-Path $tmp 'probes.zip'
Compress-Archive -Path (Join-Path $zipDir '*') -DestinationPath $zip
$env:ARBOR_PROBE_ZIP = $zip
$env:ARBOR_PROBE_RUNS = '1'
Check 'coldstart-probe-2, a normal run' { & (Join-Path $here 'coldstart-probe-2.ps1') }
Remove-Item Env:ARBOR_PROBE_ZIP, Env:ARBOR_PROBE_RUNS

Remove-Item -Recurse -Force $tmp
if ($failures) { throw "$failures check(s) failed" }
