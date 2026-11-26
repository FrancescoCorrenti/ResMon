# Measures ResMon's own cost: CPU and memory while docked (60 s) and with the full view open (30 s).
# Usage: pwsh -File tools\perf.ps1   (builds the release binary first; closes any running ResMon)
$root = Split-Path $PSScriptRoot
Push-Location $root
cargo build --release -q
Pop-Location
$exe = Join-Path $root 'target\release\resmon.exe'
$n = (Get-CimInstance Win32_ComputerSystem).NumberOfLogicalProcessors
Stop-Process -Name resmon -Force -ErrorAction SilentlyContinue

function Measure-Phase($label, $secs) {
    Start-Sleep 4   # past start-up and the opening animation
    $p = Get-Process resmon
    $c0 = $p.CPU
    Start-Sleep $secs
    $p.Refresh()
    $core = ($p.CPU - $c0) / $secs * 100
    "{0,-10} CPU {1,5:N2}% of one core ({2:N3}% of the machine)   private {3} MB   working set {4} MB" -f `
        $label, $core, ($core / $n), [int]($p.PrivateMemorySize64 / 1MB), [int]($p.WS / 1MB)
    Stop-Process -Id $p.Id -Force
}

Start-Process $exe
Measure-Phase 'docked' 60
$env:RESMON_OPEN = 'full'
Start-Process $exe
Remove-Item Env:RESMON_OPEN
Measure-Phase 'full view' 30
