use std::env;
use std::fs;
use std::path::PathBuf;

#[cfg(not(target_os = "windows"))]
fn main() {
    let bindings = bindgen::Builder::default()
        .header("libkrun_display.h")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("Unable to generate bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("display_header.rs"))
        .expect("Couldn't write bindings!");
}

#[cfg(target_os = "windows")]
fn main() {
    println!("cargo:rerun-if-changed=bindings/windows.rs");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    fs::copy("bindings/windows.rs", out_path.join("display_header.rs"))
        .expect("Couldn't copy Windows display bindings!");
}
