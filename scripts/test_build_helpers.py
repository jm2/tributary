#!/usr/bin/env python3
"""Exercise build-helper routing without compiling or launching the desktop."""

import itertools
import json
import os
import shutil
import subprocess  # nosec B404 -- checked-in scripts run against local fixtures
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory


REPOSITORY = Path(__file__).resolve().parent.parent
FAKE_TOOL = r'''#!/usr/bin/env python3
import json
import os
import shutil
import sys
from pathlib import Path

tool = Path(sys.argv[0]).name
arguments = sys.argv[1:]
with Path(os.environ["HELPER_CALLS"]).open("a") as calls:
    calls.write(json.dumps([tool, str(Path.cwd()), arguments]) + "\n")
if tool == "rustc":
    print("host: " + os.environ["HELPER_HOST"])
elif tool == "brew" and arguments == ["--prefix"]:
    print(os.environ["HELPER_BREW_PREFIX"])
elif tool == "cargo" and arguments[0] == "build":
    status = int(os.environ.get("HELPER_BUILD_STATUS", "0"))
    if status:
        sys.exit(status)
    target_dir = Path(arguments[arguments.index("--target-dir") + 1]) if "--target-dir" in arguments else Path("target")
    if "--target" in arguments:
        target_dir /= arguments[arguments.index("--target") + 1]
    binary = target_dir / "release" / "tributary"
    binary.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(__file__, binary)
    binary.chmod(0o755)
elif tool == "artifact-policy":
    sys.exit(int(os.environ.get("HELPER_POLICY_STATUS", "0")))
elif tool == "tributary":
    print("fixture desktop stdout")
    print("fixture desktop stderr", file=sys.stderr)
    sys.exit(int(os.environ.get("HELPER_APP_STATUS", "0")))
'''


class BuildHelperTests(unittest.TestCase):
    """Check side-effect ordering, native artifact selection, and launch exits."""

    def setUp(self):
        """Copy the real helpers into a repository with synthetic build tools."""
        temporary = TemporaryDirectory(prefix="tributary-build-helpers-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve() / "Repository With Spaces"
        self.scripts = self.root / "scripts"
        self.scripts.mkdir(parents=True)
        for platform in ("linux", "macos"):
            name = f"build-{platform}.sh"
            shutil.copyfile(REPOSITORY / "scripts" / name, self.scripts / name)
        self.calls = self.root / "calls.jsonl"
        self.bin = self.root / "tools"
        self.bin.mkdir()
        for name in ("cargo", "rustc", "brew", "pkg-config", "readelf", "artifact-policy"):
            self.write_executable(self.bin / name, FAKE_TOOL)
        for name, tool in (
            ("validate-package-compliance.sh", "artifact-policy"),
            ("validate-package-metadata.sh", "metadata-policy"),
        ):
            path = self.root / "build-aux" / "linux" / name
            path.parent.mkdir(parents=True, exist_ok=True)
            self.write_executable(path, f'#!/bin/sh\nexec "{self.bin / tool}" "$@"\n')
        self.write_executable(self.bin / "metadata-policy", FAKE_TOOL)
        (self.scripts / "macos-package-policy.sh").write_text(
            'macos_package_policy_load() { return 0; }\n'
            'macos_validate_macho_copy_control() {\n'
            '  MACOS_PACKAGE_POLICY_REASON="fixture policy rejection"\n'
            '  artifact-policy "$@"\n'
            '}\n'
        )
        (self.scripts / "macos-icon-bundle-policy.sh").write_text("")
        (self.root / "Cargo.toml").write_text('[package]\nversion = "0.6.2"\n')
        self.dist = self.root / "dist"
        self.dist.mkdir()
        (self.dist / "existing-package").write_text("preserve me")
        self.environment = {
            **os.environ,
            "PATH": f"{self.bin}{os.pathsep}{os.environ['PATH']}",
            "HELPER_CALLS": str(self.calls),
            "HELPER_BREW_PREFIX": str(self.root / "Homebrew With Spaces"),
            "CARGO_TARGET_DIR": str(self.root / "unexpected-target"),
            "CARGO_BUILD_TARGET": "wasm32-unknown-unknown",
            "CDPATH": str(self.root.parent),
        }

    @staticmethod
    def write_executable(path, contents):
        """Create a local tool fixture that can be found through PATH."""
        path.write_text(contents)
        path.chmod(0o755)

    def run_helper(self, platform, *arguments, **overrides):
        """Run from outside the checkout and capture output plus tool calls."""
        self.calls.write_text("")
        host = "aarch64-apple-darwin" if platform == "macos" else "x86_64-unknown-linux-gnu"
        # Both supported platforms provide /bin/bash. Execute the checked-in
        # helper copy with separate arguments; PATH selects fixture build tools.
        result = subprocess.run(  # nosec B603 -- local fixtures, no shell expansion
            ["/bin/bash", str(self.scripts / f"build-{platform}.sh"), *arguments],
            cwd=self.root.parent,
            env={**self.environment, "HELPER_HOST": host, **overrides},
            capture_output=True,
            text=True,
            shell=False,
            timeout=20,
            check=False,
        )
        calls = [json.loads(line) for line in self.calls.read_text().splitlines()]
        return result, calls

    def test_conflicts_fail_before_tool_invocation(self):
        """Reject every run conflict and ambiguous quick mode without setup."""
        quick = ("--fmt", "--check", "--clippy", "--coverage")
        for platform in ("linux", "macos"):
            packages = ("--dmg",) if platform == "macos" else ("--flatpak", "--deb", "--rpm", "--arch-pkg")
            conflicts = [("--run", flag) for flag in (*quick, *packages)]
            conflicts += list(itertools.combinations(quick, 2))
            conflicts += list(itertools.product(quick, packages))
            for flags in conflicts:
                for arguments in (flags, flags[::-1]):
                    with self.subTest(platform=platform, arguments=arguments):
                        result, calls = self.run_helper(platform, *arguments)
                        self.assertEqual(result.returncode, 2, result.stderr)
                        self.assertEqual(calls, [])

    def test_help_and_format_need_no_desktop_tools(self):
        """Help is inert; formatting invokes only Cargo in the repository."""
        for platform in ("linux", "macos"):
            with self.subTest(platform=platform):
                result, calls = self.run_helper(platform, "--help")
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("--run", result.stdout)
                self.assertEqual(calls, [])
                result, calls = self.run_helper(platform, "--fmt")
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(calls, [["cargo", str(self.root), ["fmt"]]])

    def test_run_builds_validates_then_launches_native_artifact(self):
        """Use the locked native build, preserve terminal output and app status."""
        for platform in ("linux", "macos"):
            with self.subTest(platform=platform):
                result, calls = self.run_helper(platform, "--run", HELPER_APP_STATUS="23")
                self.assertEqual(result.returncode, 23, result.stderr)
                self.assertIn("fixture desktop stdout", result.stdout)
                self.assertIn("fixture desktop stderr", result.stderr)
                names = [call[0] for call in calls]
                self.assertLess(names.index("cargo"), names.index("artifact-policy"))
                self.assertLess(names.index("artifact-policy"), names.index("tributary"))
                build = next(call[2] for call in calls if call[0] == "cargo")
                self.assertIn("--locked", build)
                target = build[build.index("--target") + 1]
                target_dir = build[build.index("--target-dir") + 1]
                self.assertEqual(target_dir, str(self.root / "target"))
                self.assertNotEqual(target, self.environment["CARGO_BUILD_TARGET"])
                binary = str(Path(target_dir) / target / "release" / "tributary")
                validation = next(call[2] for call in calls if call[0] == "artifact-policy")
                self.assertEqual(validation[-1], binary)
                self.assertTrue(all(call[1] == str(self.root) for call in calls))
                self.assertEqual(list(self.dist.iterdir()), [self.dist / "existing-package"])

    def test_failures_never_launch_an_existing_binary(self):
        """A failed rebuild or rejected artifact cannot launch stale output."""
        for platform in ("linux", "macos"):
            result, _ = self.run_helper(platform, "--run")
            self.assertEqual(result.returncode, 0, result.stderr)
            for failure in ("HELPER_BUILD_STATUS", "HELPER_POLICY_STATUS"):
                with self.subTest(platform=platform, failure=failure):
                    result, calls = self.run_helper(platform, "--run", **{failure: "17"})
                    self.assertNotEqual(result.returncode, 0)
                    self.assertNotIn("tributary", [call[0] for call in calls])

    def test_foreign_toolchain_is_rejected_before_build(self):
        """Refuse to execute an artifact built for an incompatible platform."""
        for platform in ("linux", "macos"):
            result, calls = self.run_helper(platform, "--run", HELPER_HOST="wasm32-unknown-unknown")
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual([call[0] for call in calls], ["rustc"])

    def test_linux_default_build_does_not_launch(self):
        """Keep the existing build-only behavior when --run is absent."""
        result, calls = self.run_helper("linux")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("tributary", [call[0] for call in calls])


if __name__ == "__main__":
    unittest.main()
