# Checks that the cold-start scripts put back the caller's SATCHEL_TIMING_FILE and
# SATCHEL_EXIT_AFTER_FIRST_FRAME (coldstart-probe-2: ARBOR_*, the names its published zip's exes
# read), after a normal run and after one that fails. Needs only PowerShell 7 (any OS; the "exes"
# are stand-in scripts) or Windows PowerShell 5.1:
#
#   pwsh -NoProfile -File scripts/test-coldstart-env.ps1

$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$tmp = Join-Path ([IO.Path]::GetTempPath()) ('satchel-env-test-' + [guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
if (-not $env:TEMP) { $env:TEMP = $tmp }
$failures = 0

# A stand-in for an exe: writes a probe line to $<prefix>_TIMING_FILE and exits.
function New-Fake([string]$prefix) {
    $name = $prefix.ToLower()
    $fake = Join-Path $tmp "fake-$prefix.exe"
    if ($IsWindows -or $PSVersionTable.PSEdition -eq 'Desktop') {
        $fake = Join-Path $tmp "fake-$prefix.cmd"
        Set-Content -Path $fake -Value "@echo $name 1.00 from=process epoch_us=1>> ""%${prefix}_TIMING_FILE%"""
    } else {
        Set-Content -Path $fake -Value "#!/bin/sh`necho '$name 1.00 from=process epoch_us=1' >> `"`$${prefix}_TIMING_FILE`""
        chmod +x $fake
    }
    $fake
}
$fake = New-Fake 'SATCHEL'

function Check([string]$name, [scriptblock]$run, [string]$prefix = 'SATCHEL') {
    $file = "Env:${prefix}_TIMING_FILE"
    $exit = "Env:${prefix}_EXIT_AFTER_FIRST_FRAME"
    Set-Item $file 'caller-file'
    Remove-Item $exit -ErrorAction SilentlyContinue
    try { & $run *>$null } catch { }
    $ok = (Get-Item $file -ErrorAction SilentlyContinue).Value -eq 'caller-file' -and -not (Test-Path $exit)
    Write-Host ("{0}: {1}" -f $(if ($ok) { 'ok  ' } else { 'FAIL' }), $name)
    if (-not $ok) { $script:failures++ }
}

Check 'coldstart-ab, a normal run' { & (Join-Path $here 'coldstart-ab.ps1') -Runs 1 $fake }
Check 'coldstart-ab, an exe that is not there' { & (Join-Path $here 'coldstart-ab.ps1') -Runs 1 (Join-Path $tmp 'missing.exe') }

# coldstart-probe-2 on a zip of stand-ins named as its exes (gpu_probe's output is ignored).
$zipDir = Join-Path $tmp 'zip'
New-Item -ItemType Directory -Path $zipDir | Out-Null
$oldFake = New-Fake 'ARBOR'
foreach ($n in 'gpu_probe', 'arbor-base', 'arbor', 'rune-hello') { Copy-Item $oldFake (Join-Path $zipDir ("$n.exe")) }
$zip = Join-Path $tmp 'probes.zip'
Compress-Archive -Path (Join-Path $zipDir '*') -DestinationPath $zip
$env:SATCHEL_PROBE_ZIP = $zip
$env:SATCHEL_PROBE_RUNS = '1'
Check 'coldstart-probe-2, a normal run' { & (Join-Path $here 'coldstart-probe-2.ps1') } 'ARBOR'
Remove-Item Env:SATCHEL_PROBE_ZIP, Env:SATCHEL_PROBE_RUNS

Remove-Item -Recurse -Force $tmp
if ($failures) { throw "$failures check(s) failed" }
