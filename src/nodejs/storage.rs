//! 仅操作调用方创建的用户目录。安装事务先验证暂存结果，提交失败恢复原目录。

use std::{
    fs,
    io::{self, Read},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

pub(super) fn verify_archive(path: &Path, expected: &str, cancelled: &AtomicBool) -> Result<()> {
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        if cancelled.load(Ordering::Acquire) {
            bail!(rust_i18n::t!("setup.cancel_cleaning"));
        }
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    let actual = format!("{:x}", digest.finalize());
    if !actual.eq_ignore_ascii_case(expected) {
        bail!(
            "Node.js archive checksum mismatch after download: expected {expected}, got {actual}"
        );
    }
    Ok(())
}

pub(super) fn publish_runtime(staged: &Path, target: &Path) -> Result<()> {
    let parent = target
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Node.js target has no parent"))?;
    let backup = tempfile::Builder::new()
        .prefix(".previous-")
        .tempdir_in(parent)?;
    let previous = backup.path().join("runtime");
    let had_previous = match fs::symlink_metadata(target) {
        Ok(_) => {
            fs::rename(target, &previous).with_context(|| {
                format!("preserve existing Node.js runtime {}", target.display())
            })?;
            true
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    if let Err(error) = fs::rename(staged, target) {
        if had_previous {
            if let Err(restore_error) = fs::rename(&previous, target) {
                // 保留可恢复原件；绝不能让暂存目录 Drop 删除唯一现有安装。
                let retained = backup.keep();
                bail!("publish Node.js runtime failed: {error}; restoring previous runtime failed: {restore_error}; previous runtime retained in {}", retained.display());
            }
        }
        return Err(error)
            .with_context(|| format!("publish private Node.js runtime {}", target.display()));
    }
    Ok(())
}

pub(super) fn flatten_single_child_directory(target: &Path) -> Result<()> {
    let mut entries = fs::read_dir(target)?.collect::<io::Result<Vec<_>>>()?;
    if entries.len() != 1 || !entries[0].file_type()?.is_dir() {
        return Ok(());
    }
    let wrapper = entries.remove(0).path();
    for entry in fs::read_dir(&wrapper)? {
        let entry = entry?;
        fs::rename(entry.path(), target.join(entry.file_name()))?;
    }
    fs::remove_dir(wrapper)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_verification_rejects_disk_tampering_and_observes_cancellation() {
        let directory = tempfile::tempdir().unwrap();
        let archive = directory.path().join("archive.zip");
        fs::write(&archive, b"verified payload").unwrap();
        let expected = format!("{:x}", Sha256::digest(b"verified payload"));
        verify_archive(&archive, &expected, &AtomicBool::new(false)).unwrap();
        assert!(verify_archive(&archive, &expected, &AtomicBool::new(true)).is_err());
        fs::write(&archive, b"tampered payload").unwrap();
        assert!(verify_archive(&archive, &expected, &AtomicBool::new(false)).is_err());
    }

    #[test]
    fn publishing_a_validated_runtime_replaces_only_its_private_version_directory() {
        let directory = tempfile::tempdir().unwrap();
        let existing = directory.path().join("node-runtime");
        let staged = directory.path().join("staged-runtime");
        let other = directory.path().join("other-runtime");
        for path in [&existing, &staged, &other] {
            fs::create_dir(path).unwrap();
        }
        fs::write(existing.join("node.exe"), b"old").unwrap();
        fs::write(staged.join("node.exe"), b"verified-new").unwrap();
        fs::write(other.join("keep"), b"other-installation").unwrap();
        publish_runtime(&staged, &existing).unwrap();
        assert_eq!(
            fs::read(existing.join("node.exe")).unwrap(),
            b"verified-new"
        );
        assert_eq!(fs::read(other.join("keep")).unwrap(), b"other-installation");
        assert!(!staged.exists());
    }

    #[test]
    fn failed_publication_restores_the_existing_runtime() {
        let directory = tempfile::tempdir().unwrap();
        let existing = directory.path().join("node-runtime");
        fs::create_dir(&existing).unwrap();
        fs::write(existing.join("node.exe"), b"existing").unwrap();
        assert!(publish_runtime(&directory.path().join("missing-stage"), &existing).is_err());
        assert_eq!(fs::read(existing.join("node.exe")).unwrap(), b"existing");
    }

    #[test]
    fn discarding_an_unpublished_stage_preserves_the_existing_runtime() {
        let directory = tempfile::tempdir().unwrap();
        let existing = directory.path().join("node-runtime");
        fs::create_dir(&existing).unwrap();
        fs::write(existing.join("node.exe"), b"existing").unwrap();
        let stage = tempfile::Builder::new()
            .prefix(".install-")
            .tempdir_in(directory.path())
            .unwrap();
        fs::write(stage.path().join("nodejs.zip"), b"partial-download").unwrap();
        let staging_path = stage.path().to_owned();
        drop(stage);
        assert!(!staging_path.exists());
        assert_eq!(fs::read(existing.join("node.exe")).unwrap(), b"existing");
    }

    #[test]
    fn flattening_an_official_archive_retains_node_and_npm() {
        let directory = tempfile::tempdir().unwrap();
        let wrapper = directory.path().join("node-v24.21.0-win-x64");
        fs::create_dir_all(wrapper.join("node_modules")).unwrap();
        fs::write(wrapper.join("node.exe"), b"binary").unwrap();
        fs::write(wrapper.join("npm.cmd"), b"shim").unwrap();
        flatten_single_child_directory(directory.path()).unwrap();
        assert!(directory.path().join("node.exe").is_file());
        assert!(directory.path().join("npm.cmd").is_file());
        assert!(directory.path().join("node_modules").is_dir());
        assert!(!wrapper.exists());
    }
}
