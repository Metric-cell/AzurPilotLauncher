//! Windows 后端 Job 的实进程回归测试适配器。
//!
//! 生产实现由 process_security::OwnedProcessGroup 统一提供；测试继续验证
//! 普通 worker 回收与模拟器显式 breakaway，不维护第二份 Job 实现。

#[cfg(test)]
use crate::process_security::OwnedProcessGroup;
#[cfg(test)]
use std::{
    io,
    os::windows::{
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::CommandExt,
    },
    process::{Command, ExitStatus},
};
#[cfg(test)]
use winapi::um::winbase::CREATE_NO_WINDOW;

#[cfg(test)]
const BREAKAWAY_ENV: &str = "ALAS_LAUNCHER_JOB_BREAKAWAY";

#[cfg(test)]
struct BackendChild {
    group: OwnedProcessGroup,
    status: Option<ExitStatus>,
}

#[cfg(test)]
impl BackendChild {
    fn spawn(command: &mut Command) -> io::Result<Self> {
        Ok(Self {
            group: OwnedProcessGroup::spawn_backend(command)?,
            status: None,
        })
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.group.try_wait()
    }

    fn kill(&mut self) -> io::Result<()> {
        self.status = Some(self.group.terminate()?);
        Ok(())
    }

    fn wait(&mut self) -> io::Result<ExitStatus> {
        self.status
            .ok_or_else(|| io::Error::other("test adapter requires kill before wait"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        path::Path,
        process::Stdio,
        thread::sleep,
        time::{Duration, Instant},
    };
    use winapi::um::{
        processthreadsapi::{OpenProcess, TerminateProcess},
        synchapi::WaitForSingleObject,
        winbase::CREATE_BREAKAWAY_FROM_JOB,
        winnt::{PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, SYNCHRONIZE},
    };

    const FIXTURE: &str = "backend_process::tests::backend_lifetime_fixture";

    fn fixture_command(role: &str, dir: &Path) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", FIXTURE, "--nocapture"])
            .env("ALAS_BACKEND_TEST_ROLE", role)
            .env("ALAS_BACKEND_TEST_DIR", dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        command
    }

    #[test]
    fn backend_lifetime_fixture() {
        let Ok(role) = std::env::var("ALAS_BACKEND_TEST_ROLE") else {
            return;
        };
        let dir = std::path::PathBuf::from(std::env::var_os("ALAS_BACKEND_TEST_DIR").unwrap());
        if role == "restricted" {
            assert!(std::env::var_os(BREAKAWAY_ENV).is_none());
            let escaped = fixture_command("emulator", &dir)
                .creation_flags(CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB)
                .spawn();
            if let Ok(mut child) = escaped {
                let _ = child.kill();
                let _ = child.wait();
                panic!("普通安装 Job 不应允许显式脱离");
            }
            return;
        }
        if role == "backend" {
            assert_eq!(std::env::var(BREAKAWAY_ENV).unwrap(), "1");
            let _worker = fixture_command("worker", &dir)
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()
                .unwrap();
            let _emulator = fixture_command("emulator", &dir)
                .creation_flags(CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB)
                .spawn()
                .unwrap();
        }
        fs::write(dir.join(&role), std::process::id().to_string()).unwrap();
        sleep(Duration::from_secs(30));
    }

    /// 所有测试进程都持有本次创建的句柄，断言失败时也不会留下伪模拟器。
    struct TestProcess(OwnedHandle);
    impl TestProcess {
        fn open(dir: &Path, role: &str) -> Self {
            let start = Instant::now();
            loop {
                if let Some(pid) = fs::read_to_string(dir.join(role))
                    .ok()
                    .and_then(|s| s.parse().ok())
                {
                    let handle = unsafe {
                        OpenProcess(
                            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | SYNCHRONIZE,
                            0,
                            pid,
                        )
                    };
                    assert!(!handle.is_null(), "无法打开测试进程 {role}");
                    return Self(unsafe { OwnedHandle::from_raw_handle(handle as _) });
                }
                assert!(
                    start.elapsed() < Duration::from_secs(10),
                    "测试进程 {role} 未就绪"
                );
                sleep(Duration::from_millis(20));
            }
        }
        fn exited(&self, timeout: u32) -> bool {
            unsafe { WaitForSingleObject(self.0.as_raw_handle() as _, timeout) == 0 }
        }
    }
    impl Drop for TestProcess {
        fn drop(&mut self) {
            unsafe {
                TerminateProcess(self.0.as_raw_handle() as _, 0);
                WaitForSingleObject(self.0.as_raw_handle() as _, 3000);
            }
        }
    }

    fn check_cleanup(mode: &str) {
        let dir = tempfile::tempdir().unwrap();
        let mut child = BackendChild::spawn(&mut fixture_command("backend", dir.path())).unwrap();
        let root = TestProcess::open(dir.path(), "backend");
        let worker = TestProcess::open(dir.path(), "worker");
        let emulator = TestProcess::open(dir.path(), "emulator");
        if mode == "kill" {
            child.kill().unwrap();
            child.wait().unwrap();
            assert!(worker.exited(0), "wait 返回前应完成所有普通 worker 的回收");
        } else if mode == "root_exit" {
            assert_ne!(
                unsafe { TerminateProcess(root.0.as_raw_handle() as _, 0) },
                0
            );
            assert!(root.exited(3000));
            assert!(child.try_wait().unwrap().is_some());
            assert!(!worker.exited(100), "根进程退出时应保留 worker 供 Job 回收");
        }
        drop(child);
        assert!(root.exited(3000), "后端根进程未回收");
        assert!(worker.exited(3000), "普通 worker 未回收");
        assert!(!emulator.exited(100), "显式脱离的模拟器被连带关闭");
    }

    #[test]
    fn ordinary_command_cannot_break_away_or_inherit_backend_capability() {
        let dir = tempfile::tempdir().unwrap();
        let mut command = fixture_command("restricted", dir.path());
        command.env(BREAKAWAY_ENV, "1");
        let mut group = OwnedProcessGroup::spawn(&mut command).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = group.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(Instant::now() < deadline, "普通 Job fixture 未退出");
            sleep(Duration::from_millis(20));
        }
        group.terminate().unwrap();
    }

    #[test]
    fn terminate_keeps_emulator_and_reaps_workers() {
        check_cleanup("kill");
    }

    #[test]
    fn drop_keeps_emulator_and_reaps_workers() {
        check_cleanup("drop");
    }

    #[test]
    fn root_exit_still_reaps_workers_without_closing_emulator() {
        check_cleanup("root_exit");
    }
}
