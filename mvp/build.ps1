$ErrorActionPreference = 'Stop'

$csc = 'C:\Windows\Microsoft.NET\Framework64\v4.0.30319\csc.exe'
if (-not (Test-Path $csc)) {
    throw "csc.exe not found at $csc"
}

$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$src = Join-Path $root 'src'
$bin = Join-Path $root 'bin'
New-Item -ItemType Directory -Force -Path $bin | Out-Null

Write-Host '[1/3] Building CopyToDesk.CodecProbe.exe ...'
& $csc /nologo /platform:x64 /optimize+ /target:exe `
    /out:"$bin\CopyToDesk.CodecProbe.exe" `
    "$src\CodecProbe.cs"
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host '[2/3] Building CopyToDesk.Host.exe ...'
& $csc /nologo /platform:x64 /optimize+ /target:exe `
    /r:System.Drawing.dll `
    /out:"$bin\CopyToDesk.Host.exe" `
    "$src\Protocol.cs" "$src\Capture.cs" "$src\Codec.cs" "$src\Input.cs" "$src\HostProgram.cs"
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host '[3/3] Building CopyToDesk.Viewer.exe ...'
& $csc /nologo /platform:x64 /optimize+ /target:winexe `
    /r:System.Drawing.dll /r:System.Windows.Forms.dll `
    /out:"$bin\CopyToDesk.Viewer.exe" `
    "$src\Protocol.cs" "$src\Capture.cs" "$src\ViewerProgram.cs"
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Also build a console-attached Viewer runner for CLI headless benchmarks
Write-Host '[Bonus] Building CopyToDesk.Bench.exe ...'
& $csc /nologo /platform:x64 /optimize+ /target:exe `
    /r:System.Drawing.dll /r:System.Windows.Forms.dll `
    /out:"$bin\CopyToDesk.Bench.exe" `
    "$src\Protocol.cs" "$src\Capture.cs" "$src\ViewerProgram.cs"
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host 'Build succeeded! Binaries in mvp/bin:'
Get-ChildItem $bin -Filter '*.exe' | Select-Object Name, Length | Format-Table -AutoSize
