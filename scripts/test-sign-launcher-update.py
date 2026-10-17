#!/usr/bin/env python3
"""Offline signing-tool interoperability tests using the published RFC 8032 key."""

import base64
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / "src" / "update" / "fixtures"
SCRIPT = ROOT / "scripts" / "sign-launcher-update.py"
# RFC 8032 section 7.1 test 1; published test material, never a release secret.
RFC8032_SEED = bytes.fromhex("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60")
RFC8032_PUBLIC_KEY = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"


class SigningToolTests(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory(prefix="azurpilot-sign-test-")
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name)
        self.payloads = self.root / "payloads"
        self.payloads.mkdir()
        (self.payloads / "alas-launcher-linux-x86_64").write_bytes((FIXTURES / "payload.txt").read_bytes())
        # Encode an existing, public RFC test vector; no key generation is performed.
        encoded = base64.b64encode(bytes.fromhex("302e020100300506032b657004220420") + RFC8032_SEED).decode("ascii")
        self.key = self.root / "rfc8032-test-key.pem"
        self.key.write_text(f"-----BEGIN PRIVATE KEY-----\n{encoded}\n-----END PRIVATE KEY-----\n", encoding="ascii")
        self.key.chmod(0o600)
        self.output = self.root / "out" / "stable.json"
        self.args = [sys.executable, str(SCRIPT), "--payload-dir", str(self.payloads),
                     "--output", str(self.output), "--key", str(self.key), "--public-key", RFC8032_PUBLIC_KEY,
                     "--source", "https://updates.example.invalid/stable.json", "--payload-base-url", "https://updates.example.invalid/",
                     "--version", "3.0.0", "--channel", "stable", "--sequence", "42",
                     "--issued-at", "1799999940", "--expires-at", "1800003600"]

    def run_tool(self, *extra):
        return subprocess.run([*self.args, *extra], capture_output=True, check=False)

    def assert_rejected(self, *extra):
        result = self.run_tool(*extra)
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertFalse(self.output.exists())
        self.assertFalse(Path(str(self.output) + ".sig").exists())

    def test_rust_fixture_and_openssl_signer_produce_identical_bytes(self):
        result = self.run_tool()
        self.assertEqual(result.returncode, 0, result.stderr.decode())
        self.assertEqual(self.output.read_bytes(), (FIXTURES / "valid.json").read_bytes())
        signature = Path(str(self.output) + ".sig").read_bytes()
        self.assertEqual(len(signature), 64)
        self.assertEqual(signature, (FIXTURES / "valid.json.sig").read_bytes())

    def test_untrusted_origins_credentials_expiry_and_semver_are_rejected(self):
        for args in [("--source", "http://updates.example.invalid/stable.json"),
                     ("--source", "https://user:pass@updates.example.invalid/stable.json"),
                     ("--payload-base-url", "https://untrusted.example.invalid/"),
                     ("--expires-at", "1799999940"), ("--expires-at", "1800999940"),
                     ("--version", "3.0.0-01"), ("--sequence", "0"),
                     ("--public-key", "00" * 32)]:
            with self.subTest(args=args):
                self.assert_rejected(*args)

    def test_explicit_payload_origin_is_supported(self):
        result = self.run_tool("--payload-base-url", "https://cdn.example.invalid/", "--allowed-origin", "https://cdn.example.invalid")
        self.assertEqual(result.returncode, 0, result.stderr.decode())

    def test_existing_outputs_are_preserved(self):
        self.output.parent.mkdir()
        self.output.write_bytes(b"existing release")
        result = self.run_tool()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.output.read_bytes(), b"existing release")

    def test_invalid_artifact_sizes_and_duplicate_platforms_are_rejected(self):
        payload = self.payloads / "alas-launcher-linux-x86_64"
        payload.write_bytes(b"")
        self.assert_rejected()
        with payload.open("wb") as stream:
            stream.truncate(512 * 1024 * 1024 + 1)
        self.assert_rejected()
        payload.write_bytes(b"fixture")
        (self.payloads / "alas-launcher-linux-x86_64.exe").write_bytes(b"fixture")
        self.assert_rejected()


if __name__ == "__main__":
    unittest.main()
