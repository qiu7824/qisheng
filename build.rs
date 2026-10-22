fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // The desktop player carries its icon and version information.
    #[cfg(windows)]
    if std::env::var_os("CARGO_FEATURE_DESKTOP").is_some() {
        println!("cargo:rerun-if-changed=assets/lx-native.rc");
        println!("cargo:rerun-if-changed=assets/lx-native.ico");
        embed_resource::compile_for("assets/lx-native.rc", ["lx-native"], embed_resource::NONE)
            .manifest_optional()
            .expect("compile the lx-native resources");
    }
    // Windows' default executable stack is 1 MiB. Boa's interpreter and GPUI's
    // debug render frames require additional stack even for ordinary fixtures.
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg=/STACK:16777216");
        if std::env::var("DEBUG").as_deref() == Ok("false") {
            println!("cargo:rustc-link-arg=/DEBUG:NONE");
        }
    }
}
