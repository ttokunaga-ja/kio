#Requires -RunAsAdministrator
[CmdletBinding()]
param(
    [ValidateSet('Apply','Confirm','Rollback')][string]$Phase = 'Apply',
    [ValidatePattern('^[a-f0-9]{32}$')][string]$BackupId,
    [ValidatePattern('^[a-z0-9][a-z0-9-]*$')][string]$WslOwner,
    [ValidatePattern('^[a-zA-Z0-9][a-zA-Z0-9._-]*$')][string]$Distro,
    [string]$ExistingAllowUsers,
    [string]$PublicKeyFile,
    [string]$AdminSource,
    [string]$CiSource = '100.64.0.1',
    [string]$TailscaleInterface = 'Tailscale'
)

$ErrorActionPreference = 'Stop'
$sshDir = Join-Path $env:ProgramData 'ssh'
$config = Join-Path $sshDir 'sshd_config'
$keys = Join-Path $sshDir 'kio-v1-local-authorized-keys'
$sshBin = Join-Path $env:SystemRoot 'System32\OpenSSH'
$sshd = Join-Path $sshBin 'sshd.exe'
$wsl = Join-Path $env:SystemRoot 'System32\wsl.exe'
$taskName = 'KioV1LocalGpuKeepalive'
$recoveryTask = 'KioV1LocalGpuRecovery'
$powershell = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
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

function Get-Port22Scope($Filter) {
    $protocol = [string]$Filter.Protocol
    if ($protocol -in 'UDP','ICMPv4','ICMPv6' -or
        ($protocol -match '^\d+$' -and $protocol -notin '6','256')) { return 'Ignore' }
    $unknown = $false
    foreach ($port in @($Filter.LocalPort)) {
        if ($port -in 'Any','22' -or
            ($port -match '^(\d{1,5})-(\d{1,5})$' -and [int]$Matches[1] -le 22 -and [int]$Matches[2] -ge 22)) {
            if ($protocol -in 'TCP','6','Any','256') { return 'Match' }
            return 'Warn'
        }
        if ($port -notmatch '^\d{1,5}(?:-\d{1,5})?$') { $unknown = $true }
    }
    if ($unknown -or !$Filter.LocalPort) { return 'Warn' }
    return 'Ignore'
}

function Get-SshAllowDisposition($Rule, [string]$PortScope, $Application, $ServiceFilter, $Interface, $InterfaceType,
    [hashtable]$NetworkProfiles, [string]$TailAlias, [string]$Sshd, [string]$RawRule) {
    if ($PortScope -eq 'Ignore') { return 'Ignore' }
    $package = [string]$Application.Package
    $service = [string]$ServiceFilter.Service
    # ActiveStore can omit Package even when the corresponding local rule has a PFN.
    if (($package -and $package -ne 'Any') -or $RawRule -match '(?:^|\|)PFN=[^|]+(?:\||$)') { return 'Ignore' }
    if ($service -and $service -notin 'Any','sshd' -and $service -notmatch '[*?]') { return 'Ignore' }
    $program = [Environment]::ExpandEnvironmentVariables([string]$Application.Program)
    if ($program -and $program -notin 'System','Any','sshd.exe',$Sshd -and
        $program -notmatch '[*?%]|(?:^|[\\/])sshd\.exe$') { return 'Ignore' }
    $aliases = @($Interface.InterfaceAlias | Where-Object { $_ -and $_ -ne $TailAlias })
    if ($aliases.Count -eq 0 -and $Interface.InterfaceAlias) { return 'Ignore' }
    if ($aliases -contains 'Any') { $aliases = @($NetworkProfiles.Keys) }
    $profiles = ([string]$Rule.Profile -replace '\s', '') -split ','
    $unknown = $aliases.Count -eq 0 -or
        @($profiles | Where-Object { $_ -notin 'Any','Domain','Private','Public' }).Count -gt 0
    foreach ($alias in $aliases) {
        if (!$NetworkProfiles.ContainsKey($alias)) { $unknown = $true; continue }
        if ($profiles -contains 'Any' -or @($NetworkProfiles[$alias] | Where-Object { $_ -in $profiles }).Count -gt 0) {
            if ($PortScope -eq 'Match' -and $program -in 'Any','sshd.exe',$Sshd -and
                (!$service -or $service -eq 'Any') -and ($InterfaceType.InterfaceType -join ',') -eq 'Any' -and
                ($package -eq 'Any' -or $RawRule) -and $RawRule -notmatch '(?:^|\|)LUOwn=[^|]+(?:\||$)') {
                return 'Block'
            }
            return 'Warn'
        }
    }
    if ($unknown) { return 'Warn' }
    return 'Ignore'
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

function Test-TailscaleInboundRule($Rule, $Port, $Address, $Interface, [string]$Program, [string]$Service, [string[]]$AdapterAddresses) {
    # tailscaled adds one local host destination per rule through netsh.
    # netsh does not set a vendor group, owner, or explicit interface alias.
    $profile = ([string]$Rule.Profile) -replace '\s', ''
    if ($Rule.DisplayName -cne 'Tailscale-In' -or $Rule.PolicyStoreSourceType -ne 'Local' -or
        $Rule.PolicyStoreSource -ne 'PersistentStore' -or $Rule.Group -or $Rule.Owner -or
        $profile -notin 'Private','Domain,Private','Private,Domain' -or $Rule.EdgeTraversalPolicy -ne 'Block' -or
        [string]$Port.Protocol -notin 'Any','256' -or ($Port.LocalPort -join ',') -ne 'Any' -or
        ($Port.RemotePort -join ',') -ne 'Any' -or ($Address.RemoteAddress -join ',') -ne 'Any' -or
        ($Interface.InterfaceAlias -join ',') -ne 'Any' -or $Program -ne 'Any' -or $Service -ne 'Any') {
        return $false
    }
    $local = $Address.LocalAddress -join ','
    if ($local -notmatch '^([0-9a-f:.]+)(?:/([0-9.]+))?$') { return $false }
    $destination = $Matches[1]
    $prefix = $Matches[2]
    $parsed = $null
    if (![ipaddress]::TryParse($destination, [ref]$parsed)) { return $false }
    if ($prefix) {
        if ($parsed.AddressFamily -eq 'InterNetwork' -and $prefix -notin '32','255.255.255.255') { return $false }
        if ($parsed.AddressFamily -eq 'InterNetworkV6' -and $prefix -ne '128') { return $false }
    }
    return $parsed.ToString() -in $AdapterAddresses
}

function Set-AtomicFile([string]$From, [string]$To) {
    if ([IO.File]::Exists($To)) {
        [IO.File]::Replace($From, $To, $noBackup)
    } else {
        [IO.File]::Move($From, $To)
    }
}

function Get-RecoveryArguments([string]$Backup, [string]$Id) {
    return "-NoProfile -NonInteractive -ExecutionPolicy Bypass -File `"$Backup\setup-windows.ps1`" -Phase Rollback -BackupId $Id"
}

function Get-RecoveryDisposition([string]$RequestedPhase, [string]$State, [long]$Deadline, [long]$Now) {
    if ($RequestedPhase -eq 'Confirm') {
        if ($State -eq 'Confirmed') { return 'Cleanup' }
        if ($State -eq 'Pending' -and $Now -lt $Deadline) { return 'Confirm' }
    } elseif ($RequestedPhase -eq 'Rollback') {
        if ($State -in 'Confirmed','RolledBack') { return 'Cleanup' }
        if ($State -in 'Applying','Pending','Restoring') { return 'Restore' }
    }
    return 'Reject'
}

function Set-RecoveryState([string]$Backup, [string]$State) {
    $nextState = Join-Path $Backup 'state.next'
    [IO.File]::WriteAllText($nextState, $State, $utf8)
    Set-AtomicFile $nextState (Join-Path $Backup 'state')
}

function New-ProtectedDirectory([string]$Path) {
    $acl = [Security.AccessControl.DirectorySecurity]::new()
    $acl.SetAccessRuleProtection($true, $false)
    $acl.SetOwner([Security.Principal.SecurityIdentifier]::new('S-1-5-32-544'))
    foreach ($sid in @('S-1-5-18', 'S-1-5-32-544')) {
        $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
            [Security.Principal.SecurityIdentifier]::new($sid), 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow'))
    }
    [IO.Directory]::CreateDirectory($Path, $acl) | Out-Null
}

function Open-RecoveryLock([string]$Backup, [bool]$Wait) {
    for ($attempt = 0; ; $attempt++) {
        try {
            return [IO.File]::Open((Join-Path $Backup 'lock'), 'OpenOrCreate', 'ReadWrite', 'None')
        } catch {
            # A scheduled rollback may have to wait for Apply to release the lock.
            if (!$Wait -or $attempt -ge 300 -or ($_.Exception.GetBaseException().HResult -band 0xffff) -ne 32) { throw }
            Start-Sleep -Seconds 1
        }
    }
}

function Get-OwnedRecoveryTask([string]$Backup, [string]$Id) {
    $registered = Get-ScheduledTask -TaskName $recoveryTask -TaskPath '\' -ErrorAction SilentlyContinue
    if ($registered -and @($registered.Actions).Count -eq 1 -and $registered.Actions[0].Execute -eq $powershell -and
        $registered.Actions[0].Arguments -ceq (Get-RecoveryArguments $Backup $Id)) { return $registered }
}

function Remove-RecoveryTask([string]$Backup, [string]$Id) {
    if (Get-OwnedRecoveryTask $Backup $Id) {
        Unregister-ScheduledTask -TaskName $recoveryTask -TaskPath '\' -Confirm:$false
    }
}

function Restore-WindowsSetup([string]$Backup) {
    Set-RecoveryState $Backup 'Restoring'
    $failures = @()
    $sshChanged = Test-Path -LiteralPath (Join-Path $Backup 'ssh-change')
    foreach ($path in @($keys, $config)) {
        if (!$sshChanged) { break }
        try {
            $leaf = Split-Path $path -Leaf
            $saved = Join-Path $Backup $leaf
            if (Test-Path -LiteralPath $saved) {
                $restore = Join-Path $Backup "$leaf.restore"
                Copy-Item -LiteralPath $saved -Destination $restore -Force
                Set-AtomicFile $restore $path
                $restoreAcl = Get-Acl -LiteralPath $path
                $restoreAcl.SetSecurityDescriptorSddlForm((Get-Content -LiteralPath "$saved.sddl" -Raw).Trim())
                Set-Acl -LiteralPath $path -AclObject $restoreAcl
            } elseif ($path -eq $keys -and (Test-Path -LiteralPath $path)) { Remove-Item -LiteralPath $path }
        } catch { $failures += $_ }
    }
    try {
        if (!(Test-Path -LiteralPath (Join-Path $Backup 'firewall.xml')) -and
            (Get-NetFirewallRule -Name $ruleName -PolicyStore PersistentStore -ErrorAction SilentlyContinue)) {
            Remove-NetFirewallRule -Name $ruleName -PolicyStore PersistentStore
        }
    } catch { $failures += $_ }
    try {
        $keepalive = Get-ScheduledTask -TaskName $taskName -TaskPath '\' -ErrorAction SilentlyContinue
        if ($keepalive) {
            $savedState = Join-Path $Backup 'keepalive-state'
            if (!(Test-Path -LiteralPath $savedState)) {
                if ($keepalive.State -eq 'Running') { Stop-ScheduledTask -TaskName $taskName -TaskPath '\' }
                Unregister-ScheduledTask -TaskName $taskName -TaskPath '\' -Confirm:$false
            } elseif ((Get-Content -LiteralPath $savedState) -eq 'Ready' -and $keepalive.State -eq 'Running') {
                Stop-ScheduledTask -TaskName $taskName -TaskPath '\'
            }
        }
    } catch { $failures += $_ }
    try {
        if (!(Test-Path -LiteralPath (Join-Path $Backup 'account-present')) -and
            (Get-LocalUser -Name kio-ci -ErrorAction SilentlyContinue)) { Remove-LocalUser -Name kio-ci }
    } catch { $failures += $_ }
    try {
        if ($sshChanged) {
            Invoke-Checked $sshd @('-t', '-f', $config) | Out-Host
            if ((Get-Service sshd).Status -eq 'Running') { Restart-Service sshd } else { Start-Service sshd }
            if ((Get-Service sshd).Status -ne 'Running') { throw 'Restored sshd service did not start.' }
        }
    } catch { $failures += $_ }
    if ($failures) {
        $failures | Out-String | Set-Content -LiteralPath (Join-Path $Backup 'rollback-errors.txt')
        throw "Rollback requires attention at $Backup."
    }
    Set-RecoveryState $Backup 'RolledBack'
}

function Test-HostPin([int]$Port, [string]$Address, [string]$PublicKey) {
    $expected = (($PublicKey.Trim() -split '\s+')[0..1] -join ' ')
    $result = Invoke-Native (Join-Path $sshBin 'ssh-keyscan.exe') @('-T', '3', '-p', $Port, '-t', 'ed25519', $Address)
    $actual = @($result.Output | Where-Object { $_ -match '^\S+ ssh-ed25519 [A-Za-z0-9+/]+$' })
    return $result.Code -eq 0 -and $actual.Count -eq 1 -and
        (($actual[0] -split '\s+')[1..2] -join ' ') -ceq $expected
}

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
if ($Phase -ne 'Apply') {
    if (!$BackupId) { throw 'Supply the BackupId printed by Apply.' }
    if ($Phase -eq 'Rollback' -and $identity.User.Value -ne 'S-1-5-18') { throw 'Rollback is reserved for the SYSTEM task.' }
    $backup = Join-Path $sshDir "kio-v1-local-backup-$BackupId"
    if (!(Test-Path -LiteralPath (Join-Path $backup 'state') -PathType Leaf)) { throw "No transaction state at $backup." }
    $lock = Open-RecoveryLock $backup ($Phase -eq 'Rollback')
    try {
        $state = Get-Content -LiteralPath (Join-Path $backup 'state') -Raw
        $deadline = [long](Get-Content -LiteralPath (Join-Path $backup 'deadline-utc-ticks') -Raw)
        switch (Get-RecoveryDisposition $Phase $state $deadline ([DateTime]::UtcNow.Ticks)) {
            'Confirm' {
                if (!(Get-OwnedRecoveryTask $backup $BackupId)) { throw 'The matching recovery task is missing.' }
                Set-RecoveryState $backup 'Confirmed'
            }
            'Restore' { Restore-WindowsSetup $backup }
            'Reject' { throw "$Phase refused for transaction $BackupId in state $state or after the confirmation deadline." }
        }
        Remove-RecoveryTask $backup $BackupId
        Write-Host "Backup: $backup ($((Get-Content -LiteralPath (Join-Path $backup 'state') -Raw)))"
    } finally { $lock.Dispose() }
    return
}

if ($BackupId -or !$WslOwner -or !$Distro -or !$ExistingAllowUsers -or !$PublicKeyFile -or !$AdminSource) {
    throw 'Apply requires WslOwner, Distro, ExistingAllowUsers, PublicKeyFile and AdminSource.'
}
$applyLockDir = Join-Path $sshDir 'kio-v1-local-apply'
New-ProtectedDirectory $applyLockDir
$applyLock = Open-RecoveryLock $applyLockDir $false
try {
    if (Get-ScheduledTask -TaskName $recoveryTask -TaskPath '\' -ErrorAction SilentlyContinue) {
        throw 'An earlier recovery task is still registered. Resolve that transaction before applying again.'
    }
    $owner = Get-LocalUser -Name $WslOwner
    if ($identity.User.Value -ne $owner.SID.Value) { throw 'Run elevated as the WSL registration owner.' }
    if (!(Test-TailIPv4 $AdminSource) -or !(Test-TailIPv4 $CiSource)) { throw 'Use the observed admin and a representative CI tailnet IPv4 address.' }
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
    $adapterAddresses = Get-NetIPAddress -InterfaceIndex $adapter.InterfaceIndex
    $tailIps = @($adapterAddresses |
        Where-Object { Test-TailIPv4 $_.IPAddress } | Select-Object -ExpandProperty IPAddress)
    if ($tailIps.Count -ne 1) { throw 'Expected exactly one Tailscale IPv4.' }
    $tailIp = $tailIps[0]
    $tailAddresses = @($tailIp)
    $tailAddresses += @($adapterAddresses |
        Where-Object { $_.IPAddress -match '^fd7a:115c:a1e0:' } |
        ForEach-Object { ([ipaddress]$_.IPAddress).ToString() })
    if (Get-NetFirewallProfile -PolicyStore ActiveStore |
        Where-Object { $_.Enabled -ne 'True' -or $_.DefaultInboundAction -eq 'Allow' -or $_.AllowLocalFirewallRules -eq 'False' }) {
        throw 'Active firewall policy must allow local rules and block unsolicited inbound traffic.'
    }

    $priorRule = Get-NetFirewallRule -Name $ruleName -PolicyStore PersistentStore -ErrorAction SilentlyContinue
    if ($priorRule -and !(Test-FirewallScope $priorRule)) { throw "Existing $ruleName has a different scope. Review it before applying." }
    $networkProfiles = @{}
    try {
        foreach ($connection in Get-NetConnectionProfile) {
            if ($connection.InterfaceIndex -eq $adapter.InterfaceIndex) { continue }
            $category = [string]$connection.NetworkCategory
            if ($category -eq 'DomainAuthenticated') { $category = 'Domain' }
            if ($category -in 'Domain','Private','Public') { $networkProfiles[$connection.InterfaceAlias] += @($category) }
        }
    } catch {
        Write-Warning "Could not inventory connection profiles: $_"
    }
    $localRules = $null
    try {
        $localRules = Get-Item -LiteralPath 'HKLM:\SYSTEM\CurrentControlSet\Services\SharedAccess\Parameters\FirewallPolicy\FirewallRules'
    } catch {
        Write-Warning "Could not read local firewall rule metadata: $_"
    }
    $uncertain = @()
    $overlaps = foreach ($rule in Get-NetFirewallRule -PolicyStore ActiveStore -TracePolicyStore -Enabled True -Direction Inbound -Action Allow) {
        try {
            $port = $rule | Get-NetFirewallPortFilter
            $portScope = Get-Port22Scope $port
            if ($portScope -eq 'Ignore') { continue }
            $application = $rule | Get-NetFirewallApplicationFilter
            $service = $rule | Get-NetFirewallServiceFilter
            $interface = $rule | Get-NetFirewallInterfaceFilter
            if ($rule.DisplayName -ceq 'Tailscale-In' -and
                (Test-TailscaleInboundRule $rule $port ($rule | Get-NetFirewallAddressFilter) $interface $application.Program $service.Service $tailAddresses)) {
                continue
            }
            $interfaceType = $rule | Get-NetFirewallInterfaceTypeFilter
            $rawRule = $null
            if ($localRules -and $rule.PolicyStoreSourceType -eq 'Local' -and $rule.PolicyStoreSource -eq 'PersistentStore') {
                $rawRule = $localRules.GetValue($rule.Name)
            }
            $disposition = Get-SshAllowDisposition $rule $portScope $application $service $interface $interfaceType `
                $networkProfiles $adapter.Name $sshd $rawRule
            if ($disposition -eq 'Block') { $rule.Name }
            if ($disposition -eq 'Warn') { $uncertain += $rule.Name }
        } catch {
            $uncertain += "$($rule.Name) ($_)"
        }
    }
    if ($uncertain) { Write-Warning "Inconclusive inbound TCP 22 allow rules: $($uncertain -join ', ')" }
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
        if ($taskSid -ne $owner.SID.Value -or $task.Principal.LogonType -ne 'S4U' -or $task.Principal.RunLevel -ne 'Limited' -or
            $task.State -notin 'Running','Ready' -or $task.Settings.ExecutionTimeLimit -ne 'PT0S' -or
            $triggers -notcontains 'MSFT_TaskBootTrigger' -or $triggers -notcontains 'MSFT_TaskLogonTrigger' -or
            @($task.Actions).Count -ne 1 -or $task.Actions[0].Execute -ne $wsl -or $task.Actions[0].Arguments -ne $taskCommand) {
            throw 'Existing Kio keepalive task differs. Review it before applying.'
        }
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
    $lock = $null
    $armed = $false
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
        $id = [Guid]::NewGuid().ToString('N')
        $backup = Join-Path $sshDir "kio-v1-local-backup-$id"
        New-ProtectedDirectory $backup
        $lock = Open-RecoveryLock $backup $false
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
        if ($account) { $account.SID.Value | Set-Content -LiteralPath (Join-Path $backup 'account-present') }
        [IO.File]::Copy($PSCommandPath, (Join-Path $backup 'setup-windows.ps1'))
        $deadlineUtc = [DateTime]::UtcNow.AddMinutes(10).Ticks
        $deadlineUtc -= $deadlineUtc % [TimeSpan]::TicksPerSecond
        $deadline = [DateTimeOffset]([DateTime]::new($deadlineUtc, [DateTimeKind]::Utc).ToLocalTime())
        [IO.File]::WriteAllText((Join-Path $backup 'deadline-utc-ticks'), [string]$deadlineUtc, $utf8)
        Set-RecoveryState $backup 'Applying'
        $recoveryAction = New-ScheduledTaskAction -Execute $powershell -Argument (Get-RecoveryArguments $backup $id)
        $recoveryTrigger = New-ScheduledTaskTrigger -Once -At $deadline.LocalDateTime
        $recoveryTrigger.StartBoundary = $deadline.ToString('yyyy-MM-ddTHH:mm:sszzz')
        # StartWhenAvailable needs an end boundary for a one-time trigger.
        $recoveryTrigger.EndBoundary = $deadline.AddYears(1).ToString('yyyy-MM-ddTHH:mm:sszzz')
        $recoverySettings = New-ScheduledTaskSettingsSet -StartWhenAvailable -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
        $recoveryPrincipal = New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest
        Register-ScheduledTask -TaskName $recoveryTask -TaskPath '\' -Action $recoveryAction -Trigger $recoveryTrigger `
            -Settings $recoverySettings -Principal $recoveryPrincipal | Out-Null
        $armed = $true

        if (!$account) {
            $password = ConvertTo-SecureString ('K9!a' + [Guid]::NewGuid().ToString('N') + [Guid]::NewGuid().ToString('N')) -AsPlainText -Force
            $account = New-LocalUser -Name kio-ci -Password $password -PasswordNeverExpires -UserMayNotChangePassword
            if (!(Get-LocalGroupMember -SID 'S-1-5-32-545' | Where-Object { $_.SID -eq $account.SID })) {
                Add-LocalGroupMember -SID 'S-1-5-32-545' -Member $account
            }
        }
        if (!$task) {
            $action = New-ScheduledTaskAction -Execute $wsl -Argument $taskCommand
            $triggers = @((New-ScheduledTaskTrigger -AtStartup), (New-ScheduledTaskTrigger -AtLogOn -User $identity.Name))
            $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -StartWhenAvailable `
                -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1)
            $principal = New-ScheduledTaskPrincipal -UserId $identity.Name -LogonType S4U -RunLevel Limited
            Register-ScheduledTask -TaskName $taskName -TaskPath '\' -Action $action -Trigger $triggers -Settings $settings `
                -Principal $principal | Out-Null
        }
        if ((Get-ScheduledTask -TaskName $taskName -TaskPath '\').State -ne 'Running') {
            Start-ScheduledTask -TaskName $taskName -TaskPath '\'
        }
        if (!$priorRule) {
            New-NetFirewallRule -Name $ruleName -DisplayName $ruleName -Direction Inbound -Action Allow -Profile Any `
                -Protocol TCP -LocalPort 22 -LocalAddress $tailIp -RemoteAddress '100.64.0.0/10' `
                -Program $sshd -InterfaceAlias $adapter.Name | Out-Null
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
        New-Item -ItemType File -Path (Join-Path $backup 'ssh-change') | Out-Null
        Set-AtomicFile $keyNext $keys
        Set-Acl -LiteralPath $keys -AclObject $acl
        if ($original -cne [IO.File]::ReadAllText($next)) {
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
        if (!$reachable) {
            $taskResult = (Get-ScheduledTaskInfo -TaskName $taskName -TaskPath '\').LastTaskResult
            throw "S4U keepalive or pinned SSH reachability failed (task result $taskResult)."
        }
        if ([DateTime]::UtcNow.Ticks -ge $deadlineUtc) { throw 'Apply exceeded the recovery deadline.' }
        Set-RecoveryState $backup 'Pending'
        Write-Host ("Confirm by {0:yyyy-MM-dd HH:mm:ss zzz} using -BackupId {1}" -f $deadline, $id)
    } catch {
        $failure = $_
        if ($armed) {
            try {
                Restore-WindowsSetup $backup
                Remove-RecoveryTask $backup $id
            } catch { Write-Warning "Rollback requires attention at ${backup}: $_" }
        }
        throw $failure
    } finally {
        if ($lock) { $lock.Dispose() }
        Get-ChildItem -LiteralPath $staging -File | Remove-Item
        Remove-Item -LiteralPath $staging
    }
    'kio-lab-jump ' + (($hostPub.Trim() -split '\s+')[0..1] -join ' ')
} finally {
    $applyLock.Dispose()
}
