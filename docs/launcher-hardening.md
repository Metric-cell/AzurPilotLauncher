# 启动器安全重构

范围为 Rust/Tauri 启动器、内嵌页面、Windows 安装器和发行脚本。后端源码及部署仓库来源由原上游配置维护。重构基于最新上游 main，并保留模拟器脱离 Job 的生命周期修复。

## 模块与提交边界

- `update/`：兼容原 HTTPS 清单与 mTLS 下载，逐跳约束来源、下载和安装复验；独立签名及回退保护由维护者显式选择，配套离线签名工具。
- `process_security.rs` / `backend.rs`：只回收自有进程边界，HTTP(S) 与监听者归属检查；Windows 后端显式授予模拟器脱离能力，安装命令不授予。
- `setup/` / `nodejs/`：分离环境准备职责，取消回收安装子树，保护开发配置/锁文件，链接安全清理和用户级 Node 安装。
- `preferences.rs`：原子保存关闭偏好；Windows/macOS 托盘保留 WebView，原生菜单可修改偏好，网页设置接口遵循统一安全边界。
- `commands.rs` / `webview_security.rs` / `popup_guard.rs` / `ui/`：命令能力、来源、窗口角色、主框架凭证、弹窗拒绝、CSP 与导出限制。
- `.github/workflows/package.yml`：保留维护者原有的 tag 构建、R2/CDN 上传、SSH 清单发布和旧版兼容部署，不要求新增签名密钥或迁移发行服务。

完整重构继承了原实现所依赖的启动保护：开发工作区及锁文件保护、HTTPS/IPv6 地址统一、启动取消保留环境、splash 就绪握手和旧 SSE 能力探测。未将个人仓库的 deploy 来源或便携目录后续变更带入本分支。

## 验证入口

```bash
cargo check --locked --all-targets
cargo test --locked --all-targets
node --test scripts/test-close-prompt.cjs scripts/test-webview-bridge.cjs
python3 scripts/test-sign-launcher-update.py
```

Rust 测试覆盖签名/清单负例、持久回退和安装前换包、来源角色矩阵、有界 SSE、真实临时子进程/worker、端口抢占、链接外部目标和 Unix 权限。Node 动态运行真实 bridge；Python/OpenSSL 与 Rust 共用签名字节夹具。测试不执行更新载荷、不访问正式更新服务，也不操作游戏账号。CI 保留 Linux、macOS、Windows x64/ARM64 的检查与测试。

## 平台与保证范围

Linux 验证不能替代 Windows/macOS 实机验收。Windows 条件代码的交叉检查不等于托盘、UAC、自启、安装器、Job Object 或更新助手的实际运行；macOS 的 lsof 解析器单测也不等于系统查询实测。

同一用户对安装目录与进程的主动修改不属于隔离保证。Unix 主动脱离 session、SIGKILL 以及 release 的 panic=abort 不执行常规 Drop 清理；HTTP 前后归属快照不构成密码学后端身份。当前 `.venv` 解释器版本探测仍依赖解释器正常返回，异常卡死时可能延迟取消。具体保证见 [签名更新](update-security.md)、[网页权限](webview-security.md) 与 [进程权限](process-security.md)。
