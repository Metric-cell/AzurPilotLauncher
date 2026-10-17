//! 兼容既有 HTTPS stable.json 发布协议，不把客户端 mTLS 身份当作发布签名。
//! 元数据与 SHA-256 依赖原有 HTTPS 服务；暂存和替换复用公共安装边界。
use std::{collections::HashMap, path::Path, sync::atomic::AtomicBool};

use anyhow::{bail, Context, Result};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::{
    check_cancelled,
    install::{self, Installation, StagingDir, UpdateVerifier},
    is_mini,
    manifest::{self, Artifact, MAX_MANIFEST_BYTES, MAX_PAYLOAD_BYTES},
    transport::{self, HttpTransport, Transport, UrlPolicy},
    Cancelled, Outcome, Progress,
};

const PRIMARY: &str = "https://ap-launcher-update.nanoda.work/updata/stable.json";
const FALLBACK: &str = "https://ap.launcher-update.nanoda.work/updata/stable.json";
const SNAPSHOT_MAGIC: &[u8; 8] = b"AZUPL001";

#[derive(Clone, Debug)]
pub(super) struct Config {
    primary: Url,
    metadata_urls: Vec<Url>,
    payload_origins: Vec<Url>,
}

impl Config {
    pub(super) fn from_values(primary: &str, fallback: Option<&str>) -> Result<Self> {
        let primary = manifest::secure_url(primary.trim())?;
        let mut metadata_urls = vec![primary.clone()];
        if let Some(fallback) = fallback.map(str::trim).filter(|url| !url.is_empty()) {
            let fallback = manifest::secure_url(fallback)?;
            if fallback != primary {
                metadata_urls.push(fallback);
            }
        }
        let mut payload_origins = metadata_urls.clone();
        // 自定义更新服务不会继承官方 CDN 的权限，也不会向这些主机发送身份。
        if primary.as_str() == PRIMARY || primary.as_str() == FALLBACK {
            for url in [
                "https://ap-launcher-update-cdn-cn.nanoda.work/",
                "https://alas.nanoda.work/",
            ] {
                payload_origins.push(manifest::secure_url(url)?);
            }
        }
        Ok(Self {
            primary,
            metadata_urls,
            payload_origins,
        })
    }

    fn parse_manifest(&self, bytes: &[u8]) -> Result<Manifest> {
        if bytes.is_empty() || bytes.len() > MAX_MANIFEST_BYTES {
            bail!("legacy update manifest exceeds its size limit");
        }
        let data: Manifest =
            serde_json::from_slice(bytes).context("invalid legacy update manifest")?;
        parse_version(&data.version)?;
        if data.platforms.is_empty() || data.platforms.len() > 16 {
            bail!("invalid legacy update platform count");
        }
        for artifact in data.platforms.values() {
            self.validate_artifact(&artifact.url, &artifact.sha256)?;
        }
        Ok(data)
    }

    fn validate_artifact(&self, url: &str, sha256: &str) -> Result<()> {
        self.validate_url(&manifest::secure_url(url)?, true)?;
        manifest::decode_hex::<32>(sha256).context("invalid legacy update SHA-256 digest")?;
        Ok(())
    }
}

impl UrlPolicy for Config {
    fn validate_url(&self, url: &Url, payload: bool) -> Result<()> {
        manifest::secure_url(url.as_str())?;
        let origins = if payload {
            &self.payload_origins
        } else {
            &self.metadata_urls
        };
        if !origins
            .iter()
            .any(|allowed| manifest::same_origin(allowed, url))
        {
            bail!("legacy update URL has an untrusted origin");
        }
        Ok(())
    }
}

// pub_date、notes 和旧服务的其它附加字段不参与执行授权，保持原协议兼容。
#[derive(Debug, Deserialize)]
struct Manifest {
    version: String,
    platforms: HashMap<String, Platform>,
}

#[derive(Debug, Deserialize)]
struct Platform {
    url: String,
    sha256: String,
}

fn parse_version(version: &str) -> Result<semver::Version> {
    manifest::parse_version(version.strip_prefix('v').unwrap_or(version))
}

fn fetch_manifest(
    transport: &impl Transport,
    config: &Config,
    cancelled: &AtomicBool,
) -> Result<Manifest> {
    let mut failures = Vec::new();
    let mut invalid = None;
    for url in &config.metadata_urls {
        check_cancelled(cancelled)?;
        let result =
            transport::fetch_metadata(transport, config, url, MAX_MANIFEST_BYTES, cancelled)
                .and_then(|bytes| config.parse_manifest(&bytes));
        check_cancelled(cancelled)?;
        match result {
            Ok(data) => return Ok(data),
            Err(error) if error.downcast_ref::<Cancelled>().is_some() => return Err(error),
            Err(error) => {
                warn!("Unable to fetch legacy launcher update from {url}: {error:#}");
                failures.push(format!("{url}: {error:#}"));
                if error.downcast_ref::<transport::Unavailable>().is_none() && invalid.is_none() {
                    invalid = Some(error);
                }
            }
        }
    }
    let detail = failures.join("; ");
    match invalid {
        Some(error) => Err(error).context(detail),
        None => Err(transport::Unavailable(detail).into()),
    }
}

/// 记录已完成下载的长度，供主进程与 Windows 助手重新验证同一份暂存载荷。
/// 它不是发布签名，只能在私有暂存目录和编译期协议选择的边界内使用。
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    schema: u32,
    source: String,
    version: String,
    platform: String,
    url: String,
    sha256: String,
    size: u64,
}

impl Snapshot {
    fn encode(&self) -> Result<Vec<u8>> {
        let mut bytes = SNAPSHOT_MAGIC.to_vec();
        bytes.extend_from_slice(&serde_json::to_vec(self)?);
        if bytes.len() > MAX_MANIFEST_BYTES {
            bail!("legacy update snapshot exceeds its size limit");
        }
        Ok(bytes)
    }
}

impl UpdateVerifier for Config {
    fn staged_artifact(
        &self,
        _installation: &Installation,
        snapshot_path: &Path,
        cancelled: &AtomicBool,
    ) -> Result<Artifact> {
        check_cancelled(cancelled)?;
        let bytes = install::read_metadata(snapshot_path, cancelled)?;
        if bytes.len() > MAX_MANIFEST_BYTES || !bytes.starts_with(SNAPSHOT_MAGIC) {
            bail!("invalid legacy update snapshot");
        }
        let snapshot: Snapshot = serde_json::from_slice(&bytes[SNAPSHOT_MAGIC.len()..])
            .context("invalid legacy update snapshot")?;
        if snapshot.schema != 1
            || snapshot.source != self.primary.as_str()
            || snapshot.platform != manifest::platform_key()
        {
            bail!("legacy update snapshot does not match the compiled source or platform");
        }
        if !parse_version(&snapshot.version)?
            .cmp_precedence(&parse_version(env!("CARGO_PKG_VERSION"))?)
            .is_gt()
        {
            bail!("replacement must be a strictly newer legacy launcher");
        }
        self.validate_artifact(&snapshot.url, &snapshot.sha256)?;
        if snapshot.size == 0 || snapshot.size > MAX_PAYLOAD_BYTES {
            bail!("legacy update payload size is outside its limit");
        }
        check_cancelled(cancelled)?;
        Ok(Artifact {
            platform: snapshot.platform,
            url: snapshot.url,
            sha256: snapshot.sha256,
            size: snapshot.size,
        })
    }
}

pub(super) fn check_and_restart(
    config: &Config,
    cancelled: &AtomicBool,
    progress: &mut impl FnMut(Progress),
) -> Result<Outcome> {
    check_cancelled(cancelled)?;
    let client = HttpTransport::legacy()?;
    let data = fetch_manifest(&client, config, cancelled)?;
    if !parse_version(&data.version)?
        .cmp_precedence(&parse_version(env!("CARGO_PKG_VERSION"))?)
        .is_gt()
    {
        if is_mini(env!("CARGO_PKG_VERSION")) {
            bail!("the mini launcher requires a newer full launcher");
        }
        return Ok(Outcome::Current);
    }
    let platform = manifest::platform_key();
    let artifact = data
        .platforms
        .get(platform)
        .context("legacy update has no payload for this platform")?;
    // 检查为最新版时不要求安装目录可写；只有真正更新时才建立替换边界。
    let installation = Installation::current(cancelled)?;
    let _lock = installation.lock(cancelled)?;
    progress(Progress::Available {
        version: data.version.clone(),
    });
    check_cancelled(cancelled)?;
    let mut staging = StagingDir::create(installation.parent())?;
    let size = transport::download_legacy_payload(
        &client,
        config,
        &manifest::secure_url(&artifact.url)?,
        &artifact.sha256,
        &staging.payload_path(),
        progress,
        cancelled,
    )?;
    check_cancelled(cancelled)?;
    staging.write_metadata(
        &Snapshot {
            schema: 1,
            source: config.primary.to_string(),
            version: data.version,
            platform: platform.to_owned(),
            url: artifact.url.clone(),
            sha256: artifact.sha256.clone(),
            size,
        }
        .encode()?,
    )?;
    progress(Progress::Verifying);
    check_cancelled(cancelled)?;
    progress(Progress::Restarting);
    check_cancelled(cancelled)?;
    installation.replace_and_restart(config, &mut staging, cancelled)?;
    Ok(Outcome::Restarted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, collections::VecDeque, fs, io::Cursor, sync::atomic::Ordering};

    use serde_json::{json, Value};
    use tempfile::TempDir;

    use super::super::transport::Reply;

    const SOURCE: &str = "https://updates.example.invalid/stable.json";
    const BACKUP: &str = "https://backup.example.invalid/stable.json";
    const PAYLOAD_URL: &str = "https://updates.example.invalid/launcher";
    const PAYLOAD: &[u8] = b"legacy launcher fixture";
    static ACTIVE: AtomicBool = AtomicBool::new(false);

    struct Fixture {
        replies: RefCell<VecDeque<(&'static str, Reply)>>,
        requests: RefCell<Vec<(String, bool)>>,
    }
    impl Fixture {
        fn new(replies: impl IntoIterator<Item = (&'static str, Reply)>) -> Self {
            Self {
                replies: RefCell::new(replies.into_iter().collect()),
                requests: RefCell::new(Vec::new()),
            }
        }
    }
    impl Transport for Fixture {
        fn get(&self, url: &Url, payload: bool) -> Result<Reply> {
            self.requests.borrow_mut().push((url.to_string(), payload));
            let (expected, reply) = self.replies.borrow_mut().pop_front().unwrap();
            assert_eq!(url.as_str(), expected);
            Ok(reply)
        }
    }
    fn reply(status: u16, bytes: impl Into<Vec<u8>>) -> Reply {
        Reply {
            status,
            location: None,
            length: None,
            encoding: None,
            body: Box::new(Cursor::new(bytes.into())),
        }
    }
    fn config() -> Config {
        Config::from_values(SOURCE, None).unwrap()
    }
    fn manifest_json() -> Value {
        json!({
            "version": "v999.0.0",
            "pub_date": "2025-01-01T00:00:00Z",
            "notes": "Legacy publishing has no expiry or payload size",
            "platforms": {
                (manifest::platform_key()): {
                    "url": PAYLOAD_URL,
                    "sha256": manifest::digest(PAYLOAD)
                }
            }
        })
    }
    fn snapshot() -> Snapshot {
        Snapshot {
            schema: 1,
            source: SOURCE.to_owned(),
            version: "999.0.0".to_owned(),
            platform: manifest::platform_key().to_owned(),
            url: PAYLOAD_URL.to_owned(),
            sha256: manifest::digest(PAYLOAD),
            size: PAYLOAD.len() as u64,
        }
    }
    fn installation(root: &Path) -> Installation {
        let target = root.join(if cfg!(windows) {
            "launcher.exe"
        } else {
            "launcher"
        });
        fs::write(&target, b"original launcher").unwrap();
        Installation::fixture(&target).unwrap()
    }

    #[test]
    fn old_manifest_and_unknown_length_payload_use_the_safe_installation_verifier() {
        let config = config();
        let fixture = Fixture::new([
            (
                SOURCE,
                reply(200, serde_json::to_vec(&manifest_json()).unwrap()),
            ),
            (PAYLOAD_URL, reply(200, PAYLOAD)),
        ]);
        let data = fetch_manifest(&fixture, &config, &ACTIVE).unwrap();
        assert_eq!(data.version, "v999.0.0");
        let artifact = &data.platforms[manifest::platform_key()];
        let root = TempDir::new().unwrap();
        let installation = installation(root.path());
        let staging = StagingDir::create(root.path()).unwrap();
        let size = transport::download_legacy_payload(
            &fixture,
            &config,
            &Url::parse(&artifact.url).unwrap(),
            &artifact.sha256,
            &staging.payload_path(),
            &mut |_| {},
            &ACTIVE,
        )
        .unwrap();
        assert_eq!(size, PAYLOAD.len() as u64);
        staging
            .write_metadata(&snapshot().encode().unwrap())
            .unwrap();
        installation
            .verify_before_replace(&config, &staging, &ACTIVE)
            .unwrap();
        assert_eq!(
            fixture.requests.borrow().as_slice(),
            &[(SOURCE.to_owned(), false), (PAYLOAD_URL.to_owned(), true)]
        );
        // 暂存载荷在下载完成后被换包，替换入口必须再次拒绝。
        fs::write(staging.payload_path(), b"swapped executable").unwrap();
        assert!(installation
            .verify_before_replace(&config, &staging, &ACTIVE)
            .is_err());
        // A signed build cannot use a legacy snapshot to downgrade its verifier.
        assert!(super::super::tests::config()
            .staged_artifact(&installation, &staging.snapshot_path(), &ACTIVE)
            .is_err());
        #[cfg(unix)]
        {
            fs::write(staging.payload_path(), PAYLOAD).unwrap();
            installation
                .promote_verified(&config, &staging, &ACTIVE)
                .unwrap();
            assert_eq!(fs::read(root.path().join("launcher")).unwrap(), PAYLOAD);
        }
    }

    #[test]
    fn primary_failure_uses_the_configured_backup_without_a_signature_request() {
        let config = Config::from_values(SOURCE, Some(BACKUP)).unwrap();
        let fixture = Fixture::new([
            (SOURCE, reply(503, [])),
            (
                BACKUP,
                reply(200, serde_json::to_vec(&manifest_json()).unwrap()),
            ),
        ]);
        fetch_manifest(&fixture, &config, &ACTIVE).unwrap();
        assert_eq!(fixture.requests.borrow().len(), 2);
        let unavailable = Fixture::new([(SOURCE, reply(503, [])), (BACKUP, reply(502, []))]);
        assert!(fetch_manifest(&unavailable, &config, &ACTIVE)
            .unwrap_err()
            .downcast_ref::<transport::Unavailable>()
            .is_some());
    }

    #[test]
    fn metadata_body_timeouts_allow_fallback_and_remain_unavailable() {
        struct TimedOut;
        impl std::io::Read for TimedOut {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::TimedOut.into())
            }
        }
        let timeout_reply = || Reply {
            body: Box::new(TimedOut),
            ..reply(200, [])
        };
        let config = Config::from_values(SOURCE, Some(BACKUP)).unwrap();
        let fixture = Fixture::new([
            (SOURCE, timeout_reply()),
            (
                BACKUP,
                reply(200, serde_json::to_vec(&manifest_json()).unwrap()),
            ),
        ]);
        fetch_manifest(&fixture, &config, &ACTIVE).unwrap();
        assert_eq!(fixture.requests.borrow().len(), 2);

        let fixture = Fixture::new([(SOURCE, timeout_reply()), (BACKUP, timeout_reply())]);
        let error = fetch_manifest(&fixture, &config, &ACTIVE).unwrap_err();
        assert!(error.downcast_ref::<transport::Unavailable>().is_some());
        let fixture = Fixture::new([
            (SOURCE, reply(200, b"invalid manifest")),
            (BACKUP, timeout_reply()),
        ]);
        let error = fetch_manifest(&fixture, &config, &ACTIVE).unwrap_err();
        assert!(error.downcast_ref::<transport::Unavailable>().is_none());
    }

    #[test]
    fn cancellation_after_primary_request_never_tries_the_backup() {
        struct CancelAfterRequest<'a> {
            fixture: Fixture,
            cancelled: &'a AtomicBool,
        }
        impl Transport for CancelAfterRequest<'_> {
            fn get(&self, url: &Url, payload: bool) -> Result<Reply> {
                let response = self.fixture.get(url, payload);
                self.cancelled.store(true, Ordering::SeqCst);
                response
            }
        }
        let cancelled = AtomicBool::new(false);
        let fixture = CancelAfterRequest {
            fixture: Fixture::new([(SOURCE, reply(503, []))]),
            cancelled: &cancelled,
        };
        let error = fetch_manifest(
            &fixture,
            &Config::from_values(SOURCE, Some(BACKUP)).unwrap(),
            &cancelled,
        )
        .unwrap_err();
        assert!(error.downcast_ref::<Cancelled>().is_some());
        assert_eq!(fixture.fixture.requests.borrow().len(), 1);
    }

    #[test]
    fn invalid_payload_metadata_is_rejected_before_any_payload_request() {
        for (field, value) in [
            ("url", "http://updates.example.invalid/launcher"),
            ("url", "https://attacker.example.invalid/launcher"),
            (
                "url",
                "https://user:password@updates.example.invalid/launcher",
            ),
            ("url", "https://127.0.0.1/launcher"),
            ("sha256", "bad digest"),
        ] {
            let mut value_json = manifest_json();
            value_json["platforms"][manifest::platform_key()][field] = json!(value);
            let fixture =
                Fixture::new([(SOURCE, reply(200, serde_json::to_vec(&value_json).unwrap()))]);
            assert!(fetch_manifest(&fixture, &config(), &ACTIVE).is_err());
            assert_eq!(fixture.requests.borrow().len(), 1);
        }
    }

    #[test]
    fn custom_services_do_not_inherit_official_payload_or_metadata_origins() {
        let custom = Config::from_values(SOURCE, Some(BACKUP)).unwrap();
        let official = Config::from_values(PRIMARY, Some(FALLBACK)).unwrap();
        let cdn = Url::parse("https://ap-launcher-update-cdn-cn.nanoda.work/updata/file").unwrap();
        assert!(official.validate_url(&cdn, true).is_ok());
        assert!(official.validate_url(&cdn, false).is_err());
        assert!(custom.validate_url(&cdn, true).is_err());
        assert!(custom
            .validate_url(&Url::parse(BACKUP).unwrap(), true)
            .is_ok());
    }

    #[test]
    fn snapshots_are_bound_to_the_protocol_source_version_and_payload() {
        let root = TempDir::new().unwrap();
        let installation = installation(root.path());
        let path = root.path().join("snapshot.bin");
        let config = config();
        for (field, value) in [
            ("schema", json!(2)),
            ("source", json!(BACKUP)),
            ("version", json!(env!("CARGO_PKG_VERSION"))),
            ("platform", json!("another-platform")),
            (
                "url",
                json!("https://attacker.example.invalid/arbitrary.exe"),
            ),
            ("sha256", json!("invalid")),
            ("size", json!(0)),
            ("size", json!(MAX_PAYLOAD_BYTES + 1)),
            ("unknown_field", json!(true)),
        ] {
            let mut value_json = serde_json::to_value(snapshot()).unwrap();
            value_json[field] = value;
            let mut bytes = SNAPSHOT_MAGIC.to_vec();
            bytes.extend(serde_json::to_vec(&value_json).unwrap());
            fs::write(&path, bytes).unwrap();
            assert!(config
                .staged_artifact(&installation, &path, &ACTIVE)
                .is_err());
        }
        fs::write(&path, b"AZUPD001signed-format-is-not-legacy").unwrap();
        assert!(config
            .staged_artifact(&installation, &path, &ACTIVE)
            .is_err());
        let fixture = Fixture::new([(SOURCE, reply(200, vec![b'x'; MAX_MANIFEST_BYTES + 1]))]);
        assert!(fetch_manifest(&fixture, &config, &ACTIVE).is_err());
    }
}
