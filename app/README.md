# CopyToDesk 跨平台桌面应用包（Tauri 2 + Rust）

支持从同一套 Rust 核心引擎与统一 UI，自动打包出三大桌面平台安装包（单包约 10~15 MB，无显卡设备自动降级纯 CPU 复合编码 + Canvas2D 渲染）：

- **Windows**: `.exe` (NSIS) / `.msi`
- **macOS**: Universal `.dmg` (`arm64` Apple Silicon + `x86_64` Intel)
- **Linux**: `.AppImage` / `.deb`

## 工程结构

```text
app/
├── Cargo.toml                 # Rust Workspace
├── package.json               # Tauri 2 CLI 脚本
├── crates/
│   ├── ctd-protocol/          # CTD1 二进制线协议 (与 mvp/ 字节级互通)
│   └── ctd-core/              # 跨平台采集 / 复合编码 / Host / Client / 中继 / 文件 / 受控 Exec
├── src-tauri/                 # Tauri 2 桌面应用壳与多平台打包配置 (tauri.conf.json)
└── ui/index.html              # 双模统一前端 (桌面端走 Tauri IPC，浏览器走 Relay HTTP)
```

## 方式一：零本地环境云端一键出三端包（推荐）

直接推送仓库到 GitHub 并触发 [build-multiplatform.yml](../.github/workflows/build-multiplatform.yml)（打 `v*` 标签或在 Actions 页面点击 **Run workflow**），即可在云端同时产出 Windows、macOS Universal、Linux 安装包。

## 方式二：本机编译打包（Windows）

1. 安装 Rust 工具链（`winget install Rustlang.Rustup`）与 Visual Studio C++ Build Tools。
2. 执行打包：
```powershell
cd app
npm install
npm run build:win
```
安装包将生成在 `app/target/release/bundle/` 下。
