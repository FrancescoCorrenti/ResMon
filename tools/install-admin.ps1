# Run ResMon as administrator without a UAC prompt every time.
# Registers a scheduled task "ResMon" (highest privileges, started on demand) and points the Desktop
# shortcut at it. Needs one elevation to register; afterwards `schtasks /run /tn ResMon` starts ResMon
# elevated from any process. "Start with Windows" in ResMon uses the same task when it exists.
# Usage (elevated): pwsh -File tools\install-admin.ps1 [path\to\ResMon.exe]
param([string]$Exe = "$env:LOCALAPPDATA\Programs\ResMon\ResMon.exe")
$ErrorActionPreference = 'Stop'
if (-not ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw "Run this from an elevated PowerShell (it registers a task with highest privileges)."
}
$user = "$env:USERDOMAIN\$env:USERNAME"
$action = New-ScheduledTaskAction -Execute $Exe -WorkingDirectory (Split-Path $Exe)
$principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Highest
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew
Register-ScheduledTask -TaskName 'ResMon' -Action $action -Principal $principal -Settings $settings -Force | Out-Null
$lnk = Join-Path ([Environment]::GetFolderPath('Desktop')) 'ResMon.lnk'
$sc = (New-Object -ComObject WScript.Shell).CreateShortcut($lnk)
$sc.TargetPath = "$env:WINDIR\System32\schtasks.exe"
$sc.Arguments = '/run /tn ResMon'
$sc.IconLocation = "$Exe,0"
$sc.WindowStyle = 7  # minimized: no console flash
$sc.Description = 'ResMon resource monitor (administrator)'
$sc.Save()
"Task 'ResMon' registered for $user (highest privileges); shortcut: $lnk"
