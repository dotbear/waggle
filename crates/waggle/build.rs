fn main() {
    slint_build::compile("ui/app.slint").expect("the Slint UI should compile");

    // The .exe's own icon, which Explorer, the Start Menu shortcut and toasts
    // show. Embedding needs Microsoft's resource compiler, so it only happens
    // when building on Windows (as CI does); cross builds keep the default.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") && cfg!(windows) {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("../../assets/waggle.ico");
        resource.set("ProductName", "Waggle");
        resource.set("FileDescription", "Waggle");
        resource
            .compile()
            .expect("the Windows resources should compile");
    }
}
