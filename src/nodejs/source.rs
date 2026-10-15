//! 官方便携包的不可变来源、摘要与纯版本判定，不包含安装或系统权限操作。

use anyhow::{bail, Result};

pub(super) const MIN_FRONTEND_VERSION: (u32, u32, u32) = (22, 12, 0);
pub(super) const MIN_FRONTEND_VERSION_TEXT: &str = "22.12.0";

#[derive(Clone, Copy, Debug)]
pub(super) struct NodeArchive {
    pub version: &'static str,
    pub architecture: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
}

impl NodeArchive {
    pub fn directory_name(self) -> String {
        format!("v{}-win-{}", self.version, self.architecture)
    }
}

pub(super) fn archive_for_architecture(architecture: &str) -> Result<NodeArchive> {
    match architecture {
        "x86_64" => Ok(NodeArchive {
            version: "24.21.0",
            architecture: "x64",
            url: "https://nodejs.org/dist/v24.21.0/node-v24.21.0-win-x64.zip",
            sha256: "158f7685b44de51f6c0df1d153526cbcd3e1bc739a8dfc607721cef75de9e541",
        }),
        "aarch64" => Ok(NodeArchive {
            version: "24.21.0",
            architecture: "arm64",
            url: "https://nodejs.org/dist/v24.21.0/node-v24.21.0-win-arm64.zip",
            sha256: "8779b1bde1d39f8d420e3b57aa657b39891af434d3de44a919044cec06785921",
        }),
        "x86" => Ok(NodeArchive {
            version: "22.22.2",
            architecture: "x86",
            url: "https://nodejs.org/dist/v22.22.2/node-v22.22.2-win-x86.zip",
            sha256: "ca892f829a733109e341c43585fd2094177e9d2f2c45f97c7ed3cf329d5427c5",
        }),
        other => bail!(
            "Node.js automatic installation is not available for Windows architecture {other}"
        ),
    }
}

pub(super) fn validate_source(archive: NodeArchive) -> Result<()> {
    let url = reqwest::Url::parse(archive.url)?;
    if url.scheme() != "https"
        || url.host_str() != Some("nodejs.org")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.path().ends_with(".zip")
    {
        bail!(rust_i18n::t!("errors.nodejs_url_not_https"));
    }
    if archive.sha256.len() != 64 || !archive.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!(rust_i18n::t!("errors.nodejs_digest_invalid"));
    }
    Ok(())
}

pub(super) fn parse_version(output: &[u8]) -> Option<(u32, u32, u32)> {
    let value = std::str::from_utf8(output).ok()?.trim();
    let mut components = value.strip_prefix('v')?.split('.');
    let major = components.next()?.parse().ok()?;
    let minor = components.next()?.parse().ok()?;
    let patch = components.next()?.parse().ok()?;
    if components.next().is_some() || (major, minor, patch) == (0, 0, 0) {
        return None;
    }
    Some((major, minor, patch))
}

pub(super) fn meets_minimum_version(version: (u32, u32, u32)) -> bool {
    version >= MIN_FRONTEND_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_archives_are_official_https_zip_files_with_full_digests() {
        for architecture in ["x86_64", "aarch64", "x86"] {
            let archive = archive_for_architecture(architecture).unwrap();
            validate_source(archive).unwrap();
            assert!(meets_minimum_version(
                parse_version(format!("v{}", archive.version).as_bytes()).unwrap()
            ));
            assert!(archive.directory_name().starts_with('v'));
        }
        assert!(archive_for_architecture("mips").is_err());
        assert_eq!(MIN_FRONTEND_VERSION_TEXT, "22.12.0");
    }

    #[test]
    fn source_rejects_downgrade_foreign_hosts_credentials_and_invalid_digests() {
        let archive = archive_for_architecture("x86_64").unwrap();
        for url in [
            "http://nodejs.org/dist/node.zip",
            "https://example.com/node.zip",
            "https://nodejs.org.evil.example/node.zip",
            "https://user@nodejs.org/dist/node.zip",
            "https://nodejs.org/dist/node.zip?download=1",
            "https://nodejs.org/dist/node.msi",
        ] {
            assert!(
                validate_source(NodeArchive { url, ..archive }).is_err(),
                "{url}"
            );
        }
        assert!(validate_source(NodeArchive {
            sha256: "bad",
            ..archive
        })
        .is_err());
    }

    #[test]
    fn node_version_parser_accepts_only_a_complete_version() {
        assert_eq!(parse_version(b"v24.21.0\r\n"), Some((24, 21, 0)));
        for invalid in [
            "24.21.0",
            "node v24.21.0",
            "v0.0.0",
            "v24.21.0.1",
            "v24.21.0-rc1",
        ] {
            assert_eq!(parse_version(invalid.as_bytes()), None, "{invalid}");
        }
        assert!(meets_minimum_version((22, 12, 0)));
        assert!(meets_minimum_version((24, 21, 0)));
        assert!(!meets_minimum_version((22, 11, 0)));
        assert!(!meets_minimum_version((20, 19, 0)));
    }
}
