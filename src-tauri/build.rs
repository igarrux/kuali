fn main() {
    link_clang_builtins_on_macos();
    tauri_build::build()
}

/// Links the clang runtime when required.
///
/// ggml's Metal backend checks `@available(macOS 15.0, ...)` before using newer
/// APIs. Because Kuali targets macOS 11, those checks call
/// `___isPlatformVersionAtLeast` at runtime. The symbol lives in compiler-rt,
/// while rustc links with `-nodefaultlibs`, so release builds must request it
/// explicitly.
///
/// Debug builds hide the issue because clang assumes the current macOS version
/// when no deployment target is set and resolves the checks at compile time.
fn link_clang_builtins_on_macos() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    let Ok(output) = std::process::Command::new("cc")
        .arg("-print-runtime-dir")
        .output()
    else {
        return;
    };

    let dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if dir.is_empty()
        || !std::path::Path::new(&dir)
            .join("libclang_rt.osx.a")
            .exists()
    {
        // With no runtime library there is nothing to link. If the symbol is
        // required, the linker will report the missing dependency clearly.
        return;
    }

    println!("cargo:rustc-link-search=native={dir}");
    println!("cargo:rustc-link-lib=static=clang_rt.osx");
    println!("cargo:rerun-if-changed=build.rs");
}
