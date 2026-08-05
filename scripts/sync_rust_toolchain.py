#!/usr/bin/env python3
"""
Validate or synchronize Tributary's authoritative Rust/MSRV declarations.

Dependabot is expected to propose coordinated digest updates to the two exact
`dtolnay/rust-toolchain@SHA # X.Y.0` pins. Those proposals deliberately do not
auto-merge. A trusted repairer runs this script to coordinate Cargo.toml, CI,
cache keys, and developer documentation before the complete matrix is allowed
to decide whether the new compiler is feasible.
"""

from __future__ import annotations

import argparse
import re
import sys
import tomllib
from pathlib import Path


REPOSITORY = Path(__file__).resolve().parents[1]
MANIFEST = REPOSITORY / "Cargo.toml"
CI = REPOSITORY / ".github" / "workflows" / "ci.yml"
README = REPOSITORY / "README.md"
LINE_VERSION = re.compile(r"^[1-9][0-9]*\.[0-9]+$")
RELEASE_VERSION = re.compile(r"^([1-9][0-9]*\.[0-9]+)\.0$")
EXACT_TOOLCHAIN = re.compile(
    r"(?m)^(\s*uses:\s*dtolnay/rust-toolchain@)([0-9a-f]{40})"
    r"(\s+#\s*)([1-9][0-9]*\.[0-9]+\.0)\s*$"
)
ACTION_SHA = re.compile(r"^[0-9a-f]{40}$")
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


def exact_action_pins(ci_source: str) -> list[tuple[str, str]]:
    pins = [
        (match.group(2), match.group(4))
        for match in EXACT_TOOLCHAIN.finditer(ci_source)
    ]
    if len(pins) != 2:
        raise PolicyError(
            "CI must contain exactly two digest-pinned dtolnay/rust-toolchain refs "
            f"with release comments (MSRV and coverage); found {len(pins)}"
        )
    return pins


def exact_action_releases(ci_source: str) -> list[str]:
    return [release for _, release in exact_action_pins(ci_source)]


def candidate_from_actions(ci_source: str) -> str:
    pins = exact_action_pins(ci_source)
    shas = {sha for sha, _ in pins}
    if len(shas) != 1:
        raise PolicyError(
            "Dependabot must update both exact toolchain digests together; found "
            + ", ".join(sorted(shas))
        )
    releases = {release for _, release in pins}
    if len(releases) != 1:
        raise PolicyError(
            "Dependabot must update both exact toolchain refs together; found "
            + ", ".join(sorted(releases))
        )
    release = releases.pop()
    match = RELEASE_VERSION.fullmatch(release)
    if match is None:
        raise PolicyError(f"unsupported Rust release tag {release!r}")
    return match.group(1)


def require_occurrence(source: str, needle: str, description: str) -> None:
    if needle not in source:
        raise PolicyError(f"{description} is not synchronized: missing {needle!r}")


def check_consistency() -> None:
    line = manifest_line_version()
    release = f"{line}.0"
    ci_source = CI.read_text()
    readme = README.read_text()
    pins = exact_action_pins(ci_source)
    releases = [release for _, release in pins]
    if set(releases) != {release}:
        raise PolicyError(
            f"pinned CI toolchain releases {sorted(set(releases))} do not match "
            f"Cargo.toml rust-version {line}"
        )
    shas = {sha for sha, _ in pins}
    if len(shas) != 1:
        raise PolicyError(
            "MSRV and coverage must use the same immutable toolchain action digest"
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


def synchronize(target_line: str, action_sha: str | None = None) -> None:
    if LINE_VERSION.fullmatch(target_line) is None:
        raise PolicyError("target Rust version must use canonical X.Y form")

    old_line = manifest_line_version()
    old_release = f"{old_line}.0"
    target_release = f"{target_line}.0"

    manifest = MANIFEST.read_text()
    manifest = replace_exactly(
        manifest,
        f'rust-version = "{old_line}"',
        f'rust-version = "{target_line}"',
        "Cargo rust-version",
    )

    ci_source = CI.read_text()
    pins = exact_action_pins(ci_source)
    if action_sha is None:
        matching_shas = {
            sha for sha, release in pins if release == target_release
        }
        if len(matching_shas) != 1 or any(
            release != target_release for _, release in pins
        ):
            raise PolicyError(
                "changing the Rust release with --set requires the immutable "
                "dtolnay/rust-toolchain commit via --action-sha"
            )
        action_sha = matching_shas.pop()
    elif ACTION_SHA.fullmatch(action_sha) is None:
        raise PolicyError("toolchain action SHA must be 40 lowercase hexadecimal characters")

    # Normalize both exact digest pins. Stable/nightly channels are
    # intentionally untouched.
    ci_source, count = EXACT_TOOLCHAIN.subn(
        lambda match: f"{match.group(1)}{action_sha} # {target_release}", ci_source
    )
    if count != 2:
        raise PolicyError("could not normalize both exact CI toolchain refs")
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

    MANIFEST.write_text(manifest)
    CI.write_text(ci_source)
    README.write_text(readme)
    check_consistency()


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--check", action="store_true")
    group.add_argument(
        "--from-actions",
        action="store_true",
        help="adopt the one exact Rust release proposed in both pinned CI action refs",
    )
    group.add_argument("--set", metavar="X.Y", dest="target")
    parser.add_argument(
        "--action-sha",
        help="40-character dtolnay/rust-toolchain commit required with a new --set release",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        if args.action_sha is not None and args.target is None:
            raise PolicyError("--action-sha is valid only together with --set")
        if args.check:
            check_consistency()
        elif args.from_actions:
            synchronize(candidate_from_actions(CI.read_text()))
        else:
            synchronize(args.target, args.action_sha)
        print("Rust toolchain declarations are synchronized")
        return 0
    except (OSError, PolicyError, tomllib.TOMLDecodeError) as error:
        print(f"Rust toolchain policy failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
