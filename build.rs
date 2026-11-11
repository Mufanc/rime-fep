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

const OPENCC_FILES: &[&str] = &[
    "t2s.json",
    "TSCharacters.ocd2",
    "TSPhrases.ocd2",
    "t2hk.json",
    "HKVariants.ocd2",
    "t2tw.json",
    "TWVariants.ocd2",
];

fn main() -> io::Result<()> {
    println!("cargo:rerun-if-env-changed=RIME_INCLUDE_DIR");
    println!("cargo:rerun-if-env-changed=RIME_LIB_DIR");
    println!("cargo:rerun-if-env-changed=RIME_OPENCC_DATA_DIR");

    let static_link = env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("musl");

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

    link_system_rime(static_link);
    println!("cargo:rerun-if-changed=src/rime_shim.c");
    compress_builtin_files()?;

    embed_opencc_files(static_link)
}

fn compress_builtin_files() -> io::Result<()> {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let mut manifest = File::create(out_dir.join("builtin_files.rs"))?;

    writeln!(manifest, "&[")?;

    for &source in BUILTIN_FILES {
        println!("cargo:rerun-if-changed={source}");

        let (archive, length) = compress_file(&out_dir, Path::new(source))?;
        let name = source.rsplit('/').next().unwrap();

        writeln!(
            manifest,
            "    ({name:?}, include_bytes!({archive:?}), {length}),"
        )?;
    }

    writeln!(manifest, "]")
}

fn embed_opencc_files(static_link: bool) -> io::Result<()> {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let mut manifest = File::create(out_dir.join("opencc_files.rs"))?;
    let data_dir = static_link.then(|| {
        PathBuf::from(
            env::var_os("RIME_OPENCC_DATA_DIR").expect("musl builds require RIME_OPENCC_DATA_DIR"),
        )
    });

    writeln!(manifest, "&[")?;

    for &name in OPENCC_FILES {
        if let Some(data_dir) = &data_dir {
            let path = data_dir.join(name);

            println!("cargo:rerun-if-changed={}", path.display());

            let (archive, _) = compress_file(&out_dir, &path)?;

            writeln!(
                manifest,
                "    ({name:?}, Some(include_bytes!({archive:?}))),"
            )?;
        } else {
            writeln!(manifest, "    ({name:?}, None),")?;
        }
    }

    writeln!(manifest, "]")
}

fn compress_file(out_dir: &Path, source: &Path) -> io::Result<(String, usize)> {
    let archive = format!("{}.gz", source.file_name().unwrap().to_str().unwrap());
    let contents = fs::read(source)?;
    let mut encoder = GzEncoder::new(contents.as_slice(), Compression::best());

    io::copy(&mut encoder, &mut File::create(out_dir.join(&archive))?)?;

    Ok((archive, contents.len()))
}

fn link_system_rime(static_link: bool) {
    let library_name = if static_link {
        "librime.a"
    } else if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        "librime.dylib"
    } else {
        "librime.so"
    };
    let lib_dir = find_directory(
        "RIME_LIB_DIR",
        &["/opt/homebrew/lib", "/usr/local/lib", "/usr/lib"],
        library_name,
    );

    println!("cargo:rustc-link-search=native={}", lib_dir.display());

    if static_link {
        for library in ["rime", "opencc", "marisa", "leveldb", "yaml-cpp", "stdc++"] {
            println!("cargo:rustc-link-lib=static={library}");
        }
    } else {
        println!("cargo:rustc-link-lib=dylib=rime");
    }
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
