fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("../../assets/rayo.ico");
        resource
            .compile()
            .expect("failed to compile Windows icon resources for rayo-service");
    }

    println!("cargo:rerun-if-changed=../../assets/rayo.ico");
}
