# Run ResMon as administrator without a UAC prompt every time.
#
# - Registers a scheduled task "ResMon" (highest privileges, started on demand) for the signed-in user, and
#   points a Desktop shortcut at it. Afterwards `schtasks /run /tn ResMon` starts ResMon elevated from any
#   process, and "Start with Windows" in ResMon uses the same task.
# - Adds that user to "Performance Log Users", so per-process network also works when ResMon is started
#   without elevation (takes effect at the next sign-in).
#
# Usage, from an elevated PowerShell:
#   powershell -ExecutionPolicy Bypass -File tools\install-admin.ps1 [-Exe path\to\resmon.exe] [-User DOMAIN\name]
# -Exe defaults to the build next to this script (target\release\resmon.exe).
# -User defaults to the user signed in at the console (the owner of explorer.exe), which is not the same as
# the account that elevated when a standard user typed an administrator's password.
param(
    [string]$Exe = (Join-Path $PSScriptRoot '..\target\release\resmon.exe'),
    [string]$User
)
$ErrorActionPreference = 'Stop'
if (-not ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw "Run this from an elevated PowerShell (it registers a task with highest privileges)."
}
if (-not (Test-Path $Exe)) { throw "ResMon not found at ${Exe}: build it first (cargo build --release) or pass -Exe." }
$Exe = (Resolve-Path $Exe).Path
if (-not $User) {
    $owner = Get-CimInstance Win32_Process -Filter "Name='explorer.exe'" | Select-Object -First 1 | Invoke-CimMethod -MethodName GetOwner
    $User = if ($owner -and $owner.User) { "$($owner.Domain)\$($owner.User)" } else { "$env:USERDOMAIN\$env:USERNAME" }
}

$action = New-ScheduledTaskAction -Execute $Exe -WorkingDirectory (Split-Path $Exe)
$principal = New-ScheduledTaskPrincipal -UserId $User -LogonType Interactive -RunLevel Highest
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew
Register-ScheduledTask -TaskName 'ResMon' -Action $action -Principal $principal -Settings $settings -Force | Out-Null

# "Performance Log Users" by its well-known SID: the group name is translated on non-English Windows
try {
    Add-LocalGroupMember -SID 'S-1-5-32-559' -Member $User
    $group = "added to Performance Log Users (sign out and in once)"
} catch [Microsoft.PowerShell.Commands.MemberExistsException] {
    $group = "already in Performance Log Users"
}

# the shortcut goes on the Desktop of that user, not of the administrator account running this script
$sid = (New-Object Security.Principal.NTAccount($User)).Translate([Security.Principal.SecurityIdentifier]).Value
$profileDir = (Get-CimInstance Win32_UserProfile | Where-Object SID -eq $sid).LocalPath
# the Desktop may be redirected (OneDrive backup): read it from that user's own shell folders
$key = "Registry::HKEY_USERS\$sid\Software\Microsoft\Windows\CurrentVersion\Explorer\User Shell Folders"
$raw = (Get-ItemProperty $key -Name Desktop -ErrorAction SilentlyContinue).Desktop
$desktop = if ($raw -and $profileDir) { $raw -replace '%USERPROFILE%', $profileDir }
           elseif ($profileDir) { Join-Path $profileDir 'Desktop' }
           else { [Environment]::GetFolderPath('Desktop') }
$lnk = Join-Path $desktop 'ResMon.lnk'
$sc = (New-Object -ComObject WScript.Shell).CreateShortcut($lnk)
$sc.TargetPath = "$env:WINDIR\System32\schtasks.exe"
$sc.Arguments = '/run /tn ResMon'
$sc.IconLocation = "$Exe,0"
$sc.WindowStyle = 7  # minimized: no console flash
$sc.Description = 'ResMon resource monitor (administrator)'
$sc.Save()
"Task 'ResMon' registered for $User (highest privileges), $group; shortcut: $lnk"
