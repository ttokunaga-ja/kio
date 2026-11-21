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
