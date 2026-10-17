use std::{
    cell::RefCell,
    collections::VecDeque,
    fs,
    io::{self, Cursor, Read},
    path::Path,
    sync::Arc,
};

use anyhow::Result;
use ed25519_dalek::{Signer, SigningKey};
use reqwest::Url;
use serde_json::{json, Value};
use tempfile::TempDir;

use super::{
    install::{self, Installation, StagingDir},
    manifest::{self, Config, SignedManifest, MAX_MANIFEST_BYTES},
    transport::{self, Reply, Transport},
    *,
};

// RFC 8032 section 7.1 test 1. This published test key is never a deployment key.
const RFC8032_SEED: &str = "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";
const PUBLIC_KEY: &str = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";
const SOURCE: &str = "https://updates.example.invalid/stable.json";
const NOW: u64 = 1_800_000_000;
const PAYLOAD: &[u8] = include_bytes!("fixtures/payload.txt");
static ACTIVE: AtomicBool = AtomicBool::new(false);

#[test]
fn existing_release_protocol_remains_default_and_signed_configuration_cannot_downgrade() {
    assert!(matches!(
        UpdateConfig::from_values(None, None, None, None, None, None, SOURCE).unwrap(),
        UpdateConfig::Legacy(_)
    ));
    assert!(matches!(
        UpdateConfig::from_values(
            Some(SOURCE),
            Some(PUBLIC_KEY),
            None,
            None,
            None,
            None,
            SOURCE
        )
        .unwrap(),
        UpdateConfig::Signed(_)
    ));
    for (url, key) in [(Some(SOURCE), None), (None, Some(PUBLIC_KEY))] {
        assert!(UpdateConfig::from_values(url, key, None, None, None, None, SOURCE).is_err());
    }
    assert!(UpdateConfig::from_values(
        None,
        None,
        Some("https://cdn.example.invalid"),
        None,
        None,
        None,
        SOURCE,
    )
    .is_err());
    assert!(UpdateConfig::from_values(
        Some(SOURCE),
        Some("invalid key"),
        None,
        None,
        None,
        None,
        SOURCE,
    )
    .is_err());
}

#[test]
fn authenticated_expiry_defers_updates_without_blocking_a_full_launch_or_authorizing_installation()
{
    let mut value = fixture_json();
    let now = unix_now().unwrap();
    value["issued_at"] = json!(now - 7200);
    value["expires_at"] = json!(now - 3600);
    value["artifacts"][0]["platform"] = json!(manifest::platform_key());
    let signed = sign(&value);
    let error = config().verify(&signed, now, true).unwrap_err();
    assert!(error.downcast_ref::<manifest::ExpiredManifest>().is_some());
    let root = TempDir::new().unwrap();
    assert!(can_continue_offline(&error, "2.2.4", root.path()));
    assert!(!can_continue_offline(&error, "0.0.1", root.path()));

    let installation = installation(root.path());
    let staging = StagingDir::create(root.path()).unwrap();
    staging.write_snapshot(&signed).unwrap();
    fs::write(staging.payload_path(), PAYLOAD).unwrap();
    assert!(installation
        .promote_verified(&config(), &staging, &ACTIVE)
        .is_err());
    assert_eq!(
        fs::read(root.path().join(if cfg!(windows) {
            "launcher.exe"
        } else {
            "launcher"
        }))
        .unwrap(),
        b"original launcher fixture"
    );

    let mut tampered = signed;
    tampered.signature[0] ^= 1;
    let error = config().verify(&tampered, now, true).unwrap_err();
    assert!(!can_continue_offline(&error, "2.2.4", root.path()));
    value["artifacts"][0]["url"] = json!("https://evil.example.invalid/payload");
    let error = config().verify(&sign(&value), now, true).unwrap_err();
    assert!(error.downcast_ref::<manifest::ExpiredManifest>().is_none());
    assert!(!can_continue_offline(&error, "2.2.4", root.path()));
}

pub(super) fn config() -> Config {
    Config::from_values(Some(SOURCE), Some(PUBLIC_KEY), None, None, None)
        .unwrap()
        .unwrap()
}
fn signed_fixture() -> SignedManifest {
    SignedManifest {
        manifest: include_bytes!("fixtures/valid.json").to_vec(),
        signature: include_bytes!("fixtures/valid.json.sig").to_vec(),
    }
}
fn fixture_json() -> Value {
    serde_json::from_slice(&signed_fixture().manifest).unwrap()
}
fn sign(value: &Value) -> SignedManifest {
    sign_raw(serde_json::to_vec(value).unwrap())
}
fn sign_raw(manifest: Vec<u8>) -> SignedManifest {
    let key = SigningKey::from_bytes(&manifest::decode_hex::<32>(RFC8032_SEED).unwrap());
    let mut message = manifest::DOMAIN.to_vec();
    message.extend_from_slice(&manifest);
    SignedManifest {
        signature: key.sign(&message).to_bytes().to_vec(),
        manifest,
    }
}
fn fresh_fixture(sequence: u64, version: &str) -> SignedManifest {
    let mut value = fixture_json();
    value["sequence"] = json!(sequence);
    value["version"] = json!(version);
    value["issued_at"] = json!(unix_now().unwrap().saturating_sub(10));
    value["expires_at"] = json!(unix_now().unwrap() + 3600);
    value["artifacts"][0]["platform"] = json!(manifest::platform_key());
    sign(&value)
}

struct FixtureTransport {
    replies: RefCell<VecDeque<(String, Reply)>>,
    requests: RefCell<Vec<String>>,
}
impl FixtureTransport {
    fn new(replies: impl IntoIterator<Item = (&'static str, Reply)>) -> Self {
        Self {
            replies: RefCell::new(
                replies
                    .into_iter()
                    .map(|(url, reply)| (url.to_owned(), reply))
                    .collect(),
            ),
            requests: RefCell::new(Vec::new()),
        }
    }
    fn count(&self) -> usize {
        self.requests.borrow().len()
    }
}
impl Transport for FixtureTransport {
    fn get(&self, url: &Url, _: bool) -> Result<Reply> {
        self.requests.borrow_mut().push(url.to_string());
        let (expected, reply) = self
            .replies
            .borrow_mut()
            .pop_front()
            .expect("unexpected outbound request");
        assert_eq!(url.as_str(), expected);
        Ok(reply)
    }
}
fn ok(bytes: impl Into<Vec<u8>>) -> Reply {
    let bytes = bytes.into();
    Reply {
        status: 200,
        location: None,
        length: Some(bytes.len() as u64),
        encoding: None,
        body: Box::new(Cursor::new(bytes)),
    }
}
fn redirect(location: &str) -> Reply {
    Reply {
        status: 302,
        location: Some(location.to_owned()),
        length: None,
        encoding: None,
        body: Box::new(Cursor::new(Vec::new())),
    }
}

#[test]
fn incomplete_configuration_disables_without_parsing_untrusted_urls() {
    assert!(Config::from_values(None, None, None, None, None)
        .unwrap()
        .is_none());
    assert!(
        Config::from_values(Some("http://127.0.0.1:1/"), None, None, None, None)
            .unwrap()
            .is_none()
    );
    assert!(
        Config::from_values(None, Some("not-a-key"), None, None, None)
            .unwrap()
            .is_none()
    );
    assert!(
        Config::from_values(Some(" "), Some(PUBLIC_KEY), None, None, None)
            .unwrap()
            .is_none()
    );
}

#[test]
fn fixed_fixture_is_authenticated_before_parsing_and_uses_exact_signed_bytes() {
    let signed = signed_fixture();
    assert_eq!(
        config().verify(&signed, NOW, true).unwrap().data.sequence,
        42
    );
    let mut whitespace = signed.clone();
    whitespace.manifest.push(b' ');
    assert!(config().verify(&whitespace, NOW, true).is_err());
    let mut altered = signed.clone();
    altered.manifest[0] ^= 1;
    assert!(config().verify(&altered, NOW, true).is_err());
    let wrong_key = Config::from_values(
        Some(SOURCE),
        Some("3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c"),
        None,
        None,
        None,
    )
    .unwrap()
    .unwrap();
    assert!(wrong_key.verify(&signed, NOW, true).is_err());
}

#[test]
fn unsigned_wrong_size_and_weak_keys_are_rejected() {
    for length in [0, 63, 64, 65, 128] {
        let mut signed = signed_fixture();
        signed.signature = vec![0; length];
        assert!(config().verify(&signed, NOW, true).is_err());
    }
    for key in [
        "bad".to_owned(),
        format!("01{}", "00".repeat(31)),
        "00".repeat(32),
    ] {
        assert!(Config::from_values(Some(SOURCE), Some(&key), None, None, None).is_err());
    }
}

#[test]
fn signed_metadata_is_bound_to_source_product_and_channel() {
    for (field, value) in [
        ("source", "https://other.example.invalid/stable.json"),
        ("channel", "beta"),
        ("product", "another-product"),
    ] {
        let mut data = fixture_json();
        data[field] = json!(value);
        assert!(config().verify(&sign(&data), NOW, true).is_err(), "{field}");
    }
    let mut data = fixture_json();
    data["unknown"] = json!(true);
    assert!(config().verify(&sign(&data), NOW, true).is_err());
    assert!(config()
        .verify(
            &sign_raw(b"{\"schema\":1,\"schema\":1}".to_vec()),
            NOW,
            true
        )
        .is_err());
}

#[test]
fn expired_future_oversized_and_invalid_version_manifests_are_rejected() {
    for (field, value) in [
        ("expires_at", NOW),
        ("issued_at", NOW + 301),
        ("expires_at", NOW + manifest::MAX_LIFETIME + 1),
    ] {
        let mut data = fixture_json();
        data[field] = json!(value);
        assert!(config().verify(&sign(&data), NOW, true).is_err(), "{field}");
    }
    for version in ["v3.0.0", "3.0", "3.0.0-01", "03.0.0", "3.0.0+"] {
        let mut data = fixture_json();
        data["version"] = json!(version);
        assert!(
            config().verify(&sign(&data), NOW, true).is_err(),
            "{version}"
        );
    }
    let oversized = sign_raw(vec![b' '; MAX_MANIFEST_BYTES + 1]);
    assert!(config().verify(&oversized, NOW, true).is_err());
}

#[test]
fn payload_origins_credentials_addresses_and_signed_sizes_are_checked() {
    for url in [
        "http://updates.example.invalid/file",
        "https://evil.example.invalid/file",
        "https://u:p@updates.example.invalid/file",
        "https://updates.example.invalid/file#x",
        "https://127.0.0.1/file",
        "https://[::1]/file",
        "https://updates.example.invalid:444/file",
    ] {
        let mut data = fixture_json();
        data["artifacts"][0]["url"] = json!(url);
        assert!(config().verify(&sign(&data), NOW, true).is_err(), "{url}");
    }
    for size in [0, manifest::MAX_PAYLOAD_BYTES + 1] {
        let mut data = fixture_json();
        data["artifacts"][0]["size"] = json!(size);
        assert!(config().verify(&sign(&data), NOW, true).is_err());
    }
    let mut data = fixture_json();
    let duplicate = data["artifacts"][0].clone();
    data["artifacts"].as_array_mut().unwrap().push(duplicate);
    assert!(config().verify(&sign(&data), NOW, true).is_err());
}

#[test]
fn every_redirect_is_authorized_before_the_next_request() {
    for target in [
        "https://evil.example.invalid/file",
        "http://updates.example.invalid/file",
        "https://u:p@updates.example.invalid/file",
        "https://127.0.0.1/file",
        "https://updates.example.invalid:444/file",
    ] {
        let fixture = FixtureTransport::new([(SOURCE, redirect(target))]);
        assert!(
            transport::fetch_manifest(&fixture, &config(), &ACTIVE).is_err(),
            "{target}"
        );
        assert_eq!(
            fixture.count(),
            1,
            "untrusted redirect must never be requested"
        );
    }
    let signed = signed_fixture();
    let fixture = FixtureTransport::new([
        (SOURCE, redirect("/releases/stable.json")),
        (
            "https://updates.example.invalid/releases/stable.json",
            ok(signed.manifest),
        ),
        (
            "https://updates.example.invalid/stable.json.sig",
            ok(signed.signature),
        ),
    ]);
    let signed = transport::fetch_manifest(&fixture, &config(), &ACTIVE).unwrap();
    config().verify(&signed, NOW, true).unwrap();
    assert_eq!(fixture.count(), 3);
}

#[test]
fn payload_redirects_can_only_use_explicit_compiled_origins() {
    let config = Config::from_values(
        Some(SOURCE),
        Some(PUBLIC_KEY),
        Some("https://cdn.example.invalid"),
        None,
        None,
    )
    .unwrap()
    .unwrap();
    let signed = signed_fixture();
    let verified = config.verify(&signed, NOW, true).unwrap();
    let fixture = FixtureTransport::new([
        (
            "https://updates.example.invalid/alas-launcher-linux-x86_64",
            redirect("https://cdn.example.invalid/payload"),
        ),
        ("https://cdn.example.invalid/payload", ok(PAYLOAD)),
    ]);
    let root = TempDir::new().unwrap();
    transport::download_payload(
        &fixture,
        &config,
        &verified.data.artifacts[0],
        &root.path().join("payload"),
        &mut |_| {},
        &ACTIVE,
    )
    .unwrap();
    assert_eq!(fixture.count(), 2);
    let fixture =
        FixtureTransport::new([(SOURCE, redirect("https://cdn.example.invalid/manifest"))]);
    assert!(transport::fetch_manifest(&fixture, &config, &ACTIVE).is_err());
    assert_eq!(
        fixture.count(),
        1,
        "payload origin must not authorize metadata redirects"
    );
}

#[test]
fn metadata_redirects_require_a_separate_explicit_compiled_origin() {
    let mut config = config();
    config
        .allow_metadata_origins(Some("https://cdn.example.invalid"))
        .unwrap();
    let signed = signed_fixture();
    let fixture = FixtureTransport::new([
        (SOURCE, redirect("https://cdn.example.invalid/manifest")),
        ("https://cdn.example.invalid/manifest", ok(signed.manifest)),
        (
            "https://updates.example.invalid/stable.json.sig",
            redirect("https://cdn.example.invalid/signature"),
        ),
        (
            "https://cdn.example.invalid/signature",
            ok(signed.signature),
        ),
    ]);
    let signed = transport::fetch_manifest(&fixture, &config, &ACTIVE).unwrap();
    config.verify(&signed, NOW, true).unwrap();
    assert_eq!(fixture.count(), 4);
    assert!(config
        .allow_metadata_origins(Some("https://cdn.example.invalid/subpath"))
        .is_err());
}

#[test]
fn redirect_loops_and_encoded_responses_are_rejected() {
    let fixture = FixtureTransport::new((0..6).map(|_| (SOURCE, redirect(SOURCE))));
    assert!(transport::fetch_manifest(&fixture, &config(), &ACTIVE).is_err());
    assert_eq!(fixture.count(), 6);
    let mut encoded = ok(signed_fixture().manifest);
    encoded.encoding = Some("gzip".to_owned());
    let fixture = FixtureTransport::new([(SOURCE, encoded)]);
    assert!(transport::fetch_manifest(&fixture, &config(), &ACTIVE).is_err());
}

struct NeverRead;
impl Read for NeverRead {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        panic!("oversized response was read")
    }
}

#[test]
fn metadata_limits_are_enforced_for_length_headers_and_chunked_bodies() {
    let reply = Reply {
        status: 200,
        location: None,
        length: Some(MAX_MANIFEST_BYTES as u64 + 1),
        encoding: None,
        body: Box::new(NeverRead),
    };
    let fixture = FixtureTransport::new([(SOURCE, reply)]);
    assert!(transport::fetch_manifest(&fixture, &config(), &ACTIVE).is_err());
    let mut chunked = ok(vec![b'x'; MAX_MANIFEST_BYTES + 2]);
    chunked.length = None;
    let fixture = FixtureTransport::new([(SOURCE, chunked)]);
    assert!(transport::fetch_manifest(&fixture, &config(), &ACTIVE).is_err());
    for signature in [vec![0; 63], vec![0; 65]] {
        let fixture = FixtureTransport::new([
            (SOURCE, ok(signed_fixture().manifest)),
            (
                "https://updates.example.invalid/stable.json.sig",
                ok(signature),
            ),
        ]);
        assert!(transport::fetch_manifest(&fixture, &config(), &ACTIVE).is_err());
    }
}

#[test]
fn corrupt_truncated_and_oversized_payloads_never_pass_verification() {
    let verified = config().verify(&signed_fixture(), NOW, true).unwrap();
    let artifact = &verified.data.artifacts[0];
    for payload in [
        b"corrupt payload".to_vec(),
        PAYLOAD[..PAYLOAD.len() - 1].to_vec(),
        [PAYLOAD, b"more"].concat(),
    ] {
        let mut reply = ok(payload);
        reply.length = None;
        let fixture = FixtureTransport::new([(
            "https://updates.example.invalid/alas-launcher-linux-x86_64",
            reply,
        )]);
        let root = TempDir::new().unwrap();
        let path = root.path().join("payload");
        assert!(transport::download_payload(
            &fixture,
            &config(),
            artifact,
            &path,
            &mut |_| {},
            &ACTIVE
        )
        .is_err());
        assert!(fs::metadata(path).unwrap().len() <= artifact.size);
    }
    let root = TempDir::new().unwrap();
    let path = root.path().join("payload");
    let fixture = FixtureTransport::new([(
        "https://updates.example.invalid/alas-launcher-linux-x86_64",
        ok(PAYLOAD),
    )]);
    transport::download_payload(&fixture, &config(), artifact, &path, &mut |_| {}, &ACTIVE)
        .unwrap();
    transport::verify_payload(
        &mut install::open_regular(&path).unwrap(),
        artifact,
        &ACTIVE,
    )
    .unwrap();
}

#[test]
fn cancellation_before_entry_or_request_does_not_start_work() {
    let cancelled = AtomicBool::new(true);
    assert_eq!(
        check_and_restart(&cancelled, |_| panic!("cancelled update reported progress")).unwrap(),
        Outcome::Cancelled
    );
    let fixture = FixtureTransport::new([]);
    let error = transport::fetch_manifest(&fixture, &config(), &cancelled).unwrap_err();
    assert!(error.downcast_ref::<Cancelled>().is_some());
    assert_eq!(fixture.count(), 0);
}

struct CancelAfterRequest<'a> {
    fixture: FixtureTransport,
    cancelled: &'a AtomicBool,
}
impl Transport for CancelAfterRequest<'_> {
    fn get(&self, url: &Url, payload: bool) -> Result<Reply> {
        let response = self.fixture.get(url, payload);
        self.cancelled.store(true, Ordering::SeqCst);
        response
    }
}

#[test]
fn cancellation_after_a_request_prevents_redirects_and_unavailable_fallback() {
    for response in [
        redirect("/next"),
        Reply {
            status: 503,
            ..ok([])
        },
    ] {
        let cancelled = AtomicBool::new(false);
        let transport = CancelAfterRequest {
            fixture: FixtureTransport::new([(SOURCE, response)]),
            cancelled: &cancelled,
        };
        let error = transport::fetch_manifest(&transport, &config(), &cancelled).unwrap_err();
        assert!(error.downcast_ref::<Cancelled>().is_some());
        assert!(error.downcast_ref::<transport::Unavailable>().is_none());
        assert_eq!(transport.fixture.count(), 1);
        let root = TempDir::new().unwrap();
        assert!(!can_continue_offline(&error, "2.2.4", root.path()));
    }
}

struct CancelOnRead {
    reader: Cursor<Vec<u8>>,
    cancelled: Arc<AtomicBool>,
}
impl Read for CancelOnRead {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let result = self.reader.read(buffer);
        self.cancelled.store(true, Ordering::SeqCst);
        result
    }
}

#[test]
fn cancellation_during_metadata_and_payload_reads_discards_the_received_bytes() {
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut response = ok(signed_fixture().manifest.clone());
    response.body = Box::new(CancelOnRead {
        reader: Cursor::new(signed_fixture().manifest),
        cancelled: cancelled.clone(),
    });
    let fixture = FixtureTransport::new([(SOURCE, response)]);
    let error = transport::fetch_manifest(&fixture, &config(), &cancelled).unwrap_err();
    assert!(error.downcast_ref::<Cancelled>().is_some());
    assert_eq!(
        fixture.count(),
        1,
        "signature must not be fetched after cancellation"
    );

    cancelled.store(false, Ordering::SeqCst);
    let verified = config().verify(&signed_fixture(), NOW, true).unwrap();
    let mut response = ok(PAYLOAD);
    response.body = Box::new(CancelOnRead {
        reader: Cursor::new(PAYLOAD.to_vec()),
        cancelled: cancelled.clone(),
    });
    let fixture = FixtureTransport::new([(
        "https://updates.example.invalid/alas-launcher-linux-x86_64",
        response,
    )]);
    let root = TempDir::new().unwrap();
    let path = root.path().join("payload");
    let error = transport::download_payload(
        &fixture,
        &config(),
        &verified.data.artifacts[0],
        &path,
        &mut |_| {},
        &cancelled,
    )
    .unwrap_err();
    assert!(error.downcast_ref::<Cancelled>().is_some());
    assert_eq!(fs::metadata(path).unwrap().len(), 0);
}

#[test]
fn cancellation_after_download_and_verification_never_promotes_the_payload() {
    let root = TempDir::new().unwrap();
    let installation = installation(root.path());
    let target = root.path().join(if cfg!(windows) {
        "launcher.exe"
    } else {
        "launcher"
    });
    let staging = StagingDir::create(root.path()).unwrap();
    staging.write_snapshot(&fresh_fixture(42, "3.0.0")).unwrap();
    fs::write(staging.payload_path(), PAYLOAD).unwrap();
    // 即使此前已通过全部验签与摘要检查，取消仍须在实际替换入口阻止安装。
    installation
        .verify_before_replace(&config(), &staging, &ACTIVE)
        .unwrap();
    let cancelled = AtomicBool::new(true);
    let error = installation
        .promote_verified(&config(), &staging, &cancelled)
        .unwrap_err();
    assert!(error.downcast_ref::<Cancelled>().is_some());
    assert_eq!(fs::read(&target).unwrap(), b"original launcher fixture");
    assert_eq!(fs::read(staging.payload_path()).unwrap(), PAYLOAD);
    assert!(installation.lock(&cancelled).is_err());
}

fn installation(root: &Path) -> Installation {
    let target = root.join(if cfg!(windows) {
        "launcher.exe"
    } else {
        "launcher"
    });
    fs::write(&target, b"original launcher fixture").unwrap();
    Installation::fixture(&target).unwrap()
}

#[test]
fn installation_lock_serializes_concurrent_updates() {
    let root = TempDir::new().unwrap();
    let installation = installation(root.path());
    let guard = installation.lock(&ACTIVE).unwrap();
    assert_eq!(
        installation.lock(&ACTIVE).unwrap_err().to_string(),
        "another launcher update is already running",
        "Windows sharing flags must allow a second open to reach the kernel lock"
    );
    drop(guard);
    installation.lock(&ACTIVE).unwrap();
}

#[test]
fn staging_uses_unique_random_names_and_cleans_files_on_drop() {
    let root = TempDir::new().unwrap();
    let first = StagingDir::create(root.path()).unwrap();
    let second = StagingDir::create(root.path()).unwrap();
    assert_ne!(first.path, second.path);
    let name = first.path.file_name().unwrap().to_str().unwrap();
    let nonce = name.strip_prefix(".azurpilot-update-").unwrap();
    assert_eq!(nonce.len(), 32);
    assert!(nonce.bytes().all(|byte| byte.is_ascii_hexdigit()));
    let path = first.path.clone();
    first.write_snapshot(&signed_fixture()).unwrap();
    let _file = install::create_new_file(&first.payload_path()).unwrap();
    drop(_file);
    drop(first);
    assert!(!path.exists());
}

#[cfg(unix)]
#[test]
fn verified_promotion_is_atomic_and_rejects_invalid_metadata_without_touching_target() {
    use std::os::unix::fs::PermissionsExt;
    let root = TempDir::new().unwrap();
    let installation = installation(root.path());
    let target = root.path().join("launcher");
    let staging = StagingDir::create(root.path()).unwrap();
    let mut invalid = fresh_fixture(42, "3.0.0");
    invalid.signature[0] ^= 1;
    staging.write_snapshot(&invalid).unwrap();
    fs::write(staging.payload_path(), PAYLOAD).unwrap();
    assert!(installation
        .promote_verified(&config(), &staging, &ACTIVE)
        .is_err());
    assert_eq!(fs::read(&target).unwrap(), b"original launcher fixture");
    fs::remove_file(staging.snapshot_path()).unwrap();
    staging.write_snapshot(&fresh_fixture(42, "3.0.0")).unwrap();
    installation
        .promote_verified(&config(), &staging, &ACTIVE)
        .unwrap();
    assert_eq!(fs::read(&target).unwrap(), PAYLOAD);
    assert_eq!(
        fs::metadata(target).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert!(!staging.payload_path().exists());
}

#[test]
fn persistent_high_water_rejects_sequence_version_and_same_sequence_equivocation() {
    let root = TempDir::new().unwrap();
    let installation = installation(root.path());
    let config = config();
    let first = config
        .verify(&fresh_fixture(10, "4.0.0"), unix_now().unwrap(), true)
        .unwrap();
    installation
        .accept_manifest(&config, &first, &ACTIVE)
        .unwrap();
    installation
        .accept_manifest(&config, &first, &ACTIVE)
        .unwrap();
    for (sequence, version) in [(9, "5.0.0"), (11, "3.0.0"), (10, "4.1.0")] {
        let next = config
            .verify(&fresh_fixture(sequence, version), unix_now().unwrap(), true)
            .unwrap();
        assert!(
            installation
                .accept_manifest(&config, &next, &ACTIVE)
                .is_err(),
            "{sequence} {version}"
        );
    }
    let next = config
        .verify(&fresh_fixture(11, "4.1.0"), unix_now().unwrap(), true)
        .unwrap();
    installation
        .accept_manifest(&config, &next, &ACTIVE)
        .unwrap();
    let state = fs::read_dir(root.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "state"))
        .unwrap();
    let mut bytes = fs::read(&state).unwrap();
    bytes[10] ^= 1;
    fs::write(state, bytes).unwrap();
    assert!(installation
        .accept_manifest(&config, &next, &ACTIVE)
        .is_err());
}

#[test]
fn final_installation_check_rejects_post_download_changes_and_wrong_helper_targets() {
    let root = TempDir::new().unwrap();
    let installation = installation(root.path());
    let staging = StagingDir::create(root.path()).unwrap();
    staging.write_snapshot(&fresh_fixture(42, "3.0.0")).unwrap();
    fs::write(staging.payload_path(), PAYLOAD).unwrap();
    installation
        .verify_before_replace(&config(), &staging, &ACTIVE)
        .unwrap();
    fs::write(staging.payload_path(), b"replaced after download").unwrap();
    assert!(installation
        .verify_before_replace(&config(), &staging, &ACTIVE)
        .is_err());
    fs::write(staging.payload_path(), PAYLOAD).unwrap();
    let target = root.path().join(if cfg!(windows) {
        "launcher.exe"
    } else {
        "launcher"
    });
    let helper = root.path().join("helper.exe");
    fs::write(&helper, b"unrelated executable").unwrap();
    assert!(Installation::for_helper(&target, &helper, &ACTIVE).is_err());
    fs::write(&helper, b"original launcher fixture").unwrap();
    Installation::for_helper(&target, &helper, &ACTIVE).unwrap();
    fs::write(&target, b"changed target").unwrap();
    assert!(installation
        .verify_before_replace(&config(), &staging, &ACTIVE)
        .is_err());
}

#[cfg(unix)]
#[test]
fn symlinks_hardlinks_and_public_staging_permissions_are_rejected() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let root = TempDir::new().unwrap();
    let installation = installation(root.path());
    let staging = StagingDir::create(root.path()).unwrap();
    assert_eq!(
        fs::metadata(&staging.path).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let file = root.path().join("file");
    fs::write(&file, PAYLOAD).unwrap();
    let link = root.path().join("link");
    symlink(&file, &link).unwrap();
    assert!(install::open_regular(&link).is_err());
    fs::hard_link(&file, root.path().join("hardlink")).unwrap();
    assert!(install::open_regular(&file).is_err());
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o777)).unwrap();
    assert!(Installation::fixture(&root.path().join("launcher")).is_err());
    drop(installation);
}

#[cfg(unix)]
#[test]
fn a_private_installation_under_a_replaceable_shared_ancestor_is_rejected() {
    use std::os::unix::fs::PermissionsExt;
    let root = TempDir::new().unwrap();
    let shared = root.path().join("shared");
    let private = shared.join("private");
    fs::create_dir_all(&private).unwrap();
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
    let target = private.join("launcher");
    fs::write(&target, b"original launcher fixture").unwrap();
    Installation::fixture(&target).unwrap();
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(Installation::fixture(&target).is_err());
    assert!(StagingDir::create(&private).is_err());
    // Sticky 共享祖先保护当前用户拥有的下一层目录，和系统 /tmp 一致。
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o1777)).unwrap();
    Installation::fixture(&target).unwrap();
    StagingDir::create(&private).unwrap();
}

#[test]
fn only_metadata_unavailability_can_allow_a_complete_offline_installation() {
    let root = TempDir::new().unwrap();
    let unavailable = anyhow::Error::new(transport::Unavailable("fixture offline".to_owned()));
    assert!(!can_continue_offline(&unavailable, "2.2.4", root.path()));
    fs::write(root.path().join("gui.py"), b"fixture").unwrap();
    let python = root.path().join(if cfg!(windows) {
        ".venv/Scripts/python.exe"
    } else {
        ".venv/bin/python"
    });
    fs::create_dir_all(python.parent().unwrap()).unwrap();
    fs::write(python, b"fixture").unwrap();
    assert!(can_continue_offline(&unavailable, "2.2.4", root.path()));
    assert!(!can_continue_offline(&unavailable, "0.0.1", root.path()));
    assert!(!can_continue_offline(
        &anyhow::anyhow!("invalid signature"),
        "2.2.4",
        root.path()
    ));
}

#[test]
fn semver_precedence_and_compiled_minimum_are_enforced() {
    assert!(manifest::parse_version("3.0.0")
        .unwrap()
        .cmp_precedence(&manifest::parse_version("3.0.0-rc.10").unwrap())
        .is_gt());
    assert!(manifest::parse_version("3.0.0-rc.10")
        .unwrap()
        .cmp_precedence(&manifest::parse_version("3.0.0-rc.2").unwrap())
        .is_gt());
    assert!(manifest::parse_version("3.0.0+build.1")
        .unwrap()
        .cmp_precedence(&manifest::parse_version("3.0.0+build.2").unwrap())
        .is_eq());
    let config = Config::from_values(Some(SOURCE), Some(PUBLIC_KEY), None, None, Some("43"))
        .unwrap()
        .unwrap();
    assert!(config.verify(&signed_fixture(), NOW, true).is_err());
}
