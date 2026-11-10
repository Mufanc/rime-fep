use std::env;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::read::GzEncoder;

// The order of these files is part of the Rime data fingerprint.
const BUILTIN_FILES: &[&str] = &[
    "assets/default.yaml",
    "vendor/rime-prelude/key_bindings.yaml",
    "vendor/rime-prelude/punctuation.yaml",
    "vendor/rime-prelude/symbols.yaml",
    "vendor/rime-luna-pinyin/pinyin.yaml",
    "vendor/rime-luna-pinyin/luna_pinyin.schema.yaml",
    "assets/luna_pinyin.custom.yaml",
    "vendor/rime-luna-pinyin/luna_pinyin.dict.yaml",
    "vendor/rime-stroke/stroke.schema.yaml",
    "vendor/rime-stroke/stroke.dict.yaml",
    "vendor/rime-essay/essay.txt",
];

fn main() -> io::Result<()> {
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

    compress_builtin_files()
}

fn compress_builtin_files() -> io::Result<()> {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let mut manifest = File::create(out_dir.join("builtin_files.rs"))?;

    writeln!(manifest, "&[")?;

    for &source in BUILTIN_FILES {
        println!("cargo:rerun-if-changed={source}");

        let name = source.rsplit('/').next().unwrap();
        let archive = format!("{name}.gz");
        let contents = fs::read(source)?;
        let mut encoder = GzEncoder::new(contents.as_slice(), Compression::best());

        io::copy(&mut encoder, &mut File::create(out_dir.join(&archive))?)?;
        writeln!(
            manifest,
            "    ({name:?}, include_bytes!({archive:?}), {}),",
            contents.len()
        )?;
    }

    writeln!(manifest, "]")
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
