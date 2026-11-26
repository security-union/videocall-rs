/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

//! Local storage for the CLI's device key.
//!
//! Layout of the config directory (see [`config_dir`]):
//!
//! - `hosts.toml`: non-secret metadata per API origin (device id,
//!   fingerprint, name, approving user, status, and which backend holds the
//!   key).
//! - `credentials.toml`: the Ed25519 private key seed when the file backend
//!   is used. Mode 0600, written via temp file + rename, and refused on read
//!   if group- or world-accessible.
//! - `.lock`: exclusive advisory lock taken around every read-modify-write,
//!   so two CLI processes (say, a login and a logout) never interleave.
//!
//! On macOS and Windows the key goes to the OS keychain (service
//! [`KEYCHAIN_SERVICE`], account = API origin), falling back to the file if
//! the keychain refuses the write. Linux always uses the file.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use secrecy::zeroize::{Zeroize, Zeroizing};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

/// Keychain service name used for every stored secret.
pub const KEYCHAIN_SERVICE: &str = "videocall-cli";

/// Environment variable that overrides the config directory.
pub const CONFIG_DIR_ENV: &str = "VIDEOCALL_CONFIG_DIR";

/// Environment variable that forces the file backend when set to `file`.
pub const CREDENTIAL_STORE_ENV: &str = "VIDEOCALL_CREDENTIAL_STORE";

const HOSTS_FILE: &str = "hosts.toml";
const CREDENTIALS_FILE: &str = "credentials.toml";
const LOCK_FILE: &str = ".lock";

/// Errors from the credential store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// No usable config directory could be determined.
    #[error("cannot determine a config directory; set {CONFIG_DIR_ENV}")]
    NoConfigDir,

    /// The credentials file is readable by other users.
    #[error(
        "refusing to read {path}: it is accessible by other users (mode {mode:o}). \
         Run `chmod 600 {path}` or log in again."
    )]
    InsecurePermissions {
        /// Path of the offending file.
        path: String,
        /// Its permission bits.
        mode: u32,
    },

    /// A file in the config directory could not be parsed.
    #[error("{path} is corrupt: {reason}")]
    Corrupt {
        /// Path of the offending file.
        path: String,
        /// Parser message (never contains secret values).
        reason: String,
    },

    /// A file or directory in the store belongs to another user.
    #[error(
        "refusing to use {path}: it is owned by another user (uid {owner}). \
         Set {CONFIG_DIR_ENV} to a directory you own."
    )]
    NotOwned {
        /// Path of the offending file or directory.
        path: String,
        /// Its owner's uid.
        owner: u32,
    },

    /// The OS keychain failed.
    #[error("keychain error: {0}")]
    Keychain(String),

    /// Filesystem failure.
    #[error("{context}: {source}")]
    Io {
        /// What was being done.
        context: String,
        /// Underlying error.
        source: io::Error,
    },
}

fn io_err(context: impl Into<String>) -> impl FnOnce(io::Error) -> StoreError {
    let context = context.into();
    move |source| StoreError::Io { context, source }
}

/// Where a host's secret is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StoreKind {
    /// macOS Keychain or Windows Credential Manager.
    Keychain,
    /// `credentials.toml` in the config directory (plain text, mode 0600).
    File,
}

/// Non-secret metadata for one API origin, stored in `hosts.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostEntry {
    /// Server-assigned device id (the JWT `kid` and `sub`).
    pub device_id: String,
    /// `XXXX-XXXX-XXXX-XXXX` fingerprint of the public key.
    pub fingerprint: String,
    /// Device name registered with the server.
    pub name: String,
    /// Id of the user who approved this device.
    pub user_id: String,
    /// That user's display name at approval time.
    pub user_name: String,
    /// Backend holding the private key.
    pub store: StoreKind,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct HostsFile {
    #[serde(default)]
    hosts: BTreeMap<String, HostEntry>,
}

/// The secret part of a credential: the Ed25519 private key seed,
/// base64url. `Debug` prints it redacted; it is zeroized on drop.
#[derive(Debug)]
pub struct Secret {
    /// 32-byte seed, base64url without padding.
    pub private_key: SecretString,
}

/// Serialized form of [`Secret`]. No `Debug` impl; zeroized on drop.
#[derive(Serialize, Deserialize)]
struct SecretRecord {
    private_key: String,
}

impl Drop for SecretRecord {
    fn drop(&mut self) {
        self.private_key.zeroize();
    }
}

impl From<&Secret> for SecretRecord {
    fn from(secret: &Secret) -> Self {
        Self {
            private_key: secret.private_key.expose_secret().to_string(),
        }
    }
}

impl From<&SecretRecord> for Secret {
    fn from(record: &SecretRecord) -> Self {
        Self {
            private_key: SecretString::from(record.private_key.clone()),
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct CredentialsFile {
    #[serde(default)]
    hosts: BTreeMap<String, SecretRecord>,
}

/// Resolve the config directory from the environment.
///
/// `VIDEOCALL_CONFIG_DIR` if set, else `$XDG_CONFIG_HOME/videocall`, else
/// `%APPDATA%\videocall` on Windows, else `~/.config/videocall`.
pub fn config_dir() -> Result<PathBuf, StoreError> {
    #[allow(deprecated)] // `home_dir` is correct on every platform we ship.
    let home = std::env::home_dir();
    config_dir_from(|key| std::env::var_os(key), home, cfg!(windows)).ok_or(StoreError::NoConfigDir)
}

/// Pure version of [`config_dir`] for tests.
pub(crate) fn config_dir_from(
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
    home: Option<PathBuf>,
    windows: bool,
) -> Option<PathBuf> {
    let non_empty = |key: &str| env(key).filter(|v| !v.is_empty()).map(PathBuf::from);
    if let Some(dir) = non_empty(CONFIG_DIR_ENV) {
        return Some(dir);
    }
    if let Some(xdg) = non_empty("XDG_CONFIG_HOME") {
        return Some(xdg.join("videocall"));
    }
    if windows {
        if let Some(appdata) = non_empty("APPDATA") {
            return Some(appdata.join("videocall"));
        }
    }
    home.map(|h| h.join(".config").join("videocall"))
}

/// Proof that the caller holds the store's exclusive lock. Released when the
/// last clone is dropped (clones let blocking tasks carry the proof).
#[derive(Clone)]
pub struct StoreLock {
    _file: std::sync::Arc<File>,
}

/// Run store I/O (files or the keychain) on tokio's blocking pool so it
/// never stalls the async runtime threads.
pub async fn run_blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, StoreError> + Send + 'static,
) -> Result<T, StoreError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| StoreError::Io {
            context: "credential store task".to_string(),
            source: io::Error::other(e.to_string()),
        })?
}

/// Credential store rooted at one config directory.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
    use_keychain: bool,
}

impl Store {
    /// Open the store in the default config directory, using the keychain
    /// where the platform has one unless `VIDEOCALL_CREDENTIAL_STORE=file`.
    pub fn open_default() -> Result<Self, StoreError> {
        let force_file = std::env::var(CREDENTIAL_STORE_ENV)
            .map(|v| v.eq_ignore_ascii_case("file"))
            .unwrap_or(false);
        Ok(Self::with_dir(
            config_dir()?,
            keychain::AVAILABLE && !force_file,
        ))
    }

    /// Open a store in `dir`. `use_keychain` is ignored where no keychain
    /// backend is compiled in.
    pub fn with_dir(dir: PathBuf, use_keychain: bool) -> Self {
        Self {
            dir,
            use_keychain: use_keychain && keychain::AVAILABLE,
        }
    }

    /// The config directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Path of the plain-text credentials file.
    pub fn credentials_path(&self) -> PathBuf {
        self.dir.join(CREDENTIALS_FILE)
    }

    fn hosts_path(&self) -> PathBuf {
        self.dir.join(HOSTS_FILE)
    }

    /// Create the config dir at 0700, or check an existing one: it must be
    /// ours, and anything looser than 0700 is tightened.
    fn ensure_dir(&self) -> Result<(), StoreError> {
        if self.dir.is_dir() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::{MetadataExt, PermissionsExt};
                let meta = fs::metadata(&self.dir)
                    .map_err(io_err(format!("inspecting {}", self.dir.display())))?;
                check_owner(meta.uid(), &self.dir)?;
                if meta.permissions().mode() & 0o077 != 0 {
                    fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700))
                        .map_err(io_err(format!("securing {}", self.dir.display())))?;
                }
            }
            return Ok(());
        }
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&self.dir)
            .map_err(io_err(format!("creating {}", self.dir.display())))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700))
                .map_err(io_err(format!("securing {}", self.dir.display())))?;
        }
        Ok(())
    }

    /// Take the exclusive lock, waiting for other processes to release it.
    pub async fn lock(&self) -> Result<StoreLock, StoreError> {
        let store = self.clone();
        run_blocking(move || {
            store.ensure_dir()?;
            let path = store.dir.join(LOCK_FILE);
            let mut options = OpenOptions::new();
            options.read(true).write(true).create(true).truncate(false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let file = options
                .open(&path)
                .map_err(io_err(format!("opening {}", path.display())))?;
            file.lock()
                .map_err(io_err(format!("locking {}", path.display())))?;
            Ok(StoreLock {
                _file: std::sync::Arc::new(file),
            })
        })
        .await
    }

    /// Metadata for `origin`, if logged in.
    pub fn host(&self, origin: &str) -> Result<Option<HostEntry>, StoreError> {
        Ok(self.read_hosts()?.hosts.remove(origin))
    }

    /// Load the secret for `origin` from the backend recorded in `kind`.
    pub fn load_secret(&self, origin: &str, kind: StoreKind) -> Result<Option<Secret>, StoreError> {
        match kind {
            StoreKind::Keychain => {
                let Some(text) = keychain::get(origin)? else {
                    return Ok(None);
                };
                let text = Zeroizing::new(text);
                let record: SecretRecord =
                    toml::from_str(&text).map_err(|e| StoreError::Corrupt {
                        path: format!("keychain item {KEYCHAIN_SERVICE}/{origin}"),
                        reason: e.message().to_string(),
                    })?;
                Ok(Some(Secret::from(&record)))
            }
            StoreKind::File => Ok(self.read_credentials()?.hosts.get(origin).map(Secret::from)),
        }
    }

    /// Persist `secret` and metadata for `origin`, durably, before returning.
    ///
    /// Tries the keychain first when enabled, falling back to the file. The
    /// secret is written before `hosts.toml`, and a copy left in the other
    /// backend is removed. Returns the backend actually used.
    pub fn save(
        &self,
        origin: &str,
        mut entry: HostEntry,
        secret: &Secret,
        _lock: &StoreLock,
    ) -> Result<StoreKind, StoreError> {
        self.ensure_dir()?;
        let record = SecretRecord::from(secret);
        let mut kind = StoreKind::File;
        if self.use_keychain {
            let text = Zeroizing::new(
                toml::to_string(&record).map_err(|e| StoreError::Keychain(e.to_string()))?,
            );
            match keychain::set(origin, &text) {
                Ok(()) => kind = StoreKind::Keychain,
                Err(e) => tracing::warn!("{e}; falling back to the credentials file"),
            }
        }
        match kind {
            StoreKind::Keychain => {
                let mut creds = self.read_credentials_lenient()?;
                if creds.hosts.remove(origin).is_some() {
                    self.write_credentials(&creds)?;
                }
            }
            StoreKind::File => {
                let mut creds = self.read_credentials_lenient()?;
                creds.hosts.insert(origin.to_string(), record);
                self.write_credentials(&creds)?;
                if self.use_keychain {
                    let _ = keychain::delete(origin);
                }
            }
        }
        entry.store = kind;
        let mut hosts = self.read_hosts()?;
        hosts.hosts.insert(origin.to_string(), entry);
        self.write_hosts(&hosts)?;
        Ok(kind)
    }

    /// Delete everything stored for `origin` (both backends and metadata).
    ///
    /// Works even when `credentials.toml` has loose permissions: the file is
    /// rewritten fresh at 0600 without this origin's entry.
    pub fn remove(&self, origin: &str, _lock: &StoreLock) -> Result<(), StoreError> {
        if keychain::AVAILABLE {
            keychain::delete(origin)?;
        }
        let mut creds = match self.read_credentials_checked(false) {
            Err(StoreError::Corrupt { .. }) => CredentialsFile::default(),
            other => other?,
        };
        if creds.hosts.remove(origin).is_some() {
            self.write_credentials(&creds)?;
        }
        let mut hosts = self.read_hosts()?;
        if hosts.hosts.remove(origin).is_some() {
            self.write_hosts(&hosts)?;
        }
        Ok(())
    }

    fn read_hosts(&self) -> Result<HostsFile, StoreError> {
        let path = self.hosts_path();
        match fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).map_err(|e| StoreError::Corrupt {
                path: path.display().to_string(),
                reason: e.message().to_string(),
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(HostsFile::default()),
            Err(e) => Err(io_err(format!("reading {}", path.display()))(e)),
        }
    }

    fn write_hosts(&self, hosts: &HostsFile) -> Result<(), StoreError> {
        let text = toml::to_string(hosts).map_err(|e| StoreError::Corrupt {
            path: self.hosts_path().display().to_string(),
            reason: e.to_string(),
        })?;
        write_atomic(&self.hosts_path(), text.as_bytes())
    }

    fn read_credentials(&self) -> Result<CredentialsFile, StoreError> {
        self.read_credentials_checked(true)
    }

    fn read_credentials_checked(&self, check_mode: bool) -> Result<CredentialsFile, StoreError> {
        let path = self.credentials_path();
        let mut file = match File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(CredentialsFile::default()),
            Err(e) => return Err(io_err(format!("opening {}", path.display()))(e)),
        };
        if check_mode {
            check_private(&file, &path)?;
        }
        let mut text = Zeroizing::new(String::new());
        io::Read::read_to_string(&mut file, &mut text)
            .map_err(io_err(format!("reading {}", path.display())))?;
        // Parse errors from `toml` quote the offending input, which may be a
        // token, so only the bare message is kept.
        toml::from_str(&text).map_err(|e| StoreError::Corrupt {
            path: path.display().to_string(),
            reason: e.message().to_string(),
        })
    }

    /// Read for rewriting: a file with loose permissions is still refused,
    /// but a corrupt file is replaced rather than blocking login forever.
    fn read_credentials_lenient(&self) -> Result<CredentialsFile, StoreError> {
        match self.read_credentials() {
            Err(StoreError::Corrupt { .. }) => Ok(CredentialsFile::default()),
            other => other,
        }
    }

    fn write_credentials(&self, creds: &CredentialsFile) -> Result<(), StoreError> {
        let path = self.credentials_path();
        if creds.hosts.is_empty() {
            return match fs::remove_file(&path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => {
                    Err(io_err(format!("removing {}", path.display()))(e))
                }
                _ => Ok(()),
            };
        }
        let text = Zeroizing::new(toml::to_string(creds).map_err(|e| StoreError::Corrupt {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?);
        write_atomic(&path, text.as_bytes())
    }
}

/// The effective uid of this process.
#[cfg(unix)]
fn current_uid() -> u32 {
    // SAFETY: geteuid takes no arguments, touches no memory we own, and
    // cannot fail (POSIX.1-2008).
    unsafe { libc::geteuid() }
}

/// Refuse a path owned by another user.
#[cfg(unix)]
fn check_owner(owner: u32, path: &Path) -> Result<(), StoreError> {
    if owner != current_uid() {
        return Err(StoreError::NotOwned {
            path: path.display().to_string(),
            owner,
        });
    }
    Ok(())
}

/// Refuse a credentials file that another user owns, or that other users
/// can read or write.
#[cfg(unix)]
fn check_private(file: &File, path: &Path) -> Result<(), StoreError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let meta = file
        .metadata()
        .map_err(io_err(format!("inspecting {}", path.display())))?;
    check_owner(meta.uid(), path)?;
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(StoreError::InsecurePermissions {
            path: path.display().to_string(),
            mode,
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_private(_file: &File, _path: &Path) -> Result<(), StoreError> {
    Ok(())
}

/// Write `contents` to `path` via a 0600 temp file in the same directory,
/// fsync, rename, and fsync the directory, so a crash leaves either the old
/// or the new file, never a torn one.
fn write_atomic(path: &Path, contents: &[u8]) -> Result<(), StoreError> {
    let dir = path.parent().ok_or(StoreError::NoConfigDir)?;
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{file_name}.{}.tmp", std::process::id()));
    let _ = fs::remove_file(&tmp);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)?;
        #[cfg(unix)]
        File::open(dir)?.sync_all()?;
        Ok(())
    })();
    result.map_err(|e| {
        let _ = fs::remove_file(&tmp);
        io_err(format!("writing {}", path.display()))(e)
    })
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod keychain {
    use super::{StoreError, KEYCHAIN_SERVICE};

    pub const AVAILABLE: bool = true;

    fn entry(origin: &str) -> Result<keyring::Entry, StoreError> {
        keyring::Entry::new(KEYCHAIN_SERVICE, origin)
            .map_err(|e| StoreError::Keychain(e.to_string()))
    }

    pub fn get(origin: &str) -> Result<Option<String>, StoreError> {
        match entry(origin)?.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(StoreError::Keychain(e.to_string())),
        }
    }

    pub fn set(origin: &str, value: &str) -> Result<(), StoreError> {
        entry(origin)?
            .set_password(value)
            .map_err(|e| StoreError::Keychain(e.to_string()))
    }

    pub fn delete(origin: &str) -> Result<(), StoreError> {
        match entry(origin)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(StoreError::Keychain(e.to_string())),
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod keychain {
    use super::StoreError;

    pub const AVAILABLE: bool = false;

    pub fn get(_origin: &str) -> Result<Option<String>, StoreError> {
        Ok(None)
    }

    pub fn set(_origin: &str, _value: &str) -> Result<(), StoreError> {
        Err(StoreError::Keychain(
            "no keychain backend on this platform".to_string(),
        ))
    }

    pub fn delete(_origin: &str) -> Result<(), StoreError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn secret(key: &str) -> Secret {
        Secret {
            private_key: SecretString::from(key.to_string()),
        }
    }

    fn entry(device_id: &str) -> HostEntry {
        HostEntry {
            device_id: device_id.to_string(),
            fingerprint: "9F3C-21AB-77D0-4E18".to_string(),
            name: "videocall-cli on test".to_string(),
            user_id: "alice@example.com".to_string(),
            user_name: "Alice".to_string(),
            store: StoreKind::File,
        }
    }

    fn file_store() -> (tempfile::TempDir, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::with_dir(tmp.path().join("videocall"), false);
        (tmp, store)
    }

    #[tokio::test]
    async fn round_trip_file_store() {
        let (_tmp, store) = file_store();
        let lock = store.lock().await.unwrap();
        let kind = store
            .save(
                "https://api.example.com",
                entry("dev-a"),
                &secret("seed-a1"),
                &lock,
            )
            .unwrap();
        assert_eq!(kind, StoreKind::File);
        drop(lock);

        let host = store.host("https://api.example.com").unwrap().unwrap();
        assert_eq!(host.device_id, "dev-a");
        assert_eq!(host.user_name, "Alice");
        assert_eq!(host.store, StoreKind::File);
        let loaded = store
            .load_secret("https://api.example.com", host.store)
            .unwrap()
            .unwrap();
        assert_eq!(loaded.private_key.expose_secret(), "seed-a1");

        // Metadata never carries the secret.
        let hosts = fs::read_to_string(store.dir().join(HOSTS_FILE)).unwrap();
        assert!(!hosts.contains("seed-a1"));

        let lock = store.lock().await.unwrap();
        store.remove("https://api.example.com", &lock).unwrap();
        assert!(store.host("https://api.example.com").unwrap().is_none());
        assert!(!store.credentials_path().exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn file_store_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, store) = file_store();
        let lock = store.lock().await.unwrap();
        store
            .save("https://h", entry("u"), &secret("r"), &lock)
            .unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(store.dir()), 0o700);
        assert_eq!(mode(&store.credentials_path()), 0o600);
        // No temp files left behind.
        let leftovers: Vec<_> = fs::read_dir(store.dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tightens_an_existing_loose_config_dir() {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, store) = file_store();
        fs::create_dir_all(store.dir()).unwrap();
        fs::set_permissions(store.dir(), fs::Permissions::from_mode(0o755)).unwrap();
        let _lock = store.lock().await.unwrap();
        let mode = fs::metadata(store.dir()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn owner_check() {
        let path = Path::new("/x");
        assert!(check_owner(current_uid(), path).is_ok());
        let other = current_uid().wrapping_add(1);
        assert!(matches!(
            check_owner(other, path),
            Err(StoreError::NotOwned { owner, .. }) if owner == other
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn refuses_world_readable_credentials() {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, store) = file_store();
        let lock = store.lock().await.unwrap();
        store
            .save("https://h", entry("u"), &secret("r"), &lock)
            .unwrap();
        fs::set_permissions(store.credentials_path(), fs::Permissions::from_mode(0o644)).unwrap();
        let err = store.load_secret("https://h", StoreKind::File).unwrap_err();
        assert!(matches!(
            err,
            StoreError::InsecurePermissions { mode: 0o644, .. }
        ));
        // Writes refuse too, rather than silently rewriting a leaked file.
        assert!(store
            .save("https://h", entry("u"), &secret("r2"), &lock)
            .is_err());
        // Logout still works and leaves nothing behind.
        store.remove("https://h", &lock).unwrap();
        assert!(!store.credentials_path().exists());
    }

    #[tokio::test]
    async fn profiles_are_keyed_by_origin() {
        let (_tmp, store) = file_store();
        let lock = store.lock().await.unwrap();
        store
            .save("https://a.example", entry("ua"), &secret("ra"), &lock)
            .unwrap();
        store
            .save("http://localhost:8081", entry("ub"), &secret("rb"), &lock)
            .unwrap();
        let a = store
            .load_secret("https://a.example", StoreKind::File)
            .unwrap()
            .unwrap();
        let b = store
            .load_secret("http://localhost:8081", StoreKind::File)
            .unwrap()
            .unwrap();
        assert_eq!(a.private_key.expose_secret(), "ra");
        assert_eq!(b.private_key.expose_secret(), "rb");

        store.remove("https://a.example", &lock).unwrap();
        assert!(store.host("https://a.example").unwrap().is_none());
        assert_eq!(
            store
                .host("http://localhost:8081")
                .unwrap()
                .unwrap()
                .device_id,
            "ub"
        );
    }

    #[test]
    fn secret_debug_is_redacted() {
        let s = format!("{:?}", secret("private-seed-value"));
        assert!(!s.contains("private-seed-value"));
    }

    #[test]
    fn config_dir_resolution() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == k)
                    .map(|(_, v)| OsString::from(v))
            }
        };
        let home = Some(PathBuf::from("/home/u"));
        assert_eq!(
            config_dir_from(
                env(&[(CONFIG_DIR_ENV, "/x"), ("XDG_CONFIG_HOME", "/y")]),
                home.clone(),
                false
            ),
            Some(PathBuf::from("/x"))
        );
        assert_eq!(
            config_dir_from(env(&[("XDG_CONFIG_HOME", "/y")]), home.clone(), false),
            Some(PathBuf::from("/y/videocall"))
        );
        assert_eq!(
            config_dir_from(env(&[("APPDATA", "C:\\AppData")]), home.clone(), true),
            Some(PathBuf::from("C:\\AppData").join("videocall"))
        );
        assert_eq!(
            config_dir_from(env(&[("APPDATA", "C:\\AppData")]), home.clone(), false),
            Some(PathBuf::from("/home/u/.config/videocall"))
        );
        assert_eq!(
            config_dir_from(env(&[(CONFIG_DIR_ENV, "")]), home, false),
            Some(PathBuf::from("/home/u/.config/videocall"))
        );
        assert_eq!(config_dir_from(env(&[]), None, false), None);
    }
}
