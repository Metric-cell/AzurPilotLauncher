use std::{cmp::Ordering, collections::HashSet};

use anyhow::{anyhow, bail, Context, Result};
use ed25519_dalek::{Signature, VerifyingKey};
use reqwest::Url;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};

pub(super) const DOMAIN: &[u8] = b"AzurPilot launcher update manifest v1\0";
pub(super) const MAX_MANIFEST_BYTES: usize = 64 * 1024;
pub(super) const SIGNATURE_BYTES: usize = 64;
pub(super) const MAX_PAYLOAD_BYTES: u64 = 512 * 1024 * 1024;
pub(super) const MAX_LIFETIME: u64 = 7 * 24 * 60 * 60;
pub(super) const PRODUCT: &str = "moe.taiho.alas-launcher";
pub(super) const SNAPSHOT_MAGIC: &[u8; 8] = b"AZUPD001";
pub(super) const MAX_SNAPSHOT_BYTES: usize = 8 + SIGNATURE_BYTES + MAX_MANIFEST_BYTES;

#[derive(Clone, Debug)]
pub(super) struct Config {
    pub manifest_url: Url,
    pub signature_url: Url,
    pub payload_origins: Vec<Url>,
    pub metadata_origins: Vec<Url>,
    pub channel: String,
    pub min_sequence: u64,
    public_key: VerifyingKey,
}

impl Config {
    pub(super) fn from_values(
        url: Option<&str>,
        key: Option<&str>,
        origins: Option<&str>,
        channel: Option<&str>,
        min_sequence: Option<&str>,
    ) -> Result<Option<Self>> {
        // 缺一个信任根就禁用，尤其不能读取旧 LAUNCHER_UPDATE_URL 或默认上游。
        let (Some(url), Some(key)) = (nonempty(url), nonempty(key)) else {
            return Ok(None);
        };
        let manifest_url = secure_url(url)?;
        if manifest_url.query().is_some() || manifest_url.path().ends_with('/') {
            bail!("update manifest URL must name a file without a query");
        }
        let key_bytes =
            decode_hex::<32>(key).context("update public key must be 64 hexadecimal characters")?;
        let public_key =
            VerifyingKey::from_bytes(&key_bytes).context("invalid Ed25519 update public key")?;
        if public_key.is_weak() {
            bail!("weak Ed25519 update public key");
        }
        let mut payload_origins = vec![origin(&manifest_url)?];
        if let Some(origins) = nonempty(origins) {
            for value in origins.split(',') {
                let value = secure_url(value.trim())?;
                if value.path() != "/" || value.query().is_some() {
                    bail!("trusted payload origins must not contain paths or queries");
                }
                if !payload_origins
                    .iter()
                    .any(|prior| same_origin(prior, &value))
                {
                    payload_origins.push(value);
                }
            }
        }
        let channel = nonempty(channel).unwrap_or("stable");
        if channel.len() > 32
            || !channel
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            bail!("invalid compiled update channel");
        }
        let min_sequence = nonempty(min_sequence)
            .unwrap_or("0")
            .parse()
            .context("invalid minimum update sequence")?;
        let signature_url = secure_url(&format!("{manifest_url}.sig"))?;
        let metadata_origins = vec![origin(&manifest_url)?];
        Ok(Some(Self {
            manifest_url,
            signature_url,
            payload_origins,
            metadata_origins,
            channel: channel.to_owned(),
            min_sequence,
            public_key,
        }))
    }

    pub(super) fn allow_metadata_origins(&mut self, values: Option<&str>) -> Result<()> {
        if let Some(values) = nonempty(values) {
            for value in values.split(',') {
                let value = secure_url(value.trim())?;
                if value.path() != "/" || value.query().is_some() {
                    bail!("trusted metadata origins must not contain paths or queries");
                }
                if !self
                    .metadata_origins
                    .iter()
                    .any(|previous| same_origin(previous, &value))
                {
                    self.metadata_origins.push(value);
                }
            }
        }
        Ok(())
    }

    pub(super) fn validate_url(&self, url: &Url, payload: bool) -> Result<()> {
        secure_url(url.as_str())?;
        let permitted = if payload {
            self.payload_origins
                .iter()
                .any(|allowed| same_origin(allowed, url))
        } else {
            self.metadata_origins
                .iter()
                .any(|allowed| same_origin(allowed, url))
        };
        if !permitted {
            bail!("update URL has an untrusted origin");
        }
        Ok(())
    }

    pub(super) fn verify(
        &self,
        signed: &SignedManifest,
        now: u64,
        require_fresh: bool,
    ) -> Result<VerifiedManifest> {
        if signed.manifest.is_empty() || signed.manifest.len() > MAX_MANIFEST_BYTES {
            bail!("update manifest exceeds its size limit");
        }
        if signed.signature.len() != SIGNATURE_BYTES {
            bail!("update signature must be exactly 64 raw bytes");
        }
        let signature = Signature::from_slice(&signed.signature)
            .context("invalid update signature encoding")?;
        let mut message = Vec::with_capacity(DOMAIN.len() + signed.manifest.len());
        message.extend_from_slice(DOMAIN);
        message.extend_from_slice(&signed.manifest);
        self.public_key
            .verify_strict(&message, &signature)
            .context("update manifest signature verification failed")?;
        // JSON 解析和任何业务字段的使用均发生在验签之后。签名覆盖原始字节，不重新序列化。
        let data: Manifest =
            serde_json::from_slice(&signed.manifest).context("invalid signed update manifest")?;
        if data.schema != 1
            || data.product != PRODUCT
            || data.channel != self.channel
            || data.source != self.manifest_url.as_str()
        {
            bail!("signed update manifest does not match this product, channel or source");
        }
        if data.sequence == 0 || data.sequence < self.min_sequence {
            bail!("signed update sequence is below the compiled minimum");
        }
        if data.expires_at <= data.issued_at || data.expires_at - data.issued_at > MAX_LIFETIME {
            bail!("signed update manifest has an invalid validity interval");
        }
        if require_fresh && (data.expires_at <= now || data.issued_at > now.saturating_add(300)) {
            bail!("signed update manifest is expired or issued in the future");
        }
        let version = parse_version(&data.version)?;
        if data.artifacts.is_empty() || data.artifacts.len() > 16 {
            bail!("invalid signed update artifact count");
        }
        let mut platforms = HashSet::new();
        for artifact in &data.artifacts {
            if !PLATFORMS.contains(&artifact.platform.as_str())
                || !platforms.insert(&artifact.platform)
            {
                bail!("unsupported or duplicate update platform");
            }
            self.validate_url(&secure_url(&artifact.url)?, true)?;
            decode_hex::<32>(&artifact.sha256).context("invalid signed update SHA-256 digest")?;
            if artifact.size == 0 || artifact.size > MAX_PAYLOAD_BYTES {
                bail!("signed update payload size is outside its limit");
            }
        }
        Ok(VerifiedManifest {
            data,
            version,
            signed: signed.clone(),
        })
    }
}

fn nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

pub(super) fn secure_url(value: &str) -> Result<Url> {
    let url = Url::parse(value).context("invalid update URL")?;
    // 限制为 DNS 主机的标准 HTTPS 来源；禁用 URL 用户凭据、fragment 和非443端口。
    if url.scheme() != "https"
        || url.domain().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.port_or_known_default() != Some(443)
    {
        bail!("update URLs must use credential-free HTTPS on a trusted DNS origin");
    }
    Ok(url)
}

pub(super) fn same_origin(left: &Url, right: &Url) -> bool {
    left.origin() == right.origin()
}
fn origin(url: &Url) -> Result<Url> {
    Ok(Url::parse(&format!(
        "{}/",
        url.origin().ascii_serialization()
    ))?)
}

#[derive(Clone, Debug)]
pub(super) struct SignedManifest {
    pub manifest: Vec<u8>,
    pub signature: Vec<u8>,
}

impl SignedManifest {
    pub(super) fn encode(&self) -> Result<Vec<u8>> {
        if self.signature.len() != SIGNATURE_BYTES
            || self.manifest.is_empty()
            || self.manifest.len() > MAX_MANIFEST_BYTES
        {
            bail!("invalid signed manifest snapshot size");
        }
        let mut bytes = Vec::with_capacity(8 + SIGNATURE_BYTES + self.manifest.len());
        bytes.extend_from_slice(SNAPSHOT_MAGIC);
        bytes.extend_from_slice(&self.signature);
        bytes.extend_from_slice(&self.manifest);
        Ok(bytes)
    }
    pub(super) fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() <= 8 + SIGNATURE_BYTES
            || bytes.len() > MAX_SNAPSHOT_BYTES
            || !bytes.starts_with(SNAPSHOT_MAGIC)
        {
            bail!("invalid update state snapshot");
        }
        Ok(Self {
            signature: bytes[8..8 + SIGNATURE_BYTES].to_vec(),
            manifest: bytes[8 + SIGNATURE_BYTES..].to_vec(),
        })
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Manifest {
    pub schema: u32,
    pub product: String,
    pub channel: String,
    pub source: String,
    pub sequence: u64,
    pub version: String,
    pub issued_at: u64,
    pub expires_at: u64,
    pub artifacts: Vec<Artifact>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Artifact {
    pub platform: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Clone, Debug)]
pub(super) struct VerifiedManifest {
    pub data: Manifest,
    pub version: Version,
    pub signed: SignedManifest,
}

impl VerifiedManifest {
    pub(super) fn artifact(&self, platform: &str) -> Result<&Artifact> {
        self.data
            .artifacts
            .iter()
            .find(|item| item.platform == platform)
            .ok_or_else(|| anyhow!("signed update has no payload for this platform"))
    }
    pub(super) fn check_high_water(&self, previous: &Self) -> Result<()> {
        if self.data.sequence < previous.data.sequence
            || self.version.cmp_precedence(&previous.version) == Ordering::Less
        {
            bail!("signed update would roll back a previously authenticated release");
        }
        if self.data.sequence == previous.data.sequence
            && self.signed.manifest != previous.signed.manifest
        {
            bail!("signed update reuses a sequence with different manifest bytes");
        }
        Ok(())
    }
}

pub(super) fn parse_version(value: &str) -> Result<Version> {
    if value.len() > 128 {
        bail!("update version is too long");
    }
    Version::parse(value).context("update version is not a valid SemVer")
}

pub(super) fn decode_hex<const N: usize>(value: &str) -> Result<[u8; N]> {
    if value.len() != N * 2 {
        bail!("incorrect hexadecimal length");
    }
    let mut output = [0; N];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let nibble = |byte: u8| {
            (byte as char)
                .to_digit(16)
                .map(|digit| digit as u8)
                .ok_or_else(|| anyhow!("invalid hexadecimal character"))
        };
        output[index] = nibble(pair[0])? * 16 + nibble(pair[1])?;
    }
    Ok(output)
}

pub(super) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(value, "{byte:02x}");
    }
    value
}

pub(super) fn digest(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

const PLATFORMS: &[&str] = &[
    "windows-x86_64",
    "windows-aarch64",
    "linux-x86_64",
    "linux-aarch64",
    "darwin-x86_64",
    "darwin-aarch64",
];
pub(super) fn platform_key() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => "windows-x86_64",
        ("windows", "aarch64") => "windows-aarch64",
        ("linux", "x86_64") => "linux-x86_64",
        ("linux", "aarch64") => "linux-aarch64",
        ("macos", "x86_64") => "darwin-x86_64",
        ("macos", "aarch64") => "darwin-aarch64",
        _ => "unsupported",
    }
}
