# WebView 与原生命令边界

## 来源与窗口

`WebviewPolicy` 在启动时从同一份 `WebuiLaunchConfig::backend_url()` 创建，后续导航、注入、权限和重试共用这一份不可变策略。后端配置必须是没有用户名、密码、路径、query、fragment 的 HTTP(S) origin。页面可在这个 origin 下访问不同路径；scheme、主机和有效端口必须一致。

| 文档 | window / webview label | Linux、macOS URL | Windows URL | 原生命令 |
| --- | --- | --- | --- | --- |
| 启动画面 | `splash` / `splash` | `alas-splash://localhost/` | `http://alas-splash.localhost/` | splash 命令集 |
| 后端错误页 | `main` / `main` | `alas-error://localhost/backend` | `http://alas-error.localhost/backend` | error 命令集 |
| 配置中的回环后端 | `main` / `main` | 配置的精确 HTTP(S) origin | 同左 | backend 命令集 |
| 配置中的远端后端 | `main` / `main` | 配置的精确 HTTP(S) origin | 同左 | 无 |

内部页还必须匹配精确路由、空用户信息和正确端口；splash 不接受 query 或 fragment，错误页仅允许 query。`about:blank` 仅用于 Tauri 创建窗口时的初始占位，不是允许的导航目标，也不会获得 IPC 权限。未知 label、子 WebView、未知协议、`data:`、`blob:`、`javascript:`、`file:` 和其他来源全部拒绝导航。注册协议本身不授予其任意主机或路径权限。

`is_loopback_http_origin()` 仅接受无凭据的 HTTP(S)、精确 `localhost` 或 `IpAddr::is_loopback()` 地址，支持 HTTPS 和 `[::1]`。`localhost.evil`、`evil.localhost`、`localhost.`、IPv4-mapped IPv6 和私网地址不获原生权限。配置远端 WebUI 只授权呈现页面；免密 secret、反向 SSE 自启控制和 native bridge 均须另以这个 helper 限制到回环地址。远端 WebUI 保留系统窗口边框，因为不会注入依赖原生命令的自绘标题栏。

## 命令矩阵

所有命令只操作实际 caller 的窗口，页面不能传目标窗口、端口、源日志路径或保存目录。`Caller` 必须是每个导出命令的第一个参数。

| 命令 | splash | error | loopback backend |
| --- | :---: | :---: | :---: |
| `get_launcher_preferences` | — | — | ✓ |
| `set_launcher_close_behavior` | — | — | ✓ |
| `window_choose_close_behavior` | — | ✓ | ✓ |
| `save_as` | — | — | ✓ |
| `splash_ready` | ✓ | — | — |
| `download_today_gui_log` | — | ✓ | — |
| `download_today_launcher_log` | ✓ | ✓ | — |
| `retry_backend_connection` | — | ✓ | — |
| `window_hide` | — | ✓ | ✓ |
| `window_minimize` | ✓ | ✓ | ✓ |
| `window_toggle_maximize` | — | ✓ | ✓ |
| `window_close` | ✓ | ✓ | ✓ |
| `window_exit_application` | — | ✓ | ✓ |
| `window_start_dragging` | ✓ | ✓ | ✓ |
| `window_is_maximized` | — | ✓ | ✓ |

远端 backend 不在命令矩阵内。导出文件、导出日志、关闭偏好读写/选择和退出应用还要求 caller 可见且未最小化；保存对话框的父窗口是这个 caller。

## 三层 IPC 验证

1. `build.rs` 使用 `tauri_build::AppManifest::new().commands(COMMANDS)` 声明上表全部 15 个应用命令。Tauri 为其生成 `allow-save-as` 等权限。少了 AppManifest，锁定版本默认不会对自定义应用命令执行 capability 拦截。
2. `install_capabilities()` 在 setup 最开始安装按窗口限制的运行时 capability。splash/error 只使用 `local: true`；回环 backend 单独使用 `local: false` 和精确 origin 的 remote URLPattern。没有 `core:default`、通知或 dialog 的 JavaScript 权限；Rust 内部 dialog 调用不需要这些权限。新增命令必须同步 AppManifest、Rust 来源矩阵和审核测试。
3. 每个命令的 `Caller` 从实际 `CommandItem` 获取 WebView，检查 window/webview label 一致、当前顶层 URL、命令角色及私有凭证。缺失状态、URL 读取失败和不合法参数通过 `Result` 拒绝，不依赖 panic。

`withGlobalTauri: false` 仅移除方便调用的全局对象；`__TAURI_INTERNALS__` 仍是 Tauri 的实现细节，不能单独视作访问控制。AppManifest 和 capability 必须保留。

锁定 `urlpattern 0.3.0` 将未转义的 IPv6 冒号当作命名参数，直接把 `https://[::1]:22267/*` 交给 capability 会 panic。策略只对 hostname 中的冒号转义，生成 `https://[\:\:1]:22267/*`，不放宽主机匹配；构造策略时先用 Tauri 同一个 `RemoteUrlPattern` 解析器校验。真实 `RuntimeAuthority` 测试覆盖 IPv4、IPv6、错误 scheme/主机/端口和窗口。

## 主框架与导航竞态

`webview_bridge.js` 是 plugin 的 document-start 初始化脚本，不是 HTML 中的 script。它在读取凭证之前检查 `window.top === window`，核对 Tauri 窗口元数据和当前来源，再安装只读、冻结的 `window.__ALAS_NATIVE__.invoke`。三个角色各自使用 256-bit 随机会话凭证；凭证不放入 URL、HTML 或日志，也不出现在导出函数的 `toString()` 中。用户传入的 `launcherToken` 参数会被 wrapper 的私有凭证覆盖。

wrapper 在文档启动时捕获 `initialRole` 和相应 token，每次调用仍检查顶层框架、当前角色不变及命令矩阵。不能在调用时动态选择另一个角色的 token。这样，backend 的旧请求即使在 Linux 错误地携带当前顶层 URL，也不能在导航到 error 后获取 error 专属能力。

`Caller::revalidate()` 在系统对话框返回、后台重试和 await 后复核来源/角色；重试服务回调接收 `still_authorized`，必须在可达性等待后、请求免密 URL 前调用。最终目标再次检查配置 origin。页面加载后的 UI 注入同时验证事件 URL 与当前 URL，执行脚本还检查顶层框架和 `__ALAS_NATIVE__`。

这些是**会话及角色凭证，不是逐文档凭证**。同角色、同 origin 重载后的旧异步操作仍可能通过检查；已经通过验证并开始的原生文件写入也不会因稍后导航而撤销。同 origin 页面和同 origin 子框架具有浏览器本身的同源权限；本方案不提供它们之间的隔离。受信后端中的任意脚本执行仍拥有 backend 命令集，需要后端自身防护 XSS。

这个补充凭证是锁定 Wry 平台差异下的必要防线：Linux 的 iframe IPC 使用主框架 URL，单靠 origin-aware ACL 和 `window.url()` 都无法确定发起的 frame；Windows 又会忽略初始化脚本的 `for_main_frame_only` 标志，把脚本送入子框架。跨 origin 子框架不会收到顶层私有凭证。AppManifest 仍用于其他平台的请求来源校验和跨源导航竞态，不能被当前窗口 URL 检查替代。

## 新窗口与弹窗

Tauri 2.5.1 没有公开 Wry 的 new-window handler。`popup_guard` 通过 plugin `on_webview_ready` 为现有及以后创建的 WebView 安装原生策略：

| 平台 | 原生拒绝方式 |
| --- | --- |
| Linux | WebKitGTK `decide-policy(NewWindowAction)` 调用 `ignore()`；`create` 返回 `None` |
| Windows | WebView2 `NewWindowRequested` 事件设置 `Handled = true` |
| macOS | 检查 Wry 的 WKUIDelegate 未实现 `createWebViewWithConfiguration`；WebKit 约定未实现此方法即取消新窗口。若以后依赖更换成实现该方法的 delegate，拒绝继续运行这个 WebView |

安装失败会记录错误并关闭相关窗口。macOS 保留原有 Wry delegate 的上传文件面板，不替换整个 delegate。升级 Tauri/Wry 后须重审这些平台钩子；当前实现无需升级框架。

document-start 脚本另外固定 `window.open()` 返回 `null`，拦截不允许来源的链接和表单，允许的链接/表单改为当前窗口。这是对原生 veto 的补充。全局 plugin `on_navigation` 负责统一来源限制，主窗口自己的导航回调继续执行相同策略和后端可达性检查。

平台依赖须与锁文件中的底层库一致：Linux `webkit2gtk = "=2.0.1"`，Windows `webview2-com = "=0.37.0"`，macOS `objc2 = "=0.6.1"`。Linux 已实际编译；Windows/macOS 仍须在对应 CI 和桌面环境验收原生行为。

## 文件导出

`save_as` 只接收一个最大 255 UTF-8 字节的文件名，拒绝分隔符、绝对路径、设备名、控制字符、双向文字控制符和 Windows 特殊字符。前端在 `FileReader` 前限制 Blob 为 32 MiB；Rust 再在分配解码缓冲区前验证编码长度、padding 和精确解码体积，使用 `try_reserve_exact` 与固定输出缓冲区。全应用同一时间只能存在一个导出/日志对话框及其待写数据。导出内容不再写入 console 或日志。

32 MiB 限制约束导出解码和本应用新增的缓冲区；Tauri 自己解析收到的 IPC JSON 时已经需要内存，不能把它宣称为 WebView 整体的硬内存隔离。

日志只从启动时 canonicalize 的固定 log 目录读取当天 GUI/launcher 文件，页面不能选择源路径，内容不会返回给 JavaScript。源文件按打开后的句柄验证为普通文件；Unix 用 `O_NOFOLLOW`，Windows 用 `FILE_FLAG_OPEN_REPARSE_POINT` 并拒绝 reparse 属性，避免先检查最终路径再打开的竞态。此处不声称能隔离具有本机目录修改权限的攻击者对父目录的并发替换。

所有保存操作都先写目标目录内的临时文件，成功后替换用户选择的目标，避免选中源日志或硬链接时先截断源句柄。Unix 另外用 dev/inode 拒绝源日志硬链接；跨平台拒绝同一 canonical 路径。日志复制使用流式 IO，不把整份日志读入内存。保存地址来自系统 save dialog，且 dialog 以实际 caller 为父窗口。

## UI 模块

`src/ui` 按职责分为 `splash.rs/html`、`error.rs/html`、`titlebar.rs/js` 和 `assets.rs`。渲染函数不读取后端配置，错误页明确传入显示地址和错误文本；重试不再从页面传端口。视频、字体和进度图片的 base64 使用 `OnceLock` 缓存。

写入 HTML 内 script 的 JSON 额外转义 `<`、`>`、`&`、U+2028 和 U+2029，防止错误文本中的 `</script>` 结束 script 标签。内部协议响应显式设置 CSP、`no-store` 和 `nosniff`：只加载内嵌资源及 IPC，禁止远端脚本、表单和框架。自定义协议响应不会自动继承 Tauri 资源协议的 CSP。

## 锁定版本依据与验证

检查依据为本地 Cargo registry 中与 `Cargo.lock` 一致的源代码：

- `tauri-2.5.1/src/webview/mod.rs`：`is_local_url` 对自定义协议的宽泛 Local 分类；`on_message` 使用 IPC 请求 URL 求 ACL；仅存在 AppManifest 时对自定义应用命令强制 ACL。
- `tauri-2.5.1/src/manager/webview.rs`：plugin 初始化脚本设置 `for_main_frame_only: true`。
- `tauri-utils-2.4.0/src/acl/{mod.rs,resolved.rs}`：`RemoteUrlPattern` 解析和 capability 解析失败 panic 路径。
- `wry-0.51.2/src/lib.rs`：初始化脚本的 Windows 子框架限制，以及 Linux/Android IPC URL 使用顶层 URL 的限制；`src/webkitgtk/mod.rs` 中实际采用 `webview.uri()`。
- `wry-0.51.2/src/wkwebview/class/wry_web_view_ui_delegate.rs`：锁定默认 delegate 不实现新窗口创建方法；`objc2-web-kit-0.3.1/src/generated/WKUIDelegate.rs` 说明未实现该方法会取消导航。

自动验收：

```sh
cargo check --locked --all-targets
cargo test --locked --all-targets
node --test scripts/test-close-prompt.cjs scripts/test-webview-bridge.cjs
```

无需显示服务器的安全 harness 为 `tests/webview_security.rs`。它运行来源/凭证/权限矩阵、真实锁定 Tauri ACL、编码预算、文件路径/句柄、导出互斥和 UI script 注入测试。Node 脚本直接执行实际 bridge 模板，验证顶层/子框架、未知来源、导航后旧 wrapper、原始 invoke、token 参数覆盖及链接/表单弹窗防护；mock 接收端不替代 Rust 的实际授权测试。Linux CI 通过 `node --test scripts/test-close-prompt.cjs scripts/test-webview-bridge.cjs` 运行关闭菜单及统一 bridge 回归，显式安装 Node 22；脚本要求 Node 18 或更新版本。

原始安全实现的 Linux harness 与 Node 动态测试已验证；新增偏好测试继续覆盖下述路径。WSLg `--preview-crash` 实际 smoke 中收到 `Splash main frame acknowledged readiness`，确认 splash 首次握手穿过 AppManifest、capability、主框架 bridge 和 Caller；运行 12 秒后 SIGINT 正常清理。完整仓库检查与 Windows/macOS 行为由三平台验收继续覆盖。

桌面验收仍需检查 splash 首次 `splash_ready`、更新推送与重放、错误页重试和两个日志下载、32 MiB 内文件保存、取消对话框、标题栏拖动/关闭/托盘，以及 `window.open`、`target=_blank`、表单弹窗的原生拒绝。远端配置应能呈现但没有 native wrapper、免密请求和反向系统控制。


## 关闭偏好与托盘恢复

`PreferenceStore` 保持独立的 `preferences.json` 原子持久化；关闭菜单必须先成功保存再退出/后台运行，保存失败保留窗口供重试。Windows/macOS 托盘“关闭窗口时”子菜单直接调用同一保存函数，仅修改偏好、不执行关闭，并以原生错误对话框报告失败。该入口不依赖后端设置 UI。

后端设置可通过统一的 `__ALAS_NATIVE__` 调用读取/设置命令；这两个命令仅授权本地后端。错误页只额外获得首次关闭选择命令，splash 无偏好权限。移除早期独立 `preferences_bridge.js`，避免重复定义桥或绕过角色 token。Rust 矩阵测试与 Node 动态测试覆盖三个偏好命令、错误角色/子框架/错误 token、设置参数转发及跨角色导航后的旧 wrapper 拒绝。

托盘化保留主窗口及页面连接，只执行隐藏；恢复仍受 `StartupState` 门槛控制，窗口意外丢失时可重新创建。Linux 无托盘且直接退出，macOS 保留系统标题栏并注入关闭菜单。
