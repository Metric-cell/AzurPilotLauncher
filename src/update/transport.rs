use std::{
    fs::File,
    io::{Read, Write},
    path::Path,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use reqwest::{
    blocking::Client,
    header::{ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH, LOCATION},
    redirect::Policy,
    Url,
};
use sha2::{Digest, Sha256};

use super::{
    check_cancelled, install,
    manifest::{self, Artifact, Config, SignedManifest, MAX_MANIFEST_BYTES, SIGNATURE_BYTES},
    Progress,
};

const MAX_REDIRECTS: usize = 5;

/// 通过此窄接口注入本地 fixture；生产唯一实现始终使用验证 TLS 的 HTTPS。
pub(super) trait Transport {
    fn get(&self, url: &Url, payload: bool) -> Result<Reply>;
}

pub(super) trait UrlPolicy {
    fn validate_url(&self, url: &Url, payload: bool) -> Result<()>;
}

impl UrlPolicy for Config {
    fn validate_url(&self, url: &Url, payload: bool) -> Result<()> {
        Config::validate_url(self, url, payload)
    }
}

pub(super) struct Reply {
    pub status: u16,
    pub location: Option<String>,
    pub length: Option<u64>,
    pub encoding: Option<String>,
    pub body: Box<dyn Read>,
}

pub(super) struct HttpTransport {
    metadata: Client,
    payload: Client,
}

impl HttpTransport {
    pub(super) fn new() -> Result<Self> {
        // Signed sources never inherit the legacy service's client identity.
        let metadata = Self::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .context("create signed update metadata client")?;
        let payload = Self::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("create signed update payload client")?;
        Ok(Self { metadata, payload })
    }

    fn builder() -> reqwest::blocking::ClientBuilder {
        Client::builder()
            .https_only(true)
            .redirect(Policy::none())
            .referer(false)
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .connect_timeout(Duration::from_secs(15))
            .user_agent(concat!("AzurPilot-Updater/", env!("CARGO_PKG_VERSION")))
    }

    pub(super) fn legacy() -> Result<Self> {
        // Preserve the existing service's payload authentication. The public
        // manifest client never receives the embedded mTLS identity.
        let builder = || {
            Self::builder()
                .no_proxy()
                .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36 AZURPILOT_LAUNCHER_UPDATE/2.0.4")
        };
        let metadata = builder()
            .timeout(Duration::from_secs(15))
            .build()
            .context("create public update metadata client")?;
        let identity = include_bytes!(concat!(env!("OUT_DIR"), "/launcher_mtls_identity.pem"));
        let mut payload = builder().timeout(Duration::from_secs(30));
        if !identity.is_empty() {
            payload = payload.identity(
                reqwest::Identity::from_pem(identity)
                    .context("parse compiled launcher payload client identity")?,
            );
        }
        let payload = payload.build().context("create update payload client")?;
        Ok(Self { metadata, payload })
    }
}

impl Transport for HttpTransport {
    fn get(&self, url: &Url, payload: bool) -> Result<Reply> {
        let client = if payload {
            &self.payload
        } else {
            &self.metadata
        };
        let mut request = client.get(url.clone()).header(ACCEPT_ENCODING, "identity");
        if !payload {
            request = request.timeout(Duration::from_secs(15));
        }
        // Payloads use the blocking client's per-operation timeout, reset on
        // each read. A request timeout here would cap the entire large download.
        let response = request.send().context("request launcher update")?;
        if response.headers().get_all(LOCATION).iter().count() > 1 {
            bail!("update response has multiple Location headers");
        }
        let location = response
            .headers()
            .get(LOCATION)
            .map(|value| value.to_str().map(str::to_owned))
            .transpose()?;
        let length = response
            .headers()
            .get(CONTENT_LENGTH)
            .map(|value| -> Result<u64> { Ok(value.to_str()?.parse()?) })
            .transpose()
            .context("invalid update Content-Length")?;
        let encoding = response
            .headers()
            .get(CONTENT_ENCODING)
            .map(|value| value.to_str().map(str::to_owned))
            .transpose()?;
        Ok(Reply {
            status: response.status().as_u16(),
            location,
            length,
            encoding,
            body: Box::new(response),
        })
    }
}

fn request_following(
    transport: &impl Transport,
    config: &impl UrlPolicy,
    first: &Url,
    payload: bool,
    cancelled: &AtomicBool,
) -> Result<Reply> {
    let mut url = first.clone();
    for redirects in 0..=MAX_REDIRECTS {
        // 此检查必须在 send 之前，包括重定向后的每一跳。
        config.validate_url(&url, payload)?;
        check_cancelled(cancelled)?;
        let response = transport.get(&url, payload);
        check_cancelled(cancelled)?;
        let response = response?;
        if matches!(response.status, 301 | 302 | 303 | 307 | 308) {
            if redirects == MAX_REDIRECTS {
                bail!("too many launcher update redirects");
            }
            let location = response
                .location
                .as_deref()
                .context("update redirect has no Location")?;
            url = url.join(location).context("invalid update redirect URL")?;
            continue;
        }
        if response
            .encoding
            .as_deref()
            .is_some_and(|encoding| !encoding.eq_ignore_ascii_case("identity"))
        {
            bail!("update response must not use content encoding");
        }
        return Ok(response);
    }
    unreachable!()
}

pub(super) fn fetch_manifest(
    transport: &impl Transport,
    config: &Config,
    cancelled: &AtomicBool,
) -> Result<SignedManifest> {
    let manifest = fetch_metadata(
        transport,
        config,
        &config.manifest_url,
        MAX_MANIFEST_BYTES,
        cancelled,
    )?;
    let signature = fetch_metadata(
        transport,
        config,
        &config.signature_url,
        SIGNATURE_BYTES,
        cancelled,
    )?;
    check_cancelled(cancelled)?;
    if signature.len() != SIGNATURE_BYTES {
        bail!("update signature must be exactly 64 raw bytes");
    }
    Ok(SignedManifest {
        manifest,
        signature,
    })
}

pub(super) fn fetch_metadata(
    transport: &impl Transport,
    config: &impl UrlPolicy,
    url: &Url,
    limit: usize,
    cancelled: &AtomicBool,
) -> Result<Vec<u8>> {
    let response = request_following(transport, config, url, false, cancelled)
        .map_err(classify_metadata_error)?;
    if (500..=599).contains(&response.status) {
        return Err(Unavailable(format!(
            "launcher update metadata returned HTTP {}",
            response.status
        ))
        .into());
    }
    if response.status != 200 {
        bail!("launcher update metadata returned HTTP {}", response.status);
    }
    if response.length.is_some_and(|length| length > limit as u64) {
        bail!("launcher update metadata exceeds its size limit");
    }
    read_limited(response.body, limit, cancelled).map_err(classify_metadata_error)
}

fn classify_metadata_error(error: anyhow::Error) -> anyhow::Error {
    if error.downcast_ref::<super::Cancelled>().is_some() {
        return error;
    }
    if error.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(|cause| cause.is_connect() || cause.is_timeout())
            || cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|cause| cause.kind() == std::io::ErrorKind::TimedOut)
    }) {
        anyhow::Error::new(Unavailable(
            "launcher update metadata service is unavailable".to_owned(),
        ))
        .context(error)
    } else {
        error
    }
}

pub(super) fn read_limited(
    mut reader: impl Read,
    limit: usize,
    cancelled: &AtomicBool,
) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        check_cancelled(cancelled)?;
        let read_limit = buffer.len().min(limit.saturating_sub(bytes.len()) + 1);
        let result = reader.read(&mut buffer[..read_limit]);
        check_cancelled(cancelled)?;
        let count = result.context("read bounded update metadata")?;
        if count == 0 {
            return Ok(bytes);
        }
        bytes.extend_from_slice(&buffer[..count]);
        if bytes.len() > limit {
            bail!("launcher update metadata exceeds its size limit");
        }
    }
}

pub(super) fn download_payload(
    transport: &impl Transport,
    config: &Config,
    artifact: &Artifact,
    path: &Path,
    progress: &mut impl FnMut(Progress),
    cancelled: &AtomicBool,
) -> Result<()> {
    download_with_size(
        transport,
        config,
        &manifest::secure_url(&artifact.url)?,
        &artifact.sha256,
        Some(artifact.size),
        path,
        progress,
        cancelled,
    )?;
    Ok(())
}

pub(super) fn download_legacy_payload(
    transport: &impl Transport,
    config: &impl UrlPolicy,
    url: &Url,
    sha256: &str,
    path: &Path,
    progress: &mut impl FnMut(Progress),
    cancelled: &AtomicBool,
) -> Result<u64> {
    download_with_size(
        transport, config, url, sha256, None, path, progress, cancelled,
    )
}

#[allow(clippy::too_many_arguments)]
fn download_with_size(
    transport: &impl Transport,
    config: &impl UrlPolicy,
    url: &Url,
    sha256: &str,
    expected_size: Option<u64>,
    path: &Path,
    progress: &mut impl FnMut(Progress),
    cancelled: &AtomicBool,
) -> Result<u64> {
    check_cancelled(cancelled)?;
    let expected_hash = manifest::decode_hex::<32>(sha256)?;
    if expected_size.is_some_and(|size| size == 0 || size > manifest::MAX_PAYLOAD_BYTES) {
        bail!("launcher update payload size is outside its limit");
    }
    let mut response = request_following(transport, config, url, true, cancelled)?;
    if response.status != 200 {
        bail!("launcher update payload returned HTTP {}", response.status);
    }
    if response
        .length
        .is_some_and(|size| size == 0 || size > manifest::MAX_PAYLOAD_BYTES)
        || expected_size
            .is_some_and(|expected| response.length.is_some_and(|size| size != expected))
    {
        bail!("launcher update payload has an unexpected Content-Length");
    }
    let mut file = install::create_new_file(path)?;
    let mut hasher = Sha256::new();
    let mut downloaded = 0u64;
    let mut buffer = [0u8; 128 * 1024];
    let mut last_report = Instant::now();
    let limit = expected_size.unwrap_or(manifest::MAX_PAYLOAD_BYTES);
    loop {
        check_cancelled(cancelled)?;
        let remaining = limit.saturating_sub(downloaded) + 1;
        let read_limit = (remaining.min(buffer.len() as u64)) as usize;
        let count = response.body.read(&mut buffer[..read_limit]);
        check_cancelled(cancelled)?;
        let count = count.context("read launcher update payload")?;
        if count == 0 {
            break;
        }
        downloaded += count as u64;
        if downloaded > limit {
            bail!("launcher update payload exceeds its size limit");
        }
        file.write_all(&buffer[..count])
            .context("write launcher update payload")?;
        hasher.update(&buffer[..count]);
        if last_report.elapsed() >= Duration::from_millis(200) || expected_size == Some(downloaded)
        {
            progress(Progress::Downloading {
                downloaded,
                total: expected_size.or(response.length).unwrap_or(0),
            });
            check_cancelled(cancelled)?;
            last_report = Instant::now();
        }
    }
    if downloaded == 0
        || expected_size
            .or(response.length)
            .is_some_and(|size| downloaded != size)
    {
        bail!("launcher update payload is empty or truncated");
    }
    if expected_hash[..] != hasher.finalize()[..] {
        bail!("launcher update SHA-256 verification failed");
    }
    check_cancelled(cancelled)?;
    file.sync_all().context("flush launcher update payload")?;
    Ok(downloaded)
}

pub(super) fn verify_payload(
    file: &mut File,
    artifact: &Artifact,
    cancelled: &AtomicBool,
) -> Result<()> {
    check_cancelled(cancelled)?;
    use std::io::{Seek, SeekFrom};
    file.seek(SeekFrom::Start(0))?;
    if file.metadata()?.len() != artifact.size {
        bail!("launcher update file differs from its signed size");
    }
    let mut hasher = Sha256::new();
    let mut length = 0;
    let mut buffer = [0u8; 128 * 1024];
    loop {
        check_cancelled(cancelled)?;
        let count = file.read(&mut buffer)?;
        check_cancelled(cancelled)?;
        if count == 0 {
            break;
        }
        length += count as u64;
        if length > artifact.size {
            bail!("launcher update file exceeds its signed size");
        }
        hasher.update(&buffer[..count]);
    }
    check_payload(artifact, length, &hasher.finalize())?;
    check_cancelled(cancelled)?;
    file.seek(SeekFrom::Start(0))?;
    Ok(())
}

fn check_payload(artifact: &Artifact, size: u64, hash: &[u8]) -> Result<()> {
    if size != artifact.size {
        bail!("launcher update payload is truncated");
    }
    if manifest::decode_hex::<32>(&artifact.sha256)?.as_slice() != hash {
        bail!("launcher update SHA-256 verification failed");
    }
    Ok(())
}

#[derive(Debug)]
pub(super) struct Unavailable(pub String);
impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Unavailable {}
