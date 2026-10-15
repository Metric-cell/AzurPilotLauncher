# Launcher 开发说明

必须使用中文交流。当前架构与安全边界以 [AGENTS.md](AGENTS.md) 和 [docs/launcher-hardening.md](docs/launcher-hardening.md) 为准。

## 开发与验证

```bash
scripts/wsl-dev.sh check
scripts/wsl-dev.sh test
scripts/wsl-dev.sh build
scripts/wsl-dev.sh preview
```

前三条分别执行锁定依赖的 `cargo check --all-targets`、`cargo test --all-targets` 和 debug 构建。preview 使用 Cargo target 下的独立配置，固定 `--preview-crash`，不进入真实后端安装或游戏任务。

## 启动与装配

`main.rs` 初始化运行目录、日志、语言、固定的 `WebuiLaunchConfig` 与 WebView 权限策略；创建启动画面后，在工作线程依次检查可选的签名更新、准备环境、启动并验证自有后端，最后展示主窗口。窗口退出通过同一生命周期状态回收自有后端。

部署模板指向 `Metric-cell/AzurPilot` 的 `deploy` 分支；已有用户配置保留。开发检出与 `--skip-update` 跳过自身/仓库发布更新，冻结依赖锁，取消启动不删除已有环境。`--preview-crash` 隐含跳过更新并只演示错误状态。`--start-minimized` 只在有托盘的平台生效。

Windows/macOS 有托盘，首次关闭询问退出或后台运行，保存后按偏好执行；系统设置可改为每次询问、后台运行或退出。Windows 偏好保存在 EXE 同目录的 `.launcher/preferences.json`，其他平台保存在应用配置目录，写入失败保留窗口。Linux 没有托盘，因此关闭主窗口退出。Windows/macOS 托盘化仅隐藏窗口并保留 WebView、当前页面和连接，恢复直接显示并聚焦，不重新加载前端；窗口意外丢失时才走防重入重建。保留 WebView 会占用前端内存。安全策略与调用服务由应用状态持有。远端后端使用原生窗口装饰，不获得本地原生命令权限。

## 权限约定

- Windows 普通用户运行；Node、WebView2 数据、偏好与缓存放在 EXE 同目录的 `.launcher/`，后端可放在同级 `backend/`，命令使用 `CreateNoWindow`。集中部署与迁移见 [windows-portable.md](docs/windows-portable.md)。
- 自更新默认禁用；启用方式与签名工具见 [update-security.md](docs/update-security.md)。发行私钥不得嵌入客户端。
- 自定义原生命令须同时登记 build.rs AppManifest 和 WebView 角色矩阵；不授予网页默认 core/plugin 权限。
- 「开源项目」通过固定目标的 `open_project_repository` 在系统浏览器打开个人仓库；仅可信可见的后端主框架可调用，网页不能传入任意 URL。
- 新窗口须安装 `popup_guard`；注入脚本须校验当前来源和主框架 bridge。内部 HTML 只能通过 `ui::html_response` 返回，动态 JSON 使用 script 上下文转义。
- 后端 URL 保留 HTTPS 和 IPv6；不关闭证书检查。免密、自启反控和通知只面向精确回环来源。
- 只能终止本次持有的进程内核身份；端口、环境标记、进程名不能用于授权杀进程。
- 新文案同步 `locales/{zh-CN,zh-TW,ja,en}.yml`；日志使用 tracing，本地 `log` 目录保存。
- 发布时才同步 Cargo.toml、tauri.conf.json 与 CHANGELOG.md 的版本；本地修复不擅自发布、推送或创建 PR。

进程机制、WebView 来源策略和平台验证限制分别见 [process-security.md](docs/process-security.md)、[webview-security.md](docs/webview-security.md) 与 [launcher-hardening.md](docs/launcher-hardening.md)。
