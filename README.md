# CopyToDesk — Clean-Room Cross-Platform Remote Desktop (`v0.7.0`)

Clean-Room 跨平台远程控制工作台（**Windows / macOS Universal / Linux**），支持零独立显卡纯 CPU 高效复合瓦片推流、端到端加密（E2EE）与局域网 P2P 自动直连竞速。

## 下载最新三端安装包

直接前往 **[GitHub Releases](https://github.com/MySQL-coder/copytodesk/releases)** 页面即可一键下载最新正式版安装包（无需登录解压 Actions Artifact）：
- **Windows x64**：`.exe`（NSIS 安装向导）/ `.msi`
- **macOS Universal（Apple Silicon + Intel 通用）**：`.dmg`
- **Linux x64**：`.AppImage` / `.deb`

## 核心特性总览（P0 – P4）

- **P0 低延迟传输底座**：128×128 增量脏瓦片主动推流（动态 JPEG + 静止分批 PNG 无损精修）、4K 零内存拷贝盒式锐利缩放、读写通道分离零阻塞键鼠注入、同局域网 `LOOKUP` 380ms P2P 直连竞速、20s 中继心跳保活。
- **P1 端到端安全与文件流**：`X25519 + HKDF-SHA256 + ChaCha20-Poly1305` 端到端加密（中继服务器不可解密）、设备号归属签名防抢注、滑动窗口密码防爆破、256 KB 恒定内存大文件分块流式上传/下载（SHA-256 全链路校验）、智能 `⌘ Ctrl ↔ Command` 跨系统快捷键联动。
- **P2 多屏与无人值守**：多显示器枚举与不断连毫秒级热切换、远端原生光标形状与位置增量同步、Linux `dlopen(libXtst)` 零链接依赖原生键鼠注入、RTT 智能自适应码率、双密码体系（临时密码 + 固定无人值守密码）与在线远控会话审计/一键踢出。
- **P3 深度流畅体验**：`createImageBitmap` 异构异步解码与防闪黑分辨率切换、网络闪断指数退避自动重连、「我的伙伴设备簿」一键秒连、全量开发者 IDE 键盘映射 + 触控板像素级平滑滚轮、沉浸全屏悬浮自动隐藏顶栏、中继防设备枚举隐私保护。
- **P4 系统级守护与生态**：系统托盘常驻守护（点击 `×` 最小化至托盘持续保活被控）、三端剪贴板截图/PNG 图片双向同步（`CF_DIB` / macOS `PNGf` / Linux Wayland·X11）、远端沙箱文件管理器（新建文件夹 / 重命名 / 删除 / 传输中途取消）、GitHub Tag 自动发版流水线。

## 仓库结构

- **`app/`**：Tauri 2 + Rust 跨平台桌面应用工程（`ctd-protocol` + `ctd-core` + `src-tauri` + `ui`）
- **`mvp/`**：零外部依赖 Windows 原生原型、Python/Node 信令中继服与 13 工具 AI MCP Server
- **`docs/`**：完整架构设计与模块规范文档
- **`.github/workflows/build-multiplatform.yml`**：三端自动单测、打包与 GitHub Release 自动发布流水线
