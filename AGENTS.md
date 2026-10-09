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

### WSL 本地开发

- 环境安装、编辑器配置和验收命令见 [docs/development-wsl.md](docs/development-wsl.md)。
- 使用 `scripts/wsl-dev.sh check` / `test` / `build` 检查、测试和构建。
- 使用 `scripts/wsl-dev.sh preview` 预览启动画面，或 `scripts/wsl-dev.sh dev` 监听源码并重启预览。
- 预览在 Cargo 的 `target/debug` 下建立独立布局，并固定传入 `--preview-crash`，不进入真实后端安装或任务启动流程。完整运行的 EXE/后端同目录要求仍适用。

### 构建脚本环境变量（build.rs）

- `ALAS_BOOTSTRAP_UV`：指向要内嵌的 uv 二进制。未设置时用空占位符（本地开发正常，运行时回退到 PATH 中的 uv）。
- `ALAS_LAUNCHER_MTLS_IDENTITY_PEM_B64`：launcher 自更新的 mTLS 客户端证书（base64 PEM）。未设置时回退读取仓库根目录的 `证书.txt`（不入库）。
- `LAUNCHER_UPDATE_URL`：覆盖自更新源，默认 `https://ap-launcher-update.nanoda.work/updata/stable.json`。

注意：Windows 构建的 manifest 是 `requireAdministrator`（build.rs 内嵌），本地运行会弹 UAC。

## 模块职责（src/，行数为 2026-10 实测）

| 模块 | 职责 |
|---|---|
| `main.rs`（~4600 行） | Tauri 入口：splash/主窗口管理、托盘、时间炸弹、标题栏注入、错误页、Tauri 命令注册 |
| `setup.rs`（~2600 行） | 环境搭建：Python/uv/adb/git 安装、git 更新（重试 20 次）、`uv sync`、deploy.yaml 迁移 |
| `backend.rs` | `gui.py` 进程组生命周期、启动前端口占用清理、`ALAS_LAUNCHER_PID` 泄漏扫描、HTTP(S) 就绪与启动取消 |
| `nodejs.rs`（~1000 行） | **仅 Windows**：检测系统级 Node.js，缺失时下载安装（SHA256 校验） |
| `notify.rs`（~340 行） | 旧版 SSE 通知流（`/api/notify_stream`）能力检测、平台原生通知 |
| `launcher_control.rs`（~160 行） | 旧版反向控制流（`/api/launcher/stream`）：开机自启指令与应答 |
| `autostart.rs`（~100 行） | 开机自启状态查询/设置（Windows 写注册表 Run 键，带 `--start-minimized`） |
| `i18n.rs`（~70 行） | locale 检测：`--lang` 参数 > 系统 locale > 回退 `en` |
| `window_util.rs` | Windows `CREATE_NO_WINDOW` trait，子进程禁止弹控制台 |

## 必须遵守的约定

- **i18n**：4 个语言文件 `locales/{zh-CN,zh-TW,ja,en}.yml`，`zh-CN.yml` 为基准。新增字符串要同时加 4 个文件，代码中用 `t!("module.key")`（常需 `.to_string()`），带参数用 `t!("k", p = v)` + YAML `%{p}`。
- **平台兼容**：改动涉及进程/窗口/通知/路径时必须考虑三平台，用 `#[cfg(target_os = "...")]` 分支；平台依赖在 `Cargo.toml` 的 `[target.'cfg(...)'.dependencies]`。子进程命令一律套 `CreateNoWindow`（Windows）避免黑窗。
- **日志**：用 `tracing`（info!/warn! 等），初始化后写入 `log/{date}_launcher.txt`，不要用 `println!`。
- **版本号**：发布需同步 `Cargo.toml`、`tauri.conf.json`、`CHANGELOG.md` 三处。
- **CI 打包**：仅 `.github/workflows/package.yml`（tag push / 手动触发，三平台矩阵，产 tar.xz）。六个 `deploy.*.yaml` 是运行时部署配置，`-cn` 后缀用国内镜像，改动 `setup.rs` 的依赖安装逻辑时需兼顾两种变体。
- **CI 测试**：`.github/workflows/test.yml` 在推送 `main`、PR 和手动触发时运行，覆盖 Linux、macOS、Windows x64/ARM64，执行 `cargo check --locked --all-targets` 和 `cargo test --locked --all-targets`，无需发布密钥。Windows 本地测试需在管理员终端运行（manifest 与 Node.js 私有目录 ACL 均要求提权）。

## 启动与更新保护

- 修复说明及验收见 [docs/launcher-fixes.md](docs/launcher-fixes.md)。
- 后端 URL 使用同一份 `WebuiLaunchConfig`，通知流、免密、页面导航和重试必须保留 HTTPS/IPv6；不得通过关闭证书校验解决 TLS 问题。
- 进程清理已按用户要求恢复原策略：启动前按端口清理占用进程，退出时通过进程组及 `ALAS_LAUNCHER_PID` 扫描兜底回收。进程验证使用独立临时 fixture。
- 开发检出和 `--skip-update` 保留配置与 `uv.lock`，跳过发布更新，依赖使用 `uv sync --frozen`；取消启动不得删除现有环境。
- 当前后端未提供旧桌面通知/系统自启接口时停止订阅；`startup.set` 管理账号任务，不能当作系统自启接口使用。

## 关键常量

WebUI 端口 `22267`；Python `3.14.6`；后端 HTTP(S) 就绪等待 300s（可取消）；SSE 重连 3s。时间炸弹配置在 `Cargo.toml` 的 `[package.metadata.alas-launcher.time-bomb]`（运行时由 `main.rs` 解析，当前 `enabled = false`）。
