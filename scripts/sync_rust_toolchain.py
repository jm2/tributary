#!/usr/bin/env python3
"""Validate or synchronize Tributary's authoritative Rust/MSRV declarations.

Dependabot proposes compiler updates through `.github/rust-toolchain.toml`.
Those proposals deliberately do not auto-merge. A trusted repairer runs this
script to coordinate Cargo.toml, CI, cache keys, and developer documentation
before the complete matrix decides whether the new compiler is feasible.

The two `dtolnay/rust-toolchain` action references are a separate supply-chain
boundary: they remain immutable commits from the action's permanent master
history and are never rewritten by a compiler-version synchronization.
"""

from __future__ import annotations

import argparse
import re
import sys
import tomllib
from pathlib import Path


REPOSITORY = Path(__file__).resolve().parents[1]
MANIFEST = REPOSITORY / "Cargo.toml"
TOOLCHAIN_MANIFEST = REPOSITORY / ".github" / "rust-toolchain.toml"
CI = REPOSITORY / ".github" / "workflows" / "ci.yml"
README = REPOSITORY / "README.md"
LINE_VERSION = re.compile(r"^[1-9][0-9]*\.[0-9]+$")
RELEASE_VERSION = re.compile(r"^([1-9][0-9]*\.[0-9]+)\.0$")
EXACT_TOOLCHAIN_ACTION = re.compile(
    r"(?m)^\s*uses:\s*dtolnay/rust-toolchain@([0-9a-f]{40})\s+#\s*master\s*$"
)
EXACT_CI_TOOLCHAIN = re.compile(
    r'(?m)^(\s*toolchain:\s*)([1-9][0-9]*\.[0-9]+\.0)\s*$'
)
STABLE_MSRV_CHECK = re.compile(r"(?m)^    name: MSRV\s*$")


class PolicyError(RuntimeError):
    """The Rust version declarations are incomplete or contradictory."""


def manifest_line_version() -> str:
    with MANIFEST.open("rb") as source:
        manifest = tomllib.load(source)
    value = manifest.get("package", {}).get("rust-version")
    if not isinstance(value, str) or LINE_VERSION.fullmatch(value) is None:
        raise PolicyError(
            "Cargo.toml package.rust-version must be a canonical X.Y string"
        )
    return value


def candidate_from_toolchain(source: str | None = None) -> str:
    if source is None:
        with TOOLCHAIN_MANIFEST.open("rb") as manifest_source:
            manifest = tomllib.load(manifest_source)
    else:
        manifest = tomllib.loads(source)
    value = manifest.get("toolchain", {}).get("channel")
    if not isinstance(value, str):
        raise PolicyError("rust-toolchain.toml must define toolchain.channel")
    match = RELEASE_VERSION.fullmatch(value)
    if match is None:
        raise PolicyError(
            "rust-toolchain.toml toolchain.channel must be a canonical X.Y.0 release"
        )
    return match.group(1)


def exact_action_pins(ci_source: str) -> list[str]:
    pins = EXACT_TOOLCHAIN_ACTION.findall(ci_source)
    if len(pins) != 2:
        raise PolicyError(
            "CI must contain exactly two full-SHA dtolnay/rust-toolchain refs "
            f"from permanent master history (MSRV and coverage); found {len(pins)}"
        )
    if len(set(pins)) != 1:
        raise PolicyError(
            "MSRV and coverage must use the same immutable toolchain action commit"
        )
    return pins


def exact_ci_releases(ci_source: str) -> list[str]:
    releases = EXACT_CI_TOOLCHAIN.findall(ci_source)
    values = [release for _, release in releases]
    if len(values) != 2:
        raise PolicyError(
            "CI must contain exactly two explicit X.Y.0 toolchain inputs "
            f"(MSRV and coverage); found {len(values)}"
        )
    return values


def require_occurrence(source: str, needle: str, description: str) -> None:
    if needle not in source:
        raise PolicyError(f"{description} is not synchronized: missing {needle!r}")


def check_consistency() -> None:
    line = manifest_line_version()
    release = f"{line}.0"
    toolchain_line = candidate_from_toolchain()
    if toolchain_line != line:
        raise PolicyError(
            f"rust-toolchain.toml release {toolchain_line}.0 does not match "
            f"Cargo.toml rust-version {line}"
        )

    ci_source = CI.read_text()
    readme = README.read_text()
    exact_action_pins(ci_source)
    releases = exact_ci_releases(ci_source)
    if set(releases) != {release}:
        raise PolicyError(
            f"explicit CI toolchains {sorted(set(releases))} do not match "
            f"Cargo.toml rust-version {line}"
        )
    if STABLE_MSRV_CHECK.search(ci_source) is None:
        raise PolicyError(
            "MSRV job check name must remain stable as 'MSRV' for external gates"
        )

    for needle, description in [
        (f"Install Rust toolchain ({line})", "MSRV install step"),
        (f"rustc {line}", "MSRV rationale"),
        (f"coverage-{release}-llvm-cov-", "coverage cache"),
    ]:
        require_occurrence(ci_source, needle, description)

    for needle, description in [
        (f"Rust {line}+", "README prerequisite"),
        (f"toolchain install {release}", "README coverage setup"),
        (f"cargo +{release} llvm-cov", "README coverage commands"),
        (f"pinned to Rust {release}", "README coverage policy"),
    ]:
        require_occurrence(readme, needle, description)


def replace_exactly(source: str, old: str, new: str, description: str) -> str:
    count = source.count(old)
    if count == 0:
        raise PolicyError(f"cannot locate {description}: {old!r}")
    return source.replace(old, new)


def synchronize(target_line: str, *, update_toolchain_manifest: bool = False) -> None:
    if LINE_VERSION.fullmatch(target_line) is None:
        raise PolicyError("target Rust version must use canonical X.Y form")

    old_line = manifest_line_version()
    old_release = f"{old_line}.0"
    target_release = f"{target_line}.0"

    toolchain_source = TOOLCHAIN_MANIFEST.read_text()
    proposed_line = candidate_from_toolchain(toolchain_source)
    if update_toolchain_manifest:
        toolchain_source = replace_exactly(
            toolchain_source,
            f'channel = "{proposed_line}.0"',
            f'channel = "{target_release}"',
            "toolchain manifest channel",
        )
    elif proposed_line != target_line:
        raise PolicyError(
            f"requested Rust {target_line} does not match the toolchain manifest "
            f"proposal {proposed_line}"
        )

    manifest = MANIFEST.read_text()
    manifest = replace_exactly(
        manifest,
        f'rust-version = "{old_line}"',
        f'rust-version = "{target_line}"',
        "Cargo rust-version",
    )

    ci_source = CI.read_text()
    exact_action_pins(ci_source)
    exact_ci_releases(ci_source)
    ci_source, count = EXACT_CI_TOOLCHAIN.subn(
        lambda match: f"{match.group(1)}{target_release}", ci_source
    )
    if count != 2:
        raise PolicyError("could not normalize both explicit CI toolchain inputs")
    for old, new, description in [
        (
            f"toolchain ({old_line})",
            f"toolchain ({target_line})",
            "MSRV install step",
        ),
        (f"rustc {old_line}", f"rustc {target_line}", "MSRV rationale"),
        (
            f"coverage-{old_release}-llvm-cov-",
            f"coverage-{target_release}-llvm-cov-",
            "coverage cache",
        ),
    ]:
        ci_source = replace_exactly(ci_source, old, new, description)

    readme = README.read_text()
    readme = replace_exactly(
        readme,
        f"Rust {old_line}+",
        f"Rust {target_line}+",
        "README prerequisite",
    )
    readme = replace_exactly(
        readme,
        old_release,
        target_release,
        "README exact Rust release",
    )

    TOOLCHAIN_MANIFEST.write_text(toolchain_source)
    MANIFEST.write_text(manifest)
    CI.write_text(ci_source)
    README.write_text(readme)
    check_consistency()


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--check", action="store_true")
    group.add_argument(
        "--from-toolchain",
        action="store_true",
        help="adopt the exact Rust release proposed in .github/rust-toolchain.toml",
    )
    group.add_argument("--set", metavar="X.Y", dest="target")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        if args.check:
            check_consistency()
        elif args.from_toolchain:
            synchronize(candidate_from_toolchain())
        else:
            synchronize(args.target, update_toolchain_manifest=True)
        print("Rust toolchain declarations are synchronized")
        return 0
    except (OSError, PolicyError, tomllib.TOMLDecodeError) as error:
        print(f"Rust toolchain policy failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
