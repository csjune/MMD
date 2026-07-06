fn main() {
    if cfg!(target_os = "windows") {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("Resources/AppIcon.ico");
        resource.set("ProductName", "MMD");
        resource.set("FileDescription", "MMD - Multi Monitor Dimming");
        resource.set("OriginalFilename", "mmd.exe");

        if let Err(error) = resource.compile() {
            println!("cargo:warning=failed to embed Windows resources: {error}");
        }
    }
}
