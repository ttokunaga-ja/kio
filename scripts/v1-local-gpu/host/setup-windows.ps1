#Requires -RunAsAdministrator
[CmdletBinding()]
param(
    [Parameter(Mandatory)][ValidatePattern('^[a-z0-9][a-z0-9-]*$')][string]$WslOwner,
    [Parameter(Mandatory)][ValidatePattern('^[a-zA-Z0-9][a-zA-Z0-9._-]*$')][string]$Distro,
    [Parameter(Mandatory)][string]$ExistingAllowUsers,
    [Parameter(Mandatory)][string]$PublicKeyFile,
    [Parameter(Mandatory)][string]$AdminSource,
    [Parameter(Mandatory)][string]$CiSource,
    [string]$TailscaleInterface = 'Tailscale',
    [pscredential]$KeepaliveCredential
)

$ErrorActionPreference = 'Stop'
$sshDir = Join-Path $env:ProgramData 'ssh'
$config = Join-Path $sshDir 'sshd_config'
$keys = Join-Path $sshDir 'kio-v1-local-authorized-keys'
$sshBin = Join-Path $env:SystemRoot 'System32\OpenSSH'
$sshd = Join-Path $sshBin 'sshd.exe'
$wsl = Join-Path $env:SystemRoot 'System32\wsl.exe'
$taskName = 'KioV1LocalGpuKeepalive'
$ruleName = 'KioV1LocalGpuSshAllow'
$taskCommand = "--distribution $Distro --user root --exec /usr/bin/sleep infinity"
$begin = '# BEGIN kio v1 local GPU'
$end = '# END kio v1 local GPU'
$utf8 = [Text.UTF8Encoding]::new($false)
$noBackup = [System.Management.Automation.Language.NullString]::Value

function Invoke-Native([string]$Executable, [string[]]$Arguments) {
    $ErrorActionPreference = 'Continue'
    $output = @(& $Executable @Arguments 2>&1 | ForEach-Object { "$_" })
    [pscustomobject]@{ Code = $LASTEXITCODE; Output = $output }
}

function Invoke-Checked([string]$Executable, [string[]]$Arguments) {
    $result = Invoke-Native $Executable $Arguments
    if ($result.Code -ne 0) { throw "$Executable exited with $($result.Code): $($result.Output -join ' ')" }
    $result.Output
}

function Test-TailIPv4([string]$Address) {
    $parsed = $null
    $Address -match '^100\.(6[4-9]|[7-9][0-9]|1[01][0-9]|12[0-7])\.[0-9]{1,3}\.[0-9]{1,3}$' -and
        [ipaddress]::TryParse($Address, [ref]$parsed)
}

function Get-NetworkInventory {
    Write-Host 'Windows portproxy and listeners:'
    Invoke-Checked netsh.exe @('interface', 'portproxy', 'show', 'all') | Out-Host
    $listeners = @(Get-NetTCPConnection -State Listen | Where-Object { $_.LocalPort -in 22, 2222, 18443 })
    $listeners | Format-Table LocalAddress, LocalPort, OwningProcess | Out-Host
    if ($listeners | Where-Object { $_.LocalPort -eq 2222 -and $_.LocalAddress -notin '127.0.0.1','::1' }) {
        throw 'Existing non-loopback TCP 2222 listener requires review.'
    }
    # Stop even if a stale registration is not listening yet.
    $rules = Invoke-Checked netsh.exe @('interface', 'portproxy', 'show', 'all')
    if ($rules -match '^\s*\S+\s+2222\s+\S+\s+\d+\s*$') { throw 'Existing TCP 2222 portproxy requires review.' }
}

function Test-Port22($Filter) {
    if ([string]$Filter.Protocol -notin 'TCP','6','Any','256') { return $false }
    foreach ($port in @($Filter.LocalPort)) {
        if ($port -in 'Any','22') { return $true }
        if ($port -match '^(\d+)-(\d+)$' -and [int]$Matches[1] -le 22 -and [int]$Matches[2] -ge 22) { return $true }
    }
    return $false
}

function Test-FirewallScope($Rule) {
    $port = $Rule | Get-NetFirewallPortFilter
    $address = $Rule | Get-NetFirewallAddressFilter
    $interface = $Rule | Get-NetFirewallInterfaceFilter
    $application = $Rule | Get-NetFirewallApplicationFilter
    return $Rule.Enabled -eq 'True' -and $Rule.Direction -eq 'Inbound' -and $Rule.Action -eq 'Allow' -and
        $Rule.Profile -eq 'Any' -and [string]$port.Protocol -in 'TCP','6' -and
        ($port.LocalPort -join ',') -eq '22' -and ($port.RemotePort -join ',') -eq 'Any' -and
        ($address.LocalAddress -join ',') -in $tailIp,"$tailIp/32" -and
        ($address.RemoteAddress -join ',') -in '100.64.0.0/10','100.64.0.0/255.192.0.0' -and
        ($interface.InterfaceAlias -join ',') -eq $adapter.Name -and
        [Environment]::ExpandEnvironmentVariables($application.Program) -ieq $sshd
}

function Set-AtomicFile([string]$From, [string]$To) {
    if ([IO.File]::Exists($To)) {
        [IO.File]::Replace($From, $To, $noBackup)
    } else {
        [IO.File]::Move($From, $To)
    }
}

function Test-HostPin([int]$Port, [string]$Address, [string]$PublicKey) {
    $expected = (($PublicKey.Trim() -split '\s+')[0..1] -join ' ')
    $result = Invoke-Native (Join-Path $sshBin 'ssh-keyscan.exe') @('-T', '3', '-p', $Port, '-t', 'ed25519', $Address)
    $actual = @($result.Output | Where-Object { $_ -match '^\S+ ssh-ed25519 [A-Za-z0-9+/]+$' })
    return $result.Code -eq 0 -and $actual.Count -eq 1 -and
        (($actual[0] -split '\s+')[1..2] -join ' ') -ceq $expected
}

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$owner = Get-LocalUser -Name $WslOwner
if ($identity.User.Value -ne $owner.SID.Value) { throw 'Run elevated as the WSL registration owner.' }
if (!(Test-TailIPv4 $AdminSource) -or !(Test-TailIPv4 $CiSource)) { throw 'Use observed admin and CI tailnet IPv4 addresses.' }
if ($ExistingAllowUsers -cnotmatch '^AllowUsers ([a-z0-9][a-z0-9-]*)(?:@([0-9.]+))?$' -or
    $Matches[1] -cne $WslOwner -or ($Matches[2] -and $Matches[2] -ne $AdminSource)) {
    throw 'Expected the original global AllowUsers line for this owner and admin source.'
}
$publicKey = (Get-Content -LiteralPath $PublicKeyFile -Raw).Trim()
if ($publicKey -cnotmatch '^ssh-ed25519 ([A-Za-z0-9+/]{68})(?:[ \t]+[^\r\n]+)?$') { throw 'Expected one Ed25519 public key.' }
$keyBlob = $Matches[1]
Invoke-Checked (Join-Path $sshBin 'ssh-keygen.exe') @('-l', '-f', (Resolve-Path -LiteralPath $PublicKeyFile).Path) | Out-Host
$adapter = Get-NetAdapter -Name $TailscaleInterface
if ($adapter.Status -ne 'Up' -or $adapter.InterfaceDescription -notlike '*Tailscale*') { throw 'Expected an active Tailscale adapter.' }
$tailIps = @(Get-NetIPAddress -InterfaceIndex $adapter.InterfaceIndex -AddressFamily IPv4 |
    Where-Object { Test-TailIPv4 $_.IPAddress } | Select-Object -ExpandProperty IPAddress)
if ($tailIps.Count -ne 1) { throw 'Expected exactly one Tailscale IPv4.' }
$tailIp = $tailIps[0]
if (Get-NetFirewallProfile -PolicyStore ActiveStore |
    Where-Object { $_.Enabled -ne 'True' -or $_.DefaultInboundAction -eq 'Allow' -or $_.AllowLocalFirewallRules -eq 'False' }) {
    throw 'Active firewall policy must allow local rules and block unsolicited inbound traffic.'
}

$priorRule = Get-NetFirewallRule -Name $ruleName -PolicyStore PersistentStore -ErrorAction SilentlyContinue
if ($priorRule -and !(Test-FirewallScope $priorRule)) { throw "Existing $ruleName has a different scope. Review it before applying." }
$overlaps = foreach ($rule in Get-NetFirewallRule -PolicyStore ActiveStore -Enabled True -Direction Inbound -Action Allow) {
    if (!(Test-Port22 ($rule | Get-NetFirewallPortFilter))) { continue }
    if ($rule.Name -eq $ruleName -and $priorRule -and (Test-FirewallScope $rule)) { continue }
    $program = [Environment]::ExpandEnvironmentVariables(($rule | Get-NetFirewallApplicationFilter).Program)
    if ($program -notin 'Any','System',$sshd -and $program -notmatch '(?i)sshd\.exe$|[*?]') { continue }
    $iface = ($rule | Get-NetFirewallInterfaceFilter).InterfaceAlias -join ','
    $remote = ($rule | Get-NetFirewallAddressFilter).RemoteAddress -join ','
    if ($iface -eq $adapter.Name -and $remote -in $AdminSource,"$AdminSource/32") { continue }
    $rule.Name
}
if ($overlaps) { throw "Review existing inbound TCP 22 allow rules before applying: $($overlaps -join ', ')" }

Get-NetworkInventory
$windowsService = Get-Service -Name sshd
if ($windowsService.Status -ne 'Running') { throw 'The administrator sshd service must already be running.' }
$serviceCommand = [Environment]::ExpandEnvironmentVariables(
    (Get-CimInstance Win32_Service -Filter "Name='sshd'").PathName)
if ($serviceCommand -notmatch ('^"?' + [regex]::Escape($sshd) + '"?(?: -D)?$')) {
    throw 'The running sshd service does not use the inbox executable and default config path.'
}
$task = Get-ScheduledTask -TaskName $taskName -TaskPath '\' -ErrorAction SilentlyContinue
if ($task) {
    $taskSid = if ($task.Principal.UserId -match '^S-1-') { $task.Principal.UserId } else {
        ([Security.Principal.NTAccount]$task.Principal.UserId).Translate([Security.Principal.SecurityIdentifier]).Value
    }
    $triggers = @($task.Triggers | Where-Object Enabled | ForEach-Object { $_.CimClass.CimClassName })
    if ($taskSid -ne $owner.SID.Value -or $task.Principal.LogonType -ne 'Password' -or
        $task.State -notin 'Running','Ready' -or $task.Settings.ExecutionTimeLimit -ne 'PT0S' -or
        $triggers -notcontains 'MSFT_TaskBootTrigger' -or $triggers -notcontains 'MSFT_TaskLogonTrigger' -or
        @($task.Actions).Count -ne 1 -or $task.Actions[0].Execute -ne $wsl -or $task.Actions[0].Arguments -ne $taskCommand) {
        throw 'Existing Kio keepalive task differs. Review it before applying.'
    }
} elseif (!$KeepaliveCredential -or $KeepaliveCredential.UserName -ine $identity.Name) {
    throw "Supply the Windows credential for $($identity.Name) for the boot task."
}
$account = Get-LocalUser -Name kio-ci -ErrorAction SilentlyContinue
if ($account) {
    if (!$account.Enabled) { throw 'Existing kio-ci is disabled.' }
    foreach ($group in Get-LocalGroup) {
        if ($group.SID.Value -ne 'S-1-5-32-545' -and
            (Get-LocalGroupMember -SID $group.SID | Where-Object { $_.SID -eq $account.SID })) {
            throw "kio-ci belongs to an additional group: $($group.Name)"
        }
    }
}
$hostPub = Get-Content -Raw -LiteralPath (Join-Path $sshDir 'ssh_host_ed25519_key.pub')
if (!(Test-HostPin 22 $tailIp $hostPub)) { throw 'Windows did not present its pinned host key on TCP 22.' }
# Reading WSL is only allowed after the owner has already started it for provisioning.
$running = (Invoke-Checked $wsl @('--list', '--running', '--quiet')) -replace '[\x00\s]+',''
if ($Distro -cnotin $running) { throw 'Start the selected WSL distro before applying Windows setup.' }
Invoke-Checked $wsl @('--distribution', $Distro, '--user', 'root', '--exec', '/usr/bin/systemctl', 'is-active', 'kio-v1-local-sshd.service') | Out-Host
$wslPub = (Invoke-Checked $wsl @('--distribution', $Distro, '--user', 'root', '--exec', '/usr/bin/cat', '/etc/ssh/ssh_host_ed25519_key.pub')) -join ''
if (!(Test-HostPin 2222 127.0.0.1 $wslPub)) { throw 'Loopback TCP 2222 does not present the selected WSL host key.' }

$original = [IO.File]::ReadAllText($config)
$managed = '(?ms)^' + [regex]::Escape($begin) + '\r?\n.*?^' + [regex]::Escape($end) + '(?:\r?\n|\z)'
$unmanaged = [regex]::Replace($original, $managed, '').TrimEnd()
$line = '(?m)^' + [regex]::Escape($ExistingAllowUsers) + '(?: kio-ci)?(?=\r?$)'
# The surveyed hosts use a single global AllowUsers and no Include / Match.
if ($unmanaged -match '(?im)^[ \t]*(Include|Match)(?:[ \t=]|$)' -or
    [regex]::Matches($unmanaged, '(?im)^[ \t]*AllowUsers(?:[ \t=]|$)').Count -ne 1 -or
    [regex]::Matches($unmanaged, $line).Count -ne 1) {
    throw 'Expected exactly the reviewed global AllowUsers line and no additional Match or Include.'
}
$unmanaged = [regex]::Replace($unmanaged, $line, "$ExistingAllowUsers kio-ci")
$keyPath = $keys.Replace('\', '/')
$required = @('AuthenticationMethods publickey', 'PubkeyAuthentication yes', 'PasswordAuthentication no',
    "AuthorizedKeysFile `"$keyPath`"", 'AllowTcpForwarding local', 'PermitOpen 127.0.0.1:2222',
    'GatewayPorts no', 'PermitTTY no', 'AllowAgentForwarding no', 'MaxSessions 0')
$optional = @('X11Forwarding no', 'AllowStreamLocalForwarding no', 'PermitTunnel no')
$connection = "user=kio-ci,host=$CiSource,addr=$CiSource,laddr=$tailIp,lport=22"
$adminConnection = "user=$WslOwner,host=$AdminSource,addr=$AdminSource,laddr=$tailIp,lport=22"
Write-Host "Windows sshd file version: $((Get-Item -LiteralPath $sshd).VersionInfo.FileVersion)"
$version = Invoke-Native $sshd @('-V')
if ($version.Code -eq 0) { $version.Output | Out-Host }

$staging = Join-Path $sshDir ('kio-v1-local-stage-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $staging | Out-Null
$next = Join-Path $staging 'sshd_config'
function Write-Candidate($Settings) {
    $body = ($Settings | ForEach-Object { '    ' + $_ }) -join "`n"
    [IO.File]::WriteAllText($next, "$unmanaged`n`n$begin`nMatch User kio-ci`n$body`n$end`n", $utf8)
}

$backup = $null
$createdAccount = $false
$createdRule = $false
$createdTask = $false
$startedTask = $false
$wroteConfig = $false
$wroteKeys = $false
try {
    foreach ($entry in $optional) {
        Write-Candidate ($required + $entry)
        $probe = Invoke-Native $sshd @('-T', '-f', $next, '-C', $connection)
        if ($probe.Code -eq 0 -and $entry -in $probe.Output -and
            !($probe.Output -match '(?i)unsupported|bad configuration option|not supported')) {
            $required += $entry
        } else { Write-Host "Omitting unsupported OpenSSH directive: $entry" }
    }
    Write-Candidate $required
    Invoke-Checked $sshd @('-t', '-f', $next) | Out-Host
    function Test-Effective([string]$File) {
        $effective = Invoke-Checked $sshd @('-T', '-f', $File, '-C', $connection)
        foreach ($setting in $required) {
            if ($setting -like 'AuthorizedKeysFile *') {
                if ($effective -inotcontains "authorizedkeysfile $keyPath" -and
                    $effective -inotcontains "authorizedkeysfile `"$keyPath`"") { throw 'Unexpected authorized-keys path.' }
            } elseif ($setting -notin $effective) { throw "Unexpected effective SSH setting: $setting" }
        }
        if ($effective -match '^(permituserenvironment (?!no$)|trustedusercakeys (?!none$)|authorizedkeyscommand (?!none$))' -or
            !($effective -match '^allowusers kio-ci$|^allowusers .*\bkio-ci$')) { throw 'Unexpected CI account authorization policy.' }
    }
    Test-Effective $next
    $adminBefore = @(Invoke-Checked $sshd @('-T', '-f', $config, '-C', $adminConnection) | Where-Object { $_ -notmatch '^allowusers ' })
    $adminAfter = @(Invoke-Checked $sshd @('-T', '-f', $next, '-C', $adminConnection) | Where-Object { $_ -notmatch '^allowusers ' })
    if (Compare-Object $adminBefore $adminAfter) { throw 'Administrator effective SSH settings would change.' }
    $backup = Join-Path $sshDir ('kio-v1-local-backup-' + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $backup | Out-Null
    Write-Host "Backup: $backup"
    foreach ($file in @($config, $keys)) {
        if (Test-Path -LiteralPath $file) {
            $leaf = Split-Path $file -Leaf
            Copy-Item -LiteralPath $file -Destination (Join-Path $backup $leaf)
            (Get-Acl -LiteralPath $file).Sddl | Set-Content -LiteralPath (Join-Path $backup "$leaf.sddl")
        }
    }
    $windowsService.Status | Set-Content -LiteralPath (Join-Path $backup 'sshd-state')
    if ($task) {
        Export-ScheduledTask -TaskName $taskName -TaskPath '\' | Set-Content -LiteralPath (Join-Path $backup 'keepalive.xml')
        $task.State | Set-Content -LiteralPath (Join-Path $backup 'keepalive-state')
    }
    if ($priorRule) { $priorRule | Export-Clixml -LiteralPath (Join-Path $backup 'firewall.xml') }

    if (!$account) {
        $password = ConvertTo-SecureString ('K9!a' + [Guid]::NewGuid().ToString('N') + [Guid]::NewGuid().ToString('N')) -AsPlainText -Force
        $account = New-LocalUser -Name kio-ci -Password $password -PasswordNeverExpires -UserMayNotChangePassword
        $createdAccount = $true
        New-Item -ItemType File -Path (Join-Path $backup 'created-account') | Out-Null
        if (!(Get-LocalGroupMember -SID 'S-1-5-32-545' | Where-Object { $_.SID -eq $account.SID })) {
            Add-LocalGroupMember -SID 'S-1-5-32-545' -Member $account
        }
    }
    if (!$task) {
        $action = New-ScheduledTaskAction -Execute $wsl -Argument $taskCommand
        $triggers = @((New-ScheduledTaskTrigger -AtStartup), (New-ScheduledTaskTrigger -AtLogOn -User $identity.Name))
        $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -StartWhenAvailable `
            -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1)
        Register-ScheduledTask -TaskName $taskName -TaskPath '\' -Action $action -Trigger $triggers -Settings $settings `
            -User $identity.Name -Password ($KeepaliveCredential.GetNetworkCredential().Password) -RunLevel Limited | Out-Null
        $createdTask = $true
    }
    if ((Get-ScheduledTask -TaskName $taskName -TaskPath '\').State -ne 'Running') {
        Start-ScheduledTask -TaskName $taskName -TaskPath '\'
        $startedTask = $true
    }
    if (!$priorRule) {
        New-NetFirewallRule -Name $ruleName -DisplayName $ruleName -Direction Inbound -Action Allow -Profile Any `
            -Protocol TCP -LocalPort 22 -LocalAddress $tailIp -RemoteAddress '100.64.0.0/10' `
            -Program $sshd -InterfaceAlias $adapter.Name | Out-Null
        $createdRule = $true
    }
    if (!(Test-FirewallScope (Get-NetFirewallRule -Name $ruleName -PolicyStore ActiveStore))) { throw 'Incorrect active firewall scope.' }

    $keyNext = Join-Path $staging 'authorized-keys'
    [IO.File]::WriteAllText($keyNext, "restrict,port-forwarding,permitopen=`"127.0.0.1:2222`" ssh-ed25519 $keyBlob kio-v1-local-acceptance`n", $utf8)
    $acl = [Security.AccessControl.FileSecurity]::new()
    $acl.SetAccessRuleProtection($true, $false)
    foreach ($sid in @('S-1-5-18', 'S-1-5-32-544', $account.SID.Value)) {
        $rights = if ($sid -eq $account.SID.Value) { 'Read' } else { 'FullControl' }
        $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
            [Security.Principal.SecurityIdentifier]::new($sid), $rights, 'Allow'))
    }
    $acl.SetOwner([Security.Principal.SecurityIdentifier]::new('S-1-5-32-544'))
    Set-Acl -LiteralPath $keyNext -AclObject $acl
    $wroteKeys = $true
    Set-AtomicFile $keyNext $keys
    Set-Acl -LiteralPath $keys -AclObject $acl
    if ($original -cne [IO.File]::ReadAllText($next)) {
        $wroteConfig = $true
        Set-AtomicFile $next $config
    }
    Restart-Service -Name sshd
    Test-Effective $config
    $reachable = $false
    for ($attempt = 0; $attempt -lt 10; $attempt++) {
        if ((Get-ScheduledTask -TaskName $taskName -TaskPath '\').State -eq 'Running' -and
            (Test-HostPin 22 $tailIp $hostPub) -and (Test-HostPin 2222 127.0.0.1 $wslPub)) {
            $reachable = $true
            break
        }
        Start-Sleep -Seconds 1
    }
    Get-NetworkInventory
    if (!$reachable) { throw 'Keepalive or pinned SSH reachability failed after applying.' }
    if ((Read-Host 'Confirm a NEW Mac administrator login and the README network/SSH checks, then enter verified') -cne 'verified') {
        throw 'Route verification was not confirmed.'
    }
} catch {
    $failure = $_
    $restoreFailures = @()
    foreach ($entry in @(
        @{ Path = $keys; Written = $wroteKeys }, @{ Path = $config; Written = $wroteConfig }
    )) {
        if (!$entry.Written) { continue }
        try {
            $leaf = Split-Path $entry.Path -Leaf
            $saved = Join-Path $backup $leaf
            if (Test-Path -LiteralPath $saved) {
                $restore = Join-Path $staging "$leaf.restore"
                Copy-Item -LiteralPath $saved -Destination $restore
                Set-AtomicFile $restore $entry.Path
                $restoreAcl = Get-Acl -LiteralPath $entry.Path
                $restoreAcl.SetSecurityDescriptorSddlForm((Get-Content -LiteralPath "$saved.sddl" -Raw).Trim())
                Set-Acl -LiteralPath $entry.Path -AclObject $restoreAcl
            } elseif (Test-Path -LiteralPath $entry.Path) { Remove-Item -LiteralPath $entry.Path }
        } catch { $restoreFailures += $_ }
    }
    try { if ($createdRule) { Remove-NetFirewallRule -Name $ruleName -PolicyStore PersistentStore } } catch { $restoreFailures += $_ }
    try { if ($startedTask) { Stop-ScheduledTask -TaskName $taskName -TaskPath '\' } } catch { $restoreFailures += $_ }
    try { if ($createdTask) { Unregister-ScheduledTask -TaskName $taskName -TaskPath '\' -Confirm:$false } } catch { $restoreFailures += $_ }
    try { if ($createdAccount) { Remove-LocalUser -Name kio-ci } } catch { $restoreFailures += $_ }
    try {
        if ($wroteConfig -or $wroteKeys) {
            Invoke-Checked $sshd @('-t', '-f', $config) | Out-Host
            if ((Get-Service sshd).Status -eq 'Running') { Restart-Service sshd } else { Start-Service sshd }
        }
    } catch { $restoreFailures += $_ }
    if ($restoreFailures) { Write-Warning "Rollback requires attention at $backup : $($restoreFailures -join '; ')" }
    throw $failure
} finally {
    Get-ChildItem -LiteralPath $staging -File | Remove-Item
    Remove-Item -LiteralPath $staging
}
'kio-lab-jump ' + (($hostPub.Trim() -split '\s+')[0..1] -join ' ')
