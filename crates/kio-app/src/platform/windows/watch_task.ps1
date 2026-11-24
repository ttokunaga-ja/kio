$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false)
function Error-Code($errorRecord) {
    $exception = $errorRecord.Exception
    while ($null -ne $exception.InnerException) { $exception = $exception.InnerException }
    return ([long]$exception.HResult -band 4294967295L)
}
try {
    $phase = 'decode request'
    $request = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String([Console]::In.ReadLine())) | ConvertFrom-Json
    $phase = 'connect to root folder'
    $service = New-Object -ComObject 'Schedule.Service'
    $service.Connect()
    $folder = $service.GetFolder('\')
    $script:lookupCode = 0
    function Get-Registered {
        try { $registered = $folder.GetTask($request.id); $script:lookupCode = 0; return $registered }
        catch {
            $code = Error-Code $_
            if ($code -eq 2147942402L -or $code -eq 2147942403L) { $script:lookupCode = $code; return $null }
            throw
        }
    }
    function Read-RegisteredFields($registered) {
        $xml = $registered.Definition.XmlText
        $script:registeredXml = Limit-RegisteredXml $xml
        $script:differingFields = $null
        Read-TaskFields $xml ${function:Resolve-TaskSid}
    }
    function Assert-Owned($registered) {
        if ($null -eq $registered) { throw 'Registered task disappeared' }
        $fields = Read-RegisteredFields $registered
        $firstDifference = $null
        foreach ($enabled in $request.allowed) {
            $expected['Task/Settings/Enabled'] = [string]$enabled
            $difference = @(Compare-TaskDefinition $fields $expected $registered.Path $request.id)
            if (!$difference) { $script:registeredXml = $null; return }
            if ($null -eq $firstDifference) { $firstDifference = $difference }
        }
        if (!$firstDifference) { throw 'No authorized task enable state' }
        $script:differingFields = $firstDifference
        throw 'Registered task definition differs'
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
    $phase = 'look up task'
    $task = Get-Registered
    $phase = 'prepare expected definition'
    $expected = $null
    $definition = $null
    if ($request.template -and ($null -ne $task -or $request.operation -eq 'create')) {
        $sid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
        $expected = Read-TaskFields $request.template ${function:Resolve-TaskSid}
        $expected['Task/Principals/Principal/UserId'] = $sid
        if ($request.operation -eq 'create') {
            $definition = $service.NewTask(0)
            $definition.XmlText = $request.template
            $definition.Principal.UserId = $sid
        }
    }
    $phase = 'apply operation'
    switch ($request.operation) {
        'query' { }
        'create' {
            if ($null -ne $task) { throw 'Refusing to overwrite an existing task' }
            $definition.Settings.Enabled = $true
            $phase = 'RegisterTaskDefinition'
            $task = $folder.RegisterTaskDefinition($request.id, $definition, 2, $sid, $null, 3, $null)
            $phase = 'verify created definition'
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
    $phase = 'read resulting definition'
    if ($request.operation -ne 'query') { $task = Get-Registered }
    $result = @{ hresult = 0; lookup_hresult = $script:lookupCode; present = ($null -ne $task); enabled_mismatch = $null; disabled_mismatch = $null; state = 0; arguments = $null }
    if ($null -ne $task) {
        if ($null -ne $expected) {
            $fields = Read-RegisteredFields $task
            $expected['Task/Settings/Enabled'] = 'true'
            $result.enabled_mismatch = @(Compare-TaskDefinition $fields $expected $task.Path $request.id)
            $expected['Task/Settings/Enabled'] = 'false'
            $result.disabled_mismatch = @(Compare-TaskDefinition $fields $expected $task.Path $request.id)
            if ($result.enabled_mismatch -or $result.disabled_mismatch) { $result.registered_xml = $script:registeredXml }
        }
        $result.state = [int]$task.State
        if ($task.Definition.Actions.Count -eq 1 -and $task.Definition.Actions.Item(1).Type -eq 0) {
            $result.arguments = [string]$task.Definition.Actions.Item(1).Arguments
        }
    }
    $result | ConvertTo-Json -Depth 4 -Compress
} catch {
    $errorRecord = $_
    $exception = $errorRecord.Exception
    $chain = @(for ($i = 0; $i -lt 6 -and $null -ne $exception; $i++) {
        $message = [string]$exception.Message
        if ($message.Length -gt 384) { $message = $message.Substring(0, 384) }
        @{ hresult = ([long]$exception.HResult -band 4294967295L); message = $message }
        $exception = $exception.InnerException
    })
    @{ phase = $phase; exceptions = $chain; registered_xml = $script:registeredXml; differing_fields = $script:differingFields; hresult = (Error-Code $errorRecord); lookup_hresult = 0; present = $false; enabled_mismatch = $null; disabled_mismatch = $null; state = 0; arguments = $null } | ConvertTo-Json -Depth 4 -Compress
}
