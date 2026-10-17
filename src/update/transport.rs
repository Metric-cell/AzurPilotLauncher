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

pub(super) struct Reply {
    pub status: u16,
    pub location: Option<String>,
    pub length: Option<u64>,
    pub encoding: Option<String>,
    pub body: Box<dyn Read>,
}

pub(super) struct HttpTransport(Client);

impl HttpTransport {
    pub(super) fn new() -> Result<Self> {
        // 手动逐跳校验；不加载任何旧 mTLS 身份，保留用户系统代理、禁用自动解压。
        Ok(Self(
            Client::builder()
                .https_only(true)
                .redirect(Policy::none())
                .referer(false)
                .no_gzip()
                .no_brotli()
                .no_deflate()
                .no_zstd()
                .connect_timeout(Duration::from_secs(15))
                .timeout(Duration::from_secs(15))
                .user_agent(concat!("AzurPilot-Updater/", env!("CARGO_PKG_VERSION")))
                .build()
                .context("create secure launcher update HTTP client")?,
        ))
    }
}

impl Transport for HttpTransport {
    fn get(&self, url: &Url, payload: bool) -> Result<Reply> {
        let mut request = self.0.get(url.clone()).header(ACCEPT_ENCODING, "identity");
        if payload {
            // blocking reqwest 没有独立 read_timeout；总期限保证关闭启动画面后
            // 至多等待当前请求 30 秒，然后把控制权交还取消检查。
            request = request.timeout(Duration::from_secs(30));
        }
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
    config: &Config,
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

fn fetch_metadata(
    transport: &impl Transport,
    config: &Config,
    url: &Url,
    limit: usize,
    cancelled: &AtomicBool,
) -> Result<Vec<u8>> {
    let response =
        request_following(transport, config, url, false, cancelled).map_err(|error| {
            if error.chain().any(|cause| {
                cause
                    .downcast_ref::<reqwest::Error>()
                    .is_some_and(|cause| cause.is_connect() || cause.is_timeout())
            }) {
                anyhow::Error::new(Unavailable(
                    "launcher update metadata service is unavailable".to_owned(),
                ))
                .context(error)
            } else {
                error
            }
        })?;
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
    read_limited(response.body, limit, cancelled)
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
    check_cancelled(cancelled)?;
    let url = manifest::secure_url(&artifact.url)?;
    let mut response = request_following(transport, config, &url, true, cancelled)?;
    if response.status != 200 {
        bail!("launcher update payload returned HTTP {}", response.status);
    }
    if response.length.is_some_and(|size| size != artifact.size) {
        bail!("launcher update payload has an unexpected Content-Length");
    }
    let mut file = install::create_new_file(path)?;
    let mut hasher = Sha256::new();
    let mut downloaded = 0u64;
    let mut buffer = [0u8; 128 * 1024];
    let mut last_report = Instant::now();
    loop {
        check_cancelled(cancelled)?;
        let remaining = artifact.size.saturating_sub(downloaded) + 1;
        let read_limit = (remaining.min(buffer.len() as u64)) as usize;
        let count = response.body.read(&mut buffer[..read_limit]);
        check_cancelled(cancelled)?;
        let count = count.context("read launcher update payload")?;
        if count == 0 {
            break;
        }
        downloaded += count as u64;
        if downloaded > artifact.size {
            bail!("launcher update payload exceeds its signed size");
        }
        file.write_all(&buffer[..count])
            .context("write launcher update payload")?;
        hasher.update(&buffer[..count]);
        if last_report.elapsed() >= Duration::from_millis(200) || downloaded == artifact.size {
            progress(Progress::Downloading {
                downloaded,
                total: artifact.size,
            });
            check_cancelled(cancelled)?;
            last_report = Instant::now();
        }
    }
    check_payload(artifact, downloaded, &hasher.finalize())?;
    check_cancelled(cancelled)?;
    file.sync_all().context("flush launcher update payload")?;
    Ok(())
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
