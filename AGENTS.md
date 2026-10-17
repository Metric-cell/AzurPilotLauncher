# AGENTS.md

本文件为 ZCode 在本仓库中工作时提供指引。**必须使用中文与用户交流。**

详细架构文档见 `CLAUDE.md`（运行时流程、自定义协议、标题栏注入、平台差异的完整说明），本文件只列要点与最新差异。

## 项目概述

AzurPilot Launcher：[AzurLaneAutoScript](https://github.com/LmeSzinc/AzurLaneAutoScript) 的跨平台（Windows/macOS/Linux）桌面启动器，**Tauri 2 + Rust**，无前端目录（所有"页面"都是内嵌 HTML，通过自定义 URI 协议加载）。内嵌 `uv` 管理 Python 3.14.6 可重定位 `.venv`，经 git 更新仓库后启动 Python 后端 `gui.py`。

## 常用命令

```bash
cargo build            # debug 构建
cargo build --release  # LTO + strip，体积优先
cargo tauri dev        # 开发运行
cargo test             # 全部测试；cargo test <name> 单测
cargo check            # 仅检查编译
```

### 构建脚本环境变量（build.rs）

- `ALAS_BOOTSTRAP_UV`：指向要内嵌的 uv 二进制。未设置时用空占位符（本地开发正常，运行时回退到 PATH 中的 uv）。
- 默认发行继续使用 HTTPS `stable.json` 清单及 mTLS 载荷下载，保留 tag 触发的 R2/CDN、SSH 与旧版兼容部署。
- `ALAS_LAUNCHER_MTLS_IDENTITY_PEM_B64`：内嵌 mTLS 客户端身份（base64 PEM）；未设置时回退仓库根目录的 `证书.txt`（不入库）。`REQUIRE_LAUNCHER_MTLS_IDENTITY` 设置后缺少身份即构建失败，正式打包要求提供。
- `LAUNCHER_UPDATE_URL`：覆盖默认更新源 `https://ap-launcher-update.nanoda.work/updata/stable.json`。
- `AZURPILOT_UPDATE_MANIFEST_URL` / `AZURPILOT_UPDATE_PUBLIC_KEY`：显式选择可选签名模式的清单 URL 和 Ed25519 公钥；默认均为空，签名配置不完整时拒绝更新，不回退默认发行源。
- `AZURPILOT_UPDATE_METADATA_ORIGINS` / `AZURPILOT_UPDATE_PAYLOAD_ORIGINS`：分别约束清单和载荷的 HTTPS 来源及每一跳重定向。
- `AZURPILOT_UPDATE_CHANNEL` / `AZURPILOT_UPDATE_MIN_SEQUENCE`：签名渠道和最小序号。

签名模式不嵌入签名私钥，不要求现有发行迁移到 GitHub Releases 或配置新签名密钥。可选签名配置见 [签名更新](docs/update-security.md)。

Windows 构建的 manifest 使用 `asInvoker`，安装器为当前用户安装；不要求管理员运行。Node.js 使用当前用户的便携安装，不修改系统 PATH 或 ACL。

## 模块职责

`main.rs` 负责应用装配与窗口生命周期。`update/`、`commands.rs`、`ui/`、`webview_security.rs`、`popup_guard.rs`、`process_security.rs` 分别维护默认发行兼容与可选签名更新、原生命令、界面、网页权限、弹窗控制和进程归属；`setup/` 与 `nodejs/` 拆分环境准备和便携安装。边界与验证入口见 [安全重构](docs/launcher-hardening.md)。

- 新增原生命令同时更新 build.rs AppManifest、WebviewPolicy 角色矩阵、bridge 和测试，不授予网页默认 core/plugin 权限。
- 端口占用只报错；仅结束本次持有 Job/session 的进程，不按端口、名称或环境变量杀其他程序。Windows 后端保留模拟器显式脱离协议，普通安装命令不获得该能力。
- 开发检出和 `--skip-update` 保留配置与锁文件，依赖使用冻结锁；取消启动保留已有环境。

## 必须遵守的约定

- **i18n**：4 个语言文件 `locales/{zh-CN,zh-TW,ja,en}.yml`，`zh-CN.yml` 为基准。新增字符串要同时加 4 个文件，代码中用 `t!("module.key")`（常需 `.to_string()`），带参数用 `t!("k", p = v)` + YAML `%{p}`。
- **平台兼容**：改动涉及进程/窗口/通知/路径时必须考虑三平台，用 `#[cfg(target_os = "...")]` 分支；平台依赖在 `Cargo.toml` 的 `[target.'cfg(...)'.dependencies]`。子进程命令一律套 `CreateNoWindow`（Windows）避免黑窗。
- **日志**：用 `tracing`（info!/warn! 等），初始化后写入 `log/{date}_launcher.txt`，不要用 `println!`。
- **版本号**：发布需同步 `Cargo.toml`、`tauri.conf.json`、`CHANGELOG.md` 三处。
- **CI 打包**：仅 `.github/workflows/package.yml`（tag push / 手动触发，三平台矩阵，产 tar.xz）。六个 `deploy.*.yaml` 是运行时部署配置，`-cn` 后缀用国内镜像，改动 `setup.rs` 的依赖安装逻辑时需兼顾两种变体。
- **CI 测试**：`.github/workflows/test.yml` 在推送 `main`、PR 和手动触发时运行，覆盖 Linux、macOS、Windows x64/ARM64，执行 `cargo check --locked --all-targets` 和 `cargo test --locked --all-targets`，无需发布密钥。Windows 本地测试使用普通用户终端；进程测试使用独立临时夹具。

## 关键常量

WebUI 端口 `22267`；Python `3.14.6`；后端 HTTP(S) 就绪等待 300s（可取消）；SSE 重连 3s。时间炸弹配置在 `Cargo.toml` 的 `[package.metadata.alas-launcher.time-bomb]`（运行时由 `main.rs` 解析，当前 `enabled = false`）。
