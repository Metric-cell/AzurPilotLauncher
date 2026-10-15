use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::AtomicBool,
};

#[cfg(windows)]
use anyhow::anyhow;
use anyhow::{bail, Context, Result};
use rand::RngCore;
use sha2::{Digest, Sha256};

use super::{
    check_cancelled,
    manifest::{self, Config, SignedManifest, VerifiedManifest, MAX_SNAPSHOT_BYTES},
    transport, unix_now, SKIP_ENV,
};

const STAGING_PREFIX: &str = ".azurpilot-update-";
const PAYLOAD_FILE: &str = "payload.bin";
const SNAPSHOT_FILE: &str = "manifest.bin";
#[cfg(windows)]
const HELPER_FILE: &str = "helper.exe";
#[cfg(windows)]
const APPLY_ARG: &str = "--apply-launcher-update";

pub(super) struct Installation {
    target: PathBuf,
    original_hash: [u8; 32],
}

impl Installation {
    pub(super) fn current(cancelled: &AtomicBool) -> Result<Self> {
        check_cancelled(cancelled)?;
        let target = checked_target(&std::env::current_exe()?)?;
        let original_hash = file_hash(&mut open_regular(&target)?, cancelled)?;
        Ok(Self {
            target,
            original_hash,
        })
    }

    #[cfg(any(windows, test))]
    pub(super) fn for_helper(target: &Path, helper: &Path, cancelled: &AtomicBool) -> Result<Self> {
        check_cancelled(cancelled)?;
        let target = checked_target(target)?;
        let original_hash = file_hash(&mut open_regular(helper)?, cancelled)?;
        let installation = Self {
            target,
            original_hash,
        };
        installation.target_is_unchanged(cancelled)?;
        Ok(installation)
    }

    #[cfg(test)]
    pub(super) fn fixture(target: &Path) -> Result<Self> {
        let target = checked_target(target)?;
        Ok(Self {
            original_hash: file_hash(&mut open_regular(&target)?, &AtomicBool::new(false))?,
            target,
        })
    }

    pub(super) fn parent(&self) -> &Path {
        self.target.parent().expect("checked executable parent")
    }

    fn state_path(&self, suffix: &str) -> PathBuf {
        // 每个目标文件独立加锁；固定锁文件永不删除，避免 inode 换代导致双持锁。
        let key = manifest::digest(
            self.target
                .file_name()
                .expect("checked filename")
                .to_string_lossy()
                .as_bytes(),
        );
        self.parent()
            .join(format!(".azurpilot-update-{}.{suffix}", &key[..16]))
    }

    pub(super) fn lock(&self, cancelled: &AtomicBool) -> Result<File> {
        check_cancelled(cancelled)?;
        let path = self.state_path("lock");
        let mut options = safe_options();
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // 锁文件必须允许第二个更新进程先打开，再由内核锁判定 WouldBlock。
            // 载荷使用的 deny-write sharing 会让 helper 在父进程退锁前因
            // ERROR_SHARING_VIOLATION 提前退出。锁文件始终不允许并发删除。
            options.share_mode(0x00000001 | 0x00000002);
        }
        options.read(true).write(true).create(true);
        let file = options.open(path).context("open launcher update lock")?;
        validate_file(&file)?;
        match file.try_lock() {
            Ok(()) => {
                check_cancelled(cancelled)?;
                Ok(file)
            }
            Err(std::fs::TryLockError::WouldBlock) => Err(LockBusy.into()),
            Err(std::fs::TryLockError::Error(error)) => {
                Err(error).context("lock launcher update installation")
            }
        }
    }

    pub(super) fn accept_manifest(
        &self,
        config: &Config,
        candidate: &VerifiedManifest,
        cancelled: &AtomicBool,
    ) -> Result<()> {
        check_cancelled(cancelled)?;
        let current = manifest::parse_version(env!("CARGO_PKG_VERSION"))?;
        if candidate.version.cmp_precedence(&current).is_lt() {
            bail!("signed release is older than the running launcher");
        }
        let state = self.state_path("state");
        match fs::symlink_metadata(&state) {
            Ok(_) => {
                let signed = read_snapshot(&state, cancelled)?;
                let mut prior_config = config.clone();
                // 编译期下限提升不能让此前合法保存的高水位变成非法状态。
                prior_config.min_sequence = 0;
                let previous = prior_config
                    .verify(&signed, 0, false)
                    .context("authenticate stored launcher update high-water mark")?;
                check_cancelled(cancelled)?;
                candidate.check_high_water(&previous)?;
                if candidate.signed.manifest == previous.signed.manifest {
                    return Ok(());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspect launcher update high-water mark"),
        }
        let staging = StagingDir::create(self.parent())?;
        let temporary = staging.path.join("state.bin");
        let mut file = create_new_file(&temporary)?;
        file.write_all(&candidate.signed.encode()?)?;
        file.sync_all()?;
        drop(file);
        check_cancelled(cancelled)?;
        move_replace(&temporary, &state)
            .context("save authenticated launcher update high-water mark")?;
        sync_directory(self.parent())?;
        Ok(())
    }

    fn target_is_unchanged(&self, cancelled: &AtomicBool) -> Result<()> {
        check_cancelled(cancelled)?;
        if checked_target(&self.target)? != self.target
            || file_hash(&mut open_regular(&self.target)?, cancelled)? != self.original_hash
        {
            bail!("launcher update target changed during this update");
        }
        Ok(())
    }

    pub(super) fn verify_before_replace(
        &self,
        config: &Config,
        staging: &StagingDir,
        cancelled: &AtomicBool,
    ) -> Result<File> {
        check_cancelled(cancelled)?;
        let signed = read_snapshot(&staging.snapshot_path(), cancelled)?;
        let verified = config.verify(&signed, unix_now()?, true)?;
        check_cancelled(cancelled)?;
        if !verified
            .version
            .cmp_precedence(&manifest::parse_version(env!("CARGO_PKG_VERSION"))?)
            .is_gt()
        {
            bail!("replacement must be a strictly newer signed launcher");
        }
        self.accept_manifest(config, &verified, cancelled)?;
        let artifact = verified.artifact(manifest::platform_key())?;
        let path = staging.payload_path();
        let mut file = open_regular(&path)?;
        transport::verify_payload(&mut file, artifact, cancelled)?;
        self.target_is_unchanged(cancelled)?;
        ensure_same_file(&file, &path)?;
        check_cancelled(cancelled)?;
        Ok(file)
    }

    #[cfg(not(windows))]
    pub(super) fn replace_and_restart(
        &self,
        config: &Config,
        staging: &mut StagingDir,
        cancelled: &AtomicBool,
    ) -> Result<()> {
        self.promote_verified(config, staging, cancelled)?;
        check_cancelled(cancelled)?;
        Command::new(&self.target)
            .env(SKIP_ENV, "1")
            .spawn()
            .context("restart signed launcher")?;
        Ok(())
    }

    pub(super) fn promote_verified(
        &self,
        config: &Config,
        staging: &StagingDir,
        cancelled: &AtomicBool,
    ) -> Result<()> {
        let file = self.verify_before_replace(config, staging, cancelled)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o755))?;
        }
        #[cfg(unix)]
        file.sync_all()?;
        // 载荷与目标位于同一文件系统；保留已验签文件句柄，私有目录阻止其他用户换包。
        check_cancelled(cancelled)?;
        move_replace(&staging.payload_path(), &self.target)
            .context("atomically replace launcher")?;
        sync_directory(self.parent())?;
        drop(file);
        Ok(())
    }

    #[cfg(windows)]
    pub(super) fn replace_and_restart(
        &self,
        config: &Config,
        staging: &mut StagingDir,
        cancelled: &AtomicBool,
    ) -> Result<()> {
        use std::os::windows::process::CommandExt;
        use winapi::um::winbase::CREATE_NO_WINDOW;
        // 最终权限/签名/摘要检查后才启动旧启动器的助手副本。
        let _payload = self.verify_before_replace(config, staging, cancelled)?;
        let helper = staging.path.join(HELPER_FILE);
        let mut current = open_regular(&self.target)?;
        let mut output = create_new_file(&helper)?;
        std::io::copy(&mut current, &mut output)?;
        output.sync_all()?;
        drop(output);
        check_cancelled(cancelled)?;
        if file_hash(&mut open_regular(&helper)?, cancelled)? != self.original_hash {
            bail!("launcher helper copy does not match the running launcher");
        }
        check_cancelled(cancelled)?;
        Command::new(&helper)
            .arg(APPLY_ARG)
            .arg(&self.target)
            .env(super::NO_CONSOLE_ENV, "1")
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .context("start signed launcher update helper")?;
        staging.keep = true; // 助手仍运行于此目录，助手完成后负责删除。
        Ok(())
    }
}

#[derive(Debug)]
struct LockBusy;
impl std::fmt::Display for LockBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("another launcher update is already running")
    }
}
impl std::error::Error for LockBusy {}

pub(super) struct StagingDir {
    pub(super) path: PathBuf,
    keep: bool,
}
impl StagingDir {
    pub(super) fn create(parent: &Path) -> Result<Self> {
        // macOS 的系统临时路径常经 /var -> /private/var；验证解析后的祖先。
        let canonical_parent = parent.canonicalize()?;
        let parent = canonical_parent.as_path();
        #[cfg(unix)]
        validate_install_directory(parent)?;
        for _ in 0..32 {
            let mut random = [0; 16];
            rand::rngs::OsRng.fill_bytes(&mut random);
            let path = parent.join(format!("{STAGING_PREFIX}{}", manifest::hex(&random)));
            match create_secure_directory(&path) {
                Ok(()) => return Ok(Self { path, keep: false }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error)
                        .context("create private same-directory launcher update staging")
                }
            }
        }
        bail!("unable to reserve a unique launcher update staging directory")
    }
    pub(super) fn payload_path(&self) -> PathBuf {
        self.path.join(PAYLOAD_FILE)
    }
    pub(super) fn snapshot_path(&self) -> PathBuf {
        self.path.join(SNAPSHOT_FILE)
    }
    pub(super) fn write_snapshot(&self, signed: &SignedManifest) -> Result<()> {
        let mut file = create_new_file(&self.snapshot_path())?;
        file.write_all(&signed.encode()?)?;
        file.sync_all()?;
        Ok(())
    }
}
impl Drop for StagingDir {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

pub(super) fn read_snapshot(path: &Path, cancelled: &AtomicBool) -> Result<SignedManifest> {
    check_cancelled(cancelled)?;
    let file = open_regular(path)?;
    if file.metadata()?.len() > MAX_SNAPSHOT_BYTES as u64 {
        bail!("stored launcher update metadata exceeds its size limit");
    }
    SignedManifest::decode(&transport::read_limited(
        file,
        MAX_SNAPSHOT_BYTES,
        cancelled,
    )?)
}

pub(super) fn create_new_file(path: &Path) -> Result<File> {
    let mut options = safe_options();
    options.read(true).write(true).create_new(true);
    let file = options
        .open(path)
        .context("create private launcher update file")?;
    validate_file(&file)?;
    Ok(file)
}

fn safe_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use winapi::um::winbase::FILE_FLAG_OPEN_REPARSE_POINT;
        // 禁止其他进程写入已打开的文件；允许持有句柄期间执行原子 rename。
        options
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .share_mode(0x00000001 | 0x00000004);
    }
    options
}

pub(super) fn open_regular(path: &Path) -> Result<File> {
    let metadata = fs::symlink_metadata(path).context("inspect launcher update file")?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("launcher update file must be a regular file, without a symlink");
    }
    let file = safe_options()
        .read(true)
        .open(path)
        .context("open launcher update file without following symlinks")?;
    validate_file(&file)?;
    ensure_same_file(&file, path)?;
    Ok(file)
}

fn validate_file(file: &File) -> Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        bail!("launcher update file is not regular");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 || metadata.mode() & 0o022 != 0 {
            bail!("launcher update file has unsafe hardlinks or write permissions");
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            bail!("launcher update file is a reparse point");
        }
    }
    Ok(())
}

fn ensure_same_file(file: &File, path: &Path) -> Result<()> {
    let current = fs::symlink_metadata(path)?;
    if current.file_type().is_symlink() || !current.is_file() {
        bail!("launcher update path is no longer a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let opened = file.metadata()?;
        if current.dev() != opened.dev() || current.ino() != opened.ino() {
            bail!("launcher update file identity changed");
        }
    }
    #[cfg(not(unix))]
    {
        let _ = file;
    }
    Ok(())
}

fn checked_target(target: &Path) -> Result<PathBuf> {
    if !target.is_absolute() {
        bail!("launcher update target must be absolute");
    }
    let parent = target
        .parent()
        .context("launcher update target has no parent")?
        .canonicalize()?;
    let target = parent.join(
        target
            .file_name()
            .context("launcher update target has no filename")?,
    );
    let _file = open_regular(&target)?;
    #[cfg(unix)]
    validate_install_directory(&parent)?;
    Ok(target)
}

#[cfg(unix)]
fn validate_install_directory(parent: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let user = unsafe { nix::libc::geteuid() };
    // 仅检查最终目录不足以防护 /shared(0777)/private(0700)：其他用户能
    // 整体移走 private 后重建同名路径。可信祖先必须由当前用户/root 拥有，
    // 共享祖先还须有 sticky bit（例如 /tmp）；目标目录自身一律不可共享写。
    for ancestor in parent.ancestors() {
        let metadata = fs::symlink_metadata(ancestor)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            bail!("launcher update directory must not contain symlinks");
        }
        if metadata.uid() != user && metadata.uid() != 0 {
            bail!("launcher update directory is controlled by another user");
        }
        if metadata.mode() & 0o022 != 0 && (ancestor == parent || metadata.mode() & 0o1000 == 0) {
            bail!("launcher executable directory or ancestor is writable by other users");
        }
    }
    Ok(())
}

fn file_hash(file: &mut File, cancelled: &AtomicBool) -> Result<[u8; 32]> {
    check_cancelled(cancelled)?;
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 128 * 1024];
    let mut length = 0;
    loop {
        check_cancelled(cancelled)?;
        let count = file.read(&mut buffer)?;
        check_cancelled(cancelled)?;
        if count == 0 {
            break;
        }
        length += count as u64;
        if length > manifest::MAX_PAYLOAD_BYTES {
            bail!("launcher executable exceeds its size limit");
        }
        hasher.update(&buffer[..count]);
    }
    file.seek(SeekFrom::Start(0))?;
    Ok(hasher.finalize().into())
}

#[cfg(unix)]
fn create_secure_directory(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().mode(0o700).create(path)
}

#[cfg(windows)]
fn create_secure_directory(path: &Path) -> std::io::Result<()> {
    use std::{mem::size_of, ptr};
    use winapi::{
        shared::sddl::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1},
        um::{
            fileapi::CreateDirectoryW, minwinbase::SECURITY_ATTRIBUTES, winbase::LocalFree,
            winnt::PSECURITY_DESCRIPTOR,
        },
    };
    // asInvoker：当前用户、SYSTEM 和管理员可写，禁止继承目录的其他用户写权限。
    // 同一用户已运行任意代码时不在本模块隔离边界内，不依赖管理员权限完成更新。
    let sddl = format!(
        "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;{})",
        current_user_sid()?
    );
    let path = wide_null(path)?;
    let sddl = sddl
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            u32::from(SDDL_REVISION_1),
            &mut descriptor,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let result = unsafe { CreateDirectoryW(path.as_ptr(), &mut attributes) };
    let error = (result == 0).then(std::io::Error::last_os_error);
    unsafe {
        LocalFree(descriptor);
    }
    match error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

#[cfg(windows)]
fn current_user_sid() -> std::io::Result<String> {
    use std::{io, mem::size_of, ptr};
    use winapi::{
        shared::sddl::ConvertSidToStringSidW,
        um::{
            handleapi::CloseHandle,
            processthreadsapi::{GetCurrentProcess, OpenProcessToken},
            securitybaseapi::GetTokenInformation,
            winbase::LocalFree,
            winnt::{TokenUser, HANDLE, TOKEN_QUERY, TOKEN_USER},
        },
    };
    struct Token(HANDLE);
    impl Drop for Token {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    let mut raw_token = ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = Token(raw_token);
    let mut required = 0;
    unsafe {
        GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut required);
    }
    if required == 0 {
        return Err(io::Error::last_os_error());
    }
    // TOKEN_USER 含指针，使用 usize 缓冲保证其对齐。
    let mut buffer = vec![0usize; (required as usize).div_ceil(size_of::<usize>())];
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            required,
            &mut required,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut sid = ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // Win32 保证成功结果为系统分配、以 NUL 结尾的 UTF-16 SID 字符串。
    let mut length = 0;
    unsafe {
        while *sid.add(length) != 0 {
            length += 1;
        }
    }
    let text = String::from_utf16(unsafe { std::slice::from_raw_parts(sid, length) });
    unsafe {
        LocalFree(sid.cast());
    }
    text.map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(not(any(unix, windows)))]
fn create_secure_directory(_path: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "self-update is unsupported on this platform",
    ))
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?
        .sync_all()
        .context("flush launcher update directory")
}
#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(not(windows))]
fn move_replace(from: &Path, to: &Path) -> Result<()> {
    fs::rename(from, to).context("atomically move launcher update file")
}
#[cfg(windows)]
fn move_replace(from: &Path, to: &Path) -> Result<()> {
    use winapi::um::winbase::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};
    let from = wide_null(from)?;
    let to = wide_null(to)?;
    if unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(ReplaceFailure(std::io::Error::last_os_error()).into());
    }
    Ok(())
}

#[cfg(windows)]
#[derive(Debug)]
struct ReplaceFailure(std::io::Error);
#[cfg(windows)]
impl std::fmt::Display for ReplaceFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "atomically move launcher update file: {}", self.0)
    }
}
#[cfg(windows)]
impl std::error::Error for ReplaceFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

#[cfg(windows)]
fn wide_null(path: &Path) -> std::io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;
    let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if wide.contains(&0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "NUL in update path",
        ));
    }
    wide.push(0);
    Ok(wide)
}

#[cfg(windows)]
pub(super) fn try_apply_from_args(
    config: impl FnOnce() -> Result<Option<Config>>,
    now: impl FnOnce() -> Result<u64>,
) -> Result<bool> {
    use std::{ffi::OsStr, os::windows::process::CommandExt, thread, time::Duration};
    use winapi::um::winbase::CREATE_NO_WINDOW;
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(OsStr::new(APPLY_ARG)) {
        return Ok(false);
    }
    let target = PathBuf::from(args.next().context("missing launcher update target")?);
    if args.next().is_some() {
        bail!("unexpected launcher update helper arguments");
    }
    let config =
        config()?.context("launcher update helper is disabled without compiled trust roots")?;
    // 启动助手是旧进程通过最后一次取消检查后的提交点；助手没有交互式启动界面。
    let cancelled = AtomicBool::new(false);
    let helper = std::env::current_exe()?.canonicalize()?;
    let source = helper
        .parent()
        .context("launcher update helper has no directory")?;
    let target = checked_target(&target)?;
    if helper.file_name() != Some(OsStr::new(HELPER_FILE))
        || !source
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with(STAGING_PREFIX))
        || source.parent() != target.parent()
    {
        bail!("launcher update helper must be beside its original installation");
    }
    let installation = Installation::for_helper(&target, &helper, &cancelled)?;
    // 参数不能授权任意目标：目标的字节必须与当前正在运行的旧启动器副本完全相同。
    installation.target_is_unchanged(&cancelled)?;
    let signed = read_snapshot(&source.join(SNAPSHOT_FILE), &cancelled)?;
    let verified = config.verify(&signed, now()?, true)?;
    check_cancelled(&cancelled)?;
    let artifact = verified.artifact(manifest::platform_key())?;
    let mut input = open_regular(&source.join(PAYLOAD_FILE))?;
    transport::verify_payload(&mut input, artifact, &cancelled)?;
    // 即便助手被手动调用，也只从自己新建的受保护目录执行替换。
    let staging = StagingDir::create(installation.parent())?;
    staging.write_snapshot(&signed)?;
    let mut output = create_new_file(&staging.payload_path())?;
    let copied = std::io::copy(&mut input.take(artifact.size + 1), &mut output)?;
    if copied != artifact.size {
        bail!("launcher update helper payload changed while copying");
    }
    output.sync_all()?;
    drop(output);
    let mut lock = None;
    for _ in 0..60 {
        match installation.lock(&cancelled) {
            Ok(guard) => {
                lock = Some(guard);
                break;
            }
            Err(error) if error.downcast_ref::<LockBusy>().is_some() => {
                thread::sleep(Duration::from_secs(1))
            }
            Err(error) => return Err(error),
        }
    }
    let _lock = lock.context("timed out waiting for launcher update lock")?;
    let mut last_error = None;
    for _ in 0..60 {
        // 每次尝试都重新验签/检查有效期/校验文件，受保护目录阻止其他用户换包。
        match installation.promote_verified(&config, &staging, &cancelled) {
            Ok(()) => {
                check_cancelled(&cancelled)?;
                Command::new(&installation.target)
                    .env(SKIP_ENV, "1")
                    .env(super::NO_CONSOLE_ENV, "1")
                    .creation_flags(CREATE_NO_WINDOW)
                    .spawn()
                    .context("restart signed launcher")?;
                // 旧输入与 helper 仍可能被映射，尽力清理并安排下次系统启动删除。
                let _ = fs::remove_file(source.join(SNAPSHOT_FILE));
                let _ = fs::remove_file(source.join(PAYLOAD_FILE));
                schedule_delete(&helper);
                schedule_delete(source);
                return Ok(true);
            }
            Err(error)
                if error
                    .downcast_ref::<ReplaceFailure>()
                    .is_some_and(|cause| matches!(cause.0.raw_os_error(), Some(5 | 32 | 33))) =>
            {
                last_error = Some(error);
                thread::sleep(Duration::from_secs(1));
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("launcher update replacement timed out")))
}

#[cfg(windows)]
fn schedule_delete(path: &Path) {
    use winapi::um::winbase::{MoveFileExW, MOVEFILE_DELAY_UNTIL_REBOOT};
    if let Ok(path) = wide_null(path) {
        unsafe {
            MoveFileExW(path.as_ptr(), std::ptr::null(), MOVEFILE_DELAY_UNTIL_REBOOT);
        }
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::{
        mem::{size_of, zeroed},
        ptr,
    };
    use winapi::{
        shared::sddl::ConvertSidToStringSidW,
        um::{
            securitybaseapi::{
                GetAce, GetAclInformation, GetFileSecurityW, GetSecurityDescriptorControl,
                GetSecurityDescriptorDacl,
            },
            winbase::LocalFree,
            winnt::{
                AclSizeInformation, ACCESS_ALLOWED_ACE, ACCESS_ALLOWED_ACE_TYPE,
                ACL_SIZE_INFORMATION, DACL_SECURITY_INFORMATION, SE_DACL_PROTECTED,
            },
        },
    };

    #[test]
    fn staging_acl_is_protected_and_writable_by_the_actual_current_user_only() {
        let root = tempfile::TempDir::new().unwrap();
        let staging = StagingDir::create(root.path()).unwrap();
        // 此步骤在普通用户 token 下也必须成功，不能依赖管理员组授予权限。
        let mut file = create_new_file(&staging.payload_path()).unwrap();
        file.write_all(b"ACL fixture").unwrap();
        file.sync_all().unwrap();
        let path = wide_null(&staging.path).unwrap();
        let mut needed = 0;
        unsafe {
            GetFileSecurityW(
                path.as_ptr(),
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                0,
                &mut needed,
            );
        }
        assert!(needed > 0);
        let mut descriptor = vec![0usize; (needed as usize).div_ceil(size_of::<usize>())];
        let descriptor = descriptor.as_mut_ptr().cast();
        assert_ne!(
            unsafe {
                GetFileSecurityW(
                    path.as_ptr(),
                    DACL_SECURITY_INFORMATION,
                    descriptor,
                    needed,
                    &mut needed,
                )
            },
            0
        );
        let mut control = 0;
        let mut revision = 0;
        assert_ne!(
            unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) },
            0
        );
        assert_ne!(control & SE_DACL_PROTECTED, 0);
        let mut present = 0;
        let mut defaulted = 0;
        let mut acl = ptr::null_mut();
        assert_ne!(
            unsafe {
                GetSecurityDescriptorDacl(descriptor, &mut present, &mut acl, &mut defaulted)
            },
            0
        );
        assert_ne!(present, 0);
        assert!(!acl.is_null());
        let mut info: ACL_SIZE_INFORMATION = unsafe { zeroed() };
        assert_ne!(
            unsafe {
                GetAclInformation(
                    acl,
                    (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
                    size_of::<ACL_SIZE_INFORMATION>() as u32,
                    AclSizeInformation,
                )
            },
            0
        );
        assert_eq!(info.AceCount, 3);
        let mut principals = Vec::new();
        for index in 0..info.AceCount {
            let mut raw_ace = ptr::null_mut();
            assert_ne!(unsafe { GetAce(acl, index, &mut raw_ace) }, 0);
            let ace = unsafe { &*raw_ace.cast::<ACCESS_ALLOWED_ACE>() };
            assert_eq!(ace.Header.AceType, ACCESS_ALLOWED_ACE_TYPE);
            assert_eq!(
                ace.Header.AceFlags & 0x10,
                0,
                "DACL must not inherit parent ACEs"
            );
            let mut sid = ptr::null_mut();
            assert_ne!(
                unsafe {
                    ConvertSidToStringSidW(
                        (&ace.SidStart as *const u32).cast_mut().cast(),
                        &mut sid,
                    )
                },
                0
            );
            let mut length = 0;
            unsafe {
                while *sid.add(length) != 0 {
                    length += 1;
                }
            }
            principals.push(
                String::from_utf16(unsafe { std::slice::from_raw_parts(sid, length) }).unwrap(),
            );
            unsafe {
                LocalFree(sid.cast());
            }
        }
        let mut expected = vec![
            "S-1-5-18".to_owned(),
            "S-1-5-32-544".to_owned(),
            current_user_sid().unwrap(),
        ];
        principals.sort();
        expected.sort();
        assert_eq!(principals, expected);
    }
}
