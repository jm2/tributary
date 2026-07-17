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
