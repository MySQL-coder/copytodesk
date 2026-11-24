# CopyToDesk (Clean-Room Cross-Platform Remote Desktop)

Clean-Room 跨平台远程控制与 AI MCP 工作台，支持 **Windows / macOS / Linux**，无独立显卡设备可自动降级纯 CPU 复合分块编码与零显卡 Canvas2D 渲染。

## 仓库结构

- **`app/`**：Tauri 2 + Rust 跨平台桌面应用包工程（产出 Windows `.exe/.msi`、macOS Universal `.dmg`、Linux `.AppImage/.deb`）
  - `app/crates/ctd-protocol`：`CTD1` 二进制线协议（含中继隧道握手）
  - `app/crates/ctd-core`：跨平台采集、128×128 脏块复合编码（运动 JPEG + 静止 PNG 无损精修）、Host/Client、剪贴板、SHA-256 沙箱文件传输、白名单受控 Exec
  - `app/src-tauri` + `app/ui`：Tauri 2 桌面壳与双模统一前端
- **`mvp/`**：零外部依赖的 Windows 原生 Phase 1/2 MVP（`csc.exe` 1 秒编译）+ Node.js 24 信令中继服与零显卡 Web Viewer + 13 工具 MCP Server
- **`docs/`**：完整架构设计与各模块详细技术规范
- **`.github/workflows/build-multiplatform.yml`**：GitHub Actions 三端自动测试与打包流水线

## 快速获取三端安装包

在 GitHub 仓库的 **Actions → Build Multi-Platform App Bundles** 中下载最新构建产物（Artifacts）：
- `copytodesk-windows-x64`（`.exe` / `.msi`）
- `copytodesk-macos-universal`（`.dmg`）
- `copytodesk-linux-x64`（`.AppImage` / `.deb`）
