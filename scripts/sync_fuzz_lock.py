#!/usr/bin/env python3
"""Keep root dependency updates coherent with the separate fuzz workspace.

The fuzz crate intentionally owns a separate Cargo.lock. Dependabot updates the
root lock independently, so a root PR can otherwise pass with the fuzz build
still resolving the previous direct production dependency.

`check` is read-only and intended for pull-request CI. `write` is an explicit
repair command for an isolated, trusted worktree; it invokes narrowly targeted
`cargo update --precise` operations and never commits or pushes.
"""

from __future__ import annotations

import argparse
import dataclasses
import subprocess
import sys
import tomllib
from pathlib import Path
from typing import Any


REPOSITORY = Path(__file__).resolve().parents[1]
ROOT_LOCK = REPOSITORY / "Cargo.lock"
FUZZ_LOCK = REPOSITORY / "fuzz" / "Cargo.lock"
ROOT_MANIFEST = REPOSITORY / "Cargo.toml"


class PolicyError(RuntimeError):
    """A dependency update cannot be proven safe by the deterministic policy."""


@dataclasses.dataclass(frozen=True)
class Transition:
    name: str
    current_fuzz_version: str
    target_root_version: str


def load_toml(path: Path) -> dict[str, Any]:
    with path.open("rb") as source:
        return tomllib.load(source)


def load_lock_from_git(reference: str) -> dict[str, Any]:
    try:
        encoded = subprocess.check_output(
            ["git", "show", f"{reference}:Cargo.lock"],
            cwd=REPOSITORY,
            stderr=subprocess.PIPE,
        )
    except subprocess.CalledProcessError as error:
        detail = error.stderr.decode(errors="replace").strip()
        raise PolicyError(
            f"cannot read Cargo.lock from base ref {reference!r}: {detail}"
        ) from error
    return tomllib.loads(encoded.decode())


def packages_by_name(lock: dict[str, Any]) -> dict[str, list[dict[str, Any]]]:
    result: dict[str, list[dict[str, Any]]] = {}
    for package in lock.get("package", []):
        result.setdefault(package["name"], []).append(package)
    return result


def workspace_package(lock: dict[str, Any], name: str) -> dict[str, Any]:
    candidates = [
        package
        for package in lock.get("package", [])
        if package.get("name") == name and "source" not in package
    ]
    if len(candidates) != 1:
        raise PolicyError(
            f"{name!r} must identify exactly one path package in Cargo.lock; "
            f"found {len(candidates)}"
        )
    return candidates[0]


def resolved_direct_versions(
    lock: dict[str, Any], package_name: str = "tributary"
) -> dict[str, str]:
    """Resolve Cargo.lock dependency specs to the selected package versions."""

    by_name = packages_by_name(lock)
    package = workspace_package(lock, package_name)
    result: dict[str, str] = {}

    for dependency in package.get("dependencies", []):
        fields = dependency.split()
        name = fields[0]
        explicit_version = (
            fields[1] if len(fields) >= 2 and fields[1][0].isdigit() else None
        )
        if explicit_version is not None:
            version = explicit_version
        else:
            candidates = by_name.get(name, [])
            if len(candidates) != 1:
                versions = sorted(candidate["version"] for candidate in candidates)
                raise PolicyError(
                    f"ambiguous bare dependency {name!r} in {package_name!r}; "
                    f"candidate versions: {versions}"
                )
            version = candidates[0]["version"]

        previous = result.setdefault(name, version)
        if previous != version:
            raise PolicyError(
                f"{package_name!r} resolves direct dependency {name!r} twice: "
                f"{previous} and {version}"
            )

    return result


def production_dependency_names(manifest: dict[str, Any]) -> set[str]:
    """Return package names inherited by fuzz through its Tributary path dep."""

    result: set[str] = set()

    def add_table(table: Any) -> None:
        if not isinstance(table, dict):
            return
        for alias, declaration in table.items():
            if isinstance(declaration, dict):
                result.add(str(declaration.get("package", alias)))
            else:
                result.add(alias)

    add_table(manifest.get("dependencies"))
    add_table(manifest.get("build-dependencies"))
    for target in manifest.get("target", {}).values():
        if isinstance(target, dict):
            add_table(target.get("dependencies"))
            add_table(target.get("build-dependencies"))

    return result


def required_transitions(
    base_root_lock: dict[str, Any],
    current_root_lock: dict[str, Any],
    current_fuzz_lock: dict[str, Any],
    current_manifest: dict[str, Any],
) -> list[Transition]:
    base = resolved_direct_versions(base_root_lock)
    current = resolved_direct_versions(current_root_lock)
    fuzz = resolved_direct_versions(current_fuzz_lock)
    production = production_dependency_names(current_manifest)
    transitions: list[Transition] = []

    # A removed production dependency remains visible in the stale fuzz lock.
    # This is a graph rewrite, not a version substitution, and requires a
    # reviewed lock regeneration rather than an unsafe best guess.
    for name in sorted((set(base) - set(current)) & set(fuzz)):
        raise PolicyError(
            f"root production dependency {name!r} was removed but remains in "
            "fuzz/Cargo.lock; regenerate the fuzz lock under review"
        )

    for name in sorted(set(base) | set(current)):
        before = base.get(name)
        after = current.get(name)
        if before == after or name not in production:
            continue
        if after is None:
            continue
        if name not in fuzz:
            raise PolicyError(
                f"changed root production dependency {name!r} is absent from "
                "fuzz/Cargo.lock; regenerate the fuzz lock under review"
            )
        if fuzz[name] != after:
            transitions.append(Transition(name, fuzz[name], after))

    return transitions


def update_fuzz_lock(transitions: list[Transition], *, offline: bool) -> None:
    for transition in transitions:
        # Re-read after each operation: updating a direct facade such as
        # `futures` can also bring its component crates to the target version.
        fuzz = resolved_direct_versions(load_toml(FUZZ_LOCK))
        current = fuzz.get(transition.name)
        if current == transition.target_root_version:
            continue
        if current is None:
            raise PolicyError(
                f"{transition.name!r} disappeared while repairing fuzz/Cargo.lock"
            )

        command = ["cargo", "update"]
        if offline:
            command.append("--offline")
        command.extend(
            [
                "--manifest-path",
                str(REPOSITORY / "fuzz" / "Cargo.toml"),
                "--package",
                f"{transition.name}@{current}",
                "--precise",
                transition.target_root_version,
            ]
        )
        subprocess.run(
            command,
            cwd=REPOSITORY,
            check=True,
        )


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("mode", choices=("check", "write"))
    parser.add_argument(
        "--base-ref",
        required=True,
        help="exact base commit/ref whose Cargo.lock precedes this update",
    )
    parser.add_argument(
        "--offline",
        action="store_true",
        help="require every selected crate/index record to exist in Cargo's cache",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        base = load_lock_from_git(args.base_ref)
        current = load_toml(ROOT_LOCK)
        fuzz = load_toml(FUZZ_LOCK)
        manifest = load_toml(ROOT_MANIFEST)
        transitions = required_transitions(base, current, fuzz, manifest)

        if args.mode == "write":
            update_fuzz_lock(transitions, offline=args.offline)
            transitions = required_transitions(
                base, current, load_toml(FUZZ_LOCK), manifest
            )

        if transitions:
            print(
                "fuzz/Cargo.lock does not reflect changed root production "
                "dependencies:",
                file=sys.stderr,
            )
            for transition in transitions:
                print(
                    f"  {transition.name}: "
                    f"{transition.current_fuzz_version} -> "
                    f"{transition.target_root_version}",
                    file=sys.stderr,
                )
            print(
                "repair in an isolated trusted worktree with:\n"
                f"  python3 scripts/sync_fuzz_lock.py write "
                f"--base-ref {args.base_ref}",
                file=sys.stderr,
            )
            return 1

        print("root and fuzz lockfiles are coherent for changed dependencies")
        return 0
    except (OSError, PolicyError, subprocess.CalledProcessError) as error:
        print(f"dependency lock policy failed: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
