# CLAUDE.md

本文件为 Claude Code (claude.ai/code) 在本仓库中工作时提供指引。

**必须使用中文与用户交流。**

## 项目概述

AzurPilot Launcher 是 [AzurPilot](https://github.com/wess09/AzurPilot) 的跨平台（Windows/macOS/Linux）桌面启动器，基于 **Tauri 2 + Rust** 构建。它通过内嵌的 `uv` 二进制文件管理独立的 Python 3.14.6 环境，处理基于 git 的更新，启动 Python WebUI 后端（`gui.py`），并提供原生 webview 壳（含启动画面、系统托盘、通知和自定义标题栏）。

## 构建与开发命令

```bash
# 构建（debug）
cargo build

# 构建（release — 去除符号，启用 LTO）
cargo build --release

# 开发模式运行
cargo tauri dev

# 运行全部测试
cargo test

# 运行单个测试
cargo test test_name_here

# 仅检查编译
cargo check
```

构建脚本（`build.rs`）需要 `ALAS_BOOTSTRAP_UV` 环境变量指向要内嵌的 `uv` 二进制文件。本地开发不设置时使用空占位符，启动器会在运行时从 PATH 中查找 `uv` 或通过环境变量 `UV` 指定。

## 启动参数

启动器支持以下命令行参数（Windows 使用 `/` 前缀同样有效）：

| 参数 | 别名 | 说明 |
|---|---|---|
| `--lang <locale>` | `--locale <locale>` | 覆盖系统语言，强制使用指定语言显示界面。支持的值：`zh-CN`（简体中文）、`zh-TW`（繁体中文）、`ja`（日语）、`en`（英语）。也支持 `--lang=zh-CN` 形式。 |
| `--preview-no-update` | `--skip-update`、`--no-update`、`--disable-update` | 跳过仓库更新，直接使用本地文件启动。用于开发调试。 |
| `--preview-crash` | `--preview-error`、`--crash-preview`、`--error-preview` | 模拟启动失败，停留在错误页面以检查 UI 样式。隐含 `--preview-no-update`。 |

示例：

```bash
# 以日语界面启动
alas-launcher --lang ja

# 以英语界面启动，跳过更新
alas-launcher --lang en --skip-update

# 预览错误页面
alas-launcher --preview-crash
```

## 国际化（i18n）

启动器支持 4 种界面语言，根据系统 locale 自动选择，也可通过 `--lang` 启动参数手动覆盖：

- `zh-CN` — 简体中文（基准语言）
- `zh-TW` — 繁体中文
- `ja` — 日语
- `en` — 英语

### 翻译文件

翻译文件位于 `locales/` 目录，使用 YAML 格式：

```
locales/
├── zh-CN.yml    # 简体中文
├── zh-TW.yml    # 繁体中文
├── ja.yml       # 日语
└── en.yml       # 英语
```

每个文件按模块分组（`splash`、`setup`、`dialog`、`tray`、`titlebar`、`error_page`、`notify`、`errors`、`tips`），key 用点分隔。

### 添加新字符串

1. 在 `locales/zh-CN.yml` 中添加 key 和中文值
2. 在其他 3 个语言文件中添加对应翻译
3. 在 Rust 代码中使用 `t!("module.key")` 调用（需要 `.to_string()` 转换为 `String`）
4. 带参数的字符串使用 `t!("module.key", param = value)` 语法，YAML 中用 `%{param}` 占位

### 语言检测逻辑

`src/i18n.rs` 中的 `detect_locale()` 按以下优先级选择语言：
1. 命令行参数 `--lang` / `--locale`（最高优先级）
2. 系统 locale（通过 `sys-locale` crate 获取）
3. 回退到 `en`

支持的 locale 映射：`zh*` → `zh-CN`，`zh-TW`/`zh-HK`/`zh-Hant` → `zh-TW`，`ja*` → `ja`，其他 → `en`。

## 架构

### 源码模块（`src/`）

| 模块 | 行数 | 职责 |
|---|---|---|
| `main.rs` | ~2140 | Tauri 应用入口、窗口管理、启动画面、系统托盘、时间炸弹、自定义标题栏注入、错误页面 |
| `backend.rs` | ~330 | 启动/终止 `gui.py` 子进程、端口扫描与占用进程清理、后端生命周期管理 |
| `setup.rs` | ~1460 | 环境配置：Python/uv/adb/git 安装、git 更新、uv 依赖同步、deploy.yaml 迁移、运行时清理 |
| `notify.rs` | ~320 | SSE 通知流、平台原生桌面通知（Windows Toast / Linux notify-rust / macOS Tauri 插件） |
| `i18n.rs` | ~55 | 国际化：系统语言检测、`--lang` 启动参数解析、locale 设置 |
| `window_util.rs` | ~45 | Windows `CREATE_NO_WINDOW` trait，控制子进程是否创建控制台窗口 |

### 运行时流程

1. `main()` 初始化日志（写入 `log/{date}_launcher.txt`），读取 `config/deploy.yaml` 获取 WebUI 配置
2. 创建 Tauri 应用：显示 splash 窗口（`alas-splash://` 自定义协议），隐藏 main 窗口
3. 后台线程执行 `setup_alas_repo()`：
   - `ensure_runtime_tools()`：下载 Python 3.14.6（通过 uv managed python）、创建可重定位 `.venv`、复制 uv/adb/git 到 `.venv`；检测到 `.venv` 的 Python 低于 3.14.5 时会删除并重建整个环境
   - `git_update()`：通过 Python 脚本调用 `deploy.git.GitManager` 拉取最新代码（带重试，最多 20 次）
   - `uv_sync_project()`：执行 `uv sync --frozen --no-dev --no-install-project` 安装依赖
4. `ManagedBackend::new()` 启动 `gui.py`，在五分钟内确认 HTTP(S) 就绪与监听者归属；支持取消，`ALAS_LAUNCHER_PID` 仅作子进程诊断标记
5. 启动 SSE 通知流（`/api/notify_stream`），销毁 splash 窗口，显示 main 窗口

### 自定义 URI 协议

- `alas-splash://`（Windows/Android 用 `http://alas-splash.localhost/`）：内嵌启动画面 HTML/CSS/JS，进度条通过 `window.__ALAS_SPLASH_UPDATE()` 回调更新
- `alas-error://`（Windows/Android 用 `http://alas-error.localhost/`）：后端连接失败页面，每秒自动重试连接

### 自定义标题栏

Windows 和 Linux 移除原生窗口装饰（`set_decorations(false)`），通过 `page_load_injector` 在每个页面加载完成后注入自定义标题栏 JS（红绿灯按钮 + 拖拽区域）。macOS 保留原生标题栏。注入脚本还会：
- 覆盖 `window.saveAs` 使其通过 Tauri 的 `save_as` 命令保存文件
- 阻止浏览器后退（`history.pushState` + `popstate` 监听）

### 窗口关闭行为

- **Windows/macOS**：首次关闭显示“退出 / 最小化到托盘”菜单，选择成功保存后才执行。托盘菜单“关闭窗口时”可改为每次询问、最小化到托盘或退出应用；此处仅保存偏好，不立即关闭窗口，保存失败显示原生错误对话框。此后自绘关闭按钮、系统关闭和 Alt+F4 共用原生偏好。macOS 保留原生标题栏，仅注入关闭菜单。
- **Linux**：无系统托盘，关闭直接退出，不提供后台选项，忽略 `--start-minimized`。
- 偏好由 `src/preferences.rs` 的 `PreferenceStore` 保存在 Tauri `app_config_dir()/preferences.json`，与 WebView 存储、后端部署配置独立。通过同目录临时文件、同步及原子替换持久化，保存失败保持原先内存状态和窗口，菜单允许重试。缺失或损坏的文件回退首次询问。
- 为兼容后端设置界面，提供 `window.__ALAS_NATIVE__.invoke('get_launcher_preferences')` 读取 `{closeBehavior, backgroundSupported}`，通过 `set_launcher_close_behavior` 的 `{behavior: 'ask' | 'tray' | 'exit'}` 修改。统一的 `src/webview_bridge.js` 在文档初始化时提供这些受限接口，原生命令同时检查来源、窗口角色和主框架凭证。后端设置 UI 需单独接入，本启动器独立提供托盘修改入口；关闭菜单使用 `window_choose_close_behavior` 先保存再关闭。
- 关闭菜单 JS 位于 `src/ui/titlebar.js`，用 `node --test scripts/test-close-prompt.cjs scripts/test-webview-bridge.cjs` 验证交互及设置桥接。
- 托盘化仅隐藏主窗口，保留 WebView、页面和连接；恢复直接显示并聚焦，窗口意外丢失时仍可重建。保留页面会占用前端内存。

### Tauri 前端命令

`save_as`、`download_today_gui_log`、`download_today_launcher_log`、`retry_backend_connection`、`window_hide`、`window_minimize`、`window_toggle_maximize`、`window_close`、`get_launcher_preferences`、`set_launcher_close_behavior`、`window_choose_close_behavior`、`window_start_dragging`、`window_is_maximized`

### 后端进程归属

`ManagedBackend` 只结束本次创建并持有的 Job/session，端口占用只报错，不按进程名或环境标记补杀。就绪检查使用 HTTP(S) 并验证监听者归属，TLS 始终验证证书链和主机名。Windows 的后端 Job 保留显式 breakaway 协议，让模拟器脱离而普通 worker 随启动器回收；安装命令不获得该能力。详见 [后端生命周期](docs/backend-lifecycle.md) 和 [进程边界](docs/process-security.md)。

### 平台相关代码

大量使用 `#[cfg(target_os = "...")]`。`Cargo.toml` 中的平台依赖：
- **Windows**：`winapi`、`tauri-winrt-notification`、`windows-registry`
- **Linux**：`notify-rust`、`openssl-probe`（CA 证书探测）
- **Unix**：`nix`（信号处理，SIGTERM 优雅退出）
- **桌面端（非移动端）**：`tauri-plugin-single-instance`（单实例，第二次启动时恢复窗口）

### 部署配置

六个 `deploy.*.yaml` 文件配置不同平台/镜像组合。带 `-cn` 后缀的变体使用中国大陆可访问的镜像源。`setup.rs` 中的 `migrate_dependency_config()` 会在启动时自动迁移 `config/deploy.yaml`：
- 更新 Python/Adb/Git 可执行文件路径为 `.venv` 内路径
- 移除已废弃的 `RequirementsFile` 配置项
- 强制设置 `InstallDependencies: true`

### 时间炸弹机制

`Cargo.toml` 中包含 `[package.metadata.alas-launcher.time-bomb]` 配置段。`main.rs` 通过 `include_str!("../Cargo.toml")` 在运行时解析此配置（非编译时）。当 `enabled = true` 时，启动器通过 HTTP 请求获取网络时间（`Date` 头），与过期日期比较，过期则弹窗拒绝运行。

### CI/CD

测试流水线（`.github/workflows/test.yml`）：

- 触发条件：推送 `main`、所有 PR 或手动 `workflow_dispatch`
- 测试矩阵：`ubuntu-22.04`、`macos-latest`、`windows-latest`、`windows-11-arm`
- 安装 Linux Tauri 系统依赖及 stable Rust，按 runner/架构缓存 Cargo 依赖；仅 `main` 保存缓存
- 执行 `cargo check --locked --all-targets` 和 `cargo test --locked --all-targets`，覆盖启动器和示例目标；Linux 额外通过 Node.js 22 运行关闭菜单与偏好桥接回归脚本
- 同一事件/分支的新运行取消旧运行；单个矩阵任务失败不取消其他平台，超时为 45 分钟
- 使用 `build.rs` 的 uv/mTLS 空占位符，不需要发布密钥，也不执行打包或部署
- Windows 托管 runner 以管理员身份运行，满足启动器 manifest 和 Node.js 安全目录 ACL 的要求；Windows 本地测试也需管理员终端
- 可在 GitHub Actions 中选择 **Test AzurPilot → Run workflow** 手动运行；本地使用上述两条 Cargo 命令复现

GitHub Actions（`.github/workflows/package.yml`）：
- 触发条件：tag push 或手动 `workflow_dispatch`
- 构建矩阵：`ubuntu-22.04`、`macos-latest`、`windows-latest`
- Linux/macOS 从源码编译 Git v2.49.1，Windows 下载 MinGit v2.51.0
- 下载 Android platform-tools（adb）
- 创建可重定位 `.venv`（Python 3.14.6 + uv + adb + git + requests）
- 打包为 `tar.xz` 归档（国际版 + CN 镜像版）
- 部署启动器自动更新载荷到 `alas.nanoda.work`（通过 SSH）

### 关键常量与配置

- 默认 WebUI 端口：`22267`
- Python 版本：`3.14.6`（`setup.rs` 中 `PYTHON_VERSION`）
- Git 更新最大重试：20 次，间隔 1 秒
- 后端端口等待超时：60 秒
- 后端连接检查超时：500 毫秒
- 通知流断线重连间隔：3 秒
- `UV_PYTHON_INSTALL_MIRROR` 默认使用 npmmirror 加速 Python standalone 下载，并以 python-standalone.org 作为备用源

### 签名更新模块

`src/update/` 提供 Ed25519 原始清单签名、逐跳 HTTPS 来源约束、版本与序号回退保护、长度与 SHA256 校验及安装前复验。签名私钥仅用于发行，不嵌入程序；配置和签名工具见 [签名更新](docs/update-security.md)。应用入口只通过此模块执行自身更新。

### 安装与取消边界

Windows 清单使用 `asInvoker`，安装器只安装到当前用户目录。Node.js 经用户同意后从固定官方来源下载便携包并验证固定摘要，保存到 LocalAppData；不执行 MSI、不修改持久 PATH、HKLM 或 ACL。环境准备拆入 `setup/`，安装命令的取消通过自有进程组回收；清理不跟随目录链接，开发工作区和 `--skip-update` 保留配置与锁文件，取消启动不删除已有 `.venv`。

### 网页权限与应用装配

`main.rs` 只负责应用装配与窗口生命周期；`commands.rs`、`ui/`、`webview_security.rs`、`popup_guard.rs` 分别负责原生命令、页面渲染、来源校验与原生弹窗拒绝。命令同时通过 AppManifest、窗口角色与主框架凭证验证，不给远端后端原生权限；日志不返回网页，导出需要用户选择路径并限制容量。免密与两条 SSE 接口只连接回环来源，事件和响应有长度上限。详见 [网页权限](docs/webview-security.md)。

运行入口现已切换到 `update/` 的签名校验实现。构建未同时指定 `AZURPILOT_UPDATE_MANIFEST_URL` 和 `AZURPILOT_UPDATE_PUBLIC_KEY` 时禁用自身更新，不访问默认更新站、不嵌入 mTLS 私钥。后端 Git 更新仍按原部署配置执行；六份模板保留上游来源，新安装默认仅监听回环并关闭远程访问。
