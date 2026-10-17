//! 只凭本次 spawn 保留的内核身份管理后端，端口与环境变量均不授予终止权限。
//!
//! Unix 子进程在独立 session 中运行，组长退出后保留为未回收子进程，直到
//! 最后一次组信号已发出，防止 PID/PGID 复用。Windows 使用无名、不可继承的
//! kill-on-close Job Object，在恢复悬挂主线程前完成归组。

use std::{
    collections::BTreeSet,
    io,
    net::IpAddr,
    process::{Child, Command, ExitStatus},
    thread,
    time::{Duration, Instant},
};

use tracing::warn;

const TERMINATION_GRACE: Duration = Duration::from_millis(500);

pub(crate) struct OwnedProcessGroup {
    child: Option<Child>,
    #[cfg(unix)]
    identity_valid: bool,
    #[cfg(windows)]
    job: std::os::windows::io::OwnedHandle,
}

impl OwnedProcessGroup {
    pub(crate) fn spawn(command: &mut Command) -> io::Result<Self> {
        platform::spawn(command, false)
    }

    /// 仅后端允许 ALAS 显式启动独立模拟器；安装命令不获得此能力。
    pub(crate) fn spawn_backend(command: &mut Command) -> io::Result<Self> {
        platform::spawn(command, true)
    }

    pub(crate) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        platform::try_wait(self)
    }

    pub(crate) fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.child.as_mut()?.stdout.take()
    }

    pub(crate) fn take_stderr(&mut self) -> Option<std::process::ChildStderr> {
        self.child.as_mut()?.stderr.take()
    }

    pub(crate) fn terminate(&mut self) -> io::Result<ExitStatus> {
        platform::terminate(self)
    }

    /// 无监听者时为 Pending；存在任一无法证明归属的监听者时为 Foreign。
    /// 地址过滤避免把其他网卡上同号端口误当成本次后端。
    pub(crate) fn listener_state(
        &mut self,
        port: u16,
        addresses: &[IpAddr],
    ) -> io::Result<ListenerState> {
        platform::listener_state(self, port, addresses)
    }
}

impl Drop for OwnedProcessGroup {
    fn drop(&mut self) {
        if self.child.is_some() {
            if let Err(error) = self.terminate() {
                warn!("Unable to finish owned process group cleanup: {error}");
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ListenerState {
    Pending,
    Owned,
    Foreign,
}

fn address_matches(address: IpAddr, configured: &[IpAddr]) -> bool {
    configured.iter().any(|configured| {
        configured == &address
            || configured.is_unspecified() && configured.is_ipv4() == address.is_ipv4()
            || address.is_unspecified() && configured.is_ipv4() == address.is_ipv4()
    })
}

#[cfg(unix)]
mod platform {
    use super::*;
    use nix::libc;
    use std::os::unix::process::{CommandExt, ExitStatusExt};

    pub(super) fn spawn(
        command: &mut Command,
        allow_breakaway: bool,
    ) -> io::Result<OwnedProcessGroup> {
        let _ = allow_breakaway;
        command.env_remove("ALAS_LAUNCHER_JOB_BREAKAWAY");
        // SAFETY: setsid 只调用 async-signal-safe 的系统接口，不分配内存或加锁。
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let child = command.spawn()?;
        Ok(OwnedProcessGroup {
            child: Some(child),
            identity_valid: true,
        })
    }

    /// WNOWAIT 是安全边界：不得改成 Child::try_wait（会提前回收组长）。
    pub(super) fn try_wait(group: &mut OwnedProcessGroup) -> io::Result<Option<ExitStatus>> {
        let Some(child) = &group.child else {
            return Ok(Some(ExitStatus::default()));
        };
        if !group.identity_valid {
            return Err(io::Error::other("owned process group identity was lost"));
        }
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        loop {
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    child.id() as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result == 0 {
                break;
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if error.raw_os_error() == Some(libc::ECHILD) {
                // 外部 reaper/SIGCHLD 配置若破坏了保留身份，绝不能继续发组信号。
                group.identity_valid = false;
            }
            return Err(error);
        }
        if unsafe { info.si_pid() } == 0 {
            return Ok(None);
        }
        let code = unsafe { info.si_status() };
        let raw_status = match info.si_code {
            libc::CLD_EXITED => code << 8,
            libc::CLD_KILLED => code,
            libc::CLD_DUMPED => code | 0x80,
            _ => return Err(io::Error::other("unexpected child wait status")),
        };
        Ok(Some(ExitStatus::from_raw(raw_status)))
    }

    fn signal(group: &mut OwnedProcessGroup, signal: i32) -> io::Result<()> {
        // 证明仍是本进程的未回收直接子进程，再使用被它钉住的组号。
        try_wait(group)?;
        let pid = group.child.as_ref().unwrap().id() as libc::pid_t;
        if unsafe { libc::getsid(pid) } != pid || unsafe { libc::getpgid(pid) } != pid {
            group.identity_valid = false;
            return Err(io::Error::other("owned process group session changed"));
        }
        if unsafe { libc::kill(-pid, signal) } == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }
        Ok(())
    }

    pub(super) fn terminate(group: &mut OwnedProcessGroup) -> io::Result<ExitStatus> {
        if group.child.is_none() {
            return Ok(ExitStatus::default());
        }
        signal(group, libc::SIGTERM)?;
        let deadline = Instant::now() + TERMINATION_GRACE;
        while Instant::now() < deadline {
            // 即使组长先退出，也必须给仍在同组的 worker 发送最后一次 SIGKILL。
            try_wait(group)?;
            thread::sleep(Duration::from_millis(20));
        }
        signal(group, libc::SIGKILL)?;
        // 此后不会再使用 PGID，可以安全回收。wait 失败保留句柄供 Drop 重试。
        let status = group.child.as_mut().unwrap().wait()?;
        group.child.take();
        Ok(status)
    }

    fn owns_pid(group: &OwnedProcessGroup, pid: u32) -> bool {
        let Some(child) = &group.child else {
            return false;
        };
        group.identity_valid
            && unsafe { libc::getsid(pid as libc::pid_t) } == child.id() as libc::pid_t
            && unsafe { libc::getpgid(pid as libc::pid_t) } == child.id() as libc::pid_t
    }

    #[cfg(target_os = "linux")]
    pub(super) fn listener_state(
        group: &mut OwnedProcessGroup,
        port: u16,
        addresses: &[IpAddr],
    ) -> io::Result<ListenerState> {
        try_wait(group)?;
        let mut inodes = BTreeSet::new();
        for (path, ipv6) in [("/proc/net/tcp", false), ("/proc/net/tcp6", true)] {
            let contents = match std::fs::read_to_string(path) {
                Ok(contents) => contents,
                Err(error) if ipv6 && error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            inodes.extend(parse_linux_listeners(&contents, port, addresses, ipv6));
        }
        if inodes.is_empty() {
            return Ok(ListenerState::Pending);
        }
        // 只读取本组进程的 fd；不需要访问其他用户的环境、命令行或文件描述符。
        let mut owned_pids = BTreeSet::new();
        for process in std::fs::read_dir("/proc")? {
            let process = process?;
            let Some(pid) = process
                .file_name()
                .to_str()
                .and_then(|name| name.parse().ok())
            else {
                continue;
            };
            if !owns_pid(group, pid) {
                continue;
            }
            owned_pids.insert(pid);
            let Ok(descriptors) = std::fs::read_dir(process.path().join("fd")) else {
                continue;
            };
            for descriptor in descriptors.flatten() {
                let Ok(target) = std::fs::read_link(descriptor.path()) else {
                    continue;
                };
                let target = target.to_string_lossy();
                let Some(inode) = target
                    .strip_prefix("socket:[")
                    .and_then(|value| value.strip_suffix(']'))
                    .and_then(|value| value.parse::<u64>().ok())
                else {
                    continue;
                };
                // 身份在 fd 读取前后都成立；会话外的 PID 复用不能获得归属。
                if owns_pid(group, pid) {
                    inodes.remove(&inode);
                }
            }
        }
        Ok(if inodes.is_empty() {
            ListenerState::Owned
        } else {
            warn!(
                port,
                ?inodes,
                ?owned_pids,
                child = group.child.as_ref().map(Child::id),
                "TCP listener inode has no owner in the retained backend process group"
            );
            ListenerState::Foreign
        })
    }

    #[cfg(target_os = "linux")]
    fn parse_linux_listeners(
        contents: &str,
        port: u16,
        addresses: &[IpAddr],
        ipv6: bool,
    ) -> BTreeSet<u64> {
        contents
            .lines()
            .filter_map(|line| {
                let fields: Vec<_> = line.split_whitespace().collect();
                if fields.len() < 10 || fields[3] != "0A" {
                    return None;
                }
                let (address, local_port) = fields[1].rsplit_once(':')?;
                if u16::from_str_radix(local_port, 16).ok()? != port {
                    return None;
                }
                let address: IpAddr = if ipv6 {
                    if address.len() != 32 {
                        return None;
                    }
                    let mut bytes = [0u8; 16];
                    for (index, part) in bytes.chunks_exact_mut(4).enumerate() {
                        part.copy_from_slice(
                            &u32::from_str_radix(&address[index * 8..index * 8 + 8], 16)
                                .ok()?
                                .to_ne_bytes(),
                        );
                    }
                    std::net::Ipv6Addr::from(bytes).into()
                } else {
                    std::net::Ipv4Addr::from(u32::from_str_radix(address, 16).ok()?.to_ne_bytes())
                        .into()
                };
                address_matches(address, addresses)
                    .then(|| fields[9].parse().ok())
                    .flatten()
            })
            .collect()
    }

    #[cfg(target_os = "macos")]
    pub(super) fn listener_state(
        group: &mut OwnedProcessGroup,
        port: u16,
        _addresses: &[IpAddr],
    ) -> io::Result<ListenerState> {
        use crate::window_util::CreateNoWindow as _;
        try_wait(group)?;
        // macOS 自带 lsof；固定系统路径，环境不参与可执行文件解析。
        let output = Command::new("/usr/sbin/lsof")
            .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-t"])
            .create_no_window()
            .output()?;
        if !output.status.success() && (!output.stdout.is_empty() || !output.stderr.is_empty()) {
            return Err(io::Error::other("unable to inspect backend port ownership"));
        }
        let pids: BTreeSet<u32> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.trim().parse().ok())
            .collect();
        Ok(if pids.is_empty() {
            ListenerState::Pending
        } else if pids.into_iter().all(|pid| owns_pid(group, pid)) {
            ListenerState::Owned
        } else {
            ListenerState::Foreign
        })
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::{
        mem::{size_of, zeroed},
        os::windows::{
            io::{AsRawHandle, FromRawHandle, OwnedHandle},
            process::CommandExt,
        },
        ptr,
    };
    use winapi::{
        shared::{
            iprtrmib::TCP_TABLE_OWNER_PID_LISTENER,
            minwindef::FALSE,
            tcpmib::{
                MIB_TCP6ROW_OWNER_PID, MIB_TCP6TABLE_OWNER_PID, MIB_TCPROW_OWNER_PID,
                MIB_TCPTABLE_OWNER_PID, MIB_TCP_STATE_LISTEN,
            },
            winerror::{ERROR_INSUFFICIENT_BUFFER, ERROR_NO_MORE_FILES, NO_ERROR},
            ws2def::{AF_INET, AF_INET6},
        },
        um::{
            handleapi::INVALID_HANDLE_VALUE,
            iphlpapi::GetExtendedTcpTable,
            jobapi::IsProcessInJob,
            jobapi2::{
                AssignProcessToJobObject, CreateJobObjectW, QueryInformationJobObject,
                SetInformationJobObject, TerminateJobObject,
            },
            processthreadsapi::{GetProcessIdOfThread, OpenProcess, OpenThread, ResumeThread},
            tlhelp32::{
                CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD,
                THREADENTRY32,
            },
            winbase::{CREATE_NO_WINDOW, CREATE_SUSPENDED},
            winnt::{
                JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation, HANDLE,
                JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                PROCESS_QUERY_LIMITED_INFORMATION, THREAD_QUERY_LIMITED_INFORMATION,
                THREAD_SUSPEND_RESUME,
            },
        },
    };

    fn handle(raw: HANDLE) -> io::Result<OwnedHandle> {
        if raw.is_null() || raw == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            // SAFETY: 每个调用都接收新取得且未转交其他所有者的 Win32 句柄。
            Ok(unsafe { OwnedHandle::from_raw_handle(raw.cast()) })
        }
    }

    fn checked(success: i32) -> io::Result<()> {
        if success == FALSE {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub(super) fn spawn(
        command: &mut Command,
        allow_breakaway: bool,
    ) -> io::Result<OwnedProcessGroup> {
        // 无名且不可继承，其他程序不能通过同名 job 或继承句柄加入此边界。
        let job = handle(unsafe { CreateJobObjectW(ptr::null_mut(), ptr::null()) })?;
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | if allow_breakaway {
                JOB_OBJECT_LIMIT_BREAKAWAY_OK
            } else {
                0
            };
        if allow_breakaway {
            command.env("ALAS_LAUNCHER_JOB_BREAKAWAY", "1");
        } else {
            command.env_remove("ALAS_LAUNCHER_JOB_BREAKAWAY");
        }
        checked(unsafe {
            SetInformationJobObject(
                job.as_raw_handle().cast(),
                JobObjectExtendedLimitInformation,
                (&mut limits as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        })?;

        let flags = CREATE_SUSPENDED
            | if crate::window_util::HAS_CONSOLE.load(std::sync::atomic::Ordering::Relaxed) {
                0
            } else {
                CREATE_NO_WINDOW
            };
        command.creation_flags(flags);
        let child = command.spawn()?;
        let group = OwnedProcessGroup {
            child: Some(child),
            job,
        };
        // 从 spawn 返回起即有 RAII 保护；任一步失败都会杀并等待本次 child。
        checked(unsafe {
            AssignProcessToJobObject(
                group.job.as_raw_handle().cast(),
                group.child.as_ref().unwrap().as_raw_handle().cast(),
            )
        })?;
        resume_child(group.child.as_ref().unwrap().id())?;
        Ok(group)
    }

    fn resume_child(pid: u32) -> io::Result<()> {
        let snapshot = handle(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) })?;
        let mut entry: THREADENTRY32 = unsafe { zeroed() };
        entry.dwSize = size_of::<THREADENTRY32>() as u32;
        checked(unsafe { Thread32First(snapshot.as_raw_handle().cast(), &mut entry) })?;
        let mut resumed = false;
        loop {
            if entry.th32OwnerProcessID == pid {
                let thread = handle(unsafe {
                    OpenThread(
                        THREAD_SUSPEND_RESUME | THREAD_QUERY_LIMITED_INFORMATION,
                        FALSE,
                        entry.th32ThreadID,
                    )
                })?;
                if unsafe { GetProcessIdOfThread(thread.as_raw_handle().cast()) } != pid {
                    return Err(io::Error::other("suspended child thread identity changed"));
                }
                let previous_count = unsafe { ResumeThread(thread.as_raw_handle().cast()) };
                if previous_count == u32::MAX {
                    return Err(io::Error::last_os_error());
                }
                // 快照可能包含注入线程；确保挂起的初始线程确实恢复。
                resumed |= previous_count > 0;
            }
            if unsafe { Thread32Next(snapshot.as_raw_handle().cast(), &mut entry) } == FALSE {
                let error = io::Error::last_os_error();
                return if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                    if resumed {
                        Ok(())
                    } else {
                        Err(io::Error::other(
                            "suspended child main thread was not found",
                        ))
                    }
                } else {
                    Err(error)
                };
            }
        }
    }

    pub(super) fn try_wait(group: &mut OwnedProcessGroup) -> io::Result<Option<ExitStatus>> {
        match group.child.as_mut() {
            Some(child) => child.try_wait(),
            None => Ok(Some(ExitStatus::default())),
        }
    }

    pub(super) fn terminate(group: &mut OwnedProcessGroup) -> io::Result<ExitStatus> {
        let Some(child) = group.child.as_mut() else {
            return Ok(ExitStatus::default());
        };
        // Job 的内核句柄不会因 PID 复用指向其他进程；失败时也回收尚未归组的 child。
        let job_result =
            checked(unsafe { TerminateJobObject(group.job.as_raw_handle().cast(), 1) });
        let child_result = child.kill();
        if let Err(error) = job_result {
            child_result?;
            return Err(error);
        }
        let status = child.wait()?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { zeroed() };
            checked(unsafe {
                QueryInformationJobObject(
                    group.job.as_raw_handle().cast(),
                    JobObjectBasicAccountingInformation,
                    (&mut accounting as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                    size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    ptr::null_mut(),
                )
            })?;
            if accounting.ActiveProcesses == 0 {
                group.child.take();
                return Ok(status);
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "owned job did not finish exiting",
                ));
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn owns_pid(group: &OwnedProcessGroup, pid: u32) -> io::Result<bool> {
        let process =
            handle(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid) })?;
        let mut in_job = FALSE;
        checked(unsafe {
            IsProcessInJob(
                process.as_raw_handle().cast(),
                group.job.as_raw_handle().cast(),
                &mut in_job,
            )
        })?;
        Ok(in_job != FALSE)
    }

    fn table_rows<T>(storage: &[u64], bytes: u32, offset: usize) -> io::Result<&[T]> {
        if bytes < size_of::<u32>() as u32 || bytes as usize > std::mem::size_of_val(storage) {
            return Err(io::Error::other("invalid TCP ownership table size"));
        }
        let count = unsafe { *storage.as_ptr().cast::<u32>() } as usize;
        let needed = count
            .checked_mul(size_of::<T>())
            .and_then(|length| length.checked_add(offset))
            .ok_or_else(|| io::Error::other("invalid TCP ownership table length"))?;
        if needed > bytes as usize {
            return Err(io::Error::other("truncated TCP ownership table"));
        }
        // 两种 row 都按 DWORD 对齐，u64 存储和原生 table 字段偏移满足此约束。
        Ok(unsafe {
            std::slice::from_raw_parts(storage.as_ptr().cast::<u8>().add(offset).cast(), count)
        })
    }

    pub(super) fn listener_state(
        group: &mut OwnedProcessGroup,
        port: u16,
        addresses: &[IpAddr],
    ) -> io::Result<ListenerState> {
        let mut pids = BTreeSet::new();
        for family in [AF_INET, AF_INET6] {
            if !addresses
                .iter()
                .any(|address| address.is_ipv4() == (family == AF_INET))
            {
                continue;
            }
            let mut bytes = 0;
            let first = unsafe {
                GetExtendedTcpTable(
                    ptr::null_mut(),
                    &mut bytes,
                    FALSE,
                    family as u32,
                    TCP_TABLE_OWNER_PID_LISTENER,
                    0,
                )
            };
            if first != ERROR_INSUFFICIENT_BUFFER && first != NO_ERROR {
                return Err(io::Error::from_raw_os_error(first as i32));
            }
            loop {
                // u64 保证对 TCP 表头与行的自然对齐；只在成功后读取返回大小内的数据。
                let mut table = vec![0u64; (bytes as usize + 7) / 8];
                let result = unsafe {
                    GetExtendedTcpTable(
                        table.as_mut_ptr().cast(),
                        &mut bytes,
                        FALSE,
                        family as u32,
                        TCP_TABLE_OWNER_PID_LISTENER,
                        0,
                    )
                };
                if result == ERROR_INSUFFICIENT_BUFFER {
                    continue;
                }
                if result != NO_ERROR {
                    return Err(io::Error::from_raw_os_error(result as i32));
                }
                if family == AF_INET {
                    let rows = table_rows::<MIB_TCPROW_OWNER_PID>(
                        &table,
                        bytes,
                        std::mem::offset_of!(MIB_TCPTABLE_OWNER_PID, table),
                    )?;
                    for row in rows {
                        let address =
                            std::net::Ipv4Addr::from(row.dwLocalAddr.to_ne_bytes()).into();
                        if row.dwState == MIB_TCP_STATE_LISTEN
                            && u16::from_be(row.dwLocalPort as u16) == port
                            && address_matches(address, addresses)
                        {
                            pids.insert(row.dwOwningPid);
                        }
                    }
                } else {
                    let rows = table_rows::<MIB_TCP6ROW_OWNER_PID>(
                        &table,
                        bytes,
                        std::mem::offset_of!(MIB_TCP6TABLE_OWNER_PID, table),
                    )?;
                    for row in rows {
                        let address = std::net::Ipv6Addr::from(row.ucLocalAddr).into();
                        if row.dwState == MIB_TCP_STATE_LISTEN
                            && u16::from_be(row.dwLocalPort as u16) == port
                            && address_matches(address, addresses)
                        {
                            pids.insert(row.dwOwningPid);
                        }
                    }
                }
                break;
            }
        }
        if pids.is_empty() {
            return Ok(ListenerState::Pending);
        }
        for pid in pids {
            // 权限不足/瞬间退出均不能证明归属，失败向上传递，不放行就绪。
            if !owns_pid(group, pid)? {
                return Ok(ListenerState::Foreign);
            }
        }
        Ok(ListenerState::Owned)
    }
}
