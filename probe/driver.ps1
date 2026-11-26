$ps = "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe"
$script0 = Get-Content -Raw "$PSScriptRoot\probe.ps1"
$sysm = "$env:SystemRoot\System32\WindowsPowerShell\v1.0\Modules"
$utility = "$sysm\Microsoft.PowerShell.Utility\Microsoft.PowerShell.Utility.psd1"
"runner PSModulePath = $env:PSModulePath"
$modes = @(
  @{n='runnerPSMP'; psmp=$env:PSModulePath; vars=@()},
  @{n='sysPSMP'; psmp=$sysm; vars=@()},
  @{n='sysPSMP+TEMP,TMP'; psmp=$sysm; vars=@('TEMP','TMP')},
  @{n='sysPSMP+USERPROFILE'; psmp=$sysm; vars=@('USERPROFILE')},
  @{n='sysPSMP+LOCALAPPDATA'; psmp=$sysm; vars=@('LOCALAPPDATA')},
  @{n='sysPSMP+userdirs'; psmp=$sysm; vars=@('TEMP','TMP','USERPROFILE','LOCALAPPDATA','APPDATA')},
  @{n='import'; psmp=$null; vars=@(); import=$true},
  @{n='sysPSMP+import'; psmp=$sysm; vars=@(); import=$true},
  @{n='runnerPSMP'; psmp=$env:PSModulePath; vars=@()},
  @{n='sysPSMP+TEMP,TMP'; psmp=$sysm; vars=@('TEMP','TMP')}
)
foreach ($m in $modes) {
  $script = $script0
  if ($m.import) { $script = "`$PSModuleAutoLoadingPreference = 'None'; Import-Module '$utility'`n" + $script0 }
  $psi = New-Object Diagnostics.ProcessStartInfo $ps
  foreach ($a in '-NoLogo','-NoProfile','-NonInteractive','-Command',$script) { $psi.ArgumentList.Add($a) }
  $psi.UseShellExecute = $false; $psi.RedirectStandardOutput = $true; $psi.RedirectStandardError = $true; $psi.RedirectStandardInput = $true
  $psi.WorkingDirectory = Split-Path $ps
  $psi.Environment.Clear(); $psi.Environment['SystemRoot'] = $env:SystemRoot
  if ($m.psmp) { $psi.Environment['PSModulePath'] = $m.psmp }
  foreach ($k in $m.vars) { $psi.Environment[$k] = [Environment]::GetEnvironmentVariable($k) }
  $w = [Diagnostics.Stopwatch]::StartNew()
  $p = [Diagnostics.Process]::Start($psi); $p.StandardInput.Close()
  $o = $p.StandardOutput.ReadToEnd(); $e = $p.StandardError.ReadToEnd(); $p.WaitForExit()
  "=== mode=$($m.n) exit=$($p.ExitCode) wall=$([math]::Round($w.Elapsed.TotalSeconds,2))s"; if ($e) { "stderr: $($e.Substring(0,[math]::Min(300,$e.Length)))" }
}
