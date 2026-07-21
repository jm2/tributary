use toml::Value;

const MANIFEST: &str = include_str!("../Cargo.toml");
const RPM_SPEC: &str = include_str!("../build-aux/rpm/tributary.spec");
const ARCH_PKGBUILD: &str = include_str!("../build-aux/arch/PKGBUILD");
const DESKTOP_ENTRY: &str = include_str!("../data/io.github.tributary.Tributary.desktop");
const CI_WORKFLOW: &str = include_str!("../.github/workflows/ci.yml");
const COVERAGE_BASELINE: &str = include_str!("../coverage-baseline.txt");
const README: &str = include_str!("../README.md");
const BUILD_LINUX: &str = include_str!("../scripts/build-linux.sh");
const BUILD_MACOS: &str = include_str!("../scripts/build-macos.sh");
const BUILD_WINDOWS: &str = include_str!("../scripts/build-windows.ps1");
const FORBIDDEN_BUNDLED_COMPONENTS: &str =
    include_str!("../build-aux/packaging/forbidden-bundled-components.txt");

fn manifest() -> Value {
    toml::from_str(MANIFEST).expect("Cargo.toml must parse")
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
    let msrv_job = workflow_job(CI_WORKFLOW, "msrv");
    let crlf_workflow = CI_WORKFLOW.lines().collect::<Vec<_>>().join("\r\n");
    let crlf_msrv_job = workflow_job(&crlf_workflow, "msrv");

    assert_eq!(rust_version, "1.92");
    assert!(
        crlf_msrv_job.contains("name: MSRV (1.92)"),
        "CI workflow contract checks must accept Windows CRLF checkouts"
    );
    assert!(
        msrv_job.contains("name: MSRV (1.92)"),
        "CI job name must expose the declared MSRV"
    );
    assert!(
        msrv_job.contains("uses: dtolnay/rust-toolchain@1.92.0"),
        "CI must install the exact declared Rust release"
    );
    assert!(
        msrv_job.contains("run: cargo check --all-targets --locked"),
        "CI must compile-check every target against the committed lockfile"
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
