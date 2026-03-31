fn main() {
    // Embed the app icon into the Windows executable.
    #[cfg(target_os = "windows")]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("data/tributary.ico");
        res.compile().expect("Failed to compile Windows resources");
    }
}
