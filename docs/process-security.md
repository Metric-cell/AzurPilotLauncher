# 后端进程与 Node.js 权限边界

启动器及其 Python/Node 子进程以启动用户的普通权限运行。Windows manifest 使用 `asInvoker`；本模块不申请 UAC、修改目录 ACL、安装系统 MSI 或写系统 PATH/HKLM。

## 可以终止哪些进程

`ManagedBackend` 仅持有本次 `OwnedProcessGroup::spawn_backend` 返回的进程边界。监听端口、进程名、父 PID 快照和 `ALAS_LAUNCHER_PID` 均不授予终止权限。后者只作为子进程诊断信息传入，启动器不再设置自身的该环境变量，也不扫描系统环境补杀。

- Windows：先创建无名、不可继承的 Job Object 并开启 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`，再以 `CREATE_SUSPENDED` 创建子进程；加入 job 成功后才恢复主线程。归组或恢复失败时 RAII 负责终止和等待本次子进程。后续通过 job 内核句柄终止整棵受管子树，即使组长已退出也继续回收 worker；进程 PID 复用不改变 job 所有权。后端专用 `spawn_backend` 额外启用 `JOB_OBJECT_LIMIT_BREAKAWAY_OK` 并设置 `ALAS_LAUNCHER_JOB_BREAKAWAY=1`，允许配套 ALAS 显式请求脱离的模拟器继续运行；不启用 silent breakaway，普通 worker 继续受 Job 管理。setup 的普通 `spawn` 不授予 breakaway，且清除继承的能力变量。详见 [后端进程生命周期](backend-lifecycle.md)。
- Linux/macOS：子进程通过 `setsid` 进入独立 session/进程组，其他 session 的无关进程不能加入该组。就绪轮询使用 `waitid(WNOWAIT)` 观察退出，保留直接子进程直到最后一次组信号已经发出，防止回收组长后 PGID 被复用。退出先发送 `SIGTERM`，等待 500ms 后仍向整组发送 `SIGKILL`，最后回收组长；不能因为组长先退出就漏掉 worker。若外部 reaper 或 `SIGCHLD` 配置导致直接子进程身份丢失，停止发组信号并报告错误。

Unix 保证范围是受管进程组：正常 Python worker 继承该组。主动调用 `setsid` 或切换进程组的守护进程会离开此边界，启动器不靠可伪造标记跨边界追杀。若未来后端需要强制管理会主动脱离的工作进程，应使用显式协作或受支持的 OS 容器机制，不能恢复全系统 PID/环境扫描。启动器被 Unix `SIGKILL` 直接杀死时无法执行 Rust `Drop`；release 的 `panic = "abort"` 同样不会运行 Drop，因此 Unix 异常中止可能留下 worker，不能把启动 worker 的 Drop 守卫当成 abort 保证。Windows 进程终止时由系统关闭 job 句柄，kill-on-close 仍提供最后一道回收。普通取消、错误返回、正常退出和可展开的 panic 都执行清理。

## 端口与就绪

启动前对配置监听地址执行原生 `bind` 预检，短暂占用最多等待 300ms（包含并发 fork 在 exec 前继承的 CLOEXEC socket），持续冲突会返回本地化错误，保留原占用者。主机名先经过严格的 IP/DNS 校验，拒绝凭据、路径、查询、分片、端口注入；保留明确的 HTTPS 和 IPv6 配置。

预检与 spawn 之间的竞争通过监听者归属检查处理：HTTP 请求之前和成功响应之后都必须证明监听者属于本次边界，同时确认组长尚未退出。Linux 从 `/proc/net/tcp{,6}` 的监听 socket inode 与本组 fd 对照；Windows 从原生 TCP 所有者表取得 PID，再打开进程句柄调用 `IsProcessInJob`；macOS 使用固定系统路径 `/usr/sbin/lsof` 只读查询，并核对独立 session/进程组。macOS 查询同一端口的所有地址，可能保守地拒绝另一网卡上同号端口的无关监听者。无法验证归属会使启动失败，不会转换成终止权限。

HTTP 前后快照可以阻止普通并发程序抢占端口被当成后端。它不是针对同一用户恶意进程的加密证明：要抵抗精确安排在两次快照之间的恶意换主，应由后端提供每次启动的 challenge-response，或接收启动器预先绑定并传入的 socket。启动器不向未知 HTTP 服务发送信任密钥来充当身份检查。TLS 始终校验证书链/主机名，并只额外信任用户明确配置的证书。

setup 的长命令与状态命令复用同一进程边界：取消、可展开的回调 panic 和错误返回都会清理本次安装子树。输出管道关闭后仍轮询进程与取消；根进程先退出时先回收持有管道的 worker，再收尾输出，不会无限等待 EOF。

运行时清理与后端恢复都先用 `symlink_metadata` 识别符号链接/Windows reparse point，只删除链接本身，不遍历或 chmod 外部目标。恢复流程还拒绝将链接作为 `.venv` 根目录。Unix 清只读仅追加所有者写位 `0o200`，不开放组或其他用户写位。路径检查不构成对同一用户恶意并发替换目录的文件系统沙箱；目录仍应归当前普通用户管理。

## 用户级 Node.js

缺失与过旧都安装官方便携 zip，路径为 `%LOCALAPPDATA%\AzurPilotLauncher\nodejs\v<version>-win-<arch>`。来源与 SHA-256 在 `src/nodejs/source.rs` 中固定；只允许 `https://nodejs.org`，不跟随重定向，下载流式计算摘要后再从磁盘复验。URL、版本解析和文件事务与 Windows 编排分开，纯逻辑可在三平台测试。

安装在同盘随机 `.install-*` 暂存目录下载、解压并探测版本，验证成功前不改动现有安装。发布失败恢复原目录；若恢复本身失败，保留旧目录并在错误中提供路径。取消清理仅本次暂存内容，PowerShell 解压及其子进程通过同一 owned job 回收。下载每次网络等待最长 20 秒，总时限 15 分钟；`node --version` 探测有 3 秒上限。成功后仅把已验证目录前置到当前启动器进程的 PATH，不修改系统/用户持久 PATH，不覆盖系统 Node。

这些是同一普通用户下的安装目录，权限继承当前用户目录，不会扩张 ACL；同一用户可以主动修改自己的运行时。此设计依赖启动器不以管理员身份启动可写的后端/工具目录。

## 验证

`backend::tests` 使用独立临时目录与 Rust 子测试进程创建临时监听者/worker，不启动真实 Python、游戏或用户服务。覆盖无关端口保留、相同环境标记无关进程保留、正常就绪、子树回收、组长先退出、启动取消和并发端口抢占。实际端口 fixture 串行隔离 bind0/drop 资源竞争，每个 fixture 内保留显式 fork、取消和抢占并发；Windows `backend_process::tests` 通过生产 `OwnedProcessGroup` 验证显式终止、Drop、根进程提前退出保留模拟器及普通安装 Job 拒绝脱离。`setup::tests` 另外覆盖安装命令树取消、根提前退出但 worker 持输出管道、关闭输出后取消及链接外部 sentinel/Unix 权限保持。Node 纯测试覆盖官方来源限制、版本、磁盘篡改、取消暂存、原子发布失败回滚与 zip 布局。

Linux 可执行 `cargo test --locked --bin alas-launcher backend::tests` 和 `cargo test --locked --bin alas-launcher nodejs::`。macOS/Windows CI 应执行同一测试。Windows 需普通用户终端；Linux 上可额外 `cargo check --target x86_64-pc-windows-gnu` 验证条件编译，但交叉编译不能代替 Windows job/线程/API 的实际运行测试。

参考：[Microsoft Job Object 生命周期](https://learn.microsoft.com/en-us/windows/win32/api/jobapi2/nf-jobapi2-createjobobjectw)、[Microsoft job 归组与子进程继承](https://learn.microsoft.com/en-us/windows/win32/api/jobapi2/nf-jobapi2-assignprocesstojobobject)、[waitid/WNOWAIT](https://man7.org/linux/man-pages/man2/waitpid.2.html)、[setsid](https://man7.org/linux/man-pages/man2/setsid.2.html)。
