#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(CDPATH= cd -- "${SCRIPT_DIR}/../.." && pwd)"
GENERATOR="${SCRIPT_DIR}/flatpak-cargo-generator.py"
CHECKSUM="${SCRIPT_DIR}/flatpak-cargo-generator.sha256"
REQUIREMENTS="${SCRIPT_DIR}/generator-requirements.txt"
CARGO_LOCK="${REPO_ROOT}/Cargo.lock"
OUTPUT="${SCRIPT_DIR}/cargo-sources.json"
PYTHON="${PYTHON:-python3}"

command -v sha256sum >/dev/null 2>&1 || {
  echo "sha256sum is required to verify the vendored Flatpak Cargo generator" >&2
  exit 1
}
command -v "${PYTHON}" >/dev/null 2>&1 || {
  echo "${PYTHON} is required to generate Flatpak Cargo sources" >&2
  exit 1
}

(
  cd "${SCRIPT_DIR}"
  sha256sum --check --strict "$(basename -- "${CHECKSUM}")"
)

if ! "${PYTHON}" -c '
import importlib
import importlib.metadata
import pathlib
import sys

problems = []
if sys.version_info < (3, 9):
    problems.append(
        "Python 3.9 or newer is required; found "
        f"{sys.version_info.major}.{sys.version_info.minor}"
    )

requirements = {}
for line_number, raw_line in enumerate(
    pathlib.Path(sys.argv[1]).read_text(encoding="utf-8").splitlines(), 1
):
    requirement = raw_line.split("#", 1)[0].strip()
    if not requirement:
        continue
    package, separator, expected = requirement.partition("==")
    package = package.strip()
    expected = expected.strip()
    if not separator or not package or not expected or "==" in expected:
        problems.append(
            f"{sys.argv[1]}:{line_number}: expected an exact name==version pin"
        )
        continue
    if package in requirements:
        problems.append(f"{sys.argv[1]}:{line_number}: duplicate pin for {package}")
        continue
    requirements[package] = expected

modules = {"aiohttp": "aiohttp", "tomlkit": "tomlkit"}
if set(requirements) != set(modules):
    problems.append("requirements must contain exactly aiohttp and tomlkit")

for package, module in modules.items():
    expected = requirements.get(package)
    if expected is None:
        continue
    try:
        actual = importlib.metadata.version(package)
    except importlib.metadata.PackageNotFoundError:
        problems.append(f"{package} is not installed")
        continue
    if actual != expected:
        problems.append(f"{package} {expected} is required; found {actual}")
    try:
        importlib.import_module(module)
    except Exception as error:
        problems.append(f"{package} cannot be imported: {error}")

if problems:
    print("Flatpak generator environment mismatch:", file=sys.stderr)
    for problem in problems:
        print(f"- {problem}", file=sys.stderr)
    raise SystemExit(1)
' "${REQUIREMENTS}"; then
  echo "Missing or mismatched Flatpak generator dependencies." >&2
  echo "Install the exact direct versions with:" >&2
  echo "  ${PYTHON} -m pip install --requirement ${REQUIREMENTS}" >&2
  exit 1
fi

"${PYTHON}" "${GENERATOR}" "${CARGO_LOCK}" -o "${OUTPUT}"
echo "Generated ${OUTPUT}"
