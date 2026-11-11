use std::env;
use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use blake3::Hasher;
use directories::BaseDirs;
use flate2::read::GzDecoder;
use walkdir::{DirEntry, WalkDir};

const FINGERPRINT_VERSION: &[u8] = b"rime-fep-fingerprint-v1";

const BUILTIN_FILES: &[(&str, &[u8], u64)] =
    include!(concat!(env!("OUT_DIR"), "/builtin_files.rs"));

const OPENCC_FILES: &[(&str, Option<&[u8]>)] =
    include!(concat!(env!("OUT_DIR"), "/opencc_files.rs"));

pub struct PreparedData {
    pub fingerprint: String,
    pub shared_data_dir: PathBuf,
    pub prebuilt_data_dir: PathBuf,
    pub user_data_dir: PathBuf,
    pub deploy: bool,
    ready_marker: PathBuf,
    _lock: File,
}

impl PreparedData {
    pub fn finish(self) -> Result<PathBuf, Box<dyn Error>> {
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
    let fingerprint =
        calculate_fingerprint(&config_files, opencc_data_dir.as_deref(), rime_version)?
            .to_hex()
            .to_string();
    let cache_root = cache_root()?;

    fs::create_dir_all(&cache_root)?;

    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(cache_root.join("deploy.lock"))?;

    lock.lock()?;

    let data_dir = cache_root.join(&fingerprint);
    let shared_data_dir = data_dir.join("source");
    let prebuilt_data_dir = data_dir.join("build");
    let ready_marker = data_dir.join("ready");
    let deploy = !ready_marker.is_file() || !prebuilt_data_dir.join("default.yaml").is_file();

    if deploy {
        write_sources(&shared_data_dir, opencc_data_dir.as_deref(), &config_files)?;
        fs::create_dir_all(&prebuilt_data_dir)?;
    }

    let user_data_dir = data_root()?.join("user").join(&fingerprint);
    fs::create_dir_all(&user_data_dir)?;

    for file in &config_files {
        if file.relative.parent().is_none()
            && file
                .relative
                .file_name()
                .is_some_and(|name| name.as_encoded_bytes().ends_with(b".custom.yaml"))
        {
            fs::copy(&file.source, user_data_dir.join(&file.relative))?;
        }
    }

    Ok(PreparedData {
        fingerprint,
        shared_data_dir,
        prebuilt_data_dir,
        user_data_dir,
        deploy,
        ready_marker,
        _lock: lock,
    })
}

pub fn fingerprint(
    config_dir: Option<&Path>,
    rime_version: &[u8],
) -> Result<String, Box<dyn Error>> {
    let config_dir = config_dir.map(fs::canonicalize).transpose()?;
    let config_files = config_dir
        .as_deref()
        .map(collect_config_files)
        .transpose()?
        .unwrap_or_default();
    let opencc_data_dir = opencc_data_dir(config_dir.as_deref())?;

    Ok(
        calculate_fingerprint(&config_files, opencc_data_dir.as_deref(), rime_version)?
            .to_hex()
            .to_string(),
    )
}

pub fn runtime_dir() -> Result<PathBuf, Box<dyn Error>> {
    Ok(cache_root()?.join("run"))
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

fn calculate_fingerprint(
    config_files: &[ConfigFile],
    opencc_data_dir: Option<&Path>,
    rime_version: &[u8],
) -> Result<blake3::Hash, Box<dyn Error>> {
    let mut hasher = Hasher::new();

    hash_bytes(&mut hasher, FINGERPRINT_VERSION);
    hash_bytes(&mut hasher, rime_version);

    for &(name, contents, length) in BUILTIN_FILES {
        hash_bytes(&mut hasher, b"builtin");
        hash_bytes(&mut hasher, name.as_bytes());
        hash_reader(GzDecoder::new(contents), length, &mut hasher)?;
    }

    for &(name, embedded) in OPENCC_FILES {
        hash_bytes(&mut hasher, b"opencc");
        hash_bytes(&mut hasher, name.as_bytes());
        hash_bytes(
            &mut hasher,
            &load_opencc_file(opencc_data_dir, name, embedded)?,
        );
    }

    for file in config_files {
        hash_bytes(&mut hasher, b"config");
        hash_bytes(&mut hasher, file.relative.as_os_str().as_encoded_bytes());
        hash_file(&file.source, &mut hasher)?;
    }

    Ok(hasher.finalize())
}

fn write_sources(
    shared_data_dir: &Path,
    opencc_data_dir: Option<&Path>,
    config_files: &[ConfigFile],
) -> Result<(), Box<dyn Error>> {
    fs::create_dir_all(shared_data_dir)?;

    for &(relative, contents, _) in BUILTIN_FILES {
        let mut file = File::create(shared_data_dir.join(relative))?;
        io::copy(&mut GzDecoder::new(contents), &mut file)?;
    }

    for &(name, embedded) in OPENCC_FILES {
        write_file(
            &shared_data_dir.join("opencc").join(name),
            &load_opencc_file(opencc_data_dir, name, embedded)?,
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

fn hash_bytes(hasher: &mut Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn hash_file(path: &Path, hasher: &mut Hasher) -> io::Result<()> {
    let file = File::open(path)?;
    let length = file.metadata()?.len();
    hash_reader(file, length, hasher)
}

fn hash_reader(mut reader: impl Read, length: u64, hasher: &mut Hasher) -> io::Result<()> {
    hasher.update(&length.to_le_bytes());
    io::copy(&mut reader, hasher)?;
    Ok(())
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

fn data_root() -> Result<PathBuf, Box<dyn Error>> {
    if let Some(path) = env::var_os("RIME_FEP_DATA_DIR") {
        return Ok(PathBuf::from(path));
    }

    Ok(BaseDirs::new()
        .ok_or("cannot determine the data directory")?
        .data_dir()
        .join("rime-fep"))
}

fn opencc_data_dir(config_dir: Option<&Path>) -> Result<Option<PathBuf>, Box<dyn Error>> {
    if let Some(path) = env::var_os("RIME_OPENCC_DATA_DIR") {
        return require_opencc_data(PathBuf::from(path), "RIME_OPENCC_DATA_DIR").map(Some);
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

    let data_dir = candidates.into_iter().find(|path| {
        OPENCC_FILES
            .iter()
            .all(|(name, _)| path.join(name).is_file())
    });

    if data_dir.is_some() || OPENCC_FILES.iter().all(|(_, contents)| contents.is_some()) {
        return Ok(data_dir);
    }

    Err("cannot find OpenCC data; set RIME_OPENCC_DATA_DIR".into())
}

fn require_opencc_data(path: PathBuf, variable: &str) -> Result<PathBuf, Box<dyn Error>> {
    if OPENCC_FILES
        .iter()
        .all(|(name, _)| path.join(name).is_file())
    {
        Ok(path)
    } else {
        Err(format!("{variable} does not contain the required OpenCC data").into())
    }
}

fn load_opencc_file(
    directory: Option<&Path>,
    name: &str,
    embedded: Option<&[u8]>,
) -> io::Result<Vec<u8>> {
    if let Some(directory) = directory {
        return fs::read(directory.join(name));
    }

    let contents = embedded.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("missing OpenCC data: {name}"),
        )
    })?;
    let mut decoded = Vec::new();

    GzDecoder::new(contents).read_to_end(&mut decoded)?;

    Ok(decoded)
}

#[cfg(test)]
mod tests {
    use flate2::Compression;
    use flate2::read::GzEncoder;

    use super::*;

    #[test]
    fn fingerprint_fields_are_framed() {
        let mut left = Hasher::new();
        hash_bytes(&mut left, b"a");
        hash_bytes(&mut left, b"bc");

        let mut right = Hasher::new();
        hash_bytes(&mut right, b"ab");
        hash_bytes(&mut right, b"c");

        assert_ne!(left.finalize(), right.finalize());
    }

    #[test]
    fn hashes_builtin_data_as_original_bytes() {
        let &(_, contents, length) = BUILTIN_FILES
            .iter()
            .find(|(name, _, _)| *name == "essay.txt")
            .unwrap();
        let mut original = Hasher::new();
        hash_bytes(
            &mut original,
            include_bytes!("../vendor/rime-essay/essay.txt"),
        );

        let mut compressed = Hasher::new();
        hash_reader(GzDecoder::new(contents), length, &mut compressed).unwrap();

        assert_eq!(compressed.finalize(), original.finalize());
    }

    #[test]
    fn opencc_directory_overrides_embedded_data() {
        let mut encoder = GzEncoder::new(&b"builtin"[..], Compression::fast());
        let mut embedded = Vec::new();
        encoder.read_to_end(&mut embedded).unwrap();
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("t2s.json"), b"custom").unwrap();

        assert_eq!(
            load_opencc_file(Some(directory.path()), "t2s.json", Some(&embedded)).unwrap(),
            b"custom"
        );
        assert_eq!(
            load_opencc_file(None, "t2s.json", Some(&embedded)).unwrap(),
            b"builtin"
        );
        assert!(load_opencc_file(Some(directory.path()), "missing.json", Some(&embedded)).is_err());
        assert!(load_opencc_file(None, "t2s.json", None).is_err());
        assert!(load_opencc_file(None, "t2s.json", Some(b"invalid gzip")).is_err());
    }

    #[cfg(target_env = "musl")]
    #[test]
    fn embedded_opencc_matches_external_fingerprint_and_deployment() {
        let directory = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();

        for &(name, contents) in OPENCC_FILES {
            let decoded = load_opencc_file(None, name, contents).unwrap();
            fs::write(directory.path().join(name), decoded).unwrap();
        }

        assert_eq!(
            calculate_fingerprint(&[], None, b"test").unwrap(),
            calculate_fingerprint(&[], Some(directory.path()), b"test").unwrap()
        );
        write_sources(source.path(), None, &[]).unwrap();

        for &(name, _) in OPENCC_FILES {
            assert_eq!(
                fs::read(source.path().join("opencc").join(name)).unwrap(),
                fs::read(directory.path().join(name)).unwrap()
            );
        }
    }

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
