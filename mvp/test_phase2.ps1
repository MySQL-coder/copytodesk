$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$bin = Join-Path $root 'bin'
$logsDir = Join-Path $root 'logs'
$shareDir = Join-Path $logsDir 'test-share'
$auditLog = Join-Path $logsDir 'mcp-phase2-audit.jsonl'
New-Item -ItemType Directory -Force -Path $logsDir, $shareDir | Out-Null
if (Test-Path $auditLog) { Remove-Item $auditLog -Force }

$token = 'p2_secret_token'
$deviceId = '888100200'
$httpPort = 19900
$relayPort = 19901

$relayLog = Join-Path $logsDir 'relay.log'
$hostLog = Join-Path $logsDir 'host-p2.log'

Write-Host '=== [1] Starting Signaling + Relay + Cross-Platform Web Viewer ==='
$relayProc = Start-Process -FilePath 'node' `
    -ArgumentList "`"$root\server\signaling-relay.mjs`" --bind 127.0.0.1 --http-port $httpPort --relay-port $relayPort" `
    -RedirectStandardOutput $relayLog `
    -NoNewWindow -PassThru

$hostProc = $null
try {
    Start-Sleep -Milliseconds 700

    Write-Host '=== [2] Starting CopyToDesk.Host (Outbound Relay Registration + Allowlisted Exec + File Sandbox) ==='
    $hostProc = Start-Process -FilePath "$bin\CopyToDesk.Host.exe" `
        -ArgumentList "--bind 127.0.0.1 --port 19820 --token $token --fps 30 --synthetic --relay-host 127.0.0.1 --relay-port $relayPort --device-id $deviceId --device-name Win-Dev-Host --file-root `"$shareDir`" --allow-exec --exec-allowlist echo,hostname,whoami" `
        -RedirectStandardOutput $hostLog `
        -NoNewWindow -PassThru

    Start-Sleep -Milliseconds 1400

    Write-Host ''
    Write-Host '=== [3] Signaling HTTP API & Zero-GPU Web Viewer Verification ==='
    $devs = Invoke-RestMethod -Uri "http://127.0.0.1:$httpPort/api/devices" -Method Get
    Write-Host ('Online devices: ' + ($devs | ConvertTo-Json -Compress))
    $html = Invoke-WebRequest -Uri "http://127.0.0.1:$httpPort/" -UseBasicParsing
    Write-Host ("Web Viewer HTML status=$($html.StatusCode) bytes=$($html.Content.Length)")

    $shotReq = @{ deviceId = $deviceId; token = $token; maxWidth = 640; quality = 70 } | ConvertTo-Json -Compress
    $shotResp = Invoke-WebRequest -Uri "http://127.0.0.1:$httpPort/api/screenshot" -Method Post -ContentType 'application/json' -Body $shotReq -UseBasicParsing
    Write-Host ("Web Viewer /api/screenshot status=$($shotResp.StatusCode) contentType=$($shotResp.Headers['Content-Type']) bytes=$($shotResp.RawContentLength)")
    $clipResp = Invoke-RestMethod -Uri "http://127.0.0.1:$httpPort/api/clipboard" -Method Post -ContentType 'application/json' -Body (@{ deviceId = $deviceId; token = $token; action = 'set'; text = 'UnifiedUI-OK' } | ConvertTo-Json -Compress)
    $execHttp = Invoke-RestMethod -Uri "http://127.0.0.1:$httpPort/api/exec" -Method Post -ContentType 'application/json' -Body (@{ deviceId = $deviceId; token = $token; command = 'echo Unified_UI_HTTP_OK' } | ConvertTo-Json -Compress)
    Write-Host ("Unified UI HTTP APIs -> clipboard=$($clipResp.text) exec=$($execHttp.stdout.Trim())")

    Write-Host ''
    Write-Host '=== [4] Native Viewer Loopback Benchmark THROUGH RELAY (30 frames) ==='
    $benchOut = & "$bin\CopyToDesk.Bench.exe" --relay-host 127.0.0.1 --relay-port $relayPort --device-id $deviceId --token $token --headless-bench 30
    Write-Host $benchOut

    Write-Host ''
    Write-Host '=== [5] MCP Server v2 THROUGH RELAY (13 Tools: Clipboard + SHA-256 File Transfer + Sandbox + Guarded Exec) ==='
    $sampleText = "Hello Phase2 CopyToDesk! SHA256-Verified-File-Payload-2026"
    $sampleB64 = [Convert]::ToBase64String([System.Text.Encoding]::UTF8.GetBytes($sampleText))
    $mcpScript = Join-Path $root 'mcp-server\index.mjs'
    $reqs = @(
        '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"p2-e2e","version":"1.0"}}}'
        '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
        '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"remote_clipboard_set","arguments":{"text":"CopyToDesk-Phase2-Clip-OK"}}}'
        '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"remote_clipboard_get","arguments":{}}}'
        ("{`"jsonrpc`":`"2.0`",`"id`":5,`"method`":`"tools/call`",`"params`":{`"name`":`"remote_file_upload`",`"arguments`":{`"remotePath`":`"sub/hello.txt`",`"contentBase64`":`"$sampleB64`"}}}")
        '{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"remote_list_dir","arguments":{"remotePath":"sub"}}}'
        '{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"remote_file_download","arguments":{"remotePath":"sub/hello.txt"}}}'
        ("{`"jsonrpc`":`"2.0`",`"id`":8,`"method`":`"tools/call`",`"params`":{`"name`":`"remote_file_upload`",`"arguments`":{`"remotePath`":`"../../escape.txt`",`"contentBase64`":`"$sampleB64`"}}}")
        '{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"remote_exec","arguments":{"command":"echo Phase2_Exec_OK"}}}'
        '{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"remote_exec","arguments":{"command":"del /f important.txt"}}}'
        '{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"remote_exec","arguments":{"command":"echo hi & calc"}}}'
    )
    $mcpOut = ($reqs -join "`n") | & node $mcpScript --relay-host 127.0.0.1 --relay-port $relayPort --device-id $deviceId --token $token --audit $auditLog
    foreach ($line in $mcpOut) {
        if ($line.Length -gt 320) {
            Write-Host ($line.Substring(0, 320) + "... [len=$($line.Length)]")
        } else {
            Write-Host $line
        }
    }

    Write-Host ''
    Write-Host '=== [6] Relay Health & Audit Log ==='
    $health = Invoke-RestMethod -Uri "http://127.0.0.1:$httpPort/api/health" -Method Get
    Write-Host ('Relay health: ' + ($health | ConvertTo-Json -Compress))
    Write-Host '--- Audit Log ---'
    Get-Content $auditLog
}
finally {
    if ($hostProc -and -not $hostProc.HasExited) { Stop-Process -Id $hostProc.Id -Force -ErrorAction SilentlyContinue }
    if ($relayProc -and -not $relayProc.HasExited) { Stop-Process -Id $relayProc.Id -Force -ErrorAction SilentlyContinue }
    Start-Sleep -Milliseconds 300
}
