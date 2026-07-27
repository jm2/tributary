fn main() {
    // Generate the Windows icon and version resource, then attach it
    // specifically to the application binary. `winresource::compile()` emits
    // `rustc-link-lib`, which Cargo routes only to this package's library now
    // that `src/lib.rs` exists; the resource would therefore be absent from
    // `tributary.exe`. `embed_resource::compile_for()` emits the bin-scoped
    // linker directive required by a mixed library/binary package.
    #[cfg(target_os = "windows")]
    {
        let manifest_dir = std::path::PathBuf::from(
            std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is not set"),
        );
        let output_dir =
            std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR is not set"));
        let resource_file = output_dir.join("tributary-resource.rc");
        let icon = manifest_dir.join("data/tributary.ico");

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
