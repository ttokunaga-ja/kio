$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false)
function Error-Code($errorRecord) {
    $exception = $errorRecord.Exception
    while ($null -ne $exception.InnerException) { $exception = $exception.InnerException }
    return ([long]$exception.HResult -band 4294967295L)
}
try {
    $request = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String([Console]::In.ReadLine())) | ConvertFrom-Json
    $service = New-Object -ComObject 'Schedule.Service'
    $service.Connect()
    $folder = $service.GetFolder('\')
    function Canonical($xml) {
        $definition = $service.NewTask(0)
        $definition.XmlText = $xml
        $document = New-Object System.Xml.XmlDocument
        $document.PreserveWhitespace = $false
        $document.LoadXml($definition.XmlText)
        return $document.OuterXml
    }
    $script:lookupCode = 0
    function Get-Registered {
        try { $registered = $folder.GetTask($request.id); $script:lookupCode = 0; return $registered }
        catch {
            $code = Error-Code $_
            if ($code -eq 2147942402L -or $code -eq 2147942403L) { $script:lookupCode = $code; return $null }
            throw
        }
    }
    function Assert-Owned($registered) {
        if ($null -eq $registered) { throw 'Registered task disappeared' }
        $actual = Canonical $registered.Definition.XmlText
        foreach ($enabled in $request.allowed) {
            if ($actual -ceq $expected[[string]$enabled]) { return }
        }
        throw 'Registered task differs from the private definition'
    }
    function Stop-Registered($registered) {
        try { $registered.Stop(0) }
        catch { if ((Error-Code $_) -ne 2147750667L) { throw } }
        $deadline = [DateTime]::UtcNow.AddSeconds(8)
        while ($registered.GetInstances(0).Count -ne 0) {
            if ([DateTime]::UtcNow -ge $deadline) { throw 'Task instances did not stop' }
            Start-Sleep -Milliseconds 25
        }
    }
    $task = Get-Registered
    $expected = @{}
    $definition = $null
    if ($request.template -and ($null -ne $task -or $request.operation -eq 'create')) {
        $definition = $service.NewTask(0)
        $definition.XmlText = $request.template
        $definition.Principal.UserId = [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
        $definition.Settings.Enabled = $true
        $expected['true'] = Canonical $definition.XmlText
        $definition.Settings.Enabled = $false
        $expected['false'] = Canonical $definition.XmlText
    }
    switch ($request.operation) {
        'query' { }
        'create' {
            if ($null -ne $task) { throw 'Refusing to overwrite an existing task' }
            $definition.Settings.Enabled = $true
            $sid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
            $task = $folder.RegisterTaskDefinition($request.id, $definition, 2, $sid, $null, 3, $null)
            Assert-Owned $task
        }
        'enable' { Assert-Owned $task; $task.Enabled = $true }
        'start' { Assert-Owned $task; [void]$task.Run($null) }
        'stop' {
            Assert-Owned $task
            $task.Enabled = $false
            $task = Get-Registered
            Assert-Owned $task
            Stop-Registered $task
        }
        'delete' {
            if ($null -ne $task) {
                Assert-Owned $task
                Stop-Registered $task
                $task = Get-Registered
                Assert-Owned $task
                $folder.DeleteTask($request.id, 0)
                $task = $null
            }
        }
        default { throw 'Unknown native task operation' }
    }
    if ($request.operation -ne 'query') { $task = Get-Registered }
    $result = @{ hresult = 0; lookup_hresult = $script:lookupCode; present = ($null -ne $task); actual = $null; enabled_xml = $expected['true']; disabled_xml = $expected['false']; state = 0; arguments = $null }
    if ($null -ne $task) {
        $result.actual = Canonical $task.Definition.XmlText
        $result.state = [int]$task.State
        if ($task.Definition.Actions.Count -eq 1 -and $task.Definition.Actions.Item(1).Type -eq 0) {
            $result.arguments = [string]$task.Definition.Actions.Item(1).Arguments
        }
    }
    $result | ConvertTo-Json -Depth 4 -Compress
} catch {
    @{ hresult = (Error-Code $_); lookup_hresult = 0; present = $false; actual = $null; enabled_xml = $null; disabled_xml = $null; state = 0; arguments = $null } | ConvertTo-Json -Compress
}
