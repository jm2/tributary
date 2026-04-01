fn main() {
    // Embed the app icon into the Windows executable.
    #[cfg(target_os = "windows")]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("data/tributary.ico");
        res.set("ProductName", "Tributary");
        res.set("FileDescription", "Tributary");
        res.set("LegalCopyright", "Copyright © 2026 Tributary Contributors");
        res.compile().expect("Failed to compile Windows resources");
    }
}
