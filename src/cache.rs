use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const CACHE_DIR_MODE: u32 = 0o700;
const CACHE_FILE_MODE: u32 = 0o600;
const MAX_CACHE_ENTRY_SIZE: u64 = 256 * 1024 * 1024;
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];

#[derive(Clone, Debug)]
pub struct Cache {
    root: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Envelope {
    payload: String,
    payload_sha256: String,
    created_unix: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Cached<T> {
    pub payload: T,
    pub created_unix: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CachedBody {
    pub bytes: Vec<u8>,
    pub length: u64,
    pub etag: String,
    pub sha256: String,
    #[serde(default)]
    pub source_version: Option<crate::SourceVersion>,
}

impl Cache {
    pub fn discover() -> Option<Self> {
        let root = cache_root()?;
        Self::open(root).ok()
    }

    pub fn from_root(root: PathBuf) -> Option<Self> {
        Self::open(root).ok()
    }

    pub fn open(root: PathBuf) -> std::io::Result<Self> {
        prepare_directory(&root)?;
        Ok(Self { root })
    }

    /// Removes every cached entry under the cache root. A missing root is not an error.
    pub fn clear(&self) -> std::io::Result<()> {
        verify_directory(&self.root)?;
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_dir() {
                fs::remove_dir_all(&path)?;
            } else {
                fs::remove_file(&path)?;
            }
        }
        Ok(())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn load<T: DeserializeOwned>(&self, namespace: &str, key: &[u8]) -> Option<Cached<T>> {
        let path = self.entry_path(namespace, key)?;
        let directory = path.parent()?;
        verify_directory(&self.root).ok()?;
        verify_private_directory(directory).ok()?;

        let metadata = fs::symlink_metadata(&path).ok()?;
        if !metadata.file_type().is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() > MAX_CACHE_ENTRY_SIZE
            || !safe_file_metadata(&metadata)
        {
            return None;
        }
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let file = options.open(&path).ok()?;
        let opened = file.metadata().ok()?;
        if !opened.file_type().is_file() || !safe_file_metadata(&opened) {
            return None;
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_CACHE_ENTRY_SIZE + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() as u64 > MAX_CACHE_ENTRY_SIZE {
            return None;
        }
        let envelope: Envelope = if bytes.starts_with(&GZIP_MAGIC) {
            let mut decoded = Vec::new();
            flate2::read::GzDecoder::new(bytes.as_slice())
                .take(MAX_CACHE_ENTRY_SIZE + 1)
                .read_to_end(&mut decoded)
                .ok()?;
            if decoded.len() as u64 > MAX_CACHE_ENTRY_SIZE {
                return None;
            }
            serde_json::from_slice(&decoded).ok()?
        } else {
            serde_json::from_slice(&bytes).ok()?
        };
        if sha256_hex(envelope.payload.as_bytes()) != envelope.payload_sha256 {
            return None;
        }
        let payload = serde_json::from_str(&envelope.payload).ok()?;
        Some(Cached {
            payload,
            created_unix: envelope.created_unix,
        })
    }

    pub fn store<T: Serialize>(
        &self,
        namespace: &str,
        key: &[u8],
        payload: &T,
    ) -> std::io::Result<()> {
        let path = self.entry_path(namespace, key).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid cache namespace")
        })?;
        verify_directory(&self.root)?;
        let directory = path.parent().expect("cache entry always has a parent");
        prepare_private_directory(directory)?;
        let payload = serde_json::to_string(payload)?;
        let envelope = Envelope {
            payload_sha256: sha256_hex(payload.as_bytes()),
            payload,
            created_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        };
        let json = serde_json::to_vec(&envelope)?;
        let encoded = {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(6));
            encoder.write_all(&json)?;
            encoder.finish()?
        };
        if encoded.len() as u64 > MAX_CACHE_ENTRY_SIZE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "cache entry exceeds size limit",
            ));
        }
        let (temporary_path, mut temporary_file) = create_temporary_file(directory, &path)?;
        let result = (|| {
            temporary_file.write_all(&encoded)?;
            temporary_file.sync_all()?;
            drop(temporary_file);
            verify_directory(&self.root)?;
            verify_private_directory(directory)?;
            match fs::symlink_metadata(&path) {
                Ok(metadata)
                    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() =>
                {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "refusing to replace an unsafe cache entry",
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            fs::rename(&temporary_path, &path)?;
            sync_directory(directory)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary_path);
        }
        result
    }

    pub fn load_body(&self, key: &[u8], expected_etag: &str) -> Option<CachedBody> {
        let body = self.load::<CachedBody>("object-bodies", key)?.payload;
        if body.etag != expected_etag
            || body.length != body.bytes.len() as u64
            || sha256_hex(&body.bytes) != body.sha256
        {
            return None;
        }
        Some(body)
    }

    pub fn store_body(&self, key: &[u8], etag: &str, bytes: &[u8]) -> std::io::Result<()> {
        self.store_body_with_metadata(key, etag, bytes, None)
    }

    pub fn store_body_with_metadata(
        &self,
        key: &[u8],
        etag: &str,
        bytes: &[u8],
        source_version: Option<&crate::SourceVersion>,
    ) -> std::io::Result<()> {
        self.store(
            "object-bodies",
            key,
            &CachedBody {
                bytes: bytes.to_vec(),
                length: bytes.len() as u64,
                etag: etag.to_string(),
                sha256: sha256_hex(bytes),
                source_version: source_version.cloned(),
            },
        )
    }

    fn entry_path(&self, namespace: &str, key: &[u8]) -> Option<PathBuf> {
        if namespace.is_empty()
            || !namespace
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        {
            return None;
        }
        Some(
            self.root
                .join(namespace)
                .join(format!("{}.json.gz", sha256_hex(key))),
        )
    }
}

fn cache_root() -> Option<PathBuf> {
    if let Some(root) = std::env::var_os("XDG_CACHE_HOME").filter(|path| !path.is_empty()) {
        return Some(PathBuf::from(root).join("tfdiff"));
    }
    std::env::var_os("HOME")
        .filter(|path| !path.is_empty())
        .map(|home| PathBuf::from(home).join(".cache").join("tfdiff"))
}

fn prepare_directory(path: &Path) -> std::io::Result<()> {
    if path.as_os_str().is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "cache path must not be empty",
        ));
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut cursor = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::RootDir | Component::Prefix(_) => cursor.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                cursor.push(component.as_os_str());
            }
            Component::Normal(name) => {
                cursor.push(name);
                match fs::symlink_metadata(&cursor) {
                    Ok(metadata) => verify_ancestor_directory(&metadata)?,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        match create_private_directory(&cursor) {
                            Ok(()) => {}
                            Err(create_error)
                                if create_error.kind() == std::io::ErrorKind::AlreadyExists =>
                            {
                                let metadata = fs::symlink_metadata(&cursor)?;
                                verify_ancestor_directory(&metadata)?;
                            }
                            Err(create_error) => return Err(create_error),
                        }
                    }
                    Err(error) => return Err(error),
                }
            }
        }
    }
    verify_private_directory(&absolute)
}

fn prepare_private_directory(path: &Path) -> std::io::Result<()> {
    verify_directory(path).or_else(|error| {
        if error.kind() != std::io::ErrorKind::NotFound {
            return Err(error);
        }
        match create_private_directory(path) {
            Ok(()) => verify_directory(path),
            Err(create_error) if create_error.kind() == std::io::ErrorKind::AlreadyExists => {
                verify_directory(path)
            }
            Err(create_error) => Err(create_error),
        }
    })
}

fn create_private_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.mode(CACHE_DIR_MODE).create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir(path)
    }
}

fn verify_ancestor_directory(metadata: &fs::Metadata) -> std::io::Result<()> {
    if metadata.file_type().is_symlink() || !metadata.is_dir() || !safe_ancestor_metadata(metadata)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "cache path contains a symlink or unsafe directory",
        ));
    }
    Ok(())
}

fn verify_directory(path: &Path) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() || !safe_private_metadata(&metadata)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "cache directory has unsafe ownership or permissions",
        ));
    }
    Ok(())
}

fn verify_private_directory(path: &Path) -> std::io::Result<()> {
    verify_directory(path)
}

#[cfg(unix)]
fn safe_ancestor_metadata(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    let uid = unsafe { libc::geteuid() };
    let mode = metadata.mode();
    (metadata.uid() == uid || metadata.uid() == 0) && (mode & 0o002 == 0 || mode & 0o1000 != 0)
}

#[cfg(not(unix))]
fn safe_ancestor_metadata(_metadata: &fs::Metadata) -> bool {
    true
}

#[cfg(unix)]
fn safe_private_metadata(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o777 == CACHE_DIR_MODE
}

#[cfg(not(unix))]
fn safe_private_metadata(_metadata: &fs::Metadata) -> bool {
    true
}

#[cfg(unix)]
fn safe_file_metadata(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o777 == CACHE_FILE_MODE
}

#[cfg(not(unix))]
fn safe_file_metadata(_metadata: &fs::Metadata) -> bool {
    true
}

fn create_temporary_file(directory: &Path, destination: &Path) -> std::io::Result<(PathBuf, File)> {
    let name = destination.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid cache file")
    })?;
    for attempt in 0..32_u32 {
        let temporary = directory.join(format!(
            ".{}.{}.{}.tmp",
            name.to_string_lossy(),
            std::process::id(),
            attempt
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(CACHE_FILE_MODE)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        match options.open(&temporary) {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a temporary cache file",
    ))
}

fn sync_directory(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    /// Reads a cache entry as JSON, decompressing it first when it is gzipped.
    fn read_entry_json(path: &Path) -> serde_json::Value {
        let raw = fs::read(path).unwrap();
        let json = if raw.starts_with(&GZIP_MAGIC) {
            let mut decoded = Vec::new();
            flate2::read::GzDecoder::new(raw.as_slice())
                .read_to_end(&mut decoded)
                .unwrap();
            decoded
        } else {
            raw
        };
        serde_json::from_slice(&json).unwrap()
    }

    /// Writes a cache entry back in the same gzipped format the cache produces.
    fn write_entry_json(path: &Path, value: &serde_json::Value) {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(6));
        encoder
            .write_all(&serde_json::to_vec(value).unwrap())
            .unwrap();
        fs::write(path, encoder.finish().unwrap()).unwrap();
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct TestEntry {
        value: String,
    }

    #[test]
    fn stores_and_loads_checksummed_entries_with_private_permissions() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("cache");
        let cache = Cache::open(root.clone()).unwrap();
        let value = TestEntry {
            value: "secret".into(),
        };
        cache.store("listings", b"identity", &value).unwrap();
        assert_eq!(
            cache
                .load::<TestEntry>("listings", b"identity")
                .unwrap()
                .payload,
            value
        );
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            CACHE_DIR_MODE
        );
        let nested = root.join("listings");
        assert_eq!(
            fs::metadata(&nested).unwrap().permissions().mode() & 0o777,
            CACHE_DIR_MODE
        );
        let entry = cache.entry_path("listings", b"identity").unwrap();
        assert_eq!(
            fs::metadata(entry).unwrap().permissions().mode() & 0o777,
            CACHE_FILE_MODE
        );
    }

    #[test]
    fn corrupt_entries_are_cache_misses() {
        let temp = TempDir::new().unwrap();
        let cache = Cache::open(temp.path().join("cache")).unwrap();
        cache
            .store(
                "listings",
                b"identity",
                &TestEntry {
                    value: "secret".into(),
                },
            )
            .unwrap();
        let entry = cache.entry_path("listings", b"identity").unwrap();
        let mut bytes: serde_json::Value = read_entry_json(&entry);
        let mut payload: serde_json::Value =
            serde_json::from_str(bytes["payload"].as_str().unwrap()).unwrap();
        payload["value"] = serde_json::Value::String("altered".into());
        bytes["payload"] = serde_json::Value::String(serde_json::to_string(&payload).unwrap());
        write_entry_json(&entry, &bytes);
        assert!(cache.load::<TestEntry>("listings", b"identity").is_none());
    }

    #[test]
    fn refuses_symlinked_roots_and_entries() {
        let temp = TempDir::new().unwrap();
        let real = temp.path().join("real");
        let _cache = Cache::open(real.clone()).unwrap();
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(Cache::open(link).is_err());

        let cache = Cache::open(temp.path().join("another")).unwrap();
        cache
            .store("listings", b"id", &TestEntry { value: "v".into() })
            .unwrap();
        let entry = cache.entry_path("listings", b"id").unwrap();
        let outside = temp.path().join("outside");
        fs::write(&outside, b"outside").unwrap();
        fs::remove_file(&entry).unwrap();
        std::os::unix::fs::symlink(&outside, &entry).unwrap();
        assert!(cache.load::<TestEntry>("listings", b"id").is_none());
    }

    #[test]
    fn body_cache_checks_etag_length_and_checksum() {
        let temp = TempDir::new().unwrap();
        let cache = Cache::open(temp.path().join("cache")).unwrap();
        cache.store_body(b"body", "opaque-tag", b"state").unwrap();
        assert_eq!(
            cache.load_body(b"body", "opaque-tag").unwrap().bytes,
            b"state"
        );
        assert!(cache.load_body(b"body", "different-tag").is_none());

        let entry = cache.entry_path("object-bodies", b"body").unwrap();
        let mut envelope: serde_json::Value = read_entry_json(&entry);
        let mut payload: serde_json::Value =
            serde_json::from_str(envelope["payload"].as_str().unwrap()).unwrap();
        payload["length"] = serde_json::Value::from(99);
        envelope["payload"] = serde_json::Value::String(serde_json::to_string(&payload).unwrap());
        envelope["payload_sha256"] =
            serde_json::Value::String(sha256_hex(envelope["payload"].as_str().unwrap().as_bytes()));
        write_entry_json(&entry, &envelope);
        assert!(cache.load_body(b"body", "opaque-tag").is_none());
    }

    #[test]
    fn unsafe_directory_permissions_are_rejected() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("cache");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Cache::open(root).is_err());
    }
}
