$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$bin = Join-Path $root 'bin'
$token = 'phase1_verify_token'
$port = 19811
$auditLog = Join-Path $root 'logs\mcp-audit.jsonl'
if (Test-Path $auditLog) { Remove-Item $auditLog -Force }

Write-Host '=== [Test 1] Standalone Out-of-Process CodecProbe ==='
& "$bin\CopyToDesk.CodecProbe.exe"

Write-Host ''
Write-Host '=== [Test 2] Starting CopyToDesk.Host (Synthetic 1280x720@30fps, DryRun-Audit input) ==='
$hostLog = Join-Path $root 'logs\host-test.log'
New-Item -ItemType Directory -Force -Path (Join-Path $root 'logs') | Out-Null
$hostProc = Start-Process -FilePath "$bin\CopyToDesk.Host.exe" `
    -ArgumentList "--bind 127.0.0.1 --port $port --token $token --fps 30 --synthetic" `
    -RedirectStandardOutput $hostLog `
    -NoNewWindow -PassThru

try {
    Start-Sleep -Milliseconds 1200

    Write-Host ''
    Write-Host '=== [Test 3] End-to-End Video + Compound Tile + Cursor + Input + Screenshot Loopback Benchmark (40 frames) ==='
    $benchOut = & "$bin\CopyToDesk.Bench.exe" --host 127.0.0.1 --port $port --token $token --headless-bench 40
    Write-Host $benchOut

    Write-Host ''
    Write-Host '=== [Test 4] MCP Server JSON-RPC 2.0 Verification (initialize, tools/list, screen_info, screenshot, click, type, hotkey) ==='
    $mcpScript = Join-Path $root 'mcp-server\index.mjs'
    $requests = @(
        '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-test","version":"1.0"}}}'
        '{"jsonrpc":"2.0","method":"notifications/initialized"}'
        '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
        '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"remote_screen_info","arguments":{}}}'
        '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"remote_screenshot","arguments":{"maxWidth":640,"quality":70,"format":"jpeg"}}}'
        '{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"remote_click","arguments":{"x":320,"y":240,"button":"left","clicks":1}}}'
        '{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"remote_type","arguments":{"text":"Hello CopyToDesk"}}}'
        '{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"remote_hotkey","arguments":{"keys":["ctrl","s"]}}}'
    )
    $mcpOut = ($requests -join "`n") | & node $mcpScript --host 127.0.0.1 --port $port --token $token --audit $auditLog
    foreach ($line in $mcpOut) {
        if ($line.Length -gt 280) {
            Write-Host ($line.Substring(0, 280) + "... [len=$($line.Length)]")
        } else {
            Write-Host $line
        }
    }

    Write-Host ''
    Write-Host '=== [Test 5] MCP Audit Log Verification ==='
    if (Test-Path $auditLog) {
        Get-Content $auditLog
    } else {
        throw 'Audit log was not created!'
    }
}
finally {
    if ($hostProc -and -not $hostProc.HasExited) {
        Stop-Process -Id $hostProc.Id -Force -ErrorAction SilentlyContinue
        Start-Sleep -Milliseconds 300
    }
}

Write-Host ''
Write-Host '=== Host Stdout Log ==='
if (Test-Path $hostLog) { Get-Content $hostLog }

Write-Host ''
Write-Host '=== [Test 6] Real Desktop Capture Path Smoke Test (DXGI-DD / GDI-BitBlt) ==='
$realPort = 19812
$realLog = Join-Path $root 'logs\host-real.log'
$realProc = Start-Process -FilePath "$bin\CopyToDesk.Host.exe" `
    -ArgumentList "--bind 127.0.0.1 --port $realPort --token $token --fps 30" `
    -RedirectStandardOutput $realLog `
    -NoNewWindow -PassThru
try {
    Start-Sleep -Milliseconds 1200
    $realBench = & "$bin\CopyToDesk.Bench.exe" --host 127.0.0.1 --port $realPort --token $token --headless-bench 2
    Write-Host $realBench
}
finally {
    if ($realProc -and -not $realProc.HasExited) {
        Stop-Process -Id $realProc.Id -Force -ErrorAction SilentlyContinue
        Start-Sleep -Milliseconds 200
    }
}
if (Test-Path $realLog) { Get-Content $realLog }
