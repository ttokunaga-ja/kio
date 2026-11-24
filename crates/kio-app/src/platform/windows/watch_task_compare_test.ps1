param([string]$fixtures = $PSScriptRoot)
$ErrorActionPreference = 'Stop'
. ([ScriptBlock]::Create([IO.File]::ReadAllText((Join-Path $fixtures 'watch_task_compare.ps1'))))
$taskId = 'io.kio.watch.fixture'
$taskPath = '\' + $taskId
$sid = 'S-1-5-21-100-200-300-1001'
$resolve = {
    param($identity)
    switch -CaseSensitive ($identity) {
        'BUILDER\runner' { $sid }
        'BUILDER\other' { 'S-1-5-21-100-200-300-1002' }
        default { $identity }
    }
}
$expected = @'
<?xml version='1.0' encoding='UTF-16'?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><URI>\io.kio.watch.fixture</URI><Description>Kio offline watcher io.kio.watch.fixture</Description></RegistrationInfo>
  <Triggers><LogonTrigger><Enabled>true</Enabled></LogonTrigger></Triggers>
  <Principals><Principal id="Author"><UserId>S-1-5-21-100-200-300-1001</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
  <Settings><Enabled>true</Enabled><StartWhenAvailable>false</StartWhenAvailable><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy></Settings>
  <Actions Context="Author"><Exec><Command>\\?\C:\Program Files\Kio\kio.exe</Command><Arguments>&quot;watch&quot; &quot;service&quot; &quot;runner&quot; &quot;--manifest&quot; &quot;C:\data\watch.json&quot;</Arguments></Exec></Actions>
</Task>
'@
$registered = @'
<?xml version='1.0' encoding='UTF-8'?>
<Task xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task" version="1.3">
  <RegistrationInfo>
    <Date>2026-11-24T07:36:00</Date><Author>BUILDER\runner</Author>
    <Description>Kio offline watcher io.kio.watch.fixture</Description><URI>\io.kio.watch.fixture</URI>
  </RegistrationInfo>
  <Triggers><LogonTrigger><Enabled>1</Enabled><Delay>PT0S</Delay><ExecutionTimeLimit>P3D</ExecutionTimeLimit></LogonTrigger></Triggers>
  <Principals><Principal id="Author"><UserId>BUILDER\runner</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>true</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>1</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate><StartWhenAvailable>0</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings><WaitTimeout>PT60M</WaitTimeout><StopOnIdleEnd>true</StopOnIdleEnd><RestartOnIdle>false</RestartOnIdle></IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand><Enabled>1</Enabled><Hidden>0</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle><WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>P3D</ExecutionTimeLimit><Priority>07</Priority>
  </Settings>
  <Actions Context="Author"><Exec><Command>\\?\C:\Program Files\Kio\kio.exe</Command><Arguments>"watch" "service" "runner" "--manifest" "C:\data\watch.json"</Arguments><WorkingDirectory /></Exec></Actions>
</Task>
'@
$baseline = Read-TaskFields $expected $resolve
function Assert-Difference($xml, $expectedField, $path = $taskPath) {
    $actual = Read-TaskFields $xml $resolve
    $difference = Compare-TaskDefinition $actual $baseline $path $taskId
    if ($difference -cne $expectedField) { throw "Expected '$expectedField', got '$difference'" }
}
Assert-Difference $registered ''
Assert-Difference $registered 'RegisteredTask/Path' '\foreign'
foreach ($case in @(
    @('kio.exe</Command>', 'other.exe</Command>', 'Task/Actions/Exec/Command'),
    @('watch.json"</Arguments>', 'other.json"</Arguments>', 'Task/Actions/Exec/Arguments'),
    @('<WorkingDirectory />', '<WorkingDirectory>C:\other</WorkingDirectory>', 'Task/Actions/Exec/WorkingDirectory'),
    @('<UserId>BUILDER\runner', '<UserId>BUILDER\other', 'Task/Principals/Principal/UserId'),
    @('<LogonType>InteractiveToken', '<LogonType>S4U', 'Task/Principals/Principal/LogonType'),
    @('<RunLevel>LeastPrivilege', '<RunLevel>HighestAvailable', 'Task/Principals/Principal/RunLevel'),
    @('LogonTrigger>', 'TimeTrigger>', 'Task/Triggers/LogonTrigger'),
    @('<Delay>PT0S', '<Delay>PT1S', 'Task/Triggers/LogonTrigger/Delay'),
    @('</LogonTrigger>', '<UserId>BUILDER\other</UserId></LogonTrigger>', 'Task/Triggers/LogonTrigger/UserId'),
    @('</LogonTrigger>', '<Repetition><Interval>PT5M</Interval></Repetition></LogonTrigger>', 'Task/Triggers/LogonTrigger/Repetition'),
    @('<Hidden>0', '<Hidden>1', 'Task/Settings/Hidden'),
    @('<Enabled>1</Enabled><Hidden>', '<Enabled>0</Enabled><Hidden>', 'Task/Settings/Enabled'),
    @('<MultipleInstancesPolicy>IgnoreNew', '<MultipleInstancesPolicy>Parallel', 'Task/Settings/MultipleInstancesPolicy'),
    @('</IdleSettings>', '<Duration>PT30M</Duration></IdleSettings>', 'Task/Settings/IdleSettings/Duration'),
    @('<ExecutionTimeLimit>P3D</ExecutionTimeLimit><Priority>', '<ExecutionTimeLimit>PT0S</ExecutionTimeLimit><Priority>', 'Task/Settings/ExecutionTimeLimit'),
    @('<StartWhenAvailable>0', '<StartWhenAvailable>1', 'Task/Settings/StartWhenAvailable'),
    @('<StopIfGoingOnBatteries>1', '<StopIfGoingOnBatteries>0', 'Task/Settings/StopIfGoingOnBatteries'),
    @('</Settings>', '<RestartOnFailure><Interval>PT1M</Interval><Count>3</Count></RestartOnFailure></Settings>', 'Task/Settings/RestartOnFailure'),
    @('<URI>\io.kio.watch.fixture', '<URI>\foreign', 'Task/RegistrationInfo/URI'),
    @('</Settings>', '<UnknownOption>false</UnknownOption></Settings>', 'Task/Settings/UnknownOption'),
    @('<Exec>', '<Exec unknown="true">', 'Task/Actions/Exec/@unknown')
)) {
    Assert-Difference ($registered.Replace($case[0], $case[1])) $case[2]
}
foreach ($invalidXml in @(
    $registered.Replace('</Actions>', '<Exec><Command>other</Command></Exec></Actions>'),
    $registered.Replace('<Command>', '<Command xmlns="urn:foreign">')
)) {
    try { $null = Read-TaskFields $invalidXml $resolve; throw 'Unexpectedly accepted invalid task XML' }
    catch { if ($_.Exception.Message -notmatch '^(Duplicate|Unexpected task XML namespace)') { throw } }
}

if ([Environment]::OSVersion.Platform -eq [PlatformID]::Win32NT) {
    $current = [Security.Principal.WindowsIdentity]::GetCurrent()
    if ((Resolve-TaskSid $current.Name) -cne $current.User.Value -or
        (Resolve-TaskSid $current.User.Value) -cne $current.User.Value) {
        throw 'Windows account name did not resolve to the current logon SID'
    }
}
