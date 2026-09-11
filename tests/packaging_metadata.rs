use toml::Value;

const MANIFEST: &str = include_str!("../Cargo.toml");
const ROOT_LOCK: &str = include_str!("../Cargo.lock");
const FUZZ_LOCK: &str = include_str!("../fuzz/Cargo.lock");
const RPM_SPEC: &str = include_str!("../build-aux/rpm/tributary.spec");
const ARCH_PKGBUILD: &str = include_str!("../build-aux/arch/PKGBUILD");
const DESKTOP_ENTRY: &str = include_str!("../data/io.github.tributary.Tributary.desktop");
const CI_WORKFLOW: &str = include_str!("../.github/workflows/ci.yml");
const RUST_TOOLCHAIN_MANIFEST: &str = include_str!("../.github/rust-toolchain.toml");
const DEPENDABOT_CONFIG: &str = include_str!("../.github/dependabot.yml");
const DEPENDABOT_AUTOMERGE: &str = include_str!("../.github/workflows/dependabot-automerge.yml");
const BOT_REVIEW_GATE: &str = include_str!("../.github/workflows/bot-review-gate.yml");
const BOT_REVIEW_GATE_PUBLISHER: &str =
    include_str!("../.github/workflows/bot-review-gate-publisher.yml");
const RELEASE_WORKFLOW: &str = include_str!("../.github/workflows/release.yml");
const REFINERY_CONFIG: &str = include_str!("../docs/refinery-config.md");
const COVERAGE_BASELINE: &str = include_str!("../coverage-baseline.txt");
const README: &str = include_str!("../README.md");
const BUILD_SCRIPT: &str = include_str!("../build.rs");
const BUILD_LINUX: &str = include_str!("../scripts/build-linux.sh");
const BUILD_MACOS: &str = include_str!("../scripts/build-macos.sh");
const MACOS_PACKAGE_POLICY: &str = include_str!("../scripts/macos-package-policy.sh");
const BUILD_WINDOWS: &str = include_str!("../scripts/build-windows.ps1");
const WINDOWS_AUDIO: &str = include_str!("../src/audio/windows_audio.rs");
const WINDOWS_RUNTIME_PROBE: &str = include_str!("../src/audio/runtime_probe.rs");
const MACOS_AUDIO: &str = include_str!("../src/audio/macos_audio.rs");
const MACOS_AUDIO_NATIVE: &str = include_str!("../src/audio/macos_audio_native.rs");
const MACOS_AUDIO_TESTS: &str = include_str!("../src/audio/macos_audio_tests.rs");
const PLATFORM_RUNTIME: &str = include_str!("../src/platform_runtime.rs");
const RUST_TOOLCHAIN_ACTION_SHA: &str = "6c977a6ca4077a0ceb28ffbe03f59d46e9ac8772";
const FLATPAK_BUILDER_ACTION_SHA: &str = "79327416609af08178ad73b352877e51450790b3";
const FORBIDDEN_BUNDLED_COMPONENTS: &str =
    include_str!("../build-aux/packaging/forbidden-bundled-components.txt");

fn manifest() -> Value {
    toml::from_str(MANIFEST).expect("Cargo.toml must parse")
}

fn locked_version(source: &str, package: &str) -> String {
    let lock: Value = toml::from_str(source).expect("Cargo lockfile must parse");
    let versions: Vec<_> = lock["package"]
        .as_array()
        .expect("Cargo lockfile must contain package records")
        .iter()
        .filter(|candidate| candidate["name"].as_str() == Some(package))
        .map(|candidate| {
            candidate["version"]
                .as_str()
                .expect("locked package must have a version")
        })
        .collect();
    assert_eq!(
        versions.len(),
        1,
        "{package} must resolve to exactly one version; actual: {versions:?}"
    );
    versions[0].to_owned()
}

fn yaml_string_list(value: &serde_yaml::Value, field: &str) -> Vec<String> {
    value
        .get(field)
        .unwrap_or_else(|| panic!("missing YAML field {field}"))
        .as_sequence()
        .unwrap_or_else(|| panic!("YAML field {field} must be a sequence"))
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .unwrap_or_else(|| panic!("{field} entries must be strings"))
                .to_owned()
        })
        .collect()
}

fn dependabot_update<'a>(
    config: &'a serde_yaml::Value,
    ecosystem: &str,
    directory: &str,
) -> &'a serde_yaml::Value {
    let matches: Vec<_> = config["updates"]
        .as_sequence()
        .expect("Dependabot updates must be a sequence")
        .iter()
        .filter(|update| {
            update["package-ecosystem"].as_str() == Some(ecosystem)
                && update["directory"].as_str() == Some(directory)
        })
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "Dependabot must define exactly one {ecosystem} update at {directory}"
    );
    matches[0]
}

fn parse_api_feature(feature: &str) -> Option<(u32, u32)> {
    let (major, minor) = feature.strip_prefix('v')?.split_once('_')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

fn dependency_api_floor(manifest: &Value, dependency: &str, minimum: (u32, u32)) -> String {
    let features = manifest["dependencies"][dependency]["features"]
        .as_array()
        .unwrap_or_else(|| panic!("{dependency} features must be an array"));
    let enabled = features
        .iter()
        .filter_map(Value::as_str)
        .filter_map(parse_api_feature)
        .max()
        .unwrap_or_else(|| panic!("{dependency} must enable a versioned API feature"));

    assert!(
        enabled >= minimum,
        "{dependency} API floor {enabled:?} is below required {minimum:?}"
    );
    format!("{}.{}", enabled.0, enabled.1)
}

fn constraint_package(entry: &str) -> &str {
    entry
        .split(|character: char| {
            character.is_ascii_whitespace()
                || character == '<'
                || character == '>'
                || character == '='
        })
        .next()
        .expect("a nonempty constraint must have a package name")
}

fn assert_exact_constraint(entries: &[&str], package: &str, expected: &str, field: &str) {
    let matching: Vec<_> = entries
        .iter()
        .copied()
        .filter(|entry| constraint_package(entry) == package)
        .collect();
    assert_eq!(
        matching,
        [expected],
        "{field} must declare exactly one synchronized constraint for {package}; actual: {entries:?}"
    );
}

fn shell_array<'a>(source: &'a str, name: &str) -> Vec<&'a str> {
    let marker = format!("{name}=(");
    let mut lines = source.lines();
    lines
        .find(|line| line.trim() == marker)
        .unwrap_or_else(|| panic!("{name} shell array must exist"));

    lines
        .take_while(|line| line.trim() != ")")
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| line.trim_matches(|character| character == '\'' || character == '"'))
        .collect()
}

fn desktop_value(key: &str) -> &str {
    DESKTOP_ENTRY
        .lines()
        .filter_map(|line| line.split_once('='))
        .find_map(|(candidate, value)| (candidate == key).then_some(value))
        .unwrap_or_else(|| panic!("desktop key {key} must exist"))
}

fn workflow_job<'a>(source: &'a str, name: &str) -> &'a str {
    let marker = format!("  {name}:");
    let mut body_start = None;
    let mut offset = 0;

    for line in source.split_inclusive('\n') {
        let content = line.trim_end_matches(['\r', '\n']);
        if let Some(start) = body_start {
            if content.starts_with("  ") && !content.starts_with("    ") && content.ends_with(':') {
                return &source[start..offset];
            }
        } else if content == marker {
            body_start = Some(offset + line.len());
        }
        offset += line.len();
    }

    let start = body_start.unwrap_or_else(|| panic!("workflow job {name} must exist"));
    &source[start..]
}

fn assert_flatpak_artifact_boundary(
    source: &str,
    job_name: &str,
    label: &str,
    artifact_name: &str,
    artifact_path: &str,
) {
    let job = workflow_job(source, job_name);
    let build = job
        .find("name: Build Flatpak bundle")
        .unwrap_or_else(|| panic!("{label} must name its Flatpak build boundary"));
    let validation = job
        .find("name: Validate completed Flatpak bundle payload")
        .unwrap_or_else(|| panic!("{label} must validate the completed Flatpak"));
    let upload = job
        .find("name: Upload Flatpak")
        .unwrap_or_else(|| panic!("{label} must retain one explicit Flatpak upload"));

    assert!(
        build < validation && validation < upload,
        "{label} must validate the completed Flatpak before making it a workflow artifact"
    );
    assert!(
        job[build..validation].contains("upload-artifact: false"),
        "{label} must disable flatpak-builder's implicit pre-validation artifact upload"
    );
    assert_flatpak_builder_pin(source, job_name, label);
    assert_eq!(
        job.matches("uses: actions/upload-artifact@v7").count(),
        1,
        "{label} must upload the Flatpak exactly once"
    );
    let upload_step = &job[upload..];
    assert!(
        upload_step.contains(&format!("name: {artifact_name}"))
            && upload_step.contains(&format!("path: {artifact_path}"))
            && upload_step.contains("if-no-files-found: error"),
        "{label} must upload the exact validated Flatpak and fail when it is missing"
    );
}

fn assert_flatpak_builder_pin(source: &str, job_name: &str, label: &str) {
    let workflow: serde_yaml::Value = serde_yaml::from_str(source).expect("workflow YAML");
    let steps = workflow["jobs"][job_name]["steps"]
        .as_sequence()
        .expect("Flatpak job steps");
    let builds: Vec<_> = steps
        .iter()
        .filter(|step| step["name"].as_str() == Some("Build Flatpak bundle"))
        .collect();
    assert_eq!(builds.len(), 1, "{label} must have one named build step");
    let expected =
        format!("flatpak/flatpak-github-actions/flatpak-builder@{FLATPAK_BUILDER_ACTION_SHA}");
    assert_eq!(
        builds[0]["uses"].as_str(),
        Some(expected.as_str()),
        "{label} must pin the build step to the exact reviewed revision"
    );
    assert_eq!(
        steps
            .iter()
            .filter_map(|step| step["uses"].as_str())
            .filter(|uses| uses.starts_with("flatpak/flatpak-github-actions/flatpak-builder@"))
            .count(),
        1,
        "{label} must contain exactly one Flatpak builder action"
    );
}

#[test]
fn flatpak_builder_pin_rejects_inexact_revision() {
    for revision in [
        FLATPAK_BUILDER_ACTION_SHA[..10].to_owned(),
        format!("{FLATPAK_BUILDER_ACTION_SHA}-unreviewed"),
    ] {
        let changed = CI_WORKFLOW.replace(FLATPAK_BUILDER_ACTION_SHA, &revision);
        assert!(
            std::panic::catch_unwind(|| {
                assert_flatpak_builder_pin(&changed, "build-flatpak", "CI");
            })
            .is_err(),
            "an inexact revision must not satisfy the build-step pin"
        );
    }
}

#[test]
fn flatpak_builder_pin_rejects_pin_on_another_step() {
    let changed = CI_WORKFLOW.replace(
        "      - name: Build Flatpak bundle",
        "      - name: Build Flatpak bundle\n        run: echo unreviewed\n      - name: Other step",
    );
    assert!(
        std::panic::catch_unwind(|| {
            assert_flatpak_builder_pin(&changed, "build-flatpak", "CI");
        })
        .is_err(),
        "a pinned action on another step must not satisfy the build-step pin"
    );
}

#[test]
fn flatpak_artifacts_publish_once_after_compliance_validation() {
    assert_flatpak_artifact_boundary(
        CI_WORKFLOW,
        "build-flatpak",
        "CI",
        "tributary-flatpak",
        "tributary.flatpak",
    );
    assert_flatpak_artifact_boundary(
        RELEASE_WORKFLOW,
        "flatpak",
        "release",
        "tributary-linux-${{ matrix.arch }}-flatpak",
        "tributary-linux-${{ matrix.arch }}.flatpak",
    );
}

#[test]
fn release_checksums_require_one_exact_asset_set() {
    let checksums = workflow_job(RELEASE_WORKFLOW, "checksums");
    assert_release_asset_set(checksums);
    assert_release_checksum_guards(checksums);
    assert_release_checksum_guard_order(checksums);
}

fn assert_release_asset_set(checksums: &str) {
    let expected_assets = shell_array(checksums, "expected_assets");
    assert_eq!(
        expected_assets,
        [
            "tributary-aarch64.rpm",
            "tributary-amd64.deb",
            "tributary-arm64.deb",
            "tributary-linux-aarch64.flatpak",
            "tributary-linux-x86_64.flatpak",
            "tributary-macos-aarch64.dmg",
            "tributary-windows-aarch64-setup.exe",
            "tributary-windows-aarch64.zip",
            "tributary-windows-x86_64-setup.exe",
            "tributary-windows-x86_64.zip",
            "tributary-x86_64.pkg.tar.zst",
            "tributary-x86_64.rpm",
        ],
        "release checksums must cover exactly the published package set"
    );
}

fn assert_release_checksum_guards(checksums: &str) {
    for fragment in [
        "release_file_list=\"$(mktemp)\"",
        "trap 'rm -f \"$release_file_list\"' EXIT",
        ") -print0 > \"$release_file_list\"",
        "mapfile -d '' release_files < \"$release_file_list\"",
        "declare -A release_paths=()",
        "release_paths[\"$name\"]=\"$path\"",
        "${#release_paths[@]} != ${#expected_assets[@]}",
        "${release_paths[$name]+present}",
    ] {
        assert!(
            checksums.contains(fragment),
            "release checksum validation is missing its fail-closed contract: {fragment}"
        );
    }
}

fn assert_release_checksum_guard_order(checksums: &str) {
    let discovery = checksums
        .find("release_file_list=\"$(mktemp)\"")
        .expect("release artifact discovery must use a checked temporary list");
    let list_read = checksums
        .find("mapfile -d '' release_files < \"$release_file_list\"")
        .expect("the checked artifact list must be read without process substitution");
    let duplicate_guard = checksums
        .find("Duplicate release artifact filename")
        .expect("duplicate package basenames must be rejected");
    let exact_count_guard = checksums
        .find("unique release assets; found")
        .expect("the unique package count must be exact");
    let missing_guard = checksums
        .find("Missing release artifact")
        .expect("every expected package basename must be present");
    let hashing = checksums
        .find("digest=\"$(sha256sum")
        .expect("validated release packages must be hashed");
    assert!(
        discovery < list_read
            && list_read < duplicate_guard
            && duplicate_guard < exact_count_guard
            && exact_count_guard < missing_guard
            && missing_guard < hashing,
        "checked discovery plus duplicate, extra, and missing guards must run before hashing"
    );
    assert!(
        !checksums.contains("sort -u") && !checksums.contains("< <("),
        "release checksums must neither hide duplicate names nor lose discovery failures"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn release_checksum_hash_failure_is_terminal() {
    let script = checksum_step_script(RELEASE_WORKFLOW);
    let assets = shell_array(&script, "expected_assets").join("\n");
    assert_checksum_hash_failure_is_terminal(&script, &assets, 23);
}

#[cfg(target_os = "linux")]
#[test]
fn ci_checksum_hash_failure_is_terminal() {
    assert_checksum_hash_failure_is_terminal(
        &checksum_step_script(CI_WORKFLOW),
        "tributary.zip",
        1,
    );
}

#[cfg(target_os = "linux")]
fn checksum_step_script(source: &str) -> String {
    let workflow: serde_yaml::Value = serde_yaml::from_str(source).unwrap();
    let steps = workflow["jobs"]["checksums"]["steps"]
        .as_sequence()
        .unwrap();
    steps
        .iter()
        .find(|step| step["name"].as_str() == Some("Generate SHA256SUMS"))
        .unwrap()["run"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[cfg(target_os = "linux")]
fn assert_checksum_hash_failure_is_terminal(script: &str, assets: &str, exit_code: i32) {
    let output = std::process::Command::new("bash")
        .args([
            "-e",
            "-c",
            r#"
test_dir="$(mktemp -d "${TMPDIR:-/var/tmp}/tributary-checksums.XXXXXX")"
trap 'rm -rf "$test_dir"' EXIT
cd "$test_dir"
mkdir artifacts bin
while IFS= read -r asset; do : > "artifacts/$asset"; done <<< "$EXPECTED_ASSETS"
printf '#!/bin/sh\necho "injected checksum failure" >&2\nexit 23\n' > bin/sha256sum
chmod +x bin/sha256sum
export PATH="$test_dir/bin:$PATH"
bash -e -c "$1"
"#,
            "checksum-test",
            script,
        ])
        .env("EXPECTED_ASSETS", assets)
        .output()
        .expect("run the checksum script with an injected hashing failure");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("injected checksum failure"),
        "hashing must run: {stderr}"
    );
    assert_eq!(
        output.status.code(),
        Some(exit_code),
        "hashing failure must prevent publication: {stderr}"
    );
}

fn forbidden_bundle_tokens() -> Vec<&'static str> {
    FORBIDDEN_BUNDLED_COMPONENTS
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect()
}

fn bundle_policy_matches(filename: &str, tokens: &[&str]) -> bool {
    let filename = filename.to_ascii_lowercase();
    tokens
        .iter()
        .any(|token| filename.contains(&token.to_ascii_lowercase()))
}

fn bundle_policy_matches_relative_path(path: &str, tokens: &[&str]) -> bool {
    path.split(['/', '\\'])
        .filter(|component| !component.is_empty())
        .any(|component| bundle_policy_matches(component, tokens))
}

#[test]
fn windows_build_scopes_compiler_tools_to_the_rust_target() {
    let compact_build_windows: String = BUILD_WINDOWS
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .map(|character| character.to_ascii_lowercase())
        .collect();
    assert!(
        BUILD_WINDOWS
            .contains(r#"$ToolEnvTarget = $RustTarget.Replace("-", "_").Replace(".", "_")"#),
        "Windows tool variables must use Cargo's target-qualified spelling"
    );

    for (tool, clang_binary, gcc_binary) in [
        ("DLLTOOL", "llvm-dlltool.exe", "dlltool.exe"),
        ("CC", "clang.exe", "gcc.exe"),
        ("CXX", "clang++.exe", "g++.exe"),
        ("AR", "llvm-ar.exe", "ar.exe"),
    ] {
        for binary in [clang_binary, gcc_binary] {
            let assignment = format!(
                r#"[Environment]::SetEnvironmentVariable("{tool}_$ToolEnvTarget", (Join-Path $MsysPath "bin\{binary}"), "Process")"#
            );
            assert!(
                BUILD_WINDOWS.contains(&assignment),
                "Windows build is missing target-qualified {tool} mapping to {binary}"
            );
        }
        let tool = tool.to_ascii_lowercase();
        assert!(
            !compact_build_windows.contains(&format!("$env:{tool}="))
                && !compact_build_windows.contains(&format!(r#"setenvironmentvariable("{tool}","#))
                && !compact_build_windows.contains(&format!("setenvironmentvariable('{tool}',")),
            "generic {tool} assignment must not contaminate MSVC host build dependencies"
        );
    }
}

#[test]
fn bundled_component_policy_blocks_disc_decryption_without_hiding_codecs() {
    let tokens = forbidden_bundle_tokens();
    assert!(
        !tokens.is_empty(),
        "the shared bundle policy must not be empty"
    );

    let mut unique = std::collections::HashSet::new();
    for token in &tokens {
        assert_eq!(
            *token,
            token.to_ascii_lowercase(),
            "policy tokens must use a canonical lowercase spelling"
        );
        assert!(
            token
                .chars()
                .all(|character| character.is_ascii_alphanumeric()
                    || matches!(character, '.' | '_' | '+' | '-')),
            "policy token contains a character rejected by the packaging scripts: {token}"
        );
        assert!(unique.insert(*token), "duplicate policy token: {token}");
    }

    for required in [
        "dvdcss",
        "dvd-pkg",
        "dvdread",
        "dvdnav",
        "aacs",
        "bdplus",
        "gstbluray",
        "mmbd",
        "makemkv",
        "decss",
        "dvdcpxm",
        "resindvd",
        "dvdspu",
        "widevinecdm",
        "playready",
        "fairplay",
        "keydb.cfg",
    ] {
        assert!(tokens.contains(&required), "policy is missing {required}");
    }

    for forbidden in [
        "libdvdcss-2.dll",
        "LIBDVDCSS-2.DLL",
        "libdvdread-8.dll",
        "libdvdnav-4.dll",
        "libaacs-0.dll",
        "aacs.dll",
        "vendor-AaCs-runtime-helper.dll",
        "libbdplus-0.dll",
        "bdplus.dll",
        "prefix-libbdplus-0-suffix.dll",
        "libgstbluray.dll",
        "libmmbd64.dll",
        "MakeMKVcon.exe",
        "libdecss.dll",
        "libdvdcpxm.dll",
        "libgstresindvd.dll",
        "libgstdvdspu.dll",
        "widevinecdm.dll",
        "playready.dll",
        "FairPlayRuntime.dll",
        "KEYDB.CFG",
    ] {
        assert!(
            bundle_policy_matches(forbidden, &tokens),
            "forbidden component escaped the filename policy: {forbidden}"
        );
    }

    for ordinary_runtime in [
        "libgstlibav.dll",
        "libgstfdkaac.dll",
        "libgstaudioparsers.dll",
        "libgstaes.dll",
        "libgstdvdlpcmdec.dll",
        "libgstdvdsub.dll",
        "libbluray-3.dll",
        "libsoup-3.0-0.dll",
        "libssl-3-x64.dll",
        "libcrypto-3-x64.dll",
    ] {
        assert!(
            !bundle_policy_matches(ordinary_runtime, &tokens),
            "ordinary codec/runtime is overmatched by the policy: {ordinary_runtime}"
        );
    }
    assert!(
        bundle_policy_matches_relative_path(r"plugins\WidevineCDM\helper.dll", &tokens),
        "an innocuous leaf beneath a forbidden directory must still be rejected"
    );
    assert!(
        !bundle_policy_matches_relative_path(r"plugins\audio\helper.dll", &tokens),
        "ordinary relative path components must remain eligible"
    );
}

#[test]
fn windows_bundle_loads_policy_and_rejects_reparse_points() {
    let build_windows = BUILD_WINDOWS.replace("\r\n", "\n");
    assert!(
        build_windows.contains("build-aux\\packaging\\forbidden-bundled-components.txt")
            && build_windows.contains("Required bundled-component policy is missing")
            && build_windows.contains("Bundled-component policy contains no filename tokens")
            && build_windows
                .contains("Bundled-component policy contains an invalid filename token")
            && build_windows
                .contains("Bundled-component policy contains a duplicate filename token")
            && build_windows.contains("[System.StringComparison]::OrdinalIgnoreCase"),
        "Windows packaging must load the shared policy fail-closed and match it case-insensitively"
    );
    assert!(
        build_windows.contains("-SkipForbiddenComponents")
            && build_windows.contains("Test-ForbiddenBundledRelativePath $relPath")
            && build_windows.contains("Remove-ForbiddenWindowsBundleMembers $DstDir")
            && build_windows.contains("Remove-ForbiddenWindowsBundleMembers $DIST"),
        "the plugin sync must reject forbidden relative components and purge stale destinations"
    );
    assert!(
        build_windows.contains("Get-WindowsTreeMembersWithoutReparseTraversal")
            && build_windows.contains("Get-ChildItem -LiteralPath $directory -Force -ErrorAction Stop")
            && build_windows.contains("[System.IO.FileAttributes]::ReparsePoint")
            && build_windows.contains("Sort-Object")
            && build_windows.contains("$_.FullName.Length")
            && build_windows.contains("$member.Delete()"),
        "stale/final scans must include directories and hidden members, avoid reparse traversal, and delete deepest-first without recursion"
    );
    assert!(
        build_windows.contains("Get-WindowsBundleReparsePointMembers")
            && build_windows.contains("$reparsePointMembers")
            && build_windows.contains("$rootIsReparsePoint")
            && build_windows.contains("filesystem reparse point(s)")
            && build_windows.contains(
                "Refusing to sync filesystem reparse point into the Windows bundle"
            )
            && build_windows.contains(
                "Refusing to sync into a Windows destination tree containing a filesystem reparse point"
            )
            && build_windows.contains(
                "Refusing to copy filesystem reparse point into the Windows bundle"
            ),
        "final artifacts and every copy path must reject reparse points"
    );
    assert!(
        build_windows.contains(
            "$initialDllScanTargets = @(Get-WindowsTreeMembersWithoutReparseTraversal $DIST"
        ) && build_windows.contains("$_.Extension -ieq '.dll' -or $_.Extension -ieq '.drv'")
            && !build_windows.contains("Get-ChildItem -Path \"$DIST\\lib\" -Recurse -Filter *.dll"),
        "PE import scanning must seed every hidden-inclusive DLL/DRV/EXE in the complete bundle"
    );
    let root_reparse_assertion = build_windows
        .find("Assert-WindowsBundleRootIsNotReparsePoint $DIST")
        .expect("the bundle root must receive an early reparse check");
    let lib_directory_creation = build_windows
        .find("New-Item -ItemType Directory -Force \"$DIST\\lib\"")
        .expect("the first bundle child-directory write must remain recognizable");
    assert!(
        root_reparse_assertion < lib_directory_creation,
        "the bundle root must be rejected before creating its first child"
    );
    let first_dist_assertion = build_windows
        .find("Assert-WindowsBundleComponentPolicy $DIST")
        .expect("the incremental dist tree must be validated");
    let executable_copy = build_windows
        .find("Copy-WindowsBundleFileForced $exePath $exeBundleDest")
        .expect("the executable copy boundary must remain recognizable");
    assert!(
        first_dist_assertion < executable_copy,
        "an existing destination reparse point must fail before any bundle write"
    );
    let validated_source = build_windows
        .find("function Get-ValidatedWindowsBundleCopySourceItem")
        .expect("all Windows bundle copies must share a validated source boundary");
    let forced_copy = build_windows
        .find("function Copy-WindowsBundleFileForced")
        .expect("unconditional Windows bundle copies must use a guarded helper");
    let scanner_copy = build_windows
        .find("Copy-WindowsBundleFileForced $gstScannerSrc $gstScannerDest")
        .expect("the GStreamer scanner copy must use the guarded helper");
    assert!(validated_source < forced_copy && forced_copy < executable_copy);
    assert!(forced_copy < scanner_copy);
    assert!(
        build_windows
            .contains("Refusing to overwrite filesystem reparse point in the Windows bundle")
            && !build_windows.contains("Copy-Item $exePath $DIST -Force")
            && !build_windows.contains(
                "Copy-Item -LiteralPath $gstScannerSrc -Destination $gstScannerDest -Force"
            ),
        "the executable and scanner must not bypass source/destination reparse validation"
    );
}

#[test]
fn windows_bundle_applies_policy_at_copy_and_installer_boundaries() {
    let build_windows = BUILD_WINDOWS.replace("\r\n", "\n");
    let closure_rejection = build_windows
        .find("if (Test-ForbiddenBundledComponentName $dllName)")
        .expect("the recursive PE closure must reject a forbidden import");
    let closure_copy = build_windows[closure_rejection..]
        .find("$srcPath = Join-Path $ArchitectureBin $dllName")
        .map(|offset| closure_rejection + offset)
        .expect("the PE closure copy boundary must remain recognizable");
    assert!(
        closure_rejection < closure_copy,
        "the closure must reject a forbidden DLL before resolving or copying it"
    );

    let installer_only = build_windows
        .find("# ── Inno Setup only mode")
        .expect("the installer-only path must exist");
    let installer_assertion = build_windows[installer_only..]
        .find("Assert-WindowsBundleComponentPolicy $sourceDir")
        .map(|offset| installer_only + offset)
        .expect("the installer-only path must validate its existing dist tree");
    let installer_compile = build_windows[installer_assertion..]
        .find("& $iscc")
        .map(|offset| installer_assertion + offset)
        .expect("the Inno compiler invocation must remain recognizable");
    let installer_pe_assertion = build_windows[installer_assertion..]
        .find("Assert-WindowsBundlePeImportPolicy $sourceDir $installerPeImportInspector")
        .map(|offset| installer_assertion + offset)
        .expect("installer-only mode must recheck every PE import in its stale source tree");
    assert!(installer_assertion < installer_pe_assertion);
    assert!(installer_pe_assertion < installer_compile);
    assert_eq!(
        build_windows
            .matches("Assert-WindowsBundleComponentPolicy $sourceDir")
            .count(),
        2,
        "both installer-only and normal Inno paths must validate their source tree"
    );

    let runtime_probe = build_windows
        .find("# ── Packaged Runtime Probe")
        .expect("the packaged runtime probe must exist");
    assert!(
        build_windows[..runtime_probe].ends_with("Assert-WindowsBundleComponentPolicy $DIST\n\n"),
        "the dist tree must pass policy immediately before the packaged executable is run"
    );
    assert!(
        build_windows.contains("if ([string]$line -notmatch '^\\s*Name\\s*:') { continue }")
            && build_windows.contains(
                "PE import inspector returned an unsupported dependency spelling for $SourceLabel"
            ),
        "the recursive closure must fail closed on an import spelling it cannot safely resolve"
    );
}

#[test]
fn windows_bundle_requires_dynamic_system_audio_output_support() {
    let installer_only = BUILD_WINDOWS
        .find("# ── Inno Setup only mode")
        .expect("the installer-only path must exist");
    let installer_compile = BUILD_WINDOWS[installer_only..]
        .find("& $iscc")
        .map(|offset| installer_only + offset)
        .expect("the installer-only path must invoke Inno Setup");
    let probe_success_gate = BUILD_WINDOWS
        .find("if ($probeFailure) { Write-Err \"Packaged Windows runtime probe failed")
        .expect("the normal packaged probe must have a failure gate");
    let receipt_write = BUILD_WINDOWS
        .find("Write-WindowsWasapi2ProbeReceipt $distFull")
        .expect("the successful normal probe must persist a capability receipt");
    let zip_boundary = BUILD_WINDOWS
        .find("# ── Zip Archive")
        .expect("the Windows ZIP boundary must exist");
    assert_eq!(
        dependency_api_floor(&manifest(), "gstreamer", (1, 16)),
        "1.16",
        "DeviceChanged handling requires the GStreamer 1.16 Rust API"
    );
    assert!(
        BUILD_WINDOWS.contains("$requiredWasapiPluginName = \"libgstwasapi2.dll\"")
            && BUILD_WINDOWS.contains("Assert-WindowsWasapi2BundleContract $DIST")
            && BUILD_WINDOWS.contains("Required wasapi2sink plugin was not PE-inspected"),
        "the Windows bundle must require and inspect its WASAPI2 output plugin"
    );
    assert!(
        BUILD_WINDOWS
            .contains("$plugin = Join-Path $Root \"lib\\gstreamer-1.0\\libgstwasapi2.dll\"")
            && BUILD_WINDOWS.contains("return \"$Root.wasapi2-probe-v2\"")
            && BUILD_WINDOWS.contains("\"tributary-windows-wasapi2-probe-v2\"")
            && BUILD_WINDOWS.contains("\"tributary-windows-runtime-probe-v2`n\"")
            && BUILD_WINDOWS.contains("\"tributary.exe=$(Get-WindowsProbeSha256 $application)\"")
            && BUILD_WINDOWS.contains("\"libgstwasapi2.dll=$(Get-WindowsProbeSha256 $plugin)\""),
        "the capability receipt must be versioned, external to the bundle, and hash-bound"
    );
    assert!(
        probe_success_gate < receipt_write && receipt_write < zip_boundary,
        "only a successful normal packaged probe may publish the receipt before artifacts"
    );
    assert_eq!(
        BUILD_WINDOWS[installer_only..installer_compile]
            .matches("Assert-WindowsWasapi2ProbeReceipt $sourceDir")
            .count(),
        2,
        "installer-only mode must verify the hash-bound capability before and after source checks"
    );
    assert!(
        WINDOWS_RUNTIME_PROBE.contains("bundled_factory(\"wasapi2sink\", &canonical_plugin_dir)?")
            && WINDOWS_RUNTIME_PROBE
                .contains("windows_audio::configure_wasapi2_sink(&wasapi2_sink)"),
        "the packaged runtime probe must verify dynamic WASAPI2 recovery"
    );
    assert!(
        WINDOWS_AUDIO.contains("property.flags().contains(glib::ParamFlags::WRITABLE)")
            && WINDOWS_AUDIO.contains("sink.set_property(\"continue-on-error\", true)")
            && WINDOWS_AUDIO.contains("DeviceChanged(changed) => Some(changed.device())")
            && WINDOWS_AUDIO.contains("error.matches(gst::ResourceError::Write)")
            && WINDOWS_AUDIO.contains("claim_warning_recovery(recovery_claimed)")
            && WINDOWS_AUDIO.contains("recovery_claimed.set(false)"),
        "the Windows audio path must feature-detect live switching and bound warning recovery"
    );
}

/// Guard the macOS package and probe contracts for the app-owned output route,
/// including its required plugins, native dependencies, and channel caps.
#[test]
fn macos_bundle_requires_app_owned_system_audio_output_support() {
    let manifest = manifest();
    let macos_dependencies = &manifest["target"]["cfg(target_os = \"macos\")"]["dependencies"];

    assert!(
        macos_dependencies["objc2-core-audio"].is_table()
            && macos_dependencies["block2"].is_table(),
        "CoreAudio output notifications must remain target-only macOS dependencies"
    );
    assert!(
        BUILD_MACOS.contains(
            "if ! macos_validate_audio_runtime_inventory \"$GST_PLUGIN_DEST\" \"$FRAMEWORKS_DIR\"; then"
        ) && MACOS_PACKAGE_POLICY.contains(
            "for plugin in libgstcoreelements libgstosxaudio libgstplayback libgstsoup; do"
        ),
        "the macOS bundle must fail closed when its explicit route elements are absent"
    );
    assert!(
        PLATFORM_RUNTIME.contains("gstreamer::ElementFactory::find(\"identity\").is_none()")
            && PLATFORM_RUNTIME
                .contains("required bundled GStreamer identity factory was not discovered")
            && PLATFORM_RUNTIME
                .contains("gstreamer::ElementFactory::find(\"capsfilter\").is_none()")
            && PLATFORM_RUNTIME
                .contains("required bundled GStreamer capsfilter factory was not discovered")
            && PLATFORM_RUNTIME
                .contains("gstreamer::ElementFactory::find(\"osxaudiosink\").is_none()")
            && PLATFORM_RUNTIME
                .contains("required bundled GStreamer osxaudiosink factory was not discovered"),
        "the signed macOS bundle must discover the complete route through its isolated runtime"
    );

    let configured_call = MACOS_AUDIO
        .find("let sink = match configured_sink_bin()")
        .expect("the app must construct its configured sink");
    let sink_publish = MACOS_AUDIO
        .find("playbin.set_property(\"audio-sink\", &sink.bin)")
        .expect("playbin must receive the configured sink");
    let configured_definition = MACOS_AUDIO
        .find("fn configured_sink_bin()")
        .expect("the configured sink constructor must exist");
    let cap_build = MACOS_AUDIO[configured_definition..]
        .find("let channel_caps = cap_raw_audio_channels(native_pad.pad_template_caps())")
        .map(|offset| configured_definition + offset)
        .expect("the channel guard must derive from the native template caps");
    let filter_build = MACOS_AUDIO[cap_build..]
        .find("ElementFactory::make(CHANNEL_FILTER_FACTORY)")
        .map(|offset| cap_build + offset)
        .expect("the app-owned route must construct its channel capsfilter");
    let filter_configure = MACOS_AUDIO[filter_build..]
        .find("channel_filter.set_property(\"caps\", &channel_caps)")
        .map(|offset| filter_build + offset)
        .expect("the capsfilter must receive the narrowed native template");
    let filter_link = MACOS_AUDIO[filter_configure..]
        .find("channel_filter.link(&native)")
        .map(|offset| filter_configure + offset)
        .expect("the channel guard must remain directly upstream of the native sink");
    let configured_return = MACOS_AUDIO[filter_link..]
        .find("Ok(ConfiguredSinkBin")
        .map(|offset| filter_link + offset)
        .expect("the configured sink must be returned");
    assert!(
        configured_call < sink_publish
            && configured_definition < cap_build
            && cap_build < filter_build
            && filter_build < filter_configure
            && filter_configure < filter_link
            && filter_link < configured_return
            && MACOS_AUDIO.contains("gst::PadProbeType::IDLE")
            && MACOS_AUDIO.contains("gst::PadProbeType::BLOCK_DOWNSTREAM")
            && MACOS_AUDIO.contains(
                "gst::PadProbeType::QUERY_DOWNSTREAM | gst::PadProbeType::PULL"
            )
            && MACOS_AUDIO_TESTS.contains("route_gate_stays_flow_blocking_until_removed")
            && !MACOS_AUDIO.contains("gst::Pad::query_default")
            && !MACOS_AUDIO_NATIVE.contains("gst::Pad::query_default")
            && MACOS_AUDIO_NATIVE
                .contains("sink.set_property(\"device\", CURRENT_DEFAULT_DEVICE)")
            && MACOS_AUDIO_NATIVE.contains("sink.sync_state_with_parent()"),
        "every app-owned sink must be filtered before publication, reopen on the full-width current default, and retain a safe guarded fallback"
    );
}

#[test]
fn windows_bundle_validates_the_completed_zip_and_ci_parser() {
    let build_windows = BUILD_WINDOWS.replace("\r\n", "\n");
    let windows_ci = workflow_job(CI_WORKFLOW, "build-windows");
    let archive = build_windows
        .find("Write-Info \"Creating zip archive...\"")
        .expect("the Windows ZIP boundary must exist");
    let archive_section = build_windows
        .find("# ── Zip Archive")
        .expect("the Windows ZIP section must exist");
    let final_component_assertion = build_windows[archive_section..]
        .find("Assert-WindowsBundleComponentPolicy $DIST")
        .map(|offset| archive_section + offset)
        .expect("the final dist tree must pass the filename/reparse policy");
    let final_pe_assertion = build_windows[archive_section..]
        .find("Assert-WindowsBundlePeImportPolicy $DIST $peImportInspector")
        .map(|offset| archive_section + offset)
        .expect("the final dist tree must pass a fresh PE import inspection");
    assert!(
        final_component_assertion < final_pe_assertion && final_pe_assertion < archive,
        "both final source-tree gates must run after the runtime probe and before ZIP creation"
    );
    let zip_creation = build_windows
        .find("Compress-Archive -Path $DIST -DestinationPath $zipPath")
        .expect("the ZIP creation call must remain recognizable");
    let zip_validation = build_windows
        .find("Assert-WindowsZipComponentPolicy $zipPath")
        .expect("the completed ZIP must be reopened for validation");
    assert!(
        zip_creation < zip_validation
            && build_windows.contains("[System.IO.Compression.ZipFile]::OpenRead")
            && build_windows.contains("Test-ForbiddenBundledRelativePath $entryPath"),
        "the completed ZIP entry names must pass the shared component policy"
    );
    assert!(
        build_windows.contains("function Assert-WindowsBundlePeImportPolicy")
            && build_windows.contains("$targetItems = @(Get-WindowsTreeMembersWithoutReparseTraversal $rootFull")
            && build_windows.contains("$stream.ReadByte() -ne 0x4D")
            && build_windows.contains("$stream.ReadByte() -ne 0x5A")
            && build_windows.contains("Invoke-BoundedPeImportBatch")
            && build_windows.contains(
                "Final PE import inspector returned an unsupported dependency spelling"
            )
            && build_windows.contains("$targetSnapshot.ContainsKey($finalPath)"),
        "the final gate must inspect every hidden DLL/DRV/EXE as bounded PE data, reject malformed imports, and detect a changing target set"
    );
    assert!(
        windows_ci.contains("name: Parse bundler with Windows PowerShell 5.1")
            && windows_ci.contains("if: matrix.arch == 'x86_64'")
            && windows_ci.contains("shell: powershell")
            && windows_ci.contains("System.Management.Automation.Language.Parser]::ParseFile")
            && windows_ci.contains("if (@($parseErrors).Count -gt 0)"),
        "Windows CI must prove that the bundler parses under inbox Windows PowerShell 5.1"
    );
}

#[test]
fn windows_artifacts_fail_closed_on_missing_application_resources() {
    let build_windows = BUILD_WINDOWS.replace("\r\n", "\n");
    let windows_ci = workflow_job(CI_WORKFLOW, "build-windows");
    let windows_release = workflow_job(RELEASE_WORKFLOW, "windows");

    let archive_section = build_windows
        .find("# ── Zip Archive")
        .expect("the Windows ZIP section must exist");
    let final_import_assertion = build_windows[archive_section..]
        .find("Assert-WindowsBundlePeImportPolicy $DIST $peImportInspector")
        .map(|offset| archive_section + offset)
        .expect("the final import gate must remain recognizable");
    let final_resource_assertion = build_windows[archive_section..]
        .find("Assert-WindowsApplicationResourceContract `")
        .map(|offset| archive_section + offset)
        .expect("the final executable resource gate must exist");
    let archive = build_windows
        .find("Write-Info \"Creating zip archive...\"")
        .expect("the Windows ZIP boundary must exist");
    assert!(
        final_import_assertion < final_resource_assertion && final_resource_assertion < archive,
        "the copied application must pass its resource contract after all writers and before ZIP creation"
    );

    let installer_only = build_windows
        .find("# ── Inno Setup only mode")
        .expect("the installer-only path must exist");
    let installer_resource_assertion = build_windows[installer_only..]
        .find("-Application (Join-Path $sourceDir \"tributary.exe\")")
        .map(|offset| installer_only + offset)
        .expect("installer-only mode must revalidate the application resources");
    let installer_compile = build_windows[installer_resource_assertion..]
        .find("& $iscc")
        .map(|offset| installer_resource_assertion + offset)
        .expect("the installer compiler invocation must remain recognizable");
    assert!(
        installer_resource_assertion < installer_compile,
        "a stale installer source tree must fail before Inno Setup runs"
    );

    for fragment in [
        "function Invoke-BoundedPeResourceInspection",
        "function Assert-WindowsApplicationResourceContract",
        "$arguments = '--coff-resources \"' + $Application + '\"'",
        "$outputByteLimit = 8388608",
        "$processDeadlineMs = 45000",
        "\"3\" = \"ICON\"",
        "\"14\" = \"GROUP_ICON\"",
        "\"16\" = \"VERSIONINFO\"",
        "\"3\" = 6",
        "\"14\" = 1",
        "\"16\" = 1",
        "DataSize:",
        "Name:\\s*\\(ID\\s+([0-9]+)\\)",
        "$groupIconBytes.Add(",
        "$groupIconBytes.Count -ne [int64]$groupIconDeclaredSize",
        "$groupIconReserved -ne 0 -or $groupIconType -ne 1",
        "$groupIconEntryCount -ne 6",
        "$groupIconPayload.Length -ne $expectedGroupIconSize",
        "$groupIconResourceIds.ContainsKey($iconResourceIdKey)",
        "-not $iconDataSizes.ContainsKey($iconResourceIdKey)",
        "[uint64]$bytesInResource -ne [uint64]$iconDataSizes[$iconResourceIdKey]",
        "$groupIconResourceIds.Count -ne $iconDataSizes.Count",
        "[System.Diagnostics.FileVersionInfo]::GetVersionInfo",
        "$versionInfo.ProductName -cne \"Tributary\"",
        "$versionInfo.FileVersion -cne $ExpectedVersion",
        "$finalSnapshot -ne $applicationSnapshot",
    ] {
        assert!(
            build_windows.contains(fragment),
            "the Windows application-resource gate is missing its contract: {fragment}"
        );
    }

    for (workflow, label) in [(windows_ci, "CI"), (windows_release, "release")] {
        assert!(
            workflow.contains("arch: x86_64")
                && workflow.contains("arch: aarch64")
                && workflow.contains("name: Bundle DLLs, validate app resources, and create zip")
                && workflow.contains("pwsh -File scripts/build-windows.ps1"),
            "the shared fail-closed bundler must run for both {label} Windows architectures"
        );
    }
}

#[test]
fn windows_resources_are_linked_to_the_application_binary() {
    let manifest = manifest();
    let windows_build_dependencies =
        &manifest["target"]["cfg(target_os = \"windows\")"]["build-dependencies"];

    assert_eq!(
        windows_build_dependencies["winresource"].as_str(),
        Some("0.1"),
        "winresource must remain the canonical dynamic icon/version resource generator"
    );
    assert_eq!(
        windows_build_dependencies["embed-resource"].as_str(),
        Some("3.0"),
        "embed-resource must provide mixed-package binary-scoped linkage"
    );
    for fragment in [
        "res.write_resource_file(&resource_file)",
        "embed_resource::compile_for(&resource_file, [\"tributary\"], embed_resource::NONE)",
        ".manifest_required()",
        "manifest_dir.join(\"data/tributary.ico\")",
    ] {
        assert!(
            BUILD_SCRIPT.contains(fragment),
            "Windows resource build is missing its binary-scoped contract: {fragment}"
        );
    }
    assert!(
        !BUILD_SCRIPT.contains("res.compile()"),
        "winresource's package-wide link directive must not return while a library target exists"
    );
}

#[test]
fn rust_api_features_meet_the_supported_native_runtime_floors() {
    let manifest = manifest();

    assert_eq!(dependency_api_floor(&manifest, "gtk", (4, 16)), "4.16");
    assert_eq!(dependency_api_floor(&manifest, "adw", (1, 6)), "1.6");
}

#[test]
fn debian_runtime_floors_match_the_enabled_api_levels() {
    let manifest = manifest();
    let gtk_floor = dependency_api_floor(&manifest, "gtk", (4, 16));
    let adw_floor = dependency_api_floor(&manifest, "adw", (1, 6));
    let depends = manifest["package"]["metadata"]["deb"]["depends"]
        .as_str()
        .expect("package.metadata.deb.depends must be a string");
    let entries: Vec<_> = depends.split(',').map(str::trim).collect();

    let gtk_expected = format!("libgtk-4-1 (>= {gtk_floor})");
    let adw_expected = format!("libadwaita-1-0 (>= {adw_floor})");
    assert_exact_constraint(
        &entries,
        "libgtk-4-1",
        &gtk_expected,
        "Cargo.toml package.metadata.deb.depends",
    );
    assert_exact_constraint(
        &entries,
        "libadwaita-1-0",
        &adw_expected,
        "Cargo.toml package.metadata.deb.depends",
    );
}

#[test]
fn generated_rpm_runtime_floors_match_the_enabled_api_levels() {
    let manifest = manifest();
    let gtk_expected = format!(">= {}", dependency_api_floor(&manifest, "gtk", (4, 16)));
    let adw_expected = format!(">= {}", dependency_api_floor(&manifest, "adw", (1, 6)));
    let requires = manifest["package"]["metadata"]["generate-rpm"]["requires"]
        .as_table()
        .expect("package.metadata.generate-rpm.requires must be a table");

    assert_eq!(requires["gtk4"].as_str(), Some(gtk_expected.as_str()));
    assert_eq!(requires["libadwaita"].as_str(), Some(adw_expected.as_str()));
}

#[test]
fn handwritten_rpm_build_and_runtime_floors_match_the_enabled_api_levels() {
    let manifest = manifest();
    let gtk_floor = dependency_api_floor(&manifest, "gtk", (4, 16));
    let adw_floor = dependency_api_floor(&manifest, "adw", (1, 6));
    let runtime: Vec<_> = RPM_SPEC
        .lines()
        .filter_map(|line| line.strip_prefix("Requires:"))
        .map(str::trim)
        .collect();
    let build: Vec<_> = RPM_SPEC
        .lines()
        .filter_map(|line| line.strip_prefix("BuildRequires:"))
        .map(str::trim)
        .collect();

    let gtk_runtime = format!("gtk4 >= {gtk_floor}");
    let adw_runtime = format!("libadwaita >= {adw_floor}");
    let gtk_build = format!("pkgconfig(gtk4) >= {gtk_floor}");
    let adw_build = format!("pkgconfig(libadwaita-1) >= {adw_floor}");
    assert_exact_constraint(&runtime, "gtk4", &gtk_runtime, "RPM Requires");
    assert_exact_constraint(&runtime, "libadwaita", &adw_runtime, "RPM Requires");
    assert_exact_constraint(&build, "pkgconfig(gtk4)", &gtk_build, "RPM BuildRequires");
    assert_exact_constraint(
        &build,
        "pkgconfig(libadwaita-1)",
        &adw_build,
        "RPM BuildRequires",
    );
}

#[test]
fn arch_runtime_floors_match_the_enabled_api_levels() {
    let manifest = manifest();
    let gtk_expected = format!("gtk4>={}", dependency_api_floor(&manifest, "gtk", (4, 16)));
    let adw_expected = format!(
        "libadwaita>={}",
        dependency_api_floor(&manifest, "adw", (1, 6))
    );
    let dependencies = shell_array(ARCH_PKGBUILD, "depends");

    assert_exact_constraint(&dependencies, "gtk4", &gtk_expected, "PKGBUILD depends");
    assert_exact_constraint(
        &dependencies,
        "libadwaita",
        &adw_expected,
        "PKGBUILD depends",
    );
}

#[test]
fn desktop_exec_passes_all_opened_uris_to_tributary() {
    assert_eq!(desktop_value("Exec"), "tributary %U");
}

#[test]
fn desktop_categories_include_the_required_audio_video_main_category() {
    let categories: Vec<_> = desktop_value("Categories")
        .split(';')
        .filter(|category| !category.is_empty())
        .collect();

    assert_exact_constraint(
        &categories,
        "AudioVideo",
        "AudioVideo",
        "desktop Categories",
    );
}

#[test]
fn ci_compile_proves_the_exact_declared_msrv() {
    let manifest = manifest();
    let rust_version = manifest["package"]["rust-version"]
        .as_str()
        .expect("package.rust-version must be a string");
    let rust_release = format!("{rust_version}.0");
    let toolchain_manifest: Value =
        toml::from_str(RUST_TOOLCHAIN_MANIFEST).expect("rust-toolchain.toml must parse");
    let normalized_workflow = CI_WORKFLOW.replace("\r\n", "\n");
    let msrv_job = workflow_job(&normalized_workflow, "msrv");
    let crlf_workflow = normalized_workflow.lines().collect::<Vec<_>>().join("\r\n");
    let crlf_msrv_job = workflow_job(&crlf_workflow, "msrv");

    assert!(
        rust_version.split_once('.').is_some_and(
            |(major, minor)| major.parse::<u32>().is_ok() && minor.parse::<u32>().is_ok()
        ),
        "package.rust-version must use canonical X.Y form"
    );
    assert!(
        crlf_msrv_job.contains("name: MSRV\r\n"),
        "CI workflow contract checks must accept Windows CRLF checkouts"
    );
    assert!(
        msrv_job.contains("name: MSRV\n")
            && !msrv_job.contains(&format!("name: MSRV ({rust_version})")),
        "CI job name must remain stable for branch rules and GasCity"
    );
    assert!(
        toolchain_manifest["toolchain"]["channel"].as_str() == Some(&rust_release)
            && msrv_job.contains(&format!(
                "uses: dtolnay/rust-toolchain@{RUST_TOOLCHAIN_ACTION_SHA} # master"
            ))
            && msrv_job.contains(&format!("toolchain: {rust_release}")),
        "the compiler manifest and CI must install the declared release through one immutable action commit"
    );
    assert!(
        msrv_job.contains("run: cargo check --all-targets --locked"),
        "CI must compile-check every target against the committed lockfile"
    );
    assert!(
        README.contains(&format!("Rust {rust_version}+"))
            && README.contains(&format!("toolchain install {rust_release}"))
            && README.contains(&format!("cargo +{rust_release} llvm-cov"))
            && README.contains(&format!("pinned to Rust {rust_release}")),
        "README prerequisites and coverage commands must match the declared MSRV"
    );
}

#[test]
fn seaorm_runtime_and_migration_dependencies_move_as_one_unit() {
    let manifest = manifest();
    assert_eq!(
        manifest["dependencies"]["sea-orm"]["version"],
        manifest["dependencies"]["sea-orm-migration"]["version"],
        "SeaORM runtime and migration manifest requirements must match"
    );

    for (name, source) in [("root", ROOT_LOCK), ("fuzz", FUZZ_LOCK)] {
        assert_eq!(
            locked_version(source, "sea-orm"),
            locked_version(source, "sea-orm-migration"),
            "{name} lockfile must resolve SeaORM runtime and migration to one version"
        );
    }
}

#[test]
// The assertions form one policy contract: splitting them would obscure
// whether grouping and auto-merge exclusions remain mutually consistent.
// #lizard forgives
fn dependabot_groups_coupled_updates_and_excludes_toolchains_from_automerge() {
    let config: serde_yaml::Value =
        serde_yaml::from_str(DEPENDABOT_CONFIG).expect("dependabot.yml must parse");
    assert_eq!(config["version"].as_u64(), Some(2));

    let root = dependabot_update(&config, "cargo", "/");
    let root_groups = root["groups"]
        .as_mapping()
        .expect("root Cargo groups must be a mapping");
    assert!(
        !root_groups.is_empty(),
        "root Cargo groups must not be empty"
    );
    let seaorm = &root["groups"]["seaorm"];
    assert_eq!(
        yaml_string_list(seaorm, "patterns"),
        ["sea-orm", "sea-orm-migration"]
    );
    assert!(
        seaorm.get("update-types").is_none(),
        "the SeaORM pair must stay grouped for majors as well as routine updates"
    );
    let seaorm_security = &root["groups"]["seaorm-security"];
    assert_eq!(
        seaorm_security["applies-to"].as_str(),
        Some("security-updates")
    );
    assert_eq!(
        yaml_string_list(seaorm_security, "patterns"),
        ["sea-orm", "sea-orm-migration"]
    );

    let routine_cargo = &root["groups"]["cargo-minor-and-patch"];
    assert_eq!(
        yaml_string_list(routine_cargo, "update-types"),
        ["minor", "patch"]
    );
    assert_eq!(
        yaml_string_list(routine_cargo, "exclude-patterns"),
        ["sea-orm", "sea-orm-migration"]
    );

    let fuzz = dependabot_update(&config, "cargo", "/fuzz");
    let fuzz_seaorm_security = &fuzz["groups"]["seaorm-security"];
    assert_eq!(
        fuzz_seaorm_security["applies-to"].as_str(),
        Some("security-updates")
    );
    assert_eq!(
        yaml_string_list(fuzz_seaorm_security, "patterns"),
        ["sea-orm", "sea-orm-migration"]
    );
    let fuzz_group = &fuzz["groups"]["fuzz-minor-and-patch"];
    assert_eq!(
        yaml_string_list(fuzz_group, "update-types"),
        ["minor", "patch"]
    );

    let compiler = dependabot_update(&config, "rust-toolchain", "/.github");
    assert_eq!(compiler["open-pull-requests-limit"].as_u64(), Some(1));

    let actions = dependabot_update(&config, "github-actions", "/");
    actions["groups"]
        .as_mapping()
        .expect("Actions groups must be a mapping");
    let toolchain = &actions["groups"]["rust-toolchain"];
    assert_eq!(
        yaml_string_list(toolchain, "patterns"),
        ["dtolnay/rust-toolchain"]
    );
    assert!(
        toolchain.get("update-types").is_none() && actions.get("ignore").is_none(),
        "rust-toolchain action-code updates must remain enabled at every level"
    );
    let routine_actions = &actions["groups"]["actions-minor-and-patch"];
    assert_eq!(
        yaml_string_list(routine_actions, "exclude-patterns"),
        ["dtolnay/rust-toolchain", "dependabot/fetch-metadata"]
    );
    let metadata_action = &actions["groups"]["dependabot-metadata"];
    assert_eq!(
        yaml_string_list(metadata_action, "patterns"),
        ["dependabot/fetch-metadata"]
    );
    assert_eq!(
        metadata_action["applies-to"].as_str(),
        Some("version-updates")
    );
    let metadata_action_security = &actions["groups"]["dependabot-metadata-security"];
    assert_eq!(
        yaml_string_list(metadata_action_security, "patterns"),
        ["dependabot/fetch-metadata"]
    );
    assert_eq!(
        metadata_action_security["applies-to"].as_str(),
        Some("security-updates")
    );
}

fn dependabot_automerge_workflow() -> serde_yaml::Value {
    serde_yaml::from_str(DEPENDABOT_AUTOMERGE).expect("Dependabot auto-merge workflow must parse")
}

// The full policy check-context set the live main ruleset must require, each
// entry as "check context|app id" (empty app id = unbound commit status).
// The Bot Review Gate is required from the dedicated gate-publisher App —
// never from the shared GitHub Actions integration (15368), whose check runs
// any pull-request-controlled workflow job can publish under any name — so
// its app id comes from the repository variable the precondition reads.
const fn required_policy_check_contexts() -> [&'static str; 18] {
    [
        "Security Audit|15368",
        "Linux (x86_64)|15368",
        "Linux (aarch64)|15368",
        "macOS (aarch64)|15368",
        "Windows (x86_64)|15368",
        "Windows (aarch64)|15368",
        "Flatpak (Linux)|15368",
        "MSRV|15368",
        "Coverage (Linux x86_64)|15368",
        "Desktop Metadata|15368",
        "SHA256 Checksums|15368",
        "Bot Review Gate|${GATE_PUBLISHER_APP_ID}",
        "CodeQL|57789",
        "Analyze (python)|57789",
        "Analyze (rust)|57789",
        "Analyze (actions)|57789",
        "Codacy Static Code Analysis|56611",
        "CodeRabbit|",
    ]
}

#[test]
fn dependabot_automerge_inspection_and_metadata_stay_read_only_and_head_bound() {
    let workflow = dependabot_automerge_workflow();
    assert!(
        workflow["permissions"]
            .as_mapping()
            .is_some_and(serde_yaml::Mapping::is_empty),
        "the workflow default token must have no permissions"
    );
    let inspect = &workflow["jobs"]["inspect_changed_files"];
    assert_eq!(
        inspect["permissions"]["pull-requests"].as_str(),
        Some("read")
    );
    assert!(
        inspect["permissions"].get("contents").is_none(),
        "changed-file inspection must not receive content write permission"
    );
    let inspect_steps = inspect["steps"]
        .as_sequence()
        .expect("changed-file inspection steps must be a sequence");
    assert!(
        inspect_steps
            .first()
            .is_some_and(|step| step.get("uses").is_none()),
        "changed-file and exact-head denial must be inline and action-free"
    );
    assert!(
        inspect_steps.iter().any(|step| {
            step.get("uses").and_then(serde_yaml::Value::as_str)
                == Some("dependabot/fetch-metadata@25dd0e34f4fe68f24cc83900b1fe3fe149efef98")
                && step.get("if").and_then(serde_yaml::Value::as_str)
                    == Some("steps.pre_metadata_head.outputs.matches == 'true'")
        }),
        "metadata extraction must run only after a fresh read-only exact-head preflight"
    );
}

#[test]
// These assertions jointly prove one privileged boundary and should fail as a
// unit if an action, permission, concurrency rule, or exact-head guard regresses.
// #lizard forgives
fn dependabot_automerge_writer_is_action_free_concurrent_and_exact_head_guarded() {
    let workflow = dependabot_automerge_workflow();
    let writer = &workflow["jobs"]["dependabot-automerge"];
    assert_eq!(writer["needs"].as_str(), Some("inspect_changed_files"));
    assert_eq!(writer["permissions"]["contents"].as_str(), Some("write"));
    assert_eq!(
        writer["permissions"]["pull-requests"].as_str(),
        Some("write")
    );
    assert!(
        writer["if"]
            .as_str()
            .is_some_and(|condition| condition.contains(
                "needs.inspect_changed_files.outputs.privileged_workflow_unchanged == 'true'"
            )),
        "the write job must depend on an affirmative read-only inspection result"
    );
    let writer_steps = writer["steps"]
        .as_sequence()
        .expect("write job steps must be a sequence");
    // Exactly one action may appear in the write job: the pinned GitHub-org
    // app-token minter. It executes no repository code — it only signs a JWT
    // and exchanges it for an installation token — and checkout remains
    // forbidden; anything else reintroduces an unreviewed execution context
    // into the only job that can enable auto-merge.
    let used_steps: Vec<&serde_yaml::Value> = writer_steps
        .iter()
        .filter(|step| step.get("uses").is_some())
        .collect();
    assert_eq!(
        used_steps.len(),
        1,
        "the write-capable job must contain exactly one action: the pinned ruleset-reader token minter"
    );
    assert!(
        used_steps.first().is_some_and(|step| step["uses"].as_str()
            == Some("actions/create-github-app-token@29824e69f54612133e76f7eaac726eef6c875baf")),
        "the token minter must be the GitHub-org action pinned to its full commit SHA"
    );
    assert_eq!(
        writer_steps.len(),
        3,
        "the write-capable job must contain only the token mint, the live-ruleset precondition, and the guarded merge command"
    );
    assert_eq!(
        workflow["concurrency"]["cancel-in-progress"].as_bool(),
        Some(true),
        "a newer revision of one PR must cancel its stale automation run"
    );
    assert!(
        workflow["concurrency"]["group"]
            .as_str()
            .is_some_and(|group| group.contains("github.event.pull_request.number")),
        "workflow concurrency must be scoped to the exact pull request"
    );

    assert!(
        DEPENDABOT_AUTOMERGE.contains("pull_request:")
            && !DEPENDABOT_AUTOMERGE.contains("\non: pull_request_target\n")
            && !DEPENDABOT_AUTOMERGE.contains("actions/checkout")
            && DEPENDABOT_AUTOMERGE.contains("gh api --paginate")
            && DEPENDABOT_AUTOMERGE.contains("github.event.pull_request.changed_files")
            && DEPENDABOT_AUTOMERGE.contains("github.event.pull_request.head.sha")
            && DEPENDABOT_AUTOMERGE.contains("observed_head_before")
            && DEPENDABOT_AUTOMERGE.contains("observed_head_after")
            && DEPENDABOT_AUTOMERGE.contains("pre_metadata_head")
            && DEPENDABOT_AUTOMERGE.contains("metadata_head")
            && DEPENDABOT_AUTOMERGE.contains("observed_head")
            && DEPENDABOT_AUTOMERGE.contains("--match-head-commit")
            && DEPENDABOT_AUTOMERGE.contains(".previous_filename")
            && DEPENDABOT_AUTOMERGE.contains("observed_changed_files")
            && DEPENDABOT_AUTOMERGE.contains(
                "dependabot/fetch-metadata@25dd0e34f4fe68f24cc83900b1fe3fe149efef98"
            )
            && !DEPENDABOT_AUTOMERGE.contains("dependabot/fetch-metadata@v3")
            && DEPENDABOT_AUTOMERGE
                .contains("\".github/workflows/dependabot-automerge.yml\"")
            && DEPENDABOT_AUTOMERGE.contains("github.actor == 'dependabot[bot]'")
            && DEPENDABOT_AUTOMERGE
                .contains("github.event.pull_request.user.login == 'dependabot[bot]'")
            && DEPENDABOT_AUTOMERGE.contains("github.repository == 'jm2/tributary'")
            && DEPENDABOT_AUTOMERGE.contains(
                "package_ecosystem: ${{ steps.meta.outputs.package-ecosystem }}"
            )
            && DEPENDABOT_AUTOMERGE.contains(
                "needs.inspect_changed_files.outputs.package_ecosystem != 'rust-toolchain'"
            )
            && DEPENDABOT_AUTOMERGE.contains(
                "!contains(needs.inspect_changed_files.outputs.dependency_names, 'dtolnay/rust-toolchain')"
            )
            && DEPENDABOT_AUTOMERGE.contains(
                "!contains(needs.inspect_changed_files.outputs.dependency_names, 'dependabot/fetch-metadata')"
            ),
        "Dependabot automation must be pinned, checkout-free, exact-head API-preflighted, narrowly admitted, race-contained, mixed-path self-update-safe, and refuse toolchain auto-merge"
    );
}

fn bot_review_gate_workflow() -> serde_yaml::Value {
    serde_yaml::from_str(BOT_REVIEW_GATE).expect("bot review gate announcer workflow must parse")
}

fn bot_review_gate_publisher_workflow() -> serde_yaml::Value {
    serde_yaml::from_str(BOT_REVIEW_GATE_PUBLISHER)
        .expect("bot review gate publisher workflow must parse")
}

fn bot_review_gate_run_script(workflow: &serde_yaml::Value) -> String {
    let job_steps = workflow["jobs"]["publish"]["steps"]
        .as_sequence()
        .expect("publisher steps must be a sequence");
    assert_eq!(
        job_steps.len(),
        2,
        "the publisher must stay two steps: the pinned app-token mint and the inline publication"
    );
    // The one allowed action is the GitHub-org app-token minter, pinned to a
    // full commit SHA; it executes no repository code. The publication step
    // itself stays inline and action-free.
    assert!(
        job_steps[0]["uses"].as_str()
            == Some("actions/create-github-app-token@29824e69f54612133e76f7eaac726eef6c875baf"),
        "the publisher's first step must mint the gate-publisher App token with the pinned GitHub-org action"
    );
    assert!(
        job_steps[1..].iter().all(|step| step.get("uses").is_none()),
        "the publication step must not execute any third-party action or checkout"
    );
    job_steps[1]["run"]
        .as_str()
        .expect("the publisher step must inline its script")
        .to_owned()
}

#[test]
// The announcer runs pull-request-controlled content, so it may contribute
// nothing but refresh events. These assertions jointly prove it declares the
// documented triggers, holds no token scopes, evaluates nothing, and names
// its own check-run so it can never collide with the required context.
fn bot_review_gate_announcer_declares_only_trusted_noop_refresh_events() {
    let workflow = bot_review_gate_workflow();

    // The required-check context belongs to the default-branch publisher;
    // the announcer's job check-run must carry a different name so a
    // pull request can never satisfy the context from this file.
    assert_eq!(
        workflow["jobs"]["bot-review-gate"]["name"].as_str(),
        Some("Bot Review Gate Trigger"),
        "the announcer's check-run name must stay distinct from the required context"
    );

    // YAML 1.1 parses the bare `on:` key as boolean true, so look up both
    // spellings instead of assuming one.
    let on = workflow
        .get("on")
        .or_else(|| workflow.get(serde_yaml::Value::Bool(true)))
        .expect("the announcer workflow must declare its triggers");
    assert_announcer_pull_request_triggers(on);
    assert_announcer_declares_only_documented_refresh_paths(on);
    assert_announcer_holds_no_token_scopes(&workflow);
    assert_announcer_concurrency_never_cancels_a_refresh(&workflow);
    assert_announcer_body_is_a_single_noop_step(&workflow);
}

// The announcer may only fire on pull-request lifecycle events: pushes to
// open pull requests against main, plus the review events that create,
// edit, or withdraw review evidence.
fn assert_announcer_pull_request_triggers(on: &serde_yaml::Value) {
    assert!(
        on.get("pull_request_target").is_none(),
        "the announcer must never use pull_request_target"
    );
    let pull_request_branches = yaml_string_list(&on["pull_request"], "branches");
    assert_eq!(
        pull_request_branches,
        ["main"],
        "the announcer fires only for pull requests targeting main"
    );
    let pull_request_types = yaml_string_list(&on["pull_request"], "types");
    assert_eq!(
        pull_request_types,
        [
            "opened",
            "synchronize",
            "reopened",
            "edited",
            "review_requested",
            "review_request_removed"
        ],
        "every push to a pull request must re-announce the gate at the new head; \
         `edited` covers retargeting — a base change fires edited, never synchronize, \
         and a retarget away-and-back fires no other refresh; \
         `review_requested`/`review_request_removed` re-announce the re-review \
         handshake whose outstanding request invalidates the reviewer's earlier \
         clean result at the head (best-effort coverage: GitHub does not \
         guarantee the Actions event for bot/App requesters or reviewers, so the \
         documented workflow_dispatch re-run stays the guaranteed refresh path)"
    );
    let review_types = yaml_string_list(&on["pull_request_review"], "types");
    assert_eq!(
        review_types,
        ["submitted", "edited", "dismissed"],
        "review conclusions and their formal withdrawal must re-announce the gate"
    );
    let review_comment_types = yaml_string_list(&on["pull_request_review_comment"], "types");
    assert_eq!(
        review_comment_types,
        ["created"],
        "a thread opened by a single review comment (no review submission) must re-announce the gate"
    );
}

// Beyond pull-request events the announcer may only declare the documented
// dispatch refresh path: no webhook pseudo-events, and workflow_run belongs
// to the publisher alone.
fn assert_announcer_declares_only_documented_refresh_paths(on: &serde_yaml::Value) {
    // `pull_request_review_thread` is a webhook event, not an Actions
    // trigger: declaring it here would make the workflow invalid. Thread
    // resolution is instead refreshed by the documented re-run and
    // workflow_dispatch paths.
    assert!(
        on.get("pull_request_review_thread").is_none(),
        "the announcer must declare only documented Actions events, not webhooks"
    );
    assert!(
        on["workflow_dispatch"]["inputs"]["pr_number"]["required"].as_bool() == Some(true),
        "the dispatch refresh path must name exactly the pull request it evaluates"
    );
    assert!(
        on.get("workflow_run").is_none(),
        "only the publisher may react to workflow_run events"
    );
}

// The announcer evaluates nothing, so it must hold no token scopes at all.
fn assert_announcer_holds_no_token_scopes(workflow: &serde_yaml::Value) {
    assert_eq!(
        workflow["permissions"],
        serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
        "the announcer evaluates nothing and must hold no token scopes at all"
    );
}

// A refresh burst must queue, never cancel: a cancelled announcer run is
// terminal and its check run would sit red at the live head forever (no
// later event re-runs it), so any mechanical reader of head check runs —
// the ruleset UI, the refinery guard — would see a failed
// `Bot Review Gate Trigger` on every multi-event burst. The per-PR group
// still serializes duplicates across event kinds, and the publisher's own
// per-head concurrency collapses the resulting burst.
fn assert_announcer_concurrency_never_cancels_a_refresh(workflow: &serde_yaml::Value) {
    assert_eq!(
        workflow["concurrency"]["cancel-in-progress"].as_bool(),
        Some(false),
        "a cancelled announcer check run is a terminal red state at the live head; \
         duplicate refreshes must queue instead"
    );
    assert!(
        workflow["concurrency"]["group"].as_str().is_some_and(
            |group| group.contains("github.event.pull_request.number || inputs.pr_number")
        ),
        "announcer concurrency must be scoped to the exact pull request across event kinds"
    );
}

// The announcer body must be a single trivial step: no checkout, no
// third-party action, and no API evaluation of any kind.
fn assert_announcer_body_is_a_single_noop_step(workflow: &serde_yaml::Value) {
    let job_steps = workflow["jobs"]["bot-review-gate"]["steps"]
        .as_sequence()
        .expect("announcer steps must be a sequence");
    assert_eq!(
        job_steps.len(),
        1,
        "the announcer must stay a single no-op step"
    );
    assert!(
        job_steps.iter().all(|step| step.get("uses").is_none()),
        "the announcer must not execute any third-party action or checkout"
    );
    let run = job_steps[0]["run"]
        .as_str()
        .expect("the announcer step must inline its script");
    assert!(
        !run.contains("gh api") && !run.contains("graphql") && !run.contains("check-runs"),
        "the announcer must not query or publish anything itself"
    );
}

#[test]
// The publisher is the only writer of the required `Bot Review Gate`
// context, and its entire evaluation runs from the default-branch revision
// GitHub executes for `workflow_run` events. These assertions jointly prove
// that trust boundary: the trigger, the permission set, the event fields it
// refuses to trust, and the check-run it publishes.
fn bot_review_gate_publisher_publishes_the_required_context_from_default_branch_content() {
    let workflow = bot_review_gate_publisher_workflow();

    assert_publisher_runs_exclusively_on_announcer_completions(&workflow);
    assert_publisher_declares_exactly_three_read_only_grants(&workflow);
    assert_publisher_binds_the_evaluation_to_the_announcing_head(&workflow);

    // Extracting the run script also proves the step shape: the pinned
    // app-token mint plus exactly one API-only publication step, no
    // checkout, no other actions.
    let script = bot_review_gate_run_script(&workflow);
    assert_publisher_publishes_the_verdict_at_the_evaluated_head(&script);
    assert_publisher_mints_the_gate_app_token(&workflow);
    assert_publication_uses_the_minted_gate_token(&workflow, &script);
}

// The publisher is the only writer of the required context, so it must run
// exclusively on announcer completions: no pull-request, push, or dispatch
// trigger may reach it.
fn assert_publisher_runs_exclusively_on_announcer_completions(workflow: &serde_yaml::Value) {
    let on = workflow
        .get("on")
        .or_else(|| workflow.get(serde_yaml::Value::Bool(true)))
        .expect("the publisher workflow must declare its triggers");
    assert!(
        on.get("pull_request").is_none()
            && on.get("pull_request_target").is_none()
            && on.get("push").is_none()
            && on.get("workflow_dispatch").is_none(),
        "the publisher must run exclusively on announcer completions"
    );
    assert_eq!(
        yaml_string_list(&on["workflow_run"], "workflows"),
        ["Bot review gate"],
        "the publisher must react only to the bot review gate announcer"
    );
    assert_eq!(
        yaml_string_list(&on["workflow_run"], "types"),
        ["completed"],
        "every announcer completion — including a cancellation — must re-evaluate the current state"
    );
}

// Publishing the required check-run is the gate-publisher App's one write;
// the workflow token itself holds only read-only grants and structurally
// cannot publish a check run, so no pull-request-controlled job's identity
// (the shared GitHub Actions integration) can be mistaken for the
// publisher's.
fn assert_publisher_declares_exactly_three_read_only_grants(workflow: &serde_yaml::Value) {
    let permissions = workflow["permissions"]
        .as_mapping()
        .expect("publisher permissions must be a mapping");
    assert_eq!(
        permissions.len(),
        3,
        "the publisher must declare exactly its three workflow-level permissions"
    );
    assert!(
        workflow["permissions"].get("checks").is_none(),
        "the workflow token must structurally be unable to publish check runs; \
         the required context is published only under the minted gate-publisher App token"
    );
    assert_eq!(
        workflow["permissions"]["pull-requests"].as_str(),
        Some("read"),
        "review evidence is read through a read-only grant"
    );
    assert_eq!(
        workflow["permissions"]["contents"].as_str(),
        Some("read"),
        "the substitution policy file is read through a read-only grant"
    );
    assert_eq!(
        workflow["permissions"]["actions"].as_str(),
        Some("read"),
        "the announcing-run record fallback (head recovery) is read through \
         a read-only grant; without it the recovery call is refused and the \
         refresh would skip the supersession it owes the announced head"
    );
}

// The required context must be bound to an identity no pull-request job can
// produce. The publisher's first step mints the dedicated gate-publisher App
// token with the pinned GitHub-org action.
fn assert_publisher_mints_the_gate_app_token(workflow: &serde_yaml::Value) {
    let steps = workflow["jobs"]["publish"]["steps"]
        .as_sequence()
        .expect("publisher steps must be a sequence");
    let mint = steps.first().expect("the mint step must exist");
    assert_eq!(
        mint["name"].as_str(),
        Some("Mint the gate-publisher App token"),
        "the gate-publisher App token must be minted in its own step"
    );
    assert_eq!(
        mint["uses"].as_str(),
        Some("actions/create-github-app-token@29824e69f54612133e76f7eaac726eef6c875baf"),
        "the minter must be the GitHub-org action pinned to its full commit SHA"
    );
    assert_eq!(
        mint["with"]["app-id"].as_str(),
        Some("${{ secrets.BOT_REVIEW_GATE_APP_ID }}"),
        "the gate-publisher App id must come from a repository secret"
    );
    assert_eq!(
        mint["with"]["private-key"].as_str(),
        Some("${{ secrets.BOT_REVIEW_GATE_PRIVATE_KEY }}"),
        "the gate-publisher App key must come from a repository secret"
    );
    assert_eq!(
        mint["with"]["permission-checks"].as_str(),
        Some("write"),
        "the minted token must carry exactly the checks:write publication grant"
    );
}

// Publication happens exclusively under the minted gate-publisher App token,
// and the publisher refuses to publish anything when that identity is
// missing — a verdict from the shared GitHub Actions integration would be
// forgeable by any pull-request-controlled job.
fn assert_publication_uses_the_minted_gate_token(workflow: &serde_yaml::Value, script: &str) {
    let steps = workflow["jobs"]["publish"]["steps"]
        .as_sequence()
        .expect("publisher steps must be a sequence");
    let publish = steps.get(1).expect("the publication step must exist");
    assert_eq!(
        publish["env"]["GATE_TOKEN"].as_str(),
        Some("${{ steps.gate_publisher_token.outputs.token }}"),
        "the publication step must authenticate with the minted gate-publisher App token"
    );
    assert!(
        script.contains("GH_TOKEN=\"${GATE_TOKEN}\" gh api"),
        "the shared verdict must be published under the gate-publisher App token, never the workflow token"
    );
    assert!(
        script.contains("[ -z \"${GATE_TOKEN:-}\" ]"),
        "publication must refuse when the gate-publisher App identity is missing (fail closed)"
    );
    assert!(
        script.contains("refusing to publish the required context under any shared identity"),
        "the identity refusal must explain the forged-context threat"
    );
}

// The event's pull-request fields are derived from branch-name matches
// and are never trusted: the binding head comes from the announcing
// run's commit, and the pull requests are re-derived through the API.
fn assert_publisher_binds_the_evaluation_to_the_announcing_head(workflow: &serde_yaml::Value) {
    let steps = workflow["jobs"]["publish"]["steps"]
        .as_sequence()
        .expect("publisher steps must be a sequence");
    let publish = steps.get(1).expect("the publication step must exist");
    assert!(
        publish["env"]["EVENT_HEAD_SHA"].as_str()
            == Some("${{ github.event.workflow_run.head_sha }}"),
        "the evaluated head must be the announcing run's head commit"
    );
    assert!(
        publish["env"]["EVENT_RUN_ID"].as_str() == Some("${{ github.event.workflow_run.id }}"),
        "a completion event without a head commit must allow recovering it from the \
         announcing run record, or a dead refresh would leave a stale verdict standing"
    );
    assert!(
        !publish["run"]
            .as_str()
            .unwrap_or_default()
            .contains("github.event.workflow_run.pull_requests"),
        "the event's pull-request fields must never be used as authority"
    );
    assert_eq!(
        workflow["concurrency"]["cancel-in-progress"].as_bool(),
        Some(true),
        "a newer announcement for the same head must cancel the stale evaluation"
    );
    assert!(
        workflow["concurrency"]["group"]
            .as_str()
            .is_some_and(|group| group.contains("github.event.workflow_run.head_sha")),
        "publisher concurrency must be scoped to the exact announcing head"
    );
}

// Pull-request discovery is API-derived from the announcing run's commit,
// filtered to open main pull requests actually HEADED by that commit (the
// association endpoint also returns stacked descendants that merely contain
// it), and the published verdict is one shared check-run under the required
// context name — opened in progress before any evaluation and finalized by
// the same refresh — bound to the evaluated head.
fn assert_publisher_publishes_the_verdict_at_the_evaluated_head(script: &str) {
    assert_pull_requests_are_rederived_from_the_announced_head(script);
    assert_one_shared_verdict_is_published_per_announced_head(script);
    assert_the_required_context_is_opened_in_progress_and_finalized(script);
}

// Discovery: the candidate set comes from the announcer's exact commit,
// restricted to open pull requests against main and matched by exact head
// SHA (case-normalized), never from event-supplied pull-request fields.
fn assert_pull_requests_are_rederived_from_the_announced_head(script: &str) {
    assert!(
        script.contains("commits/${announcer_head}/pulls")
            && script.contains(".base.ref == \"main\"")
            && script.contains(".state == \"open\"")
            && script.contains("($announcer_head | ascii_downcase)"),
        "pull requests must be re-derived from the announcer's exact commit, open against main, and selected by exact head SHA"
    );
}

// Publication shape: one shared verdict per announced head — opened in
// progress before evaluation, finalized to an aggregated failure or success.
fn assert_one_shared_verdict_is_published_per_announced_head(script: &str) {
    assert!(
        script.contains("start_gate_check_run \"${announcer_head}\"")
            && script.contains("finish_gate_check_run \"${announcer_head}\" \"failure\"")
            && script.contains("finish_gate_check_run \"${announcer_head}\" \"success\""),
        "exactly one shared verdict — opened in progress before evaluation, \
         finalized to an aggregated failure or success — must be published per announced head"
    );
}

// The check-run API calls: the required context is created in progress at
// the evaluated head and that same run is finalized to its conclusion.
fn assert_the_required_context_is_opened_in_progress_and_finalized(script: &str) {
    assert!(
        script.contains("gate_context=\"Bot Review Gate\"")
            && script.contains("-F name=\"${gate_context}\"")
            && script.contains("-F head_sha=")
            && script.contains("-F status=in_progress")
            && script.contains("-F status=completed")
            && script.contains("-F conclusion="),
        "the publisher must open the required check-run in progress at the evaluated head and finalize that same run to its conclusion"
    );
}

#[test]
// These assertions jointly prove the gate's one privileged boundary — it is
// the machine-readable merge evidence for bot reviews — and should fail as a
// unit if its fail-closed queries, review-conclusion semantics, head
// binding, or stable check name regresses. The decision logic itself is
// exercised against recorded fixtures by tests/bot_review_gate/
// (fixture-driven decision tests); the two tests above pin the workflow
// contracts.
fn bot_review_gate_script_fails_closed_and_binds_evidence_to_one_head() {
    let script = bot_review_gate_run_script(&bot_review_gate_publisher_workflow());
    let workflow = bot_review_gate_publisher_workflow();

    assert_review_conclusions_block_or_clear_the_gate(&script);
    assert_only_explicit_resolution_clears_a_thread(&script);
    assert_paginated_evidence_is_complete(&script);
    assert_evidence_is_bound_to_one_exact_head(&script);
    assert_every_query_failure_fails_closed(&script);
    assert_substitution_policy_is_read_from_main(&script, &workflow);
}

// Review-conclusion semantics: an outstanding change request blocks even
// without an inline thread, a formal dismissal clears it, and otherwise
// the latest review must be bound to the evaluated head.
fn assert_review_conclusions_block_or_clear_the_gate(script: &str) {
    assert!(
        script.contains("CHANGES_REQUESTED") && script.contains("outstanding_bot_change_request"),
        "a bot change request must block the gate even when it opened no thread"
    );
    assert!(
        script.contains("as $decisive"),
        "conclusions must come from the author's latest decisive review \
         (CHANGES_REQUESTED, APPROVED or DISMISSED): a comment-only review \
         never clears an outstanding change request"
    );
    assert!(
        script.contains("DISMISSED"),
        "a formally dismissed review must clear its author's conclusion"
    );
    assert!(
        script.contains("stale_bot_review_evidence") && script.contains("commit { oid }"),
        "bot review evidence must be bound to the exact evaluated head"
    );
}

// Thread semantics: only explicit resolution clears a thread. GitHub's
// "outdated" flag is reported but never substitutes for resolution —
// code movement is not evidence that a finding was addressed.
fn assert_only_explicit_resolution_clears_a_thread(script: &str) {
    assert!(
        script.contains("select(.isResolved == false)"),
        "the gate must fail on bot review threads that are not explicitly resolved"
    );
    assert!(
        !script.contains(".isOutdated == false"),
        "an outdated unresolved thread must stay blocking: movement is not resolution"
    );
    assert!(
        script.contains("isOutdated"),
        "the report must disclose whether a blocking thread is outdated"
    );
    assert!(
        script.contains("endswith(\"[bot]\")"),
        "the gate must recognise both GitHub App accounts and [bot]-suffixed logins"
    );
}

// Complete paginated evidence: review conclusions (a change request can
// exist without any inline thread) and review threads alike.
fn assert_paginated_evidence_is_complete(script: &str) {
    assert!(
        script.contains("reviewThreads(first: 100") && script.contains("reviews(first: 100"),
        "the gate must paginate both review threads and reviews completely"
    );
    assert!(
        script.contains("group_by(.author)") && script.contains("sort_by(.database_id)"),
        "review conclusions must be evaluated per bot author from their latest review"
    );
}

// Head binding: pagination and the published result are bound to one
// exact head, re-verified immediately before publication.
fn assert_evidence_is_bound_to_one_exact_head(script: &str) {
    assert!(
        script.contains("headRefOid == $head") && script.contains("Pull request head moved to"),
        "paginated evidence and the published result must be bound to one exact head"
    );
    assert!(
        script.contains("no longer matches pull request head"),
        "an announcer head other than the pull request head must be refused"
    );
}

// Every query failure, incomplete pagination, or wrong base must fail
// the check instead of passing it.
fn assert_every_query_failure_fails_closed(script: &str) {
    for fragment in [
        "Pull request query failed; failing closed.",
        "Review-thread query failed; failing closed.",
        "Review query failed; failing closed.",
        "Review-thread response omitted the pull request; failing closed.",
        "Review response omitted the pull request; failing closed.",
        "Review-timeline response omitted the pull request; failing closed.",
    ] {
        assert!(
            script.contains(fragment),
            "a failed or incomplete query must fail the check: {fragment}"
        );
    }
    // `gh api graphql --paginate` emits one JSON document per page and a
    // plain `jq -e` derives its exit status from the last value only, so the
    // incompleteness guard must slurp every page and validate each one.
    assert!(
        script.contains("all(.[]; .data.repository.pullRequest != null)"),
        "the fail-closed guard must validate every paginated page, not just the last one"
    );
    assert!(
        script.contains("!= \"main\"") || script.contains("!= 'main'"),
        "the gate must re-derive and require the main base branch"
    );
}

// The substitution policy is read from `main` (never from the pull
// request), so a pull request cannot edit its own waiver; reading it
// needs a read-only contents grant.
fn assert_substitution_policy_is_read_from_main(script: &str, workflow: &serde_yaml::Value) {
    let permissions = workflow["permissions"]
        .as_mapping()
        .expect("permissions must map");
    assert_eq!(
        permissions.get("contents").and_then(|value| value.as_str()),
        Some("read"),
        "the substitution policy read must be covered by a read-only contents grant"
    );
    assert!(
        script.contains(".github/bot-review-substitution.json") && script.contains("ref=main"),
        "the substitution policy must be read from the repository-owned file on main"
    );
}

#[test]
fn bot_review_gate_substitution_defaults_to_disabled_on_policy_failures() {
    let script = bot_review_gate_run_script(&bot_review_gate_publisher_workflow());

    // The substitution is an opt-in relaxation: a missing file, a failed
    // read, or a malformed document must disable the waiver — never enable
    // it.
    assert!(
        script.contains("stays disabled"),
        "every policy-fetch or policy-parse failure must keep the substitution disabled"
    );
    assert!(
        script.contains("(HTTP 404)") || script.contains("Not Found"),
        "a repository without a policy file must simply have no waiver available"
    );
}

#[test]
fn bot_review_gate_substitution_waiver_is_scoped_and_audited() {
    let script = bot_review_gate_run_script(&bot_review_gate_publisher_workflow());

    // Waiver conditions: a listed reviewer, an affirmative (APPROVED)
    // substitute review bound to the exact evaluated head, every thread
    // resolved, and no outstanding change request from any author. The
    // waiver only ever removes stale_bot_review_evidence violations.
    for fragment in [
        "substitute_reviewer",
        "rate_limited_reviewers",
        "stale_bot_review_evidence",
    ] {
        assert!(
            script.contains(fragment),
            "the substitution must be keyed on {fragment}"
        );
    }
    assert!(
        script.contains("select(.state == \"APPROVED\" and .commit.oid == $head)"),
        "the substitute approval must be affirmative and bound to the exact evaluated head"
    );
    assert!(
        script.contains("select(.isResolved == false)") && script.contains("length == 0"),
        "the waiver must require every review thread to be resolved"
    );
    assert!(
        script.contains("(sort_by(.database_id) | last).state != \"CHANGES_REQUESTED\""),
        "the waiver must require no outstanding change request from any author"
    );
    assert!(
        script.contains(".reason != \"stale_bot_review_evidence\""),
        "the waiver must only remove stale_bot_review_evidence violations"
    );
    assert!(
        script.contains("RATE-LIMIT SUBSTITUTION"),
        "every granted waiver must be reported for auditability"
    );
}

// The auto-merge writer must hold exactly contents + pull-requests on the
// workflow GITHUB_TOKEN: `administration` is not a valid GITHUB_TOKEN scope
// and declaring it makes GitHub reject the whole workflow at validation.
fn assert_writer_permissions_are_valid_github_token_scopes(workflow: &serde_yaml::Value) {
    let writer = &workflow["jobs"]["dependabot-automerge"];
    let writer_permissions = writer["permissions"]
        .as_mapping()
        .expect("writer permissions must be a mapping");
    assert_eq!(
        writer_permissions.len(),
        2,
        "the writer must hold exactly contents and pull-requests on the workflow GITHUB_TOKEN"
    );
    assert_eq!(writer["permissions"]["contents"].as_str(), Some("write"));
    assert_eq!(
        writer["permissions"]["pull-requests"].as_str(),
        Some("write")
    );
    assert!(
        writer["permissions"].get("administration").is_none(),
        "administration is not a GITHUB_TOKEN scope and must never be declared"
    );
}

// The ruleset read must authenticate with a single-purpose minted GitHub App
// installation token (administration: read only), and the ruleset
// precondition must be the writer's next step, ahead of any merge request.
fn assert_ruleset_read_uses_the_minted_app_token(workflow: &serde_yaml::Value) {
    let writer = &workflow["jobs"]["dependabot-automerge"];
    let writer_steps = writer["steps"]
        .as_sequence()
        .expect("write job steps must be a sequence");
    assert!(
        writer_steps
            .first()
            .is_some_and(|step| step["name"].as_str()
                == Some("Mint a read-only ruleset-reader token")
                && step["uses"]
                    .as_str()
                    .is_some_and(|uses| uses.starts_with("actions/create-github-app-token@"))
                && step["with"]["permission-administration"].as_str() == Some("read")
                && step["with"]["app-id"].as_str() == Some("${{ secrets.RULESET_READER_APP_ID }}")
                && step["with"]["private-key"].as_str()
                    == Some("${{ secrets.RULESET_READER_APP_PRIVATE_KEY }}")),
        "the ruleset read must authenticate with a minted installation token restricted to administration: read"
    );
    let precondition = writer_steps.get(1).expect("precondition step must exist");
    assert_eq!(
        precondition["env"]["GH_TOKEN"].as_str(),
        Some("${{ steps.ruleset_reader.outputs.token }}"),
        "the ruleset precondition must use the minted read-only token, not the workflow GITHUB_TOKEN"
    );
    assert_eq!(
        precondition["name"].as_str(),
        Some("Require the live ruleset to enforce the full policy gate"),
        "the ruleset precondition must run before any merge request"
    );
}

// Every policy context must be present in the workflow's expected set, the
// applicable rulesets must be discovered via the branch-rules endpoint, and
// every query failure or coverage gap must fail closed.
fn assert_precondition_enforces_the_full_policy() {
    for expected in required_policy_check_contexts() {
        assert!(
            DEPENDABOT_AUTOMERGE.contains(&format!("\"{expected}\"")),
            "the auto-merge precondition must require live ruleset context {expected}"
        );
    }
    // The Bot Review Gate context must be required from the dedicated
    // gate-publisher App, never from the shared Actions integration a
    // pull-request job can forge a same-named check under: the app id comes
    // from a repository variable, the literal 15368 binding must be gone,
    // and an unset or non-numeric value must refuse auto-merge.
    assert!(
        DEPENDABOT_AUTOMERGE.contains("\"Bot Review Gate|${GATE_PUBLISHER_APP_ID}\"")
            && !DEPENDABOT_AUTOMERGE.contains("\"Bot Review Gate|15368\"")
            && DEPENDABOT_AUTOMERGE
                .contains("GATE_PUBLISHER_APP_ID: ${{ vars.BOT_REVIEW_GATE_APP_ID }}"),
        "the gate context must be bound to the repository-variable gate-publisher App id"
    );
    assert!(
        DEPENDABOT_AUTOMERGE.contains("''|*[!0-9]*"),
        "an unset or non-numeric gate app id must refuse auto-merge"
    );
    // The ruleset list endpoint ignores a ref parameter and returns rulesets
    // for every branch, so the precondition must read the branch-rules
    // endpoint to know which rulesets actually apply to main. That endpoint
    // reports per-rule records carrying `ruleset_id` (rules inherited from
    // active rulesets only), not ruleset records with `id` and `enforcement`.
    assert!(
        DEPENDABOT_AUTOMERGE.contains("rules/branches/main")
            && !DEPENDABOT_AUTOMERGE.contains("rulesets?ref=main")
            && DEPENDABOT_AUTOMERGE.contains(".[].ruleset_id"),
        "the precondition must derive the applicable rulesets from their per-rule records"
    );
    assert!(
        DEPENDABOT_AUTOMERGE
            .contains("The per-ruleset detail endpoint is the authoritative source")
            && DEPENDABOT_AUTOMERGE.contains("rulesets/${rule_id}"),
        "the precondition must fetch each active ruleset's actual required checks"
    );
    // Require-conversation-resolution is the native merge-time backstop for
    // a reopened review thread (reopening fires no Actions event), so the
    // rollout mandates it in the same edit that widens the checks; the
    // precondition must read the saved ruleset — not the intent — and
    // refuse auto-merge unless every applicable ruleset carries the flag.
    assert!(
        DEPENDABOT_AUTOMERGE
            .contains("select(.type == \"pull_request\")")
            && DEPENDABOT_AUTOMERGE.contains("required_review_thread_resolution")
            && DEPENDABOT_AUTOMERGE
                .contains("with require-conversation-resolution"),
        "the precondition must refuse auto-merge unless the live ruleset enforces require-conversation-resolution"
    );
    assert!(
        DEPENDABOT_AUTOMERGE.contains("Refusing to enable auto-merge")
            && DEPENDABOT_AUTOMERGE.contains("refusing auto-merge"),
        "every query failure or coverage gap must keep routine auto-merge off"
    );
}

#[test]
fn dependabot_automerge_waits_for_the_live_full_policy_ruleset() {
    let workflow = dependabot_automerge_workflow();
    assert_writer_permissions_are_valid_github_token_scopes(&workflow);
    assert_ruleset_read_uses_the_minted_app_token(&workflow);
    assert_precondition_enforces_the_full_policy();
}

fn repository_workflow_names() -> (std::path::PathBuf, Vec<String>) {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let workflows_dir = manifest_dir.join(".github/workflows");
    let mut workflows = std::fs::read_dir(&workflows_dir)
        .expect("the GitHub workflow directory must be readable")
        .map(|entry| {
            let entry = entry.expect("every GitHub workflow entry must be readable");
            assert!(
                entry
                    .file_type()
                    .expect("every GitHub workflow type must be readable")
                    .is_file(),
                "the workflow directory must contain only regular files"
            );
            entry
                .file_name()
                .into_string()
                .expect("workflow file names must be UTF-8")
        })
        .collect::<Vec<_>>();
    workflows.sort();
    (workflows_dir, workflows)
}

fn assert_retired_review_providers_absent(label: &str, source: &str) {
    let forbidden_provider_tokens = [
        ["clau", "de"].concat(),
        ["anth", "ropic"].concat(),
        ["co", "dex"].concat(),
        ["open", "ai"].concat(),
    ];
    let source = source.to_ascii_lowercase();
    assert!(
        forbidden_provider_tokens
            .iter()
            .all(|token| !source.contains(token)),
        "{label} must not restore a retired AI reviewer provider"
    );
}

#[test]
fn hosted_ai_reviewer_remains_decommissioned() {
    let (workflows_dir, workflows) = repository_workflow_names();
    assert_eq!(
        workflows,
        [
            "bot-review-gate-publisher.yml",
            "bot-review-gate.yml",
            "ci.yml",
            "dependabot-automerge.yml",
            "fuzz.yml",
            "release.yml"
        ],
        "the complete workflow set must stay explicit; review every addition"
    );

    for workflow in &workflows {
        let source = std::fs::read_to_string(workflows_dir.join(workflow))
            .unwrap_or_else(|error| panic!("workflow {workflow} must be readable text: {error}"));
        assert_retired_review_providers_absent(&format!("workflow {workflow}"), &source);
    }
    assert_retired_review_providers_absent("refinery policy", REFINERY_CONFIG);

    assert!(
        REFINERY_CONFIG.contains("GLM 5.3 reviewer")
            && REFINERY_CONFIG
                .contains("Repository-owned AI review workflows are deliberately absent")
            && REFINERY_CONFIG.contains("not GitHub Actions"),
        "refinery policy must bind operational review to the local reviewer, not GitHub Actions"
    );
}

#[test]
fn ci_coverage_is_pinned_comprehensive_and_threshold_gated() {
    let manifest = manifest();
    let rust_version = manifest["package"]["rust-version"]
        .as_str()
        .expect("package.rust-version must be a string");
    let coverage_job = workflow_job(CI_WORKFLOW, "coverage");
    let minimum: f64 = COVERAGE_BASELINE
        .trim()
        .parse()
        .expect("coverage-baseline.txt must contain one numeric percentage");

    assert!(
        (0.0..=100.0).contains(&minimum) && minimum > 0.0,
        "the line-coverage baseline must be a meaningful percentage"
    );
    assert!(
        coverage_job.contains("name: Coverage (Linux x86_64)"),
        "CI must expose one comparable aggregate coverage gate"
    );
    assert!(
        coverage_job.contains(&format!(
            "uses: dtolnay/rust-toolchain@{RUST_TOOLCHAIN_ACTION_SHA} # master"
        )) && coverage_job.contains(&format!("toolchain: {rust_version}.0")),
        "coverage must use the declared Rust release through the immutable action commit"
    );
    assert!(
        coverage_job.contains("components: llvm-tools-preview"),
        "coverage must install the matching LLVM coverage tools"
    );
    assert!(
        coverage_job.contains("cargo install cargo-llvm-cov --version 0.8.7 --locked"),
        "coverage must pin cargo-llvm-cov and its dependency resolution"
    );
    assert!(
        coverage_job.contains(
            "cargo llvm-cov --all-targets --all-features --locked --html --output-dir coverage --fail-under-lines \"$minimum\""
        ),
        "coverage must execute every host target and feature before enforcing the line floor"
    );
    assert!(
        coverage_job.contains("coverage_status=0")
            && coverage_job.contains("|| coverage_status=$?")
            && coverage_job.contains("cargo llvm-cov report --summary-only")
            && coverage_job.contains("exit \"$coverage_status\""),
        "coverage must print the exact measured summary without masking the test or threshold status"
    );
    assert!(
        coverage_job.contains("coverage-baseline.txt"),
        "the CI threshold must come from the reviewed baseline file"
    );
    assert!(
        coverage_job.contains("if: always()")
            && coverage_job.contains("path: coverage/")
            && coverage_job.contains("if-no-files-found: error"),
        "the HTML upload must run after failure and reject a missing report"
    );
    assert!(
        !CI_WORKFLOW.contains("--ignore-filename-regex"),
        "the only CI coverage report must not hide source areas"
    );
}

#[test]
fn developer_coverage_commands_do_not_hide_source_areas() {
    for (platform, script) in [
        ("Linux", BUILD_LINUX),
        ("macOS", BUILD_MACOS),
        ("Windows", BUILD_WINDOWS),
    ] {
        assert!(
            script.contains("cargo install cargo-llvm-cov --version 0.8.7 --locked"),
            "{platform} must install the reviewed cargo-llvm-cov release"
        );
        assert!(
            script.contains("cargo-llvm-cov 0.8.7") && script.contains("--locked --force"),
            "{platform} must detect and replace a mismatched coverage frontend"
        );
        assert!(
            script.contains("cargo llvm-cov --all-targets --all-features --locked"),
            "{platform} coverage must include every host target and feature"
        );
        assert!(
            !script.contains("--ignore-filename-regex"),
            "{platform} coverage must not hide source areas"
        );
    }

    assert!(
        BUILD_LINUX.contains("informational coverage")
            && BUILD_LINUX.contains("active Rust toolchain")
            && !BUILD_LINUX.contains("--fail-under-lines"),
        "the ambient-toolchain Linux helper must not impersonate the pinned CI gate"
    );
    assert!(
        BUILD_WINDOWS.contains("-or $Coverage")
            && BUILD_WINDOWS.contains("rustup component add llvm-tools-preview")
            && BUILD_WINDOWS.contains("--target $RustTarget --summary-only"),
        "Windows coverage must retain its native target and matching LLVM tools"
    );
    assert!(
        README.contains("coverage-baseline.txt")
            && README.contains("does not compare it with the base branch")
            && README.contains("repository review policy treats the floor as a")
            && README.contains("ratchet: ordinary changes keep or raise it")
            && README.contains("lowering it requires a dedicated")
            && README.contains("measurement-definition change"),
        "the threshold enforcement and separate review ratchet must be documented accurately"
    );
}
