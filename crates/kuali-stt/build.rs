fn main() {
    // Match the runtimes shipped by the pinned sherpa-onnx 1.13.7 desktop
    // archives. These are headers only: sherpa-onnx-sys links the runtime.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").expect("Cargo target OS");
    let ort = if target_os == "macos" {
        "native/include/onnxruntime-1.28.1"
    } else {
        "native/include/onnxruntime-1.27.1"
    };
    println!("cargo:rerun-if-changed=native");
    cc::Build::new()
        .cpp(true)
        .std("c++17")
        .include("native/include")
        .include(ort)
        .file("native/diarization.cc")
        .compile("kuali_diarization");
}
