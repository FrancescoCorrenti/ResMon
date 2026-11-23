# Runs ResMon's "perf" selftest (tab 10 s -> full view 20 s -> tab 15 s) and samples the Flet client's
# private RAM, shared GPU memory and GPU 3D use every second. Prints the mean / peak per phase.
# Usage: pwsh -File tools\perf.ps1 [treemap,table,spark,cores]   (parts to switch off; closes any running ResMon)
param([string]$Off = "")
$root = Split-Path $PSScriptRoot
$env:RESMON_PERF_OFF = $Off
$base = (Get-Command python).Source | Split-Path   # base interpreter, as start.pyw expects
Get-CimInstance Win32_Process | Where-Object { $_.CommandLine -match 'ResMon' -and $_.Name -match 'python' } |
    ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
Stop-Process -Name flet -Force -ErrorAction SilentlyContinue
Start-Sleep 1
$env:RESMON_TEST = 'perf'
$p = Start-Process "$base\python.exe" -ArgumentList "`"$root\start.pyw`"" -WorkingDirectory $root -PassThru -WindowStyle Hidden
Remove-Item Env:RESMON_TEST
$samples = @()
while (-not $p.HasExited) {
    $fl = Get-Process flet -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($fl) {
        $c = (Get-Counter "\GPU Process Memory(pid_$($fl.Id)*)\Shared Usage",
            "\GPU Engine(pid_$($fl.Id)*engtype_3D)\Utilization Percentage" -ErrorAction SilentlyContinue).CounterSamples
        $samples += [pscustomobject]@{
            t    = [DateTimeOffset]::Now.ToUnixTimeSeconds()
            ram  = $fl.PrivateMemorySize64 / 1MB
            gmem = ($c | Where-Object Path -match 'shared usage' | Measure-Object CookedValue -Sum).Sum / 1MB
            g3d  = ($c | Where-Object Path -match 'utilization' | Measure-Object CookedValue -Sum).Sum
        }
    }
    Start-Sleep -Milliseconds 900
}
Stop-Process -Name flet -Force -ErrorAction SilentlyContinue
$phases = Get-Content "$root\docs\selftest\selftest.log" | Where-Object { $_ -match 'phase (\S+) at (\d+)' } |
    ForEach-Object { if ($_ -match 'phase (\S+) at (\d+)') { [pscustomobject]@{ name = $Matches[1]; t = [long]$Matches[2] } } }
for ($i = 0; $i -lt $phases.Count; $i++) {
    $a = $phases[$i].t + 2   # skip the transition itself
    $b = if ($i + 1 -lt $phases.Count) { $phases[$i + 1].t } else { [long]::MaxValue }
    $s = $samples | Where-Object { $_.t -ge $a -and $_.t -lt $b }
    if (-not $s) { continue }
    '{0,-10} RAM {1,5:N0} MB (peak {2,5:N0})  GPU mem {3,5:N0} MB (peak {4,5:N0})  GPU 3D {5,5:N1}% (peak {6,5:N1}%)' -f $phases[$i].name,
        ($s | Measure-Object ram -Average).Average, ($s | Measure-Object ram -Maximum).Maximum,
        ($s | Measure-Object gmem -Average).Average, ($s | Measure-Object gmem -Maximum).Maximum,
        ($s | Measure-Object g3d -Average).Average, ($s | Measure-Object g3d -Maximum).Maximum
}
