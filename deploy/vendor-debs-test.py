#!/usr/bin/env python3
import hashlib
import importlib.util
import pathlib
import tempfile
import unittest
import urllib.error
from unittest import mock


MODULE_PATH = pathlib.Path(__file__).with_name("vendor-debs.py")
SPEC = importlib.util.spec_from_file_location("vendor_debs", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
VENDOR_DEBS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(VENDOR_DEBS)


class VendorDebsPlatformTest(unittest.TestCase):
    def test_fetch_retries_transient_failures(self):
        response = mock.MagicMock()
        response.__enter__.return_value.read.return_value = b"downloaded"
        failures = [
            urllib.error.URLError("timeout-1"),
            urllib.error.URLError("timeout-2"),
            response,
        ]

        with mock.patch("urllib.request.urlopen", side_effect=failures) as urlopen, mock.patch.object(
            VENDOR_DEBS.time, "sleep"
        ):
            self.assertEqual(
                VENDOR_DEBS.fetch("https://example.invalid/a.deb"), b"downloaded"
            )
        self.assertEqual(urlopen.call_count, 3)

    def test_platforms_use_isolated_output_directories(self):
        amd64 = VENDOR_DEBS.resolve_target("linux/amd64")
        arm64 = VENDOR_DEBS.resolve_target("linux/arm64")

        self.assertEqual((amd64.deb_arch, amd64.output_dirname), ("amd64", "vendor-debs"))
        self.assertEqual((arm64.deb_arch, arm64.output_dirname), ("arm64", "vendor-debs-arm64"))
        self.assertNotEqual(amd64.output_dirname, arm64.output_dirname)

    def test_arm64_archive_names_and_manifest_are_arch_bound(self):
        payload = b"arm64 deb fixture"
        digest = hashlib.sha256(payload).hexdigest()
        stanza = {
            "Package": ["curl"],
            "Version": ["1.0"],
            "Filename": ["pool/c/curl/curl_1.0_arm64.deb"],
            "SHA256": [digest],
            "Size": [str(len(payload))],
        }
        target = VENDOR_DEBS.resolve_target("linux/arm64")

        with tempfile.TemporaryDirectory() as tmp:
            output = pathlib.Path(tmp) / "vendor"
            with mock.patch.object(VENDOR_DEBS, "fetch", return_value=payload):
                VENDOR_DEBS.build_vendor({"curl": ("1.0", stanza)}, target, str(output))

            archives = sorted(path.name for path in output.glob("*.deb"))
            self.assertEqual(archives, ["curl_1.0_arm64.deb"])
            manifest = (output / "manifest.sha256").read_text(encoding="utf-8")
            self.assertEqual(manifest, f"{digest}  curl_1.0_arm64.deb\n")

    def test_debian_epoch_colon_is_sanitized_for_windows_checkouts(self):
        # R3-WINDOWS-001: Debian epoch separators (':') in package versions
        # must never reach tracked file names — colon-bearing paths fail
        # actions/checkout on Windows with "invalid path".
        payload = b"epoch deb fixture"
        digest = hashlib.sha256(payload).hexdigest()
        stanza = {
            "Package": ["libgmp10"],
            "Version": ["2:6.3.0+dfsg-3"],
            "Filename": ["pool/g/gmp/libgmp10_2:6.3.0+dfsg-3_amd64.deb"],
            "SHA256": [digest],
            "Size": [str(len(payload))],
        }
        target = VENDOR_DEBS.resolve_target("linux/amd64")

        with tempfile.TemporaryDirectory() as tmp:
            output = pathlib.Path(tmp) / "vendor"
            with mock.patch.object(VENDOR_DEBS, "fetch", return_value=payload):
                VENDOR_DEBS.build_vendor(
                    {"libgmp10": ("2:6.3.0+dfsg-3", stanza)}, target, str(output)
                )

            archives = sorted(path.name for path in output.glob("*.deb"))
            self.assertEqual(archives, ["libgmp10_2_6.3.0+dfsg-3_amd64.deb"])
            for name in archives:
                self.assertNotRegex(name, VENDOR_DEBS.WINDOWS_INVALID_CHARS)
            manifest = (output / "manifest.sha256").read_text(encoding="utf-8")
            self.assertEqual(
                manifest,
                f"{digest}  libgmp10_2_6.3.0+dfsg-3_amd64.deb\n",
            )

    def test_unknown_platform_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "unsupported platform"):
            VENDOR_DEBS.resolve_target("linux/s390x")


if __name__ == "__main__":
    unittest.main()
