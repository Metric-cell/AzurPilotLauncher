//! Protect source workspaces and published lockfiles before install-time writes.

use super::config::{default_deploy_config, platform_git_config_path};
use crate::window_util::CreateNoWindow as _;
use anyhow::{Context, Result};
use rust_i18n::t;
use serde_json::Value as JsonValue;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::warn;

/// Protect source checkouts before any deployment configuration or lockfile writes.
/// A checkout is publishable only when its configured branch has no local work
/// and HEAD is an ancestor of the corresponding remote tracking ref.
pub fn repository_is_development_workspace(root: &Path) -> bool {
    if !root.join(".git").exists() {
        return false;
    }
    if root.join(".git").is_file() {
        return true;
    }
    let git = if root.join(platform_git_config_path()).is_file() {
        root.join(platform_git_config_path())
    } else {
        PathBuf::from("git")
    };
    let output = |args: &[&str]| -> Option<String> {
        let result = Command::new(&git)
            .current_dir(root)
            .args(args)
            .create_no_window()
            .output()
            .ok()?;
        result
            .status
            .success()
            .then(|| String::from_utf8_lossy(&result.stdout).trim().to_owned())
    };
    let config = fs::read_to_string(root.join("config/deploy.yaml"))
        .unwrap_or_else(|_| default_deploy_config().to_owned());
    let config: JsonValue = match serde_yaml::from_str(&config) {
        Ok(config) => config,
        Err(_) => return true,
    };
    let Some(branch) = config
        .pointer("/Deploy/Git/Branch")
        .and_then(JsonValue::as_str)
    else {
        return true;
    };
    if output(&["symbolic-ref", "--quiet", "--short", "HEAD"]).as_deref() != Some(branch)
        || output(&["status", "--porcelain", "--untracked-files=normal"]).as_deref() != Some("")
    {
        return true;
    }
    let remote = format!("refs/remotes/origin/{branch}");
    output(&["merge-base", "--is-ancestor", "HEAD", &remote]).is_none()
}

#[derive(Debug)]
pub(super) struct WorkspaceProtected;

impl std::fmt::Display for WorkspaceProtected {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", t!("errors.workspace_protected"))
    }
}

impl std::error::Error for WorkspaceProtected {}

pub(super) const GIT_UPDATE_SCRIPT: &str = r#"
import os
import subprocess
import sys
import deploy.git

had_git_checkout = (os.path.lexists('.git') and
                    os.environ.get('AZURPILOT_RELEASE_BOOTSTRAP') != '1')

def protect_workspace(gm):
    # Published archives intentionally bootstrap a new repository. Only an
    # existing checkout has local Git history/work to protect before reset.
    if not had_git_checkout:
        return
    def check(args):
        result = subprocess.run([gm.git, *args], stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, text=True,
                                creationflags=getattr(subprocess, 'CREATE_NO_WINDOW', 0))
        if result.returncode:
            raise RuntimeError('Git safety check failed')
        return result.stdout.strip()
    try:
        if not os.path.isdir('.git'):
            raise RuntimeError('Git worktree requires development protection')
        if check(['symbolic-ref', '--quiet', '--short', 'HEAD']) != gm.Branch:
            raise RuntimeError('Current branch differs from deployment branch')
        if check(['status', '--porcelain', '--untracked-files=normal']):
            raise RuntimeError('Local files have uncommitted changes')
        check(['merge-base', '--is-ancestor', 'HEAD', 'refs/remotes/origin/' + gm.Branch])
    except (OSError, RuntimeError, TypeError) as error:
        print('Workspace protected: ' + str(error), file=sys.stderr)
        sys.exit(73)

def decorate_execute(fn):
    def new_fn(*args, **kwargs):
        if args and ' reset --hard ' in args[0]:
            protect_workspace(gm)
        if len(args) >= 1 and ' fetch ' in args[0] and '--progress' not in args[0]:
            args = (args[0].replace(' fetch ', ' fetch --progress '),) + args[1:]
        return fn(*args, **kwargs)
    return new_fn
gm = deploy.git.GitManager()
protect_workspace(gm)
gm.execute = decorate_execute(gm.execute)
gm.git_install()
"#;

/// Restore the published lockfile after temporary mirror resolution, on every exit.
pub(super) struct LockfileRestore {
    path: PathBuf,
    contents: Option<Vec<u8>>,
}

impl LockfileRestore {
    pub(super) fn capture(path: &Path) -> Result<Self> {
        let contents = match fs::read(path) {
            Ok(contents) => Some(contents),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("Unable to preserve published uv.lock"),
        };
        let path = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()?.join(path)
        };
        Ok(Self { path, contents })
    }
}

impl Drop for LockfileRestore {
    fn drop(&mut self) {
        let result = match &self.contents {
            Some(contents) => fs::write(&self.path, contents),
            None => match fs::remove_file(&self.path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                result => result,
            },
        };
        if let Err(error) = result {
            warn!("Unable to restore published uv.lock after mirror sync: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::find_on_path;
    use super::*;

    #[test]
    fn git_update_script_refuses_changes_created_during_install() {
        let Some(python) = find_on_path("python3").or_else(|| find_on_path("python")) else {
            // CI without Python still exercises the Rust repository guard.
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for args in [
            vec!["init", "-b", "master"],
            vec!["config", "user.email", "fixture@example.invalid"],
            vec!["config", "user.name", "Fixture"],
        ] {
            assert!(Command::new("git")
                .current_dir(root)
                .args(args)
                .create_no_window()
                .output()
                .unwrap()
                .status
                .success());
        }
        fs::write(root.join("source.txt"), "published").unwrap();
        for args in [
            vec!["add", "."],
            vec!["commit", "-m", "fixture"],
            vec!["update-ref", "refs/remotes/origin/master", "HEAD"],
        ] {
            assert!(Command::new("git")
                .current_dir(root)
                .args(args)
                .create_no_window()
                .output()
                .unwrap()
                .status
                .success());
        }
        // Fake only deploy.git, retaining real Git and the actual updater script.
        // The update starts clean, then local work appears just before reset.
        let stub = r#"
import sys, types, pathlib
package = types.ModuleType('deploy')
module = types.ModuleType('deploy.git')
class GitManager:
    git = 'git'
    Branch = 'master'
    def execute(self, *args, **kwargs):
        pathlib.Path('reset-executed').write_text('unsafe')
    def git_install(self):
        pathlib.Path('source.txt').write_text('new developer work')
        self.execute('git reset --hard origin/master')
module.GitManager = GitManager
package.git = module
sys.modules['deploy'] = package
sys.modules['deploy.git'] = module
"#;
        let output = Command::new(python)
            .current_dir(root)
            .env_remove("AZURPILOT_RELEASE_BOOTSTRAP")
            .args(["-c", &format!("{stub}\n{GIT_UPDATE_SCRIPT}")])
            .create_no_window()
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(73),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!root.join("reset-executed").exists());
        assert_eq!(
            fs::read_to_string(root.join("source.txt")).unwrap(),
            "new developer work"
        );
    }

    #[test]
    fn update_protection_distinguishes_release_and_local_work() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let git = |args: &[&str]| {
            assert!(Command::new("git")
                .current_dir(root)
                .args(args)
                .create_no_window()
                .output()
                .unwrap()
                .status
                .success());
        };
        assert!(!repository_is_development_workspace(root));
        git(&["init", "-b", "master"]);
        git(&["config", "user.email", "fixture@example.invalid"]);
        git(&["config", "user.name", "Fixture"]);
        fs::write(root.join(".gitignore"), "config/\nlog/\n.venv/\n").unwrap();
        // 显式固定夹具分支，避免依赖发行模板的默认分支。
        fs::create_dir(root.join("config")).unwrap();
        fs::write(
            root.join("config/deploy.yaml"),
            "Deploy:\n  Git:\n    Branch: master\n",
        )
        .unwrap();
        fs::write(root.join("source.txt"), "published").unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "published"]);
        git(&["update-ref", "refs/remotes/origin/master", "HEAD"]);
        assert!(!repository_is_development_workspace(root));
        fs::create_dir(root.join("log")).unwrap();
        fs::write(root.join("log/runtime.txt"), "runtime").unwrap();
        assert!(!repository_is_development_workspace(root));
        fs::write(root.join("source.txt"), "uncommitted").unwrap();
        assert!(repository_is_development_workspace(root));
        git(&["add", "source.txt"]);
        git(&["commit", "-m", "local work"]);
        assert!(repository_is_development_workspace(root));
        git(&["update-ref", "refs/remotes/origin/master", "HEAD"]);
        assert!(!repository_is_development_workspace(root));
        git(&["reset", "--hard", "HEAD~1"]);
        assert!(!repository_is_development_workspace(root));
        git(&["reset", "--hard", "refs/remotes/origin/master"]);
        git(&["checkout", "-b", "codex/fixture"]);
        assert!(repository_is_development_workspace(root));
        git(&["checkout", "master"]);
        fs::write(root.join("new-source.txt"), "untracked work").unwrap();
        assert!(repository_is_development_workspace(root));
        assert_eq!(
            fs::read_to_string(root.join("source.txt")).unwrap(),
            "uncommitted"
        );
    }

    #[test]
    fn lockfile_guard_restores_existing_contents_and_removes_new_file() {
        let directory = tempfile::tempdir().unwrap();
        let existing = directory.path().join("uv.lock");
        fs::write(&existing, b"published lockfile").unwrap();
        {
            let _restore = LockfileRestore::capture(&existing).unwrap();
            fs::write(&existing, b"mirror lockfile").unwrap();
        }
        assert_eq!(fs::read(&existing).unwrap(), b"published lockfile");
        let absent = directory.path().join("new.lock");
        {
            let _restore = LockfileRestore::capture(&absent).unwrap();
            fs::write(&absent, b"temporary").unwrap();
        }
        assert!(!absent.exists());
    }
}
