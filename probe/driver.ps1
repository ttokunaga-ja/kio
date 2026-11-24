$ps = "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe"
$script = Get-Content -Raw "$PSScriptRoot\probe.ps1"
foreach ($mode in 'cleared+psmodulepath','cleared+userdirs','cleared+psmodulepath','cleared+userdirs','cleared+psmp+import') {
  $psi = New-Object Diagnostics.ProcessStartInfo $ps
  foreach ($a in '-NoLogo','-NoProfile','-NonInteractive','-Command',$script) { $psi.ArgumentList.Add($a) }
  $psi.UseShellExecute = $false; $psi.RedirectStandardOutput = $true; $psi.RedirectStandardError = $true; $psi.RedirectStandardInput = $true
  $psi.WorkingDirectory = Split-Path $ps
  if ($mode -ne 'full') {
    $psi.Environment.Clear(); $psi.Environment['SystemRoot'] = $env:SystemRoot
    if ($mode -like 'cleared+ps*') { $psi.Environment['PSModulePath'] = "$env:SystemRoot\System32\WindowsPowerShell\v1.0\Modules" }
    if ($mode -eq 'cleared+userdirs') { foreach ($k in 'TEMP','TMP','USERPROFILE','LOCALAPPDATA','APPDATA') { $psi.Environment[$k] = [Environment]::GetEnvironmentVariable($k) } }
  }
  $w = [Diagnostics.Stopwatch]::StartNew()
  $p = [Diagnostics.Process]::Start($psi); $p.StandardInput.Close()
  $o = $p.StandardOutput.ReadToEnd(); $e = $p.StandardError.ReadToEnd(); $p.WaitForExit()
  "=== mode=$mode exit=$($p.ExitCode) wall=$([math]::Round($w.Elapsed.TotalSeconds,2))s"; $o; if ($e) { "stderr: $e" }
}
"=== Defender"; Get-MpComputerStatus | Select-Object RealTimeProtectionEnabled,AMServiceEnabled,IsTamperProtected | Format-List | Out-String
Get-MpPreference | Select-Object DisableRealtimeMonitoring,MAPSReporting,CloudBlockLevel,CloudExtendedTimeout,DisableScriptScanning | Format-List | Out-String
