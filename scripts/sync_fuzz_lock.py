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


def load_toml_from_git(reference: str, path: str) -> dict[str, Any]:
    try:
        encoded = subprocess.check_output(
            ["git", "show", f"{reference}:{path}"],
            cwd=REPOSITORY,
            stderr=subprocess.PIPE,
        )
    except subprocess.CalledProcessError as error:
        detail = error.stderr.decode(errors="replace").strip()
        raise PolicyError(
            f"cannot read {path} from base ref {reference!r}: {detail}"
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


def freeze(value: Any) -> Any:
    if isinstance(value, dict):
        return tuple(sorted((key, freeze(item)) for key, item in value.items()))
    if isinstance(value, list):
        return tuple(freeze(item) for item in value)
    return value


def production_dependency_declarations(
    manifest: dict[str, Any],
) -> dict[str, tuple[Any, ...]]:
    """Return normalized declarations inherited through the Tributary path dep."""

    result: dict[str, list[Any]] = {}

    def add_table(scope: str, table: Any) -> None:
        if not isinstance(table, dict):
            return
        for alias, declaration in table.items():
            if isinstance(declaration, dict):
                package = str(declaration.get("package", alias))
            else:
                package = alias
            result.setdefault(package, []).append(
                (scope, alias, freeze(declaration))
            )

    add_table("dependencies", manifest.get("dependencies"))
    add_table("build-dependencies", manifest.get("build-dependencies"))
    for condition, target in manifest.get("target", {}).items():
        if isinstance(target, dict):
            add_table(
                f"target.{condition}.dependencies",
                target.get("dependencies"),
            )
            add_table(
                f"target.{condition}.build-dependencies",
                target.get("build-dependencies"),
            )

    return {
        package: tuple(sorted(declarations))
        for package, declarations in result.items()
    }


def production_dependency_names(manifest: dict[str, Any]) -> set[str]:
    """Return package names inherited by fuzz through its Tributary path dep."""

    return set(production_dependency_declarations(manifest))


def package_version_sets(lock: dict[str, Any]) -> dict[str, set[str]]:
    result: dict[str, set[str]] = {}
    for package in lock.get("package", []):
        result.setdefault(package["name"], set()).add(package["version"])
    return result


def changed_package_version_names(
    before: dict[str, Any], after: dict[str, Any]
) -> set[str]:
    before_versions = package_version_sets(before)
    after_versions = package_version_sets(after)
    return {
        name
        for name in set(before_versions) | set(after_versions)
        if before_versions.get(name, set()) != after_versions.get(name, set())
    }


def package_records_by_identity(
    lock: dict[str, Any],
) -> dict[tuple[str, str], dict[str, Any]]:
    records: dict[tuple[str, str], dict[str, Any]] = {}
    for package in lock.get("package", []):
        identity = (package["name"], package["version"])
        if identity in records:
            raise PolicyError(
                "Cargo.lock contains multiple records for "
                f"{identity[0]}@{identity[1]}; dependency closure is ambiguous"
            )
        records[identity] = package
    return records


def dependency_identity(
    specification: str,
    records: dict[tuple[str, str], dict[str, Any]],
) -> tuple[str, str]:
    fields = specification.split()
    name = fields[0]
    if len(fields) >= 2 and fields[1][0].isdigit():
        identity = (name, fields[1])
        if identity not in records:
            raise PolicyError(
                f"Cargo.lock dependency {specification!r} has no package record"
            )
        return identity

    candidates = [identity for identity in records if identity[0] == name]
    if len(candidates) != 1:
        raise PolicyError(
            f"ambiguous bare Cargo.lock dependency {specification!r}; "
            f"candidate versions: {sorted(version for _, version in candidates)}"
        )
    return candidates[0]


def dependency_closure_identities(
    lock: dict[str, Any],
    roots: set[tuple[str, str]],
) -> set[tuple[str, str]]:
    """Return the exact package identities reachable from lockfile roots."""

    records = package_records_by_identity(lock)
    pending = list(roots)
    visited: set[tuple[str, str]] = set()
    while pending:
        identity = pending.pop()
        if identity in visited:
            continue
        package = records.get(identity)
        if package is None:
            raise PolicyError(
                f"Cargo.lock does not contain {identity[0]}@{identity[1]}"
            )
        visited.add(identity)
        pending.extend(
            dependency_identity(specification, records)
            for specification in package.get("dependencies", [])
        )
    return visited


def dependency_closure_names(
    lock: dict[str, Any],
    roots: set[tuple[str, str]],
) -> set[str]:
    return {
        name
        for name, _ in dependency_closure_identities(lock, roots)
    }


def resolved_dependency_edges(
    package: dict[str, Any],
    records: dict[tuple[str, str], dict[str, Any]],
) -> dict[str, tuple[tuple[str, str], ...]]:
    edges: dict[str, list[tuple[str, str]]] = {}
    for specification in package.get("dependencies", []):
        identity = dependency_identity(specification, records)
        edges.setdefault(identity[0], []).append(identity)
    return {
        name: tuple(sorted(identities))
        for name, identities in edges.items()
    }


def validate_dependency_edges(
    before_lock: dict[str, Any],
    after_lock: dict[str, Any],
    transitions: list[Transition],
    authorized_names: set[str],
    authorized_identities: set[tuple[str, str]],
) -> None:
    """Reject semantic edge changes outside the reviewed dependency surface."""

    before_records = package_records_by_identity(before_lock)
    after_records = package_records_by_identity(after_lock)
    transition_targets = {
        transition.name: (transition.name, transition.target_root_version)
        for transition in transitions
    }
    for identity in sorted(before_records.keys() & after_records.keys()):
        before_metadata = {
            key: value
            for key, value in before_records[identity].items()
            if key != "dependencies"
        }
        after_metadata = {
            key: value
            for key, value in after_records[identity].items()
            if key != "dependencies"
        }
        if freeze(before_metadata) != freeze(after_metadata):
            raise PolicyError(
                "fuzz lock repair changed immutable package metadata for "
                f"{identity[0]}@{identity[1]}"
            )

        before_edges = resolved_dependency_edges(
            before_records[identity], before_records
        )
        after_edges = resolved_dependency_edges(after_records[identity], after_records)
        if before_edges == after_edges:
            continue

        before_names = sorted(before_edges)
        after_names = sorted(after_edges)
        if before_names != after_names:
            raise PolicyError(
                "fuzz lock repair rewrote the dependency-name surface of "
                f"{identity[0]}@{identity[1]}: {before_names} -> {after_names}"
            )

        for dependency_name in before_names:
            before_targets = before_edges[dependency_name]
            after_targets = after_edges[dependency_name]
            if before_targets == after_targets:
                continue

            if identity[0] == "tributary":
                target = transition_targets.get(dependency_name)
                if target is not None and after_targets == (target,):
                    continue
                raise PolicyError(
                    "fuzz lock repair changed an unrequested Tributary edge "
                    f"{dependency_name!r}: {before_targets} -> {after_targets}"
                )

            edge_surface = set(before_targets) | set(after_targets)
            if (
                identity in authorized_identities
                or identity[0] in authorized_names
                or edge_surface <= authorized_identities
                or {name for name, _ in edge_surface} <= authorized_names
            ):
                continue

            raise PolicyError(
                "fuzz lock repair semantically rebound an edge outside the "
                f"reviewed dependency surface: {identity[0]}@{identity[1]} "
                f"{dependency_name}: {before_targets} -> {after_targets}"
            )


def validate_bounded_package_changes(
    base_root_lock: dict[str, Any],
    current_root_lock: dict[str, Any],
    before_fuzz_lock: dict[str, Any],
    after_fuzz_lock: dict[str, Any],
    transitions: list[Transition],
) -> None:
    """Reject package-version drift outside the exact root update surface."""

    old_roots = {
        (transition.name, transition.current_fuzz_version)
        for transition in transitions
    }
    new_roots = {
        (transition.name, transition.target_root_version)
        for transition in transitions
    }
    authorized_identities = dependency_closure_identities(
        before_fuzz_lock, old_roots
    ) | dependency_closure_identities(current_root_lock, new_roots)
    allowed_names = changed_package_version_names(base_root_lock, current_root_lock)
    allowed_names.update(transition.name for transition in transitions)
    allowed_names.update(name for name, _ in authorized_identities)
    validate_dependency_edges(
        before_fuzz_lock,
        after_fuzz_lock,
        transitions,
        allowed_names,
        authorized_identities,
    )
    actual_names = changed_package_version_names(before_fuzz_lock, after_fuzz_lock)
    unexpected_names = actual_names - allowed_names
    if unexpected_names:
        raise PolicyError(
            "fuzz lock repair changed package versions outside the root update "
            f"surface: {sorted(unexpected_names)}"
        )


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

    for name in sorted(set(current) & production):
        after = current[name]
        if name not in fuzz:
            raise PolicyError(
                f"root production dependency {name!r} is absent from "
                "fuzz/Cargo.lock; regenerate the fuzz lock under review"
            )
        if fuzz[name] != after:
            transitions.append(Transition(name, fuzz[name], after))

    return transitions


def cargo_command(arguments: list[str], *, offline: bool) -> None:
    command = ["cargo", *arguments]
    if offline:
        command.insert(2, "--offline")
    subprocess.run(command, cwd=REPOSITORY, check=True)


def refresh_tributary_dependency_graph(*, offline: bool) -> None:
    """Re-resolve the path package so new direct majors can coexist with old."""

    cargo_command(
        [
            "update",
            "--manifest-path",
            str(REPOSITORY / "fuzz" / "Cargo.toml"),
            "--package",
            "tributary",
        ],
        offline=offline,
    )


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

        cargo_command(
            [
                "update",
                "--manifest-path",
                str(REPOSITORY / "fuzz" / "Cargo.toml"),
                "--package",
                f"{transition.name}@{current}",
                "--precise",
                transition.target_root_version,
            ],
            offline=offline,
        )


def validate_locked_fuzz_metadata(*, offline: bool) -> None:
    command = [
        "metadata",
        "--manifest-path",
        str(REPOSITORY / "fuzz" / "Cargo.toml"),
        "--format-version",
        "1",
        "--locked",
        "--no-deps",
    ]
    if offline:
        command.insert(1, "--offline")
    subprocess.run(
        ["cargo", *command],
        cwd=REPOSITORY,
        stdout=subprocess.DEVNULL,
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
        base = load_toml_from_git(args.base_ref, "Cargo.lock")
        base_manifest = load_toml_from_git(args.base_ref, "Cargo.toml")
        current = load_toml(ROOT_LOCK)
        fuzz = load_toml(FUZZ_LOCK)
        manifest = load_toml(ROOT_MANIFEST)
        transitions = required_transitions(base, current, fuzz, manifest)

        if args.mode == "write":
            original_lock = FUZZ_LOCK.read_bytes()
            original_transitions = transitions
            base_declarations = production_dependency_declarations(base_manifest)
            current_declarations = production_dependency_declarations(manifest)
            graph_refresh_names = {
                transition.name
                for transition in transitions
                if base_declarations.get(transition.name)
                != current_declarations.get(transition.name)
            }
            try:
                if graph_refresh_names:
                    refresh_tributary_dependency_graph(offline=args.offline)

                transitions = required_transitions(
                    base, current, load_toml(FUZZ_LOCK), manifest
                )
                update_fuzz_lock(transitions, offline=args.offline)
                repaired_fuzz = load_toml(FUZZ_LOCK)
                transitions = required_transitions(
                    base, current, repaired_fuzz, manifest
                )
                if transitions:
                    raise PolicyError(
                        "repair completed without proving every requested "
                        f"transition: {transitions}"
                    )
                validate_bounded_package_changes(
                    base,
                    current,
                    fuzz,
                    repaired_fuzz,
                    original_transitions,
                )
                validate_locked_fuzz_metadata(offline=args.offline)
            except (
                OSError,
                PolicyError,
                subprocess.CalledProcessError,
                tomllib.TOMLDecodeError,
            ):
                FUZZ_LOCK.write_bytes(original_lock)
                raise

        if transitions:
            print(
                "fuzz/Cargo.lock does not match root production dependencies:",
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

        print("root and fuzz lockfiles have coherent production dependencies")
        return 0
    except (
        OSError,
        PolicyError,
        subprocess.CalledProcessError,
        tomllib.TOMLDecodeError,
    ) as error:
        print(f"dependency lock policy failed: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
