# Only these omitted values are equivalent to explicitly serialized defaults.
# https://learn.microsoft.com/windows/win32/taskschd/task-scheduler-schema
$taskDefaults = [ordered]@{
    'Task/Principals/Principal/RunLevel' = 'LeastPrivilege'
    'Task/Actions/Exec/WorkingDirectory' = ''
    'Task/Triggers/LogonTrigger/UserId' = ''
    'Task/Triggers/LogonTrigger/Delay' = 'PT0S'
    'Task/Triggers/LogonTrigger/Enabled' = 'true'
    'Task/Triggers/LogonTrigger/ExecutionTimeLimit' = 'PT72H'
    'Task/Settings/AllowStartOnDemand' = 'true'
    'Task/Settings/MultipleInstancesPolicy' = 'IgnoreNew'
    'Task/Settings/DisallowStartIfOnBatteries' = 'true'
    'Task/Settings/StopIfGoingOnBatteries' = 'true'
    'Task/Settings/AllowHardTerminate' = 'true'
    'Task/Settings/StartWhenAvailable' = 'false'
    'Task/Settings/RunOnlyIfNetworkAvailable' = 'false'
    'Task/Settings/WakeToRun' = 'false'
    'Task/Settings/Enabled' = 'true'
    'Task/Settings/Hidden' = 'false'
    'Task/Settings/DeleteExpiredTaskAfter' = 'PT0S'
    'Task/Settings/ExecutionTimeLimit' = 'PT72H'
    'Task/Settings/Priority' = '7'
    'Task/Settings/RunOnlyIfIdle' = 'false'
    'Task/Settings/UseUnifiedSchedulingEngine' = 'false'
    'Task/Settings/DisallowStartOnRemoteAppSession' = 'false'
    'Task/Settings/Volatile' = 'false'
    'Task/Settings/IdleSettings' = ''
    'Task/Settings/IdleSettings/Duration' = 'PT10M'
    'Task/Settings/IdleSettings/WaitTimeout' = 'PT1H'
    'Task/Settings/IdleSettings/StopOnIdleEnd' = 'true'
    'Task/Settings/IdleSettings/RestartOnIdle' = 'false'
}
function Resolve-TaskSid([string]$identity) {
    if ($identity -cmatch '^S-[0-9-]+$') {
        return [Security.Principal.SecurityIdentifier]::new($identity).Value
    }
    return [Security.Principal.NTAccount]::new($identity).Translate(
        [Security.Principal.SecurityIdentifier]).Value
}
function Read-TaskFields([string]$xml, [scriptblock]$resolveSid) {
    $settings = New-Object System.Xml.XmlReaderSettings
    $settings.DtdProcessing = [System.Xml.DtdProcessing]::Prohibit
    $settings.MaxCharactersInDocument = 65536
    $reader = [System.Xml.XmlReader]::Create([IO.StringReader]::new($xml), $settings)
    try {
        $document = New-Object System.Xml.XmlDocument
        $document.PreserveWhitespace = $true
        $document.Load($reader)
    } finally { $reader.Dispose() }
    foreach ($node in $document.get_ChildNodes()) {
        if ($node.get_NodeType() -notin 'Element', 'XmlDeclaration', 'Comment', 'Whitespace') {
            throw 'Unexpected task XML document node'
        }
    }
    $fields = [Collections.Generic.Dictionary[string,string]]::new([StringComparer]::Ordinal)
    function Visit-TaskElement($node, [string]$key, [int]$depth) {
        if ($depth -gt 12 -or $key.Length -gt 180) { throw 'Task XML exceeds ownership inspection limits' }
        if ($node.get_NamespaceURI() -cne 'http://schemas.microsoft.com/windows/2004/02/mit/task') {
            throw ('Unexpected task XML namespace at ' + $key)
        }
        if ($fields.ContainsKey($key)) { throw ('Duplicate task XML field ' + $key) }
        foreach ($attribute in $node.get_Attributes()) {
            if ($attribute.NamespaceURI -ceq 'http://www.w3.org/2000/xmlns/') { continue }
            if ($attribute.NamespaceURI) { throw ('Unexpected task XML attribute namespace at ' + $key) }
            $fields.Add(($key + '/@' + $attribute.Name), $attribute.Value)
        }
        $elements = @($node.get_ChildNodes() | Where-Object { $_.get_NodeType() -eq 'Element' })
        $text = ''
        foreach ($child in $node.get_ChildNodes()) {
            switch ($child.get_NodeType()) {
                'Element' { }
                'Comment' { }
                { $_ -in 'Text', 'CDATA', 'Whitespace', 'SignificantWhitespace' } { $text += $child.get_Value() }
                default { throw ('Unexpected task XML node at ' + $key) }
            }
        }
        if ($elements.Count -gt 0) {
            if (![string]::IsNullOrWhiteSpace($text)) { throw ('Mixed task XML content at ' + $key) }
            $text = ''
        }
        if ($key -cin @('Task/Principals/Principal/UserId', 'Task/Triggers/LogonTrigger/UserId') -and $text) {
            try { $text = & $resolveSid $text }
            catch { throw ('Cannot resolve task SID at ' + $key) }
        }
        $fields.Add($key, $text)
        foreach ($element in $elements) {
            Visit-TaskElement $element ($key + '/' + $element.get_LocalName()) ($depth + 1)
        }
    }
    $root = $document.get_DocumentElement()
    Visit-TaskElement $root $root.get_LocalName() 1
    if (!$fields.ContainsKey('Task')) { throw 'Expected Task XML root' }
    return ,$fields
}
function Convert-TaskField([string]$key, [string]$value, [string]$default) {
    try {
        if ($default -cin @('true', 'false')) {
            return [Xml.XmlConvert]::ToBoolean($value.Trim())
        }
        if ($default.StartsWith('PT', [StringComparison]::Ordinal)) {
            # Task durations here are fixed-day/time intervals. Do not conflate
            # calendar years or months with a fixed number of seconds.
            if ($value.Trim() -cnotmatch '^P(?:[0-9]+D)?(?:T[0-9HMS.]+)?$') { return $null }
            return [Xml.XmlConvert]::ToTimeSpan($value.Trim())
        }
        if ($key -ceq 'Task/Settings/Priority') { return [Xml.XmlConvert]::ToInt32($value.Trim()) }
        return $value
    } catch { return $null }
}
function Compare-TaskDefinition($actual, $expected, [string]$registeredPath, [string]$taskId) {
    $count = 0
    if ($registeredPath -cne ('\' + $taskId)) { 'RegisteredTask/Path'; $count++ }
    $keys = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    $keys.UnionWith($actual.Keys)
    $keys.UnionWith($expected.Keys)
    $orderedKeys = [string[]]@($keys)
    [Array]::Sort($orderedKeys, [StringComparer]::Ordinal)
    foreach ($key in $orderedKeys) {
        $hasActual = $actual.ContainsKey($key)
        $hasExpected = $expected.ContainsKey($key)
        if ($key -ceq 'Task/@version' -and $actual[$key] -cin @('1.2', '1.3', '1.4', '1.5', '1.6') -and
            $expected[$key] -cin @('1.2', '1.3', '1.4', '1.5', '1.6')) { continue }
        # Scheduler-authored registration metadata with no effect on launch.
        if (!$hasExpected -and $key -cin @('Task/RegistrationInfo/Author', 'Task/RegistrationInfo/Date')) { continue }
        if ($key -ceq 'Task/RegistrationInfo/URI' -and !$hasActual -and
            $expected[$key] -ceq $registeredPath) { continue }
        # The host can materialize either engine selector when the template
        # leaves it unspecified. This exception applies only to this one field.
        if (!$hasExpected -and $key -ceq 'Task/Settings/UseUnifiedSchedulingEngine' -and
            $null -ne (Convert-TaskField $key $actual[$key] 'false')) { continue }
        if ($taskDefaults.Keys -ccontains $key) {
            $default = $taskDefaults[$key]
            $left = if ($hasActual) { Convert-TaskField $key $actual[$key] $default } else { Convert-TaskField $key $default $default }
            $right = if ($hasExpected) { Convert-TaskField $key $expected[$key] $default } else { Convert-TaskField $key $default $default }
            $different = $null -eq $left -or $null -eq $right -or $left -cne $right
        } else {
            $different = $hasActual -ne $hasExpected -or $actual[$key] -cne $expected[$key]
        }
        if ($different) {
            $key
            $count++
            if ($count -ge 10) { break }
        }
    }
}

function Limit-RegisteredXml([string]$xml) {
    if (!$xml) { return $null }
    $bytes = [Text.Encoding]::UTF8.GetBytes($xml)
    if ($bytes.Length -le 8192) { return $xml }
    $end = 8192
    while (($bytes[$end] -band 192) -eq 128) { $end-- }
    return [Text.Encoding]::UTF8.GetString($bytes, 0, $end)
}
