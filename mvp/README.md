# CopyToDesk Clean-Room MVP (Phase 1 + Phase 2)

零外部依赖的跨平台架构远控 MVP，无独立显卡设备也可全功能运行（自动降级纯 CPU 复合分块编码 + Canvas2D 零显卡 Web Viewer）：

- **硬件探针（`CopyToDesk.CodecProbe.exe`）**：独立子进程探测 DXGI 显卡、D3D11 与 Media Foundation 硬件编解码能力。
- **信令与中继服 + 跨平台 Web Viewer（`server/signaling-relay.mjs`）**：
  - NAT 友好的外连设备注册（9 位设备码）与透明双向 TCP 中继。
  - 内置零显卡 **HTML5 Canvas2D Web Viewer**（Windows/macOS/Linux/iPad 浏览器即开即控）。
- **被控端（`CopyToDesk.Host.exe`）**：支持局域网直连或外连中继注册，支持 `DXGI-DD` → `GDI-BitBlt` 自动降级、128×128 有损+无损复合编码、120Hz 独立光标、双向剪贴板、SHA-256 沙箱文件传输、以及白名单受控 `Exec`。
- **控制端（`CopyToDesk.Viewer.exe`）**：原生 WinForms 控制端，支持直连或通过中继设备码连接。
- **MCP Server（`mcp-server/index.mjs`）**：13 个 AI 工具（GUI 7 + 剪贴板 2 + 文件传输 3 + 受控命令 1），支持直连或中继寻址，带 JSONL 全链路审计。配置示例见 `mcp-server/mcp-config.example.json`。

## 编译

```powershell
powershell -ExecutionPolicy Bypass -File mvp\build.ps1
```

## Phase 2 完整中继模式快速启动

```powershell
# 1. 启动信令 + 中继服 + 跨平台 Web Viewer (http://127.0.0.1:19900)
node .\mvp\server\signaling-relay.mjs --bind 127.0.0.1 --http-port 19900 --relay-port 19901

# 2. 启动被控端并外连注册到中继（设备号 888100200）
.\mvp\bin\CopyToDesk.Host.exe --token demo123 --relay-host 127.0.0.1 --relay-port 19901 --device-id 888100200 --allow-exec

# 3. 方式 A：原生桌面 Viewer 通过中继连接
.\mvp\bin\CopyToDesk.Viewer.exe --relay-host 127.0.0.1 --relay-port 19901 --device-id 888100200 --token demo123

# 3. 方式 B：任意系统浏览器访问 http://127.0.0.1:19900 选择设备 888100200 输入 demo123

# 3. 方式 C：接入 AI 客户端 MCP（参考 mcp-server/mcp-config.example.json）
```
