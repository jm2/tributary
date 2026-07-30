#!/usr/bin/env python3
"""Focused unit tests for the dependency-update repair helpers."""

from __future__ import annotations

import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

import sync_fuzz_lock
import sync_rust_toolchain


def lock(direct: list[str], versions: dict[str, list[str]]) -> dict:
    packages = [
        {
            "name": "tributary",
            "version": "0.5.1",
            "dependencies": direct,
        }
    ]
    for name, releases in versions.items():
        packages.extend(
            {
                "name": name,
                "version": release,
                "source": "registry+https://github.com/rust-lang/crates.io-index",
            }
            for release in releases
        )
    return {"version": 4, "package": packages}


class FuzzLockPolicyTests(unittest.TestCase):
    def test_changed_shared_production_dependency_requires_exact_fuzz_version(self):
        base = lock(["tokio"], {"tokio": ["1.52.0"]})
        current = lock(["tokio"], {"tokio": ["1.53.0"]})
        fuzz = lock(["tokio"], {"tokio": ["1.52.0"]})
        manifest = {"dependencies": {"tokio": "1"}}

        self.assertEqual(
            sync_fuzz_lock.required_transitions(base, current, fuzz, manifest),
            [sync_fuzz_lock.Transition("tokio", "1.52.0", "1.53.0")],
        )

    def test_changed_root_dev_dependency_is_not_inherited_by_fuzz(self):
        base = lock(["toml"], {"toml": ["1.1.2"]})
        current = lock(["toml"], {"toml": ["1.1.3"]})
        fuzz = lock([], {})
        manifest = {"dev-dependencies": {"toml": "1"}}

        self.assertEqual(
            sync_fuzz_lock.required_transitions(base, current, fuzz, manifest),
            [],
        )

    def test_alias_uses_the_actual_package_name(self):
        manifest = {
            "dependencies": {
                "gtk": {"version": "0.11", "package": "gtk4"},
                "tokio": {"version": "1"},
            },
            "target": {
                "cfg(target_os = 'windows')": {
                    "dependencies": {
                        "windows-sys": {"version": "0.61"},
                    }
                }
            },
        }
        self.assertEqual(
            sync_fuzz_lock.production_dependency_names(manifest),
            {"gtk4", "tokio", "windows-sys"},
        )

    def test_removed_production_dependency_fails_closed(self):
        base = lock(["tokio"], {"tokio": ["1.52.0"]})
        current = lock([], {})
        fuzz = lock(["tokio"], {"tokio": ["1.52.0"]})
        with self.assertRaises(sync_fuzz_lock.PolicyError):
            sync_fuzz_lock.required_transitions(base, current, fuzz, {})


class RustToolchainPolicyTests(unittest.TestCase):
    def test_candidate_requires_both_exact_action_refs_to_agree(self):
        source = """
        uses: dtolnay/rust-toolchain@1.93.0
        uses: dtolnay/rust-toolchain@stable
        uses: dtolnay/rust-toolchain@1.93.0
"""
        self.assertEqual(
            sync_rust_toolchain.candidate_from_actions(source),
            "1.93",
        )

    def test_mixed_exact_action_refs_fail_closed(self):
        source = """
        uses: dtolnay/rust-toolchain@1.92.0
        uses: dtolnay/rust-toolchain@1.93.0
"""
        with self.assertRaises(sync_rust_toolchain.PolicyError):
            sync_rust_toolchain.candidate_from_actions(source)

    def test_dependabot_action_proposal_synchronizes_every_contract(self):
        with TemporaryDirectory() as temporary:
            root = Path(temporary)
            manifest = root / "Cargo.toml"
            ci = root / "ci.yml"
            readme = root / "README.md"
            manifest.write_text(
                '[package]\nname = "fixture"\nversion = "0.0.0"\n'
                'rust-version = "1.92"\n'
            )
            ci.write_text(
                "# rustc 1.92 is the supported floor\n"
                "msrv:\n"
                "  name: MSRV (1.92)\n"
                "  steps:\n"
                "    - name: Install Rust toolchain (1.92)\n"
                "      uses: dtolnay/rust-toolchain@1.93.0\n"
                "coverage:\n"
                "  steps:\n"
                "    - name: Install coverage toolchain\n"
                "      uses: dtolnay/rust-toolchain@1.93.0\n"
                "  key: coverage-1.92.0-llvm-cov-fixture\n"
            )
            readme.write_text(
                "Rust 1.92+\n"
                "rustup toolchain install 1.92.0\n"
                "cargo +1.92.0 llvm-cov\n"
                "coverage is pinned to Rust 1.92.0\n"
            )

            original = (
                sync_rust_toolchain.MANIFEST,
                sync_rust_toolchain.CI,
                sync_rust_toolchain.README,
            )
            try:
                sync_rust_toolchain.MANIFEST = manifest
                sync_rust_toolchain.CI = ci
                sync_rust_toolchain.README = readme
                target = sync_rust_toolchain.candidate_from_actions(ci.read_text())
                sync_rust_toolchain.synchronize(target)
                sync_rust_toolchain.check_consistency()
            finally:
                (
                    sync_rust_toolchain.MANIFEST,
                    sync_rust_toolchain.CI,
                    sync_rust_toolchain.README,
                ) = original

            self.assertIn('rust-version = "1.93"', manifest.read_text())
            self.assertNotIn("1.92", ci.read_text() + readme.read_text())


if __name__ == "__main__":
    unittest.main()
