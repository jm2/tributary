use toml::Value;

const MANIFEST: &str = include_str!("../Cargo.toml");
const ROOT_LOCK: &str = include_str!("../Cargo.lock");
const FUZZ_LOCK: &str = include_str!("../fuzz/Cargo.lock");
const RPM_SPEC: &str = include_str!("../build-aux/rpm/tributary.spec");
const ARCH_PKGBUILD: &str = include_str!("../build-aux/arch/PKGBUILD");
const DESKTOP_ENTRY: &str = include_str!("../data/io.github.tributary.Tributary.desktop");
const CI_WORKFLOW: &str = include_str!("../.github/workflows/ci.yml");
const DEPENDABOT_CONFIG: &str = include_str!("../.github/dependabot.yml");
const DEPENDABOT_AUTOMERGE: &str = include_str!("../.github/workflows/dependabot-automerge.yml");
const CLAUDE_REVIEW_WORKFLOW: &str = include_str!("../.github/workflows/claude-review.yml");
const RELEASE_WORKFLOW: &str = include_str!("../.github/workflows/release.yml");
const COVERAGE_BASELINE: &str = include_str!("../coverage-baseline.txt");
const README: &str = include_str!("../README.md");
const BUILD_SCRIPT: &str = include_str!("../build.rs");
const BUILD_LINUX: &str = include_str!("../scripts/build-linux.sh");
const BUILD_MACOS: &str = include_str!("../scripts/build-macos.sh");
const BUILD_WINDOWS: &str = include_str!("../scripts/build-windows.ps1");
const WINDOWS_AUDIO: &str = include_str!("../src/audio/windows_audio.rs");
const WINDOWS_RUNTIME_PROBE: &str = include_str!("../src/audio/runtime_probe.rs");
const MACOS_AUDIO: &str = include_str!("../src/audio/macos_audio.rs");
const MACOS_AUDIO_NATIVE: &str = include_str!("../src/audio/macos_audio_native.rs");
const MACOS_AUDIO_TESTS: &str = include_str!("../src/audio/macos_audio_tests.rs");
const PLATFORM_RUNTIME: &str = include_str!("../src/platform_runtime.rs");
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
    assert_eq!(
        job.matches("uses: flatpak/flatpak-github-actions/flatpak-builder@v6")
            .count(),
        1,
        "{label} must contain exactly one Flatpak builder action"
    );
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
        BUILD_MACOS.contains("for required_route_plugin in libgstcoreelements libgstosxaudio; do")
            && BUILD_MACOS.contains(
                "error \"Missing required GStreamer audio-route plugin: ${required_route_plugin}\""
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
        msrv_job.contains(&format!("uses: dtolnay/rust-toolchain@{rust_release}")),
        "CI must install the exact declared Rust release"
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
        "Rust release updates must remain enabled at every SemVer level"
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
                == Some("dependabot/fetch-metadata@d7267f607e9d3fb96fc2fbe83e0af444713e90b7")
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
    assert!(
        writer_steps.iter().all(|step| step.get("uses").is_none()),
        "the write-capable job must remain third-party-action-free"
    );
    assert_eq!(
        writer_steps.len(),
        1,
        "the write-capable job must contain only the guarded merge command"
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
                "dependabot/fetch-metadata@d7267f607e9d3fb96fc2fbe83e0af444713e90b7"
            )
            && !DEPENDABOT_AUTOMERGE.contains("dependabot/fetch-metadata@v3")
            && DEPENDABOT_AUTOMERGE
                .contains("\".github/workflows/dependabot-automerge.yml\"")
            && DEPENDABOT_AUTOMERGE.contains("github.actor == 'dependabot[bot]'")
            && DEPENDABOT_AUTOMERGE
                .contains("github.event.pull_request.user.login == 'dependabot[bot]'")
            && DEPENDABOT_AUTOMERGE.contains("github.repository == 'jm2/tributary'")
            && DEPENDABOT_AUTOMERGE.contains(
                "!contains(needs.inspect_changed_files.outputs.dependency_names, 'dtolnay/rust-toolchain')"
            )
            && DEPENDABOT_AUTOMERGE.contains(
                "!contains(needs.inspect_changed_files.outputs.dependency_names, 'dependabot/fetch-metadata')"
            ),
        "Dependabot automation must be pinned, checkout-free, exact-head API-preflighted, narrowly admitted, race-contained, mixed-path self-update-safe, and refuse toolchain auto-merge"
    );
}

fn claude_review_workflow() -> serde_yaml::Value {
    serde_yaml::from_str(CLAUDE_REVIEW_WORKFLOW).expect("Claude review workflow must parse")
}

fn claude_review_job<'a>(workflow: &'a serde_yaml::Value, name: &str) -> &'a serde_yaml::Value {
    workflow
        .get("jobs")
        .and_then(serde_yaml::Value::as_mapping)
        .and_then(|jobs| jobs.get(serde_yaml::Value::String(name.into())))
        .unwrap_or_else(|| panic!("Claude review job {name} must exist"))
}

fn claude_review_steps(job: &serde_yaml::Value) -> &[serde_yaml::Value] {
    job.get("steps")
        .and_then(serde_yaml::Value::as_sequence)
        .expect("Claude review job steps must be a sequence")
}

fn claude_review_step_named<'a>(job: &'a serde_yaml::Value, name: &str) -> &'a serde_yaml::Value {
    claude_review_steps(job)
        .iter()
        .find(|step| step.get("name").and_then(serde_yaml::Value::as_str) == Some(name))
        .unwrap_or_else(|| panic!("Claude review step {name} must exist"))
}

fn claude_review_step_using<'a>(job: &'a serde_yaml::Value, action: &str) -> &'a serde_yaml::Value {
    claude_review_steps(job)
        .iter()
        .find(|step| {
            step.get("uses")
                .and_then(serde_yaml::Value::as_str)
                .is_some_and(|uses| uses.starts_with(action))
        })
        .unwrap_or_else(|| panic!("Claude review action {action} must exist"))
}

fn claude_review_step_with_id<'a>(job: &'a serde_yaml::Value, id: &str) -> &'a serde_yaml::Value {
    claude_review_steps(job)
        .iter()
        .find(|step| step.get("id").and_then(serde_yaml::Value::as_str) == Some(id))
        .unwrap_or_else(|| panic!("Claude review step id {id} must exist"))
}

fn claude_review_inputs(step: &serde_yaml::Value) -> &serde_yaml::Mapping {
    step.get("with")
        .and_then(serde_yaml::Value::as_mapping)
        .expect("Claude review action inputs must be a mapping")
}

fn claude_input<'a>(inputs: &'a serde_yaml::Mapping, name: &str) -> Option<&'a serde_yaml::Value> {
    inputs.get(serde_yaml::Value::String(name.into()))
}

fn normalized_job_admission(job: &serde_yaml::Value) -> String {
    job.get("if")
        .and_then(serde_yaml::Value::as_str)
        .expect("Claude review job must gate its event")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn assert_claude_review_triggers(workflow: &serde_yaml::Value) {
    let workflow_run = workflow
        .get("on")
        .and_then(serde_yaml::Value::as_mapping)
        .and_then(|triggers| triggers.get(serde_yaml::Value::String("workflow_run".into())))
        .and_then(serde_yaml::Value::as_mapping)
        .expect("bot reviews must use a trusted workflow_run follow-up");
    let source_workflows: Vec<_> = workflow_run
        .get(serde_yaml::Value::String("workflows".into()))
        .and_then(serde_yaml::Value::as_sequence)
        .expect("workflow_run must name its source workflow")
        .iter()
        .filter_map(serde_yaml::Value::as_str)
        .collect();
    assert_eq!(
        source_workflows,
        ["CI"],
        "bot reviews must be coupled to the unprivileged CI request"
    );
    let activity_types: Vec<_> = workflow_run
        .get(serde_yaml::Value::String("types".into()))
        .and_then(serde_yaml::Value::as_sequence)
        .expect("workflow_run must restrict its activity type")
        .iter()
        .filter_map(serde_yaml::Value::as_str)
        .collect();
    assert_eq!(
        activity_types,
        ["requested"],
        "the trusted bot follow-up must start without waiting for CI completion"
    );
}

fn assert_direct_claude_review_boundaries(workflow: &serde_yaml::Value) {
    let job = claude_review_job(workflow, "claude-review");
    assert_eq!(
        normalized_job_admission(job),
        "github.event_name == 'pull_request' && github.event.sender.type == 'User'",
        "direct reviews must remain limited to User-triggered pull_request events"
    );
    let inputs = claude_review_inputs(claude_review_step_using(
        job,
        "anthropics/claude-code-action@",
    ));
    assert_eq!(
        claude_input(inputs, "track_progress").and_then(serde_yaml::Value::as_bool),
        Some(true),
        "direct reviews must retain their tracking comment"
    );
    assert!(
        claude_input(inputs, "allowed_bots").is_none()
            && claude_input(inputs, "allowed_non_write_users").is_none(),
        "the direct User path must not contain an actor bypass"
    );
}

fn assert_bot_claude_review_admission(workflow: &serde_yaml::Value) {
    let job = claude_review_job(workflow, "claude-bot-review");
    assert_eq!(
        normalized_job_admission(job),
        "github.event_name == 'workflow_run' && github.event.workflow_run.event == 'pull_request' && github.event.workflow_run.actor.type != 'User' && github.event.workflow_run.pull_requests[0]",
        "the trusted follow-up must admit only non-User PR workflow runs"
    );
    let permissions = job
        .get("permissions")
        .and_then(serde_yaml::Value::as_mapping)
        .expect("trusted Claude bot review permissions must be explicit");
    assert!(
        !permissions.contains_key(serde_yaml::Value::String("id-token".into())),
        "the untrusted-diff review path must not receive OIDC minting authority"
    );
    let concurrency = job
        .get("concurrency")
        .and_then(serde_yaml::Value::as_mapping)
        .expect("trusted Claude bot reviews must serialize per pull request");
    assert_eq!(
        claude_input(concurrency, "group").and_then(serde_yaml::Value::as_str),
        Some("claude-bot-review-${{ github.event.workflow_run.pull_requests[0].number }}"),
        "bot reviews must use a pull-request-specific concurrency group"
    );
    assert_eq!(
        claude_input(concurrency, "cancel-in-progress").and_then(serde_yaml::Value::as_bool),
        Some(true),
        "a newer bot review must retire an in-progress predecessor"
    );
}

fn assert_bot_claude_review_checkout(workflow: &serde_yaml::Value) {
    let job = claude_review_job(workflow, "claude-bot-review");
    let checkouts: Vec<_> = claude_review_steps(job)
        .iter()
        .filter(|step| {
            step.get("uses")
                .and_then(serde_yaml::Value::as_str)
                .is_some_and(|uses| uses.starts_with("actions/checkout@"))
        })
        .collect();
    assert_eq!(
        checkouts.len(),
        1,
        "the privileged job must perform exactly one trusted checkout"
    );
    let inputs = claude_review_inputs(checkouts[0]);
    assert_eq!(
        claude_input(inputs, "ref").and_then(serde_yaml::Value::as_str),
        Some("${{ github.sha }}"),
        "workflow_run must check out only its trusted default-branch SHA"
    );
    assert_eq!(
        claude_input(inputs, "persist-credentials").and_then(serde_yaml::Value::as_bool),
        Some(false),
        "the privileged checkout must not persist its job token"
    );
}

fn bot_claude_review_input_script(workflow: &serde_yaml::Value) -> &str {
    claude_review_step_named(
        claude_review_job(workflow, "claude-bot-review"),
        "Prepare inert PR diff",
    )
    .get("run")
    .and_then(serde_yaml::Value::as_str)
    .expect("bot review input preparation must be a shell script")
}

fn assert_bot_claude_review_input_revision(workflow: &serde_yaml::Value) {
    let script = bot_claude_review_input_script(workflow);
    assert!(
        script.starts_with("set -euo pipefail\n"),
        "trusted input preparation must fail closed on shell and pipeline errors"
    );
    assert!(
        script.contains("--json baseRefOid,headRefOid,state")
            && script.contains(".baseRefOid == $base")
            && script.contains(".headRefOid == $head")
            && script.contains("\"repos/$GITHUB_REPOSITORY/compare/$BASE_SHA...$HEAD_SHA\"")
            && !script.contains("gh pr diff"),
        "bot reviews must bind both admission and diff retrieval to the triggering revision"
    );
}

fn assert_bot_claude_review_input_limits(workflow: &serde_yaml::Value) {
    let script = bot_claude_review_input_script(workflow);
    assert!(
        script.contains("head -c 50000")
            && script.contains("tr -d '\\000-\\010\\013\\014\\016-\\037\\177'")
            && script.contains("[Diff truncated at 50,000 bytes.]"),
        "the normalized diff must remain below the action's per-environment-string limit"
    );
    assert!(
        script.contains("if ! gh api \\")
            && script.contains("Compare diff unavailable; skipping bot review.")
            && script.matches("current=false").count() >= 2,
        "an unavailable comparison must use the established no-review path"
    );
}

fn assert_bot_claude_review_input_boundary(workflow: &serde_yaml::Value) {
    let script = bot_claude_review_input_script(workflow);
    assert!(
        script.contains("delimiter=\"DIFF_$(cat /proc/sys/kernel/random/uuid)\"")
            && script
                .contains("prompt_boundary=\"UNTRUSTED_DIFF_$(cat /proc/sys/kernel/random/uuid)\"")
            && script.contains("printf 'prompt_boundary=%s\\n' \"$prompt_boundary\""),
        "the prompt boundary must be unpredictable when the proposed diff is authored"
    );
}

fn assert_bot_claude_review_action_inputs(workflow: &serde_yaml::Value) {
    let action = claude_review_step_with_id(
        claude_review_job(workflow, "claude-bot-review"),
        "bot-review",
    );
    let inputs = claude_review_inputs(action);
    assert_eq!(
        claude_input(inputs, "allowed_bots").and_then(serde_yaml::Value::as_str),
        Some("*"),
        "Claude review must admit every GitHub Bot/App actor"
    );
    assert_eq!(
        claude_input(inputs, "github_token").and_then(serde_yaml::Value::as_str),
        Some("${{ github.token }}"),
        "the bot path must use only the short-lived, job-scoped GitHub token"
    );
    for input in [
        "classify_inline_comments",
        "display_report",
        "include_fix_links",
    ] {
        assert_eq!(
            claude_input(inputs, input).and_then(serde_yaml::Value::as_bool),
            Some(false),
            "the tool-free bot path must disable action side-channel output"
        );
    }
    assert!(
        claude_input(inputs, "allowed_non_write_users").is_none()
            && claude_input(inputs, "track_progress").is_none(),
        "bot admission must not bypass write-permission checks for ordinary User actors"
    );
}

fn assert_bot_claude_review_tools(workflow: &serde_yaml::Value) {
    let action = claude_review_step_with_id(
        claude_review_job(workflow, "claude-bot-review"),
        "bot-review",
    );
    let args = claude_input(claude_review_inputs(action), "claude_args")
        .and_then(serde_yaml::Value::as_str)
        .expect("tool-free bot review must constrain Claude arguments");
    assert!(
        args.contains("--tools \"Read\"")
            && args.contains("--allowedTools \"mcp__disabled__no_tools\"")
            && args.contains("--disallowedTools \"Bash,Read,")
            && args.contains("\"maxLength\":12000")
            && args.contains("\"pattern\":\"^[^\\\\u0000"),
        "the sole advertised built-in must also be bare-denied in the schema-constrained session"
    );
    assert_eq!(
        action.get("if").and_then(serde_yaml::Value::as_str),
        Some("steps.bot-input.outputs.current == 'true'"),
        "Claude must not run after a stale revision is detected"
    );
}

fn assert_bot_claude_review_prompt_boundary(workflow: &serde_yaml::Value) {
    let action = claude_review_step_with_id(
        claude_review_job(workflow, "claude-bot-review"),
        "bot-review",
    );
    let prompt = claude_input(claude_review_inputs(action), "prompt")
        .and_then(serde_yaml::Value::as_str)
        .expect("tool-free bot review must receive an explicit prompt");
    let boundary = "${{ steps.bot-input.outputs.prompt_boundary }}";
    assert_eq!(
        prompt.matches(boundary).count(),
        2,
        "the hostile diff must be enclosed by matching random prompt boundaries"
    );
    assert!(
        !prompt.contains("<untrusted-pr-diff>"),
        "the hostile diff must not use a predictable author-injectable closing fence"
    );
}

fn bot_claude_review_publisher(workflow: &serde_yaml::Value) -> &serde_yaml::Value {
    claude_review_step_named(
        claude_review_job(workflow, "claude-bot-review"),
        "Publish structured bot review",
    )
}

fn bot_claude_review_publish_script(workflow: &serde_yaml::Value) -> &str {
    bot_claude_review_publisher(workflow)
        .get("run")
        .and_then(serde_yaml::Value::as_str)
        .expect("structured review publication must be a fixed shell script")
}

fn assert_bot_claude_review_publisher_gate(workflow: &serde_yaml::Value) {
    let publisher = bot_claude_review_publisher(workflow);
    let publication_gate = publisher
        .get("if")
        .and_then(serde_yaml::Value::as_str)
        .expect("the trusted publisher must gate structured output")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(
        publication_gate,
        "steps.bot-input.outputs.current == 'true' && steps.bot-review.outputs.structured_output != ''",
        "the trusted publisher must require current, nonempty structured output"
    );
}

fn assert_bot_claude_review_publisher_revalidation(workflow: &serde_yaml::Value) {
    let script = bot_claude_review_publish_script(workflow);
    assert!(
        script.starts_with("set -euo pipefail\n"),
        "trusted publication must fail closed on shell and pipeline errors"
    );
    assert!(
        script.contains("--json baseRefOid,headRefOid,state")
            && script.contains(".baseRefOid == $base")
            && script.contains(".headRefOid == $head"),
        "the publisher must discard results made stale while Claude was running"
    );
}

fn assert_bot_claude_review_publisher_rendering(workflow: &serde_yaml::Value) {
    let script = bot_claude_review_publish_script(workflow);
    assert!(
        script.contains("tr -d '\\000-\\010\\013\\014\\016-\\037\\177'")
            && script.contains("tr -d '[:space:]'")
            && script.contains("[ ! -s \"$RUNNER_TEMP/bot-review-nonblank.txt\" ]")
            && script.contains("Claude returned a blank review; skipping publication.")
            && script.contains("sed 's/^/    /' \"$RUNNER_TEMP/bot-review.txt\"")
            && !script.contains("--body \"$REVIEW\""),
        "blank output must be skipped and nonblank model output rendered as inert text"
    );
}

fn assert_bot_claude_review_publisher_idempotency(workflow: &serde_yaml::Value) {
    let script = bot_claude_review_publish_script(workflow);
    assert!(
        script.contains("marker='<!-- claude-bot-review:v1 -->'")
            && script.contains(".user.login == \"github-actions[bot]\"")
            && script.contains("startswith(\"<!-- claude-bot-review:v1 -->\\n\")")
            && !script.contains("contains(\"<!-- claude-bot-review:v1 -->")
            && script.contains("jq -n --rawfile body \"$RUNNER_TEMP/bot-review.md\"")
            && script.contains("--method PATCH")
            && script.contains("--method POST")
            && script
                .matches("--input \"$RUNNER_TEMP/bot-review-request.json\"")
                .count()
                == 2
            && !script.contains("-F \"body=@"),
        "publication must update only its actor-owned marked comment or create it once"
    );
}

#[test]
fn claude_review_accepts_bot_prs_without_opening_the_non_write_user_gate() {
    let workflow = claude_review_workflow();
    assert_claude_review_triggers(&workflow);

    assert_direct_claude_review_boundaries(&workflow);

    assert_bot_claude_review_admission(&workflow);

    assert_bot_claude_review_checkout(&workflow);

    assert_bot_claude_review_input_revision(&workflow);
    assert_bot_claude_review_input_limits(&workflow);
    assert_bot_claude_review_input_boundary(&workflow);

    assert_bot_claude_review_action_inputs(&workflow);
    assert_bot_claude_review_tools(&workflow);
    assert_bot_claude_review_prompt_boundary(&workflow);

    assert_bot_claude_review_publisher_gate(&workflow);
    assert_bot_claude_review_publisher_revalidation(&workflow);
    assert_bot_claude_review_publisher_rendering(&workflow);
    assert_bot_claude_review_publisher_idempotency(&workflow);
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
        coverage_job.contains(&format!("uses: dtolnay/rust-toolchain@{rust_version}.0")),
        "coverage must use the exact declared Rust release"
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
