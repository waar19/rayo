fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("../../assets/rayo.ico");
        resource
            .compile()
            .expect("failed to compile Windows icon resources for rayo-gui");
    }

    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    slint_build::compile_with_config("ui/main.slint", config).expect("failed to compile Slint UI");
    println!("cargo:rerun-if-changed=ui/main.slint");
    println!("cargo:rerun-if-changed=ui/assets/rayo.png");
    println!("cargo:rerun-if-changed=../../assets/rayo.ico");
}
