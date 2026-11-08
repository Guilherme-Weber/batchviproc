fn main() {
    // Compile the Slint UI with the Windows 11 Fluent style, always dark.
    let config = slint_build::CompilerConfiguration::new().with_style("fluent-dark".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("failed to compile the UI");

    // Embed the app icon into batchviproc.exe (shown in Explorer and the taskbar).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.compile().expect("failed to embed the Windows icon");
    }
    println!("cargo:rerun-if-changed=assets/icon.ico");
}
