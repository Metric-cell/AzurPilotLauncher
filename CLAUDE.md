# CLAUDE.md

必须使用中文交流。仓库协作约定、四语言维护、平台依赖和发布版本规则见 [AGENTS.md](AGENTS.md)。

## 开发与验证

```bash
cargo check --locked --all-targets
cargo test --locked --all-targets
cargo build --locked
node --test scripts/test-close-prompt.cjs scripts/test-webview-bridge.cjs
python3 scripts/test-sign-launcher-update.py
```

本地未设置 `ALAS_BOOTSTRAP_UV` 时使用空占位符，运行时从 PATH 查找 uv。完整运行仍需启动器与后端部署布局配套；测试使用临时夹具，不启动用户的后端或游戏。

## 启动与窗口

应用先显示内嵌 splash，后台完成自身更新检查、环境准备和后端 HTTP(S) 就绪检查，然后显示主窗口并按后端能力订阅桌面通知与控制流。`--skip-update` 跳过发布更新，开发检出保留配置与锁文件；`--preview-crash` 隐含跳过更新并模拟启动失败。取消启动等待自有工作停止，保留现有环境。

Windows/macOS 提供托盘；首次关闭询问退出或托盘化，成功保存选择后才执行，后续系统关闭和标题栏共用原生偏好。托盘化只隐藏窗口，保留 WebView、页面和连接；窗口意外丢失时仍可重建。macOS 保留系统窗口按钮并切换 Dock 激活策略。Linux 没有托盘，关闭退出，`--start-minimized` 不隐藏窗口。

`preferences.rs` 把关闭偏好原子保存在 `app_config_dir()/preferences.json`，保存失败不更新内存状态。托盘菜单“关闭窗口时”提供询问、托盘和退出选项，仅保存设置，不立即关闭窗口；该功能无需后端修改。后端设置页面可选调用统一安全 bridge 的 `get_launcher_preferences` / `set_launcher_close_behavior` 接口，关闭选择使用 `window_choose_close_behavior`；三个命令按来源、角色、主框架凭证和窗口可见性校验。

## 更新与安装

`update/` 只接受由编译期公钥验证的原始清单，逐跳约束 HTTPS 来源，验证版本、序号、有效期、长度和摘要，安装前再次校验。未同时配置可信清单和公钥时禁用自身更新；不嵌入 mTLS 私钥。后端 Git 更新继续使用六份原有部署配置的上游来源，新安装默认关闭远程访问并监听回环。配置与签名操作见 [签名更新](docs/update-security.md)。

Windows 清单为 `asInvoker`，安装器采用当前用户安装。Node.js 经用户同意后下载固定官方便携包、验证摘要并放入 LocalAppData，不改持久 PATH、HKLM 或 ACL。`setup/` 分离配置、工作区、进度、下载来源与链接安全清理；开发模式的恢复入口直接拒绝删除环境。

## 网页与进程边界

`commands.rs` 中的命令通过 AppManifest、来源/窗口角色和主框架凭证共同校验。`ui/` 负责内嵌页面、CSP 和动态内容转义；`popup_guard.rs` 在平台原生层拒绝弹窗。远端后端仅可展示页面，免密、原生命令、通知和反向控制限于回环来源。导出需要可见窗口和用户选择保存路径，受容量与并发限制。见 [网页权限](docs/webview-security.md)。

`process_security.rs` 只凭本次 spawn 持有的 Job/session 回收进程。端口冲突只报错，HTTP 就绪前后核对监听者归属。Windows 后端保留上游模拟器显式 Job breakaway 协议，普通 worker 继续回收，安装命令不获得脱离能力。见 [进程权限](docs/process-security.md) 和 [后端生命周期](docs/backend-lifecycle.md)。

模块边界、测试入口和平台验证限制见 [安全重构](docs/launcher-hardening.md)。
