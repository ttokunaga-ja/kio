# Run with Windows PowerShell. Extract the predicate without executing setup.
$ErrorActionPreference = 'Stop'
$tokens = $null
$parseErrors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile(
    (Join-Path $PSScriptRoot 'setup-windows.ps1'), [ref]$tokens, [ref]$parseErrors)
if ($parseErrors) { throw ($parseErrors -join [Environment]::NewLine) }
$predicate = $ast.Find({
    param($node)
    $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and
        $node.Name -eq 'Test-TailscaleInboundRule'
}, $false)
if (!$predicate) { throw 'Missing Tailscale-In predicate.' }
. ([scriptblock]::Create($predicate.Extent.Text))

function Assert-TailscaleRule([string]$Case, [bool]$Expected, [hashtable]$Change = @{},
    [string]$Program = 'Any', [string]$Service = 'Any') {
    $rule = @{
        DisplayName = 'Tailscale-In'
        PolicyStoreSource = 'PersistentStore'
        PolicyStoreSourceType = 'Local'
        Group = $null
        Owner = $null
        Profile = 'Domain, Private'
        EdgeTraversalPolicy = 'Block'
    }
    $port = @{ Protocol = 'Any'; LocalPort = 'Any'; RemotePort = 'Any' }
    $address = @{ LocalAddress = '100.97.109.109'; RemoteAddress = 'Any' }
    $interface = @{ InterfaceAlias = 'Any' }
    foreach ($filter in @($rule, $port, $address, $interface)) {
        foreach ($key in $Change.Keys) {
            if ($filter.ContainsKey($key)) { $filter[$key] = $Change[$key] }
        }
    }
    $actual = Test-TailscaleInboundRule ([pscustomobject]$rule) ([pscustomobject]$port) `
        ([pscustomobject]$address) ([pscustomobject]$interface) $Program $Service `
        @('100.97.109.109', 'fd7a:115c:a1e0::1901:444a')
    if ($actual -ne $Expected) { throw "${Case}: expected $Expected, got $actual" }
}

Assert-TailscaleRule 'observed IPv4' $true
Assert-TailscaleRule 'IPv4 host prefix' $true @{ LocalAddress = '100.97.109.109/32' }
Assert-TailscaleRule 'IPv6 host prefix' $true @{ LocalAddress = 'fd7a:115c:a1e0:0:0:0:1901:444a/128' }
Assert-TailscaleRule 'Private-only vendor profile' $true @{ Profile = 'Private' }
Assert-TailscaleRule 'different display name' $false @{ DisplayName = 'Other' }
Assert-TailscaleRule 'GPO source' $false @{ PolicyStoreSourceType = 'GroupPolicy' }
Assert-TailscaleRule 'different store' $false @{ PolicyStoreSource = 'OtherStore' }
Assert-TailscaleRule 'different rule group' $false @{ Group = 'Other' }
Assert-TailscaleRule 'different rule owner' $false @{ Owner = 'S-1-5-18' }
Assert-TailscaleRule 'LAN interface' $false @{ InterfaceAlias = 'Ethernet' }
Assert-TailscaleRule 'all destinations' $false @{ LocalAddress = 'Any' }
Assert-TailscaleRule 'different tailnet destination' $false @{ LocalAddress = '100.97.109.110' }
Assert-TailscaleRule 'multiple destinations' $false @{ LocalAddress = @('100.97.109.109', '192.168.1.2') }
Assert-TailscaleRule 'overbroad prefix' $false @{ LocalAddress = '100.97.109.109/24' }
Assert-TailscaleRule 'overbroad IPv6 prefix' $false @{ LocalAddress = 'fd7a:115c:a1e0::1901:444a/48' }
Assert-TailscaleRule 'public profile' $false @{ Profile = 'Public' }
Assert-TailscaleRule 'edge traversal' $false @{ EdgeTraversalPolicy = 'Allow' }
Assert-TailscaleRule 'explicit TCP' $false @{ Protocol = 'TCP' }
Assert-TailscaleRule 'explicit remote address' $false @{ RemoteAddress = '100.64.0.0/10' }
Assert-TailscaleRule 'explicit program' $false -Program 'C:\Windows\System32\OpenSSH\sshd.exe'
Assert-TailscaleRule 'explicit service' $false -Service 'sshd'

foreach ($functionName in 'Get-Port22Scope','Get-SshAllowDisposition') {
    $definition = $ast.Find({
        param($node)
        $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $functionName
    }, $false)
    if (!$definition) { throw "Missing $functionName." }
    . ([scriptblock]::Create($definition.Extent.Text))
}

function Assert-SshAllow([string]$Case, [string]$Expected, [hashtable]$Change = @{}) {
    $rule = @{ Profile = 'Any'; RawRule = $null }
    $port = @{ Protocol = 'TCP'; LocalPort = 'Any' }
    $application = @{ Program = 'Any'; Package = 'Any' }
    $service = @{ Service = 'Any' }
    $interface = @{ InterfaceAlias = 'Any' }
    $interfaceType = @{ InterfaceType = 'Any' }
    foreach ($filter in @($rule, $port, $application, $service, $interface, $interfaceType)) {
        foreach ($key in $Change.Keys) {
            if ($filter.ContainsKey($key)) { $filter[$key] = $Change[$key] }
        }
    }
    $profiles = @{ Ethernet = 'Private'; 'Office Wi-Fi' = 'Domain' }
    $actual = Get-SshAllowDisposition $rule (Get-Port22Scope $port) $application $service $interface $interfaceType `
        $profiles 'Tailscale' 'C:\Windows\System32\OpenSSH\sshd.exe' $rule.RawRule
    if ($actual -ne $Expected) { throw "${Case}: expected $Expected, got $actual" }
}

Assert-SshAllow 'unrestricted TCP on LAN' 'Block'
Assert-SshAllow 'configured sshd path' 'Block' @{ Program = 'C:\Windows\System32\OpenSSH\sshd.exe' }
Assert-SshAllow 'port range includes TCP 22' 'Block' @{ Protocol = '6'; LocalPort = @('20-24', '443') }
Assert-SshAllow 'domain profile on active IF' 'Block' @{ Profile = 'Domain'; InterfaceAlias = 'Office Wi-Fi' }
Assert-SshAllow 'System program' 'Warn' @{ Program = 'System' }
Assert-SshAllow 'System with Windows app PFN' 'Ignore' @{ Program = 'System'; Package = 'Microsoft.AAD.BrokerPlugin_cw5n1h2txyewy' }
Assert-SshAllow 'observed Windows app with hidden PFN' 'Ignore' @{
    Package = ''; RawRule = 'v2.33|Action=Allow|Dir=In|PFN=Microsoft.AAD.BrokerPlugin_cw5n1h2txyewy|LUOwn=S-1-5-18|'
}
Assert-SshAllow 'empty package with verified unrestricted local rule' 'Block' @{
    Package = ''; RawRule = 'v2.33|Action=Allow|Dir=In|Protocol=6|'
}
Assert-SshAllow 'empty package without persistent metadata' 'Warn' @{ Package = '' }
Assert-SshAllow 'owner metadata alone' 'Warn' @{ Package = ''; RawRule = 'v2.33|Action=Allow|LUOwn=S-1-5-18|' }
Assert-SshAllow 'AppContainer SID' 'Ignore' @{ Package = 'S-1-15-2-123456789-123456789' }
Assert-SshAllow 'another executable' 'Ignore' @{ Program = 'C:\Windows\System32\svchost.exe' }
Assert-SshAllow 'another service' 'Ignore' @{ Service = 'upnphost' }
Assert-SshAllow 'specific sshd service' 'Warn' @{ Service = 'sshd' }
Assert-SshAllow 'only Tailscale alias' 'Ignore' @{ InterfaceAlias = 'Tailscale' }
Assert-SshAllow 'Tailscale and LAN aliases' 'Block' @{ InterfaceAlias = @('Tailscale', 'Ethernet') }
Assert-SshAllow 'inactive profile on LAN' 'Ignore' @{ InterfaceAlias = 'Ethernet'; Profile = 'Public' }
Assert-SshAllow 'unknown interface profile' 'Warn' @{ InterfaceAlias = 'Unlisted' }
Assert-SshAllow 'wildcard program' 'Warn' @{ Program = 'C:\Windows\*' }
Assert-SshAllow 'interface type filter' 'Warn' @{ InterfaceType = 'Wireless' }
Assert-SshAllow 'other port' 'Ignore' @{ LocalPort = '443' }
Assert-SshAllow 'dynamic port' 'Warn' @{ LocalPort = 'RPC' }
Assert-SshAllow 'UDP' 'Ignore' @{ Protocol = 'UDP' }

foreach ($functionName in 'Get-RecoveryDisposition','Get-RecoveryArguments') {
    $definition = $ast.Find({
        param($node)
        $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $functionName
    }, $false)
    if (!$definition) { throw "Missing $functionName." }
    . ([scriptblock]::Create($definition.Extent.Text))
}

function Assert-Recovery([string]$Phase, [string]$State, [long]$Now, [string]$Expected) {
    $actual = Get-RecoveryDisposition $Phase $State 100 $Now
    if ($actual -ne $Expected) { throw "$Phase / $State at ${Now}: expected $Expected, got $actual" }
}

Assert-Recovery Confirm Applying 50 Reject
Assert-Recovery Confirm Pending 99 Confirm
Assert-Recovery Confirm Pending 100 Reject
Assert-Recovery Confirm RolledBack 101 Reject
Assert-Recovery Confirm Confirmed 101 Cleanup
Assert-Recovery Rollback Applying 100 Restore
Assert-Recovery Rollback Pending 101 Restore
Assert-Recovery Rollback Restoring 101 Restore
Assert-Recovery Rollback Confirmed 100 Cleanup
Assert-Recovery Rollback RolledBack 100 Cleanup
Assert-Recovery Rollback '' 100 Reject

$id = 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'
$backupPath = 'C:\Program Data\ssh\kio-v1-local-backup-' + $id
$expectedArgs = "-NoProfile -NonInteractive -ExecutionPolicy Bypass -File `"$backupPath\setup-windows.ps1`" -Phase Rollback -BackupId $id"
if ((Get-RecoveryArguments $backupPath $id) -cne $expectedArgs) { throw 'Unexpected recovery task arguments.' }

$commands = $ast.FindAll({ param($node) $node -is [System.Management.Automation.Language.CommandAst] }, $true)
if ($commands | Where-Object { $_.GetCommandName() -eq 'Read-Host' }) { throw 'Setup requires interactive input.' }
if (!($commands | Where-Object { $_.GetCommandName() -eq 'New-ScheduledTaskTrigger' -and $_.Extent.Text -match '\bOnce\b' })) {
    throw 'Missing one-time recovery trigger.'
}
if ($ast.ParamBlock.Extent.Text -notmatch '\$CiSource\s*=\s*''100\.64\.0\.1''') {
    throw 'Unexpected default CI source for sshd configuration evaluation.'
}
