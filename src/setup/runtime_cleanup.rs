//! Runtime deletion boundaries, including cancellation and link-safe recovery.
//! Repository paths are supplied by the orchestrator rather than discovered here.

use anyhow::{anyhow, bail, Context, Result};
use rust_i18n::t;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use tracing::info;

const CLEANUP_RETRIES: usize = 20;

/// 递归删除目录/文件，删除前先清除只读属性。
///
/// Windows 上处于只读属性的文件无法直接删除，需先清位；
/// 目录按先清空后删除的顺序递归处理。
///
/// # Errors
/// 任一文件或目录删除失败时返回 Err（含 i18n 错误信息）。
fn remove_runtime_entry(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if is_symlink_or_reparse_point(&metadata) {
        // Never chmod or traverse the target of a runtime entry link.
        return remove_venv_link_or_reparse_point(path, &metadata);
    }
    clear_readonly(path)?;
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            remove_runtime_entry(&entry?.path())?;
        }
        fs::remove_dir(path).with_context(|| {
            t!(
                "errors.delete_dir_failed",
                error = path.display().to_string()
            )
        })?;
    } else {
        fs::remove_file(path).with_context(|| {
            t!(
                "errors.delete_file_failed",
                error = path.display().to_string()
            )
        })?;
    }
    Ok(())
}

/// 带重试的删除：被杀进程尚未完全释放文件时退避重试。
///
/// 每次失败后检查目标是否已消失（可能被并发进程删掉），消失即成功。
///
/// # Errors
/// 重试次数用尽仍失败时返回最后一次的错误。
pub(super) fn remove_runtime_entry_with_retry(path: &Path) -> Result<()> {
    let mut last_error = None;
    for attempt in 0..CLEANUP_RETRIES {
        match remove_runtime_entry(path) {
            Ok(()) => return Ok(()),
            Err(err) => {
                last_error = Some(err);
                if !path.exists() {
                    return Ok(());
                }
                thread::sleep(Duration::from_millis(250 + attempt as u64 * 100));
            }
        }
    }

    Err(last_error.unwrap_or_else(|| {
        anyhow!(t!(
            "errors.delete_failed",
            error = path.display().to_string()
        ))
    }))
}

/// 后端恢复流程中删除 `.venv`，删除前做严格的安全校验。
///
/// 校验目的：`.venv` 必须是真实目录（不能是符号链接/Reparse Point）
/// 且规范化后必须位于仓库目录之内——防止环境变量被篡改后
/// 误删仓库外的任意目录。
///
/// # Errors
/// 用户取消、路径校验不通过或删除失败时返回 Err。
pub(super) fn remove_venv_for_backend_recovery(
    repo: &Path,
    venv: &Path,
    cancel_requested: &AtomicBool,
) -> Result<()> {
    let repo_dir = repo.canonicalize()?;
    if !venv.exists() {
        return Ok(());
    }

    let venv_metadata = fs::symlink_metadata(&venv)?;
    if !venv_metadata.is_dir() || is_symlink_or_reparse_point(&venv_metadata) {
        bail!(t!(
            "errors.refuse_cleanup",
            actual = venv.display().to_string(),
            expected = repo_dir.display().to_string()
        ));
    }
    let venv_target = venv.canonicalize()?;
    if venv_target == repo_dir || !venv_target.starts_with(&repo_dir) {
        bail!(t!(
            "errors.refuse_cleanup",
            actual = venv_target.display().to_string(),
            expected = repo_dir.display().to_string()
        ));
    }

    remove_venv_path_for_backend_recovery(&venv, cancel_requested).with_context(|| {
        t!(
            "errors.reset_venv_failed",
            error = venv.display().to_string()
        )
    })
}

/// 实际执行 `.venv` 删除（带取消检查与重试），仅记录日志后委托重试逻辑。
///
/// # Errors
/// 用户取消或删除重试用尽仍失败时返回 Err。
fn remove_venv_path_for_backend_recovery(venv: &Path, cancel_requested: &AtomicBool) -> Result<()> {
    if cancel_requested.load(Ordering::SeqCst) {
        bail!(t!("setup.cancel_cleaning"));
    }
    if !venv.exists() {
        return Ok(());
    }

    info!("Removing {} after backend startup timeout", venv.display());
    remove_venv_entry_with_retry(venv, cancel_requested)?;
    if cancel_requested.load(Ordering::SeqCst) {
        bail!(t!("setup.cancel_cleaning"));
    }
    Ok(())
}

/// 带取消检查与退避重试的删除循环（恢复流程专用）。
///
/// 与 [`remove_runtime_entry_with_retry`] 的区别在于每次尝试前后
/// 都检查取消标志，保证用户取消时能尽快中止。
///
/// # Errors
/// 用户取消或重试用尽仍失败时返回 Err。
fn remove_venv_entry_with_retry(path: &Path, cancel_requested: &AtomicBool) -> Result<()> {
    let mut last_error = None;
    for attempt in 0..CLEANUP_RETRIES {
        if cancel_requested.load(Ordering::SeqCst) {
            bail!(t!("setup.cancel_cleaning"));
        }

        match remove_venv_entry(path, cancel_requested) {
            Ok(()) => return Ok(()),
            Err(error) => {
                last_error = Some(error);
                if !path.exists() {
                    return Ok(());
                }
                wait_for_venv_recovery_retry(
                    Duration::from_millis(250 + attempt as u64 * 100),
                    cancel_requested,
                )?;
            }
        }
    }

    Err(last_error.unwrap_or_else(|| {
        anyhow!(t!(
            "errors.delete_failed",
            error = path.display().to_string()
        ))
    }))
}

/// 递归删除 `.venv` 内的单个条目（恢复流程专用，逐步检查取消标志）。
///
/// 符号链接与 Reparse Point 只删链接本身、不递归目标——防止
/// `.venv` 内的链接指向目录外时误删外部文件。
///
/// # Errors
/// 用户取消或删除失败时返回 Err。
fn remove_venv_entry(path: &Path, cancel_requested: &AtomicBool) -> Result<()> {
    if cancel_requested.load(Ordering::SeqCst) {
        bail!(t!("setup.cancel_cleaning"));
    }

    let metadata = fs::symlink_metadata(path)?;
    if is_symlink_or_reparse_point(&metadata) {
        return remove_venv_link_or_reparse_point(path, &metadata);
    }

    clear_readonly(path)?;
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            if cancel_requested.load(Ordering::SeqCst) {
                bail!(t!("setup.cancel_cleaning"));
            }
            remove_venv_entry(&entry?.path(), cancel_requested)?;
        }
        fs::remove_dir(path).with_context(|| {
            t!(
                "errors.delete_dir_failed",
                error = path.display().to_string()
            )
        })?;
    } else {
        fs::remove_file(path).with_context(|| {
            t!(
                "errors.delete_file_failed",
                error = path.display().to_string()
            )
        })?;
    }
    Ok(())
}

/// 判断给定元数据是否为符号链接或 Windows Reparse Point。
///
/// Reparse Point（含 junction）在标准 `is_symlink()` 下不可见，
/// Windows 需额外检查文件属性位，二者都不应被递归删除。
fn is_symlink_or_reparse_point(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        return metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    }

    #[cfg(not(windows))]
    false
}

/// 删除符号链接或 Reparse Point 本身（不递归目标内容）。
///
/// Windows 需区分"指向目录"与"指向文件"两种链接选用不同 API；
/// Unix 一律按文件链接删除。
///
/// # Errors
/// 删除失败时返回 Err（含 i18n 错误信息）。
fn remove_venv_link_or_reparse_point(path: &Path, _metadata: &fs::Metadata) -> Result<()> {
    #[cfg(windows)]
    let result = if fs::metadata(path)
        .map(|target_metadata| target_metadata.is_dir())
        .unwrap_or(false)
    {
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    };

    #[cfg(not(windows))]
    let result = fs::remove_file(path);

    result.with_context(|| {
        t!(
            "errors.delete_file_failed",
            error = path.display().to_string()
        )
    })
}

/// 在重试间隔内以 50ms 粒度小步等待，随时响应取消请求。
///
/// 不能用一次长 sleep——用户取消后应立刻停止而不是等完整间隔。
///
/// # Errors
/// 等待期间检测到取消请求时返回 Err。
fn wait_for_venv_recovery_retry(delay: Duration, cancel_requested: &AtomicBool) -> Result<()> {
    let deadline = Instant::now() + delay;
    while Instant::now() < deadline {
        if cancel_requested.load(Ordering::SeqCst) {
            bail!(t!("setup.cancel_cleaning"));
        }
        thread::sleep(
            Duration::from_millis(50).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
    Ok(())
}

/// 清除路径的只读属性（Windows 删除只读文件前必须先清位）。
///
/// 路径已消失时静默成功；非只读时不做任何操作。
///
/// # Errors
/// 属性修改失败时返回 Err。
fn clear_readonly(path: &Path) -> Result<()> {
    let Ok(metadata) = fs::metadata(path) else {
        return Ok(());
    };
    let mut permissions = metadata.permissions();
    if permissions.readonly() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            permissions.set_mode(permissions.mode() | 0o200);
        }
        #[cfg(not(unix))]
        permissions.set_readonly(false);
        fs::set_permissions(path, permissions)
            .with_context(|| t!("errors.chmod_failed", error = path.display().to_string()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_timeout_recovery_removes_only_venv() {
        let temp_dir = tempfile::tempdir().expect("create temporary repository");
        let venv = temp_dir.path().join(".venv");
        let keep = temp_dir.path().join("keep.txt");
        fs::create_dir_all(venv.join("Lib")).expect("create temporary venv");
        fs::write(venv.join("Lib").join("package.txt"), "dependency")
            .expect("write temporary dependency");
        fs::write(&keep, "keep").expect("write sibling file");
        let cancel_requested = AtomicBool::new(false);

        remove_venv_path_for_backend_recovery(&venv, &cancel_requested)
            .expect("remove temporary venv");

        assert!(!venv.exists());
        assert_eq!(fs::read_to_string(keep).expect("read sibling file"), "keep");
    }

    #[test]
    fn backend_timeout_recovery_does_not_remove_venv_after_cancellation() {
        let temp_dir = tempfile::tempdir().expect("create temporary repository");
        let venv = temp_dir.path().join(".venv");
        fs::create_dir_all(&venv).expect("create temporary venv");
        let cancel_requested = AtomicBool::new(true);

        assert!(remove_venv_path_for_backend_recovery(&venv, &cancel_requested).is_err());
        assert!(venv.exists());
    }

    #[cfg(unix)]
    #[test]
    fn clearing_readonly_adds_write_permission_only_for_the_owner() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("readonly");
        fs::write(&path, "runtime").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o440)).unwrap();
        clear_readonly(&path).unwrap();
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[cfg(unix)]
    #[test]
    fn ordinary_runtime_cleanup_unlinks_external_targets_without_changing_them() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let repository = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let keep = outside.path().join("keep.txt");
        fs::write(&keep, "user data").unwrap();
        fs::set_permissions(&keep, fs::Permissions::from_mode(0o400)).unwrap();
        let runtime = repository.path().join(".venv");
        fs::create_dir(&runtime).unwrap();
        symlink(outside.path(), runtime.join("directory-link")).unwrap();
        symlink(&keep, runtime.join("file-link")).unwrap();
        symlink(outside.path().join("absent"), runtime.join("dangling-link")).unwrap();
        remove_runtime_entry_with_retry(&runtime).unwrap();
        assert!(!runtime.exists());
        assert_eq!(fs::read_to_string(&keep).unwrap(), "user data");
        assert_eq!(
            fs::metadata(&keep).unwrap().permissions().mode() & 0o777,
            0o400
        );

        let root_link = repository.path().join("runtime-link");
        symlink(outside.path(), &root_link).unwrap();
        remove_runtime_entry_with_retry(&root_link).unwrap();
        assert!(!root_link.exists());
        assert_eq!(fs::read_to_string(keep).unwrap(), "user data");
    }

    #[cfg(unix)]
    #[test]
    fn recovery_does_not_follow_links_outside_the_virtual_environment() {
        use std::os::unix::fs::symlink;
        let repository = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let keep = outside.path().join("keep.txt");
        fs::write(&keep, "user data").unwrap();
        let venv = repository.path().join(".venv");
        fs::create_dir(&venv).unwrap();
        symlink(outside.path(), venv.join("external")).unwrap();
        remove_venv_for_backend_recovery(repository.path(), &venv, &AtomicBool::new(false))
            .unwrap();
        assert!(!venv.exists());
        assert_eq!(fs::read_to_string(keep).unwrap(), "user data");
    }

    #[cfg(unix)]
    #[test]
    fn recovery_rejects_a_link_used_as_the_virtual_environment_root() {
        use std::os::unix::fs::symlink;
        let repository = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let keep = outside.path().join("keep.txt");
        fs::write(&keep, "user data").unwrap();
        let venv = repository.path().join(".venv");
        symlink(outside.path(), &venv).unwrap();
        assert!(remove_venv_for_backend_recovery(
            repository.path(),
            &venv,
            &AtomicBool::new(false)
        )
        .is_err());
        assert_eq!(fs::read_to_string(keep).unwrap(), "user data");
    }
}
