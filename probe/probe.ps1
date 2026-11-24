$sw = [Diagnostics.Stopwatch]::StartNew()
$start = [Diagnostics.Process]::GetCurrentProcess().StartTime
function T($n) { "{0,-28} {1,7:N2}s (since proc start {2,7:N2}s)" -f $n, $sw.Elapsed.TotalSeconds, ((Get-Date) - $start).TotalSeconds }
T 'script first line'
$j = '{"a":1}' | ConvertFrom-Json
T 'ConvertFrom-Json'
$s = New-Object -ComObject 'Schedule.Service'
T 'New-Object COM'
$s.Connect()
T 'Connect'
$f = $s.GetFolder('\')
T 'GetFolder'
try { $f.GetTask('kio-probe-absent') | Out-Null } catch { }
T 'GetTask absent'
try { $f.GetTask('kio-probe-absent2') | Out-Null } catch { }
T 'GetTask absent 2'
$d = $s.NewTask(0)
$a = $d.Actions.Create(0); $a.Path = "$env:SystemRoot\System32\cmd.exe"; $a.Arguments = '/c exit 0'
$d.Principal.LogonType = 3
T 'NewTask'
$t = $f.RegisterTaskDefinition('kio-probe', $d, 6, $null, $null, 3)
T 'Register'
$t = $f.GetTask('kio-probe'); $x = $t.Definition.XmlText
T 'GetTask present + Xml'
$t.Run($null) | Out-Null
T 'Run'
$t.GetInstances(0).Count | Out-Null
T 'GetInstances'
$f.DeleteTask('kio-probe', 0)
T 'Delete'
$sid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
T 'WindowsIdentity'
