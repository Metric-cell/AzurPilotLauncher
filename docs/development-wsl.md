WSL 启动器开发环境

在 WSL 的 Ubuntu 中编辑、检查、测试并构建 Linux 启动器。Windows EXE 的托盘、UAC、自启和 MuMu 原生控制仍需在 Windows 上验收。

**本机配置**

工作区为 `/home/dreamydust/Azurpilot`，启动器在 `launcher/`，Python 后端在 `dev-wsl/`。后端使用个人仓库 [Metric-cell/AzurPilot](https://github.com/Metric-cell/AzurPilot/tree/deploy) 的 `deploy` 分支，`origin` 指向该仓库。六个平台部署模板及 CI 打包的引导部署代码均使用此来源；国内模板保留 PyPI 镜像，但后端代码直接从个人 GitHub 仓库拉取。

本地后端已重新建立 `.venv/` 和 React 静态资源，`config/deploy.yaml` 同样指向个人仓库的 `deploy` 分支，WebUI 监听 `127.0.0.1:25548`。启动后端：

```bash
cd /home/dreamydust/Azurpilot/dev-wsl
uv run --frozen python gui.py
```

Node.js 24 安装在 `~/.local/share/nodejs/`，`node`、`npm`、`npx` 入口位于 `~/.local/bin/`。更新后端源码后，`gui.py` 会按源码指纹检查并在需要时重建前端。旧后端的虚拟环境、配置和日志已随目录替换删除。

Rust 工具安装在用户的 `~/.cargo/bin`、`~/.rustup`；rustup 已在 `~/.profile` 和 `~/.bashrc` 加载 Cargo 环境。旧终端可以运行：

```bash
source ~/.cargo/env
cd /home/dreamydust/Azurpilot/launcher
```

本次安装 Rust/Cargo 1.99.0，包含 rustfmt、Clippy、rust-analyzer 和 rust-src；Tauri CLI 为 2.12.1。系统依赖包括 C/C++ 编译器、pkg-config、GTK 3、WebKitGTK 4.1、libxdo、OpenSSL、Ayatana AppIndicator、librsvg 和 patchelf。启动器的 HTML 内嵌在 Rust 中，这里的 Cargo 开发流程不依赖 npm。

**日常命令**

```bash
scripts/wsl-dev.sh check     # cargo check --locked --all-targets
scripts/wsl-dev.sh test      # cargo test --locked --all-targets
scripts/wsl-dev.sh build     # cargo build --locked
scripts/wsl-dev.sh preview   # 构建后在 WSLg 中显示启动画面的错误预览
scripts/wsl-dev.sh dev       # Tauri 监听源码，修改后重新构建并启动预览
```

检查和测试不需要显示服务器。预览需要 WSLg 的 `DISPLAY` 或 `WAYLAND_DISPLAY`，在终端中按 Ctrl+C 结束。预览使用源码已有的 `--preview-crash` 标记，在安装后端和启动任务之前停止，并隐含跳过启动器和仓库更新；这个模式用于模拟启动失败。启动画面现在通过 IPC 确认页面就绪，并重放缓存的最新状态；错误预览应显示模拟失败信息和最终进度，修复记录见 [launcher-fixes.md](launcher-fixes.md)。

启动器根据自身可执行文件所在目录寻找 `deploy/installer.py`，所以普通 `cargo run` 在缺少同目录后端布局时会失败。开发脚本在 Cargo 的 `target/debug` 中建立独立的布局标记与最小配置，不引用 `dev-wsl/config`。生成的安装脚本仅用于布局探测，不能安装后端。不要把该预览目录当成完整运行副本。

本机默认图形路径出现 DRI/DMABUF 警告，WebKit 进程持续高 CPU。开发脚本仅在 WSL 的窗口预览中默认使用 `GDK_BACKEND=x11`、`WEBKIT_DISABLE_DMABUF_RENDERER=1`，调用者显式设置会被保留；这两个参数没有写入登录环境或发行版代码。处理方式参考 [Tauri Linux 图形问题](https://v2.tauri.app/develop/debug/linux-graphics/)。

**编辑器**

WSL 端 VS Code 已安装 `rust-lang.rust-analyzer`。工作区的 `.vscode/settings.json` 指向 `launcher/Cargo.toml`，使用 rustup 中的语言服务器，并为其补齐 Cargo PATH；保存后的检查使用 `cargo check`。从工作区根目录打开项目即可识别 Rust 工程；若编辑器仍沿用安装前的环境，运行「Developer: Reload Window」。

**网络与重装**

GitHub CLI 2.102.0 已通过 GitHub 官方 APT 源安装，程序位于 `/usr/bin/gh`。当前用户的 `~/.local/bin/gh` 入口在 WSL 中自动读取默认网关，使用 Windows 的 `7890` 代理端口；已显式设置的代理变量优先。Git 的用户配置仅为 `https://github.com` 设置当前 Windows 代理，网关变化后需更新 `git config --global http.https://github.com.proxy http://新地址:7890`。

账户授权使用浏览器设备登录，随后让 Git 使用同一账户凭据：

```bash
gh auth login --hostname github.com --git-protocol https --web
gh auth setup-git --hostname github.com
gh auth status
```

登录方式见 [GitHub CLI 官方说明](https://cli.github.com/manual/gh_auth_login)。账户凭据保存在当前 WSL 用户环境中，不应放进仓库或开发文档。

本机 Rust 下载和 Cargo 使用 Windows 代理 `http://172.27.144.1:7890`。Cargo 的持久配置在 `~/.cargo/config.toml`：

```toml
[http]
proxy = "http://172.27.144.1:7890"
```

该地址是当前 WSL 默认网关；执行 `ip route show default` 可核对。网关或代理端口改变后应更新这个用户配置；临时覆盖可用 `CARGO_HTTP_PROXY=http://新地址:端口 cargo check --locked --all-targets`。代理配置没有写入仓库。

重装 Ubuntu 依赖的命令如下；具备 sudo 权限的终端中执行。本机的系统包通过 `wsl.exe -d Ubuntu-24.04 -u root -- ...` 安装：

```bash
sudo apt-get update
sudo apt-get install -y --no-install-recommends \
  build-essential pkg-config libwebkit2gtk-4.1-dev libxdo-dev \
  libssl-dev libayatana-appindicator3-dev librsvg2-dev patchelf
rustup toolchain install stable --profile minimal \
  --component rustfmt --component clippy --component rust-analyzer --component rust-src
cargo install tauri-cli --version '^2.0.0' --locked
```

本地编译不需要发布 mTLS 证书：`build.rs` 会写空占位符；uv 已在 PATH 中，运行时可回退使用。本机配置 Linux 开发环境，未安装 Windows 编译链；当前后端来源与启动方式见上方本机配置。

依据：[Tauri 系统依赖](https://v2.tauri.app/start/prerequisites/)、[Tauri Cargo CLI](https://v2.tauri.app/reference/cli/)、[rust-analyzer 安装说明](https://rust-analyzer.github.io/book/installation.html)及仓库 `.github/workflows/test.yml`。

**本次验收**

- `cargo check --locked --all-targets -j 4` 通过。
- `cargo test --locked --all-targets -j 4` 通过，输出 37 passed、0 failed；包含源码已有重复测试注解产生的一次重复执行。
- `cargo build --locked` 通过，产物为 `target/debug/alas-launcher`。
- `cargo tauri info` 识别 Rust、WebKitGTK 和 Tauri CLI。
- 编辑器使用的环境可正常运行语言服务器、Cargo 项目解析及 pkg-config。
- WSLg/X11 预览已渲染 800×450 中文窗口；默认图形路径和 DMABUF 参数对照已完成。
- `scripts/wsl-dev.sh dev` 已实际运行；修改开发脚本/触发源码变动后，Tauri 重新编译并重启窗口，仍携带隔离预览参数。完整后端与 Windows 运行尚未验收。

环境安装记录中的检查针对原始源码，后续功能修复的验收见 [launcher-fixes.md](launcher-fixes.md)。源码仍有平台相关未使用代码警告。检查日志存放在 `target/wsl-validation/`，不纳入 Git。实际预览截图：[preview-configured.png](../target/wsl-validation/preview-configured.png)。

**后端替换验收（2026-10-15）**

- `dev-wsl/` 重新检出个人仓库的 `deploy` 分支，提交为 `5e3ba2aa10bb1e8f9380f23a9e42375d37d2c314`，工作区无源码修改。
- `uv sync --frozen`、`npm ci --prefix frontend --no-audit --no-fund` 和 `npm run build --prefix frontend` 通过。
- 后端的启动器通道、信任登录、静态资源和 clean 网络策略测试共 36 项通过；启动器 `setup::tests` 的 15 项部署测试通过。
- 真实 `gui.py` 在本机临时端口启动，健康接口、React 页面及 JavaScript、启动器状态与免密登录通过，非回环 Host 被拒绝；验证后已停止，没有启动游戏任务。
- 六个平台模板、CI 引导部署来源与本机配置校验通过；`scripts/wsl-dev.sh build --offline` 构建成功。
- 本次修改的 `src/setup.rs` 通过 rustfmt 检查；全仓库格式检查仍有既有的 `src/nodejs.rs` 格式差异。
