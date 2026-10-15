# 启动器权限与模块边界

本次范围仅为 Rust/Tauri 启动器、内嵌页面、Windows 安装器和发行脚本。Python 后端仍使用 `Metric-cell/AzurPilot` 的 `deploy` 分支，未修改后端源码。

## 更新权限

启动器自身更新和后端 Git 更新是两条独立流程。自身更新默认禁用：构建时必须同时提供更新清单 URL 和 Ed25519 发行公钥；没有原作者更新站、备用站或 mTLS 私钥回退。远端清单先验证原始字节的签名，再检查产品、来源、通道、版本、时间、序号、载荷来源、长度和 SHA256。安装前和 Windows 助手中重新验证授权。

具体配置、信任模型、七天清单有效期和签名操作见 [update-security.md](update-security.md)。私钥不进入应用程序或仓库。GitHub 发布仅在仓库变量 `AZURPILOT_SIGNED_UPDATES=true` 时启用；编译配置不会自动创建密钥或启用签名发布。GitHub metadata 和 payload 的 CDN 重定向分别需要明确允许。

后端仍会在发布模式启动时按部署配置更新 Git 仓库；开发工作区及 `--skip-update` 跳过这一步，并使用冻结依赖锁。直接运行 `gui.py` 不会调用启动器的更新器。上游 `master` 的运行期后台更新器不属于此次修改范围。

## 系统权限

Windows 清单为 `asInvoker`；启动器不主动请求管理员权限。安装器为 `PrivilegesRequired=lowest`，默认安装到当前用户的 LocalAppData，保留已有配置及运行数据，使用系统 Restart Manager 处理本安装注册文件的占用。已去掉全局 Python/Git 进程强杀、递归 ACL 重置、Everyone 完全控制、在线 ActiveX 协议页和静默系统 VC++ 安装。

Node.js 缺失或版本不足时，经用户同意下载固定官方便携包，使用源码固定 SHA256 校验，放入当前用户数据目录；不运行 MSI、不改系统 PATH。旧管理员安装不会被新安装器自动改权限或迁移，请使用新的用户安装目录。

`asInvoker` 会继承调用者令牌；手动以管理员启动仍会继承该权限。此处没有声称可以防御已经控制当前账号或已能修改应用文件的攻击者。

## 网页与原生权限

导航限于固定的后端来源及各窗口自己的内部协议路由。远端配置的后端可以展示网页，但没有原生命令、免密密钥、系统自启控制或桌面通知权限。

自定义命令同时经过 Tauri AppManifest/capability 和原生调用者校验。主框架 bridge 限制命令集合，splash、错误页、后端各有独立凭证；主窗口不能借导航获得其他页面角色的权限。外部窗口使用平台原生机制拒绝。内部页面携带 CSP，嵌入 script 的动态 JSON 单独转义。

导出须在可见窗口的系统保存对话框选择位置。文件名不接受路径或设备名，网页提交的导出内容最多 32 MiB，一次仅处理一个导出。日志来源由 Rust 固定，文件内容不返回网页；日志先写临时文件再替换，避免误截断源文件。详见 [webview-security.md](webview-security.md)。

免密换票、反向控制与通知只连接精确回环 HTTP(S) 来源，不跟随重定向。两条旧 SSE 接口共用有界解码器，不接受无限行或无限事件；不存在接口时停止订阅。系统自启只操作当前用户、本安装拥有的 Run 条目。

## 进程与代码职责

端口占用只报告冲突；环境变量和端口不会赋予结束进程的权限。Unix 保留本次启动的独立 session/组长身份直到最后一次终止信号；Windows 在恢复主线程前归入本次持有的 Job Object。HTTP 就绪前后还检查监听者归属。详细保证与边界见 [process-security.md](process-security.md)。

- `main.rs`：应用装配、托盘、页面切换和退出；`startup_state.rs` 管理工作线程寿命与启动期窗口状态。
- `update/`：签名策略、逐跳传输、下载、安装和 Windows 助手。
- `webview_security.rs` / `webview_bridge.js` / `popup_guard.rs`：来源、调用者、能力与弹窗控制。
- `commands.rs`：导出、日志和窗口命令；通过回调连接启动流程。
- `ui/`：启动画面、错误页面、标题栏、静态资源和 HTML 上下文转义。
- `process_security.rs`：进程内核身份与监听者归属。
- `backend.rs`：后端配置、HTTP 就绪和生命周期。
- `nodejs/`：固定官方来源、版本判定和暂存发布。
- `setup/`：环境准备的进度、下载源和工作区保护。

启动期间托盘和重复启动只唤醒 splash，不销毁或提前打开主窗口。启动工作线程用 Drop guard 释放清理等待，取消不会删除现有运行环境；release 仍使用 `panic="abort"`，进程 panic 直接终止，不能依赖 Rust Drop 清理 Unix 子进程。

## 验证与限制

本机验收（2026-10-15）：

- `cargo check --locked --all-targets` 与 `cargo build --locked` 通过。
- `cargo test --locked --all-targets`：主程序 117/117，安全集成 harness 18/18；两者包含部分共用安全用例。
- 原始 bridge 的 Node 动态测试 6/6；Python/OpenSSL 签名互操作测试 5/5。
- 后端 fixture 在 8 线程下重复 30 轮，每轮 22 项均通过；setup 31 项包含实际子进程取消、提前关闭输出、组长先退出、外部链接与权限保持。
- 四份语言文件新增错误键与参数、CI YAML 和三个内嵌 Python 脚本语法校验通过。

测试覆盖签名篡改/过期/回滚/重定向负例、权限矩阵、真实临时子进程与端口竞争。WSLg 错误预览实际完成主框架 IPC 握手并能正常退出。测试不启动游戏任务，也不访问实际更新站执行更新。

Linux/WSLg 的验证不能替代 Windows/macOS 实机验收。Windows API 的独立交叉编译不等同于完整安装器、托盘、UAC、自启、Job Object 或更新助手的运行验收。CI 保留四个平台的完整检查，后续发行仍需在对应系统验证这些用户流程。
