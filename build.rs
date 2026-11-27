fn main() {
    slint_build::compile_with_config(
        "ui/app.slint",
        slint_build::CompilerConfiguration::new().with_style("fluent".into()),
    )
    .expect("compile Slint UI");
    println!("cargo:rerun-if-changed=res/app.rc");
    println!("cargo:rerun-if-changed=res/app.manifest");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    embed_resource::compile("res/app.rc", embed_resource::NONE)
        .manifest_optional()
        .expect("compile Windows resources");
}
