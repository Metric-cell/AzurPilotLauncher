#!/usr/bin/env python3
"""Sign raw launcher artifacts using an existing external Ed25519 PEM key.

Requires Python 3.10+ and OpenSSL 3; never generates keys or contacts a server.
Only the exact JSON bytes and detached signature produced here are published.
"""

import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import tempfile
import time
from urllib.parse import quote, urlsplit


DOMAIN = b"AzurPilot launcher update manifest v1\0"
PRODUCT = "moe.taiho.alas-launcher"
MAX_MANIFEST_BYTES = 64 * 1024
MAX_PAYLOAD_BYTES = 512 * 1024 * 1024
MAX_LIFETIME = 7 * 24 * 60 * 60
PLATFORMS = {
    "windows-x86_64", "windows-aarch64", "linux-x86_64", "linux-aarch64",
    "darwin-x86_64", "darwin-aarch64",
}
ED25519_SPKI_PREFIX = bytes.fromhex("302a300506032b6570032100")
SEMVER = re.compile(
    r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
    r"(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?"
    r"(?:\+([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?\Z"
)


def secure_url(value):
    parsed = urlsplit(value)
    if (parsed.scheme != "https" or not parsed.hostname or parsed.username is not None
            or parsed.password is not None or parsed.fragment or parsed.port not in (None, 443)
            or "[" in parsed.netloc or re.fullmatch(r"[0-9.]+", parsed.hostname)):
        raise ValueError("URLs must use credential-free HTTPS with a DNS host and port 443")
    # URL normalization must be identical to url::Url in the verifier. Keep release
    # configuration deliberately ASCII and already canonical instead of guessing.
    expected_netloc = parsed.hostname.lower()
    if parsed.netloc != expected_netloc or "\\" in value or any(ord(c) <= 32 or ord(c) > 126 for c in value):
        raise ValueError("URLs must be canonical ASCII HTTPS URLs without an explicit port")
    return parsed


def origin(value):
    parsed = secure_url(value)
    return f"https://{parsed.hostname}"


def run_openssl(*args):
    result = subprocess.run(["openssl", *map(str, args)], check=False, capture_output=True)
    if result.returncode:
        # OpenSSL errors contain paths, never print PEM material or command input.
        raise ValueError("OpenSSL operation failed: " + result.stderr.decode("utf-8", "replace").strip())
    return result.stdout


def validate_version(value):
    match = SEMVER.fullmatch(value)
    if len(value) > 128 or not match or any(int(part) > 2**64 - 1 for part in match.groups()[:3]):
        raise ValueError("version must be a valid SemVer")
    if match.group(4):
        for identifier in match.group(4).split("."):
            if identifier.isdigit() and len(identifier) > 1 and identifier.startswith("0"):
                raise ValueError("numeric SemVer prerelease identifiers must not have leading zeros")


def build_manifest(args):
    validate_version(args.version)
    source = secure_url(args.source)
    if source.query or not source.path or source.path.endswith("/"):
        raise ValueError("source must name the exact manifest file without a query")
    if not re.fullmatch(r"[A-Za-z0-9-]{1,32}", args.channel):
        raise ValueError("channel must contain 1 to 32 ASCII letters, digits or hyphens")
    if not 0 < args.sequence < 2**64:
        raise ValueError("sequence must be a positive u64 and increase for every publication")
    if not (0 <= args.issued_at < args.expires_at < 2**64) or args.expires_at - args.issued_at > MAX_LIFETIME:
        raise ValueError("validity interval must be positive and no longer than seven days")
    base = secure_url(args.payload_base_url)
    if base.query or not base.path.endswith("/"):
        raise ValueError("payload-base-url must end with '/' and have no query")
    trusted = {origin(args.source)}
    for value in args.allowed_origin:
        parsed = secure_url(value)
        if parsed.path not in ("", "/") or parsed.query:
            raise ValueError("allowed-origin must contain only an HTTPS origin")
        trusted.add(origin(value))
    if origin(args.payload_base_url) not in trusted:
        raise ValueError("payload-base-url origin is not in the compiled origin allowlist")
    root = args.payload_dir.resolve(strict=True)
    if not root.is_dir():
        raise ValueError("payload-dir must be a directory")
    artifacts = []
    found = set()
    for path in sorted(root.glob("alas-launcher-*")):
        if path.is_symlink() or not path.is_file():
            raise ValueError("launcher artifacts must be regular files without symlinks")
        platform = path.name.removeprefix("alas-launcher-")
        if platform.endswith(".exe"):
            platform = platform[:-4]
        if platform not in PLATFORMS or platform in found:
            raise ValueError(f"unsupported or duplicate platform artifact: {path.name}")
        if platform.startswith("windows-") != path.name.endswith(".exe"):
            raise ValueError("Windows raw launcher artifacts must have an .exe suffix")
        found.add(platform)
        size = path.stat().st_size
        if not 0 < size <= MAX_PAYLOAD_BYTES:
            raise ValueError("launcher artifact is empty or larger than 512 MiB")
        hasher = hashlib.sha256()
        with path.open("rb") as stream:
            while chunk := stream.read(1024 * 1024):
                hasher.update(chunk)
        artifacts.append({"platform": platform, "url": args.payload_base_url + quote(path.name, safe=""),
                          "sha256": hasher.hexdigest(), "size": size})
    if not artifacts:
        raise ValueError("payload-dir has no supported raw launcher artifacts")
    manifest = {"schema": 1, "product": PRODUCT, "channel": args.channel, "source": args.source,
                "sequence": args.sequence, "version": args.version, "issued_at": args.issued_at,
                "expires_at": args.expires_at, "artifacts": artifacts}
    raw = json.dumps(manifest, ensure_ascii=True, separators=(",", ":"), sort_keys=True).encode("ascii") + b"\n"
    if len(raw) > MAX_MANIFEST_BYTES:
        raise ValueError("manifest is larger than 64 KiB")
    return raw


def sign(args):
    raw = build_manifest(args)
    der = run_openssl("pkey", "-in", args.key, "-pubout", "-outform", "DER")
    if not der.startswith(ED25519_SPKI_PREFIX) or len(der) != len(ED25519_SPKI_PREFIX) + 32:
        raise ValueError("signing key must be Ed25519")
    public_key = der[-32:].hex()
    if args.public_key and (not re.fullmatch(r"[0-9a-fA-F]{64}", args.public_key) or public_key != args.public_key.lower()):
        raise ValueError("signing key does not match the compiled public key")
    output = args.output.absolute()
    signature_path = Path(str(output) + ".sig")
    if output.exists() or output.is_symlink() or signature_path.exists() or signature_path.is_symlink():
        raise ValueError("output manifest or signature already exists; publish to a fresh directory")
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="azurpilot-update-sign-") as scratch:
        message = Path(scratch) / "message.bin"
        message.write_bytes(DOMAIN + raw)
        signature = run_openssl("pkeyutl", "-sign", "-rawin", "-inkey", args.key, "-in", message)
    if len(signature) != 64:
        raise ValueError("OpenSSL returned an incorrect Ed25519 signature length")
    # x mode refuses accidental overwrite; only public release data is written.
    try:
        with output.open("xb") as stream:
            stream.write(raw)
        with signature_path.open("xb") as stream:
            stream.write(signature)
    except Exception:
        output.unlink(missing_ok=True)
        raise
    print(f"Signed {output.name} and {signature_path.name}; public key {public_key}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--payload-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True, help="JSON file; signature is written to FILE.sig")
    parser.add_argument("--source", required=True, help="exact compiled AZURPILOT_UPDATE_MANIFEST_URL")
    parser.add_argument("--payload-base-url", required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--channel", default="stable")
    parser.add_argument("--sequence", type=int, required=True)
    parser.add_argument("--issued-at", type=int, default=int(time.time()))
    parser.add_argument("--expires-at", type=int, required=True)
    parser.add_argument("--allowed-origin", action="append", default=[])
    parser.add_argument("--key", type=Path, required=True, help="existing external Ed25519 PEM key; never generated")
    parser.add_argument("--public-key", help="expected 64-character hex Ed25519 public key")
    args = parser.parse_args()
    try:
        sign(args)
    except (OSError, ValueError) as error:
        parser.exit(1, f"Signing failed: {error}\n")


if __name__ == "__main__":
    main()
