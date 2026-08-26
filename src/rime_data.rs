use std::env;
use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use directories::BaseDirs;
use tempfile::TempDir;
use walkdir::{DirEntry, WalkDir};

const BUILTIN_FILES: &[(&str, &[u8])] = &[
    ("default.yaml", include_bytes!("../assets/default.yaml")),
    (
        "key_bindings.yaml",
        include_bytes!("../vendor/rime-prelude/key_bindings.yaml"),
    ),
    (
        "punctuation.yaml",
        include_bytes!("../vendor/rime-prelude/punctuation.yaml"),
    ),
    (
        "symbols.yaml",
        include_bytes!("../vendor/rime-prelude/symbols.yaml"),
    ),
    (
        "pinyin.yaml",
        include_bytes!("../vendor/rime-luna-pinyin/pinyin.yaml"),
    ),
    (
        "luna_pinyin.schema.yaml",
        include_bytes!("../vendor/rime-luna-pinyin/luna_pinyin.schema.yaml"),
    ),
    (
        "luna_pinyin.dict.yaml",
        include_bytes!("../vendor/rime-luna-pinyin/luna_pinyin.dict.yaml"),
    ),
    (
        "stroke.schema.yaml",
        include_bytes!("../vendor/rime-stroke/stroke.schema.yaml"),
    ),
    (
        "stroke.dict.yaml",
        include_bytes!("../vendor/rime-stroke/stroke.dict.yaml"),
    ),
    (
        "essay.txt",
        include_bytes!("../vendor/rime-essay/essay.txt"),
    ),
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

pub struct PreparedData {
    pub shared_data_dir: PathBuf,
    pub prebuilt_data_dir: PathBuf,
    pub user_data_dir: TempDir,
    pub deploy: bool,
    ready_marker: PathBuf,
    _lock: File,
}

impl PreparedData {
    pub fn finish(self) -> Result<TempDir, Box<dyn Error>> {
        if self.deploy {
            if !self.prebuilt_data_dir.join("default.yaml").is_file() {
                return Err("Rime did not compile the built-in schema".into());
            }
            fs::write(&self.ready_marker, b"ready\n")?;
        }
        Ok(self.user_data_dir)
    }
}

pub fn prepare(
    config_dir: Option<&Path>,
    rime_version: &[u8],
) -> Result<PreparedData, Box<dyn Error>> {
    let config_dir = config_dir.map(fs::canonicalize).transpose()?;
    let config_files = config_dir
        .as_deref()
        .map(collect_config_files)
        .transpose()?
        .unwrap_or_default();
    let opencc_data_dir = opencc_data_dir(config_dir.as_deref())?;
    let fingerprint = fingerprint(&config_files, &opencc_data_dir, rime_version)?;
    let cache_root = cache_root()?;

    fs::create_dir_all(&cache_root)?;

    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(cache_root.join("deploy.lock"))?;

    lock.lock()?;

    let data_dir = cache_root.join(format!("{fingerprint:016x}"));
    let shared_data_dir = data_dir.join("source");
    let prebuilt_data_dir = data_dir.join("build");
    let ready_marker = data_dir.join("ready");
    let deploy = !ready_marker.is_file() || !prebuilt_data_dir.join("default.yaml").is_file();

    if deploy {
        write_sources(&shared_data_dir, &opencc_data_dir, &config_files)?;
        fs::create_dir_all(&prebuilt_data_dir)?;
    }

    let user_data_dir = tempfile::Builder::new().prefix("rime-fep-").tempdir()?;

    for file in &config_files {
        if file.relative.parent().is_none()
            && file
                .relative
                .file_name()
                .is_some_and(|name| name.as_encoded_bytes().ends_with(b".custom.yaml"))
        {
            fs::copy(&file.source, user_data_dir.path().join(&file.relative))?;
        }
    }

    Ok(PreparedData {
        shared_data_dir,
        prebuilt_data_dir,
        user_data_dir,
        deploy,
        ready_marker,
        _lock: lock,
    })
}

struct ConfigFile {
    relative: PathBuf,
    source: PathBuf,
}

fn collect_config_files(root: &Path) -> Result<Vec<ConfigFile>, Box<dyn Error>> {
    if !root.is_dir() {
        return Err(format!("{} is not a directory", root.display()).into());
    }

    let mut files = Vec::new();

    for entry in WalkDir::new(root)
        .follow_links(true)
        .into_iter()
        .filter_entry(keep_entry)
    {
        let entry = entry?;

        if !entry.file_type().is_file() {
            continue;
        }

        let relative = entry.path().strip_prefix(root)?.to_path_buf();

        if matches!(
            relative.file_name().and_then(|name| name.to_str()),
            Some("installation.yaml" | "user.yaml")
        ) {
            continue;
        }
        files.push(ConfigFile {
            relative,
            source: entry.into_path(),
        });
    }

    files.sort_by(|left, right| left.relative.cmp(&right.relative));
    Ok(files)
}

fn keep_entry(entry: &DirEntry) -> bool {
    entry.depth() == 0
        || !entry.file_type().is_dir()
        || !matches!(entry.file_name().to_str(), Some(".git" | "build" | "sync"))
}

fn fingerprint(
    config_files: &[ConfigFile],
    opencc_data_dir: &Path,
    rime_version: &[u8],
) -> Result<u64, Box<dyn Error>> {
    let mut hasher = std::hash::DefaultHasher::new();

    rime_version.hash(&mut hasher);

    for (name, contents) in BUILTIN_FILES {
        name.hash(&mut hasher);
        contents.hash(&mut hasher);
    }

    for name in OPENCC_FILES {
        name.hash(&mut hasher);
        fs::read(opencc_data_dir.join(name))?.hash(&mut hasher);
    }

    for file in config_files {
        file.relative.hash(&mut hasher);
        hash_file(&file.source, &mut hasher)?;
    }

    Ok(hasher.finish())
}

fn write_sources(
    shared_data_dir: &Path,
    opencc_data_dir: &Path,
    config_files: &[ConfigFile],
) -> Result<(), Box<dyn Error>> {
    for (relative, contents) in BUILTIN_FILES {
        write_file(&shared_data_dir.join(relative), contents)?;
    }

    for name in OPENCC_FILES {
        write_file(
            &shared_data_dir.join("opencc").join(name),
            &fs::read(opencc_data_dir.join(name))?,
        )?;
    }

    for file in config_files {
        copy_file(&file.source, &shared_data_dir.join(&file.relative))?;
    }

    Ok(())
}

fn write_file(path: &Path, contents: &[u8]) -> Result<(), Box<dyn Error>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    fs::write(path, contents)?;
    Ok(())
}

fn copy_file(source: &Path, destination: &Path) -> Result<(), Box<dyn Error>> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }

    fs::copy(source, destination)?;
    Ok(())
}

fn hash_file(path: &Path, hasher: &mut impl Hasher) -> io::Result<()> {
    let mut file = File::open(path)?;
    let mut buffer = [0; 64 * 1024];

    loop {
        let length = file.read(&mut buffer)?;
        if length == 0 {
            return Ok(());
        }
        hasher.write(&buffer[..length]);
    }
}

fn cache_root() -> Result<PathBuf, Box<dyn Error>> {
    if let Some(path) = env::var_os("RIME_FEP_CACHE_DIR") {
        return Ok(PathBuf::from(path));
    }

    Ok(BaseDirs::new()
        .ok_or("cannot determine the cache directory")?
        .cache_dir()
        .join("rime-fep"))
}

fn opencc_data_dir(config_dir: Option<&Path>) -> Result<PathBuf, Box<dyn Error>> {
    if let Some(path) = env::var_os("RIME_OPENCC_DATA_DIR") {
        return require_opencc_data(PathBuf::from(path), "RIME_OPENCC_DATA_DIR");
    }

    let mut candidates = Vec::new();

    if let Some(config_dir) = config_dir {
        candidates.push(config_dir.join("opencc"));
    }
    if let Some(path) = env::var_os("RIME_SHARED_DATA_DIR") {
        candidates.push(PathBuf::from(path).join("opencc"));
    }
    candidates.extend([
        PathBuf::from("/opt/homebrew/share/opencc"),
        PathBuf::from("/usr/local/share/opencc"),
        PathBuf::from("/usr/share/opencc"),
    ]);

    candidates
        .into_iter()
        .find(|path| OPENCC_FILES.iter().all(|name| path.join(name).is_file()))
        .ok_or_else(|| "cannot find OpenCC data; set RIME_OPENCC_DATA_DIR".into())
}

fn require_opencc_data(path: PathBuf, variable: &str) -> Result<PathBuf, Box<dyn Error>> {
    if OPENCC_FILES.iter().all(|name| path.join(name).is_file()) {
        Ok(path)
    } else {
        Err(format!("{variable} does not contain the required OpenCC data").into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_generated_user_data() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("build")).unwrap();
        fs::write(directory.path().join("default.custom.yaml"), "patch: {}\n").unwrap();
        fs::write(directory.path().join("user.yaml"), "var: {}\n").unwrap();
        fs::write(directory.path().join("build/default.yaml"), "generated\n").unwrap();

        let files = collect_config_files(directory.path()).unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].relative, Path::new("default.custom.yaml"));
    }
}
