use std::env;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=RIME_INCLUDE_DIR");
    println!("cargo:rerun-if-env-changed=RIME_LIB_DIR");

    let include_dir = find_directory(
        "RIME_INCLUDE_DIR",
        &[
            "/opt/homebrew/include",
            "/usr/local/include",
            "/usr/include",
        ],
        "rime_api.h",
    );
    cc::Build::new()
        .file("src/rime_shim.c")
        .include(include_dir)
        .warnings(true)
        .compile("rime_fep_rime");

    link_system_rime();
    println!("cargo:rerun-if-changed=src/rime_shim.c");
}

fn link_system_rime() {
    let lib_dir = find_directory(
        "RIME_LIB_DIR",
        &["/opt/homebrew/lib", "/usr/local/lib", "/usr/lib"],
        library_name(),
    );
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=dylib=rime");
}

fn find_directory(variable: &str, candidates: &[&str], file: &str) -> PathBuf {
    if let Some(directory) = env::var_os(variable).map(PathBuf::from) {
        if directory.join(file).is_file() {
            return directory;
        }
        panic!("{variable} does not contain {file}");
    }

    candidates
        .iter()
        .map(Path::new)
        .find(|directory| directory.join(file).is_file())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| panic!("cannot find {file}; set {variable}"))
}

#[cfg(target_os = "macos")]
fn library_name() -> &'static str {
    "librime.dylib"
}

#[cfg(not(target_os = "macos"))]
fn library_name() -> &'static str {
    "librime.so"
}
