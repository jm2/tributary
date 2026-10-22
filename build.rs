fn main() {
    let manifest_dir = std::path::PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is not set"),
    );
    let out_dir =
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR is not set"));

    // Compile the app's own icons into a GResource bundle that `src/ui/icons.rs`
    // embeds with `gio::resources_register_include!`, so they resolve wherever
    // the binary is installed rather than only from the source tree.
    let resource_xml = manifest_dir.join("data/icons.gresource.xml");
    let icons_dir = manifest_dir.join("data/icons");
    let glib_compile_resources = |extra: &[std::ffi::OsString]| {
        let output = std::process::Command::new("glib-compile-resources")
            .arg(&resource_xml)
            .arg("--sourcedir")
            .arg(&icons_dir)
            .args(extra)
            .output()
            .expect("failed to run glib-compile-resources; install GLib's development tools");
        assert!(
            output.status.success(),
            "glib-compile-resources failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };
    glib_compile_resources(&["--target".into(), out_dir.join("icons.gresource").into()]);

    // Emitting any rerun-if-changed replaces Cargo's default of rerunning on
    // every package change, so name each input: this script, the manifest,
    // every file the manifest bundles, and (below) the Windows icon.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", resource_xml.display());
    let dependencies = glib_compile_resources(&["--generate-dependencies".into()]);
    for dependency in String::from_utf8_lossy(&dependencies).lines() {
        println!("cargo:rerun-if-changed={dependency}");
    }

    // Generate the Windows icon and version resource, then attach it
    // specifically to the application binary. `winresource::compile()` emits
    // `rustc-link-lib`, which Cargo routes only to this package's library now
    // that `src/lib.rs` exists; the resource would therefore be absent from
    // `tributary.exe`. `embed_resource::compile_for()` emits the bin-scoped
    // linker directive required by a mixed library/binary package.
    #[cfg(target_os = "windows")]
    {
        let resource_file = out_dir.join("tributary-resource.rc");
        let icon = manifest_dir.join("data/tributary.ico");
        println!("cargo:rerun-if-changed={}", icon.display());

        let mut res = winresource::WindowsResource::new();
        res.set_icon(
            icon.to_str()
                .expect("Windows resource icon path is not valid UTF-8"),
        );
        res.set("ProductName", "Tributary");
        res.set("FileDescription", "Tributary");
        res.set("LegalCopyright", "Copyright © 2026 Tributary Contributors");
        res.write_resource_file(&resource_file)
            .expect("Failed to generate Windows resources");

        embed_resource::compile_for(&resource_file, ["tributary"], embed_resource::NONE)
            .manifest_required()
            .expect("Failed to compile Windows resources for tributary.exe");
    }
}
