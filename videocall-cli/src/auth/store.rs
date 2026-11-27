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

//! Where this machine's device key lives: one record per API origin.
//!
//! macOS and Windows: the OS keychain (service [`KEYCHAIN_SERVICE`],
//! account = API origin), falling back to the file if the keychain refuses
//! the write. Elsewhere: `{config dir}/<origin>.toml`, mode 0600, in a 0700
//! directory owned by this user, written via temp file + rename, and refused
//! on read if another user owns it or can access it.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use secrecy::zeroize::Zeroizing;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Keychain service name for every stored device.
pub const KEYCHAIN_SERVICE: &str = "videocall-cli";

/// Environment variable that overrides the config directory.
pub const CONFIG_DIR_ENV: &str = "VIDEOCALL_CONFIG_DIR";

/// A store failure. Messages never contain the key.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("cannot determine a config directory; set {CONFIG_DIR_ENV}")]
    NoConfigDir,
    #[error("refusing to use {0}: {1}")]
    Insecure(String, String),
    #[error("{0} is corrupt; run `videocall-cli auth logout`, then log in again")]
    Corrupt(String),
    #[error("keychain error: {0}")]
    Keychain(String),
    #[error("{0}: {1}")]
    Io(String, io::Error),
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> StoreError + '_ {
    move |e| StoreError::Io(path.display().to_string(), e)
}

/// One registered device, as stored. `Debug` redacts the key.
// Serialize writes the seed in clear: only `Store::save` may serialize this.
#[derive(Debug, Serialize, Deserialize)]
pub struct StoredDevice {
    /// The API origin. Checked on load, so two origins can never share a key.
    pub origin: String,
    pub device_id: String,
    pub fingerprint: String,
    pub name: String,
    pub user_id: String,
    pub user_name: String,
    /// The 32-byte Ed25519 seed, base64url.
    #[serde(serialize_with = "expose", deserialize_with = "secret")]
    pub private_key: SecretString,
}

fn expose<S: Serializer>(key: &SecretString, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(key.expose_secret())
}

fn secret<'de, D: Deserializer<'de>>(deserializer: D) -> Result<SecretString, D::Error> {
    String::deserialize(deserializer).map(SecretString::from)
}

/// Where a record was found or saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    Keychain,
    File(PathBuf),
}

impl std::fmt::Display for Location {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Location::Keychain => write!(f, "the OS keychain (service \"{KEYCHAIN_SERVICE}\")"),
            Location::File(path) => write!(f, "{}", path.display()),
        }
    }
}

/// `$VIDEOCALL_CONFIG_DIR`, else `$XDG_CONFIG_HOME/videocall`, else
/// `%APPDATA%\videocall` on Windows, else `~/.config/videocall`.
pub(crate) fn config_dir_from(
    env: impl Fn(&str) -> Option<String>,
    home: Option<PathBuf>,
    windows: bool,
) -> Option<PathBuf> {
    let var = |key: &str| env(key).filter(|v| !v.is_empty()).map(PathBuf::from);
    var(CONFIG_DIR_ENV)
        .or_else(|| var("XDG_CONFIG_HOME").map(|d| d.join("videocall")))
        .or_else(|| {
            windows
                .then(|| var("APPDATA"))
                .flatten()
                .map(|d| d.join("videocall"))
        })
        .or_else(|| home.map(|h| h.join(".config").join("videocall")))
}

/// The device store rooted at one config directory.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
    use_keychain: bool,
}

impl Store {
    /// The default config directory, with the keychain where there is one.
    pub fn open_default() -> Result<Self, StoreError> {
        #[allow(deprecated)] // `home_dir` is correct on every platform we ship.
        let home = std::env::home_dir();
        let dir = config_dir_from(crate::constants::env_lookup, home, cfg!(windows))
            .ok_or(StoreError::NoConfigDir)?;
        Ok(Self::with_dir(dir, true))
    }

    /// A store in `dir`; `use_keychain` is ignored where there is none.
    pub fn with_dir(dir: PathBuf, use_keychain: bool) -> Self {
        Self {
            dir,
            use_keychain: use_keychain && keychain::AVAILABLE,
        }
    }

    /// `<origin>.toml` with everything outside `[A-Za-z0-9.-]` as `_`.
    fn path(&self, origin: &str) -> PathBuf {
        let name: String = origin
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '-') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.dir.join(format!("{name}.toml"))
    }

    /// The device for `origin` and where it was found, if any. The file
    /// comes first: it exists only if the keychain refused the last save,
    /// so any keychain item is older.
    pub fn load(&self, origin: &str) -> Result<Option<(StoredDevice, Location)>, StoreError> {
        let path = self.path(origin);
        match File::open(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(&path)(e)),
            Ok(mut file) => {
                check_private(&file, &path)?;
                let mut text = Zeroizing::new(String::new());
                file.read_to_string(&mut text).map_err(io_err(&path))?;
                let source = path.display().to_string();
                if let Some(device) = parse(&text, origin, &source)? {
                    return Ok(Some((device, Location::File(path))));
                }
            }
        }
        if !self.use_keychain {
            return Ok(None);
        }
        match keychain::get(origin) {
            Ok(Some(text)) => {
                let text = Zeroizing::new(text);
                Ok(parse(&text, origin, "keychain item")?.map(|d| (d, Location::Keychain)))
            }
            Ok(None) => Ok(None),
            Err(e) => {
                tracing::warn!("{e}; no key file either");
                Ok(None)
            }
        }
    }

    /// Save `device` for its origin, in the keychain if possible, else in
    /// the file, and remove any copy in the other place.
    pub fn save(&self, device: &StoredDevice) -> Result<Location, StoreError> {
        let origin = device.origin.as_str();
        let text = Zeroizing::new(toml::to_string(device).map_err(|e| {
            StoreError::Io("encoding the device record".into(), io::Error::other(e))
        })?);
        if self.use_keychain {
            match keychain::set(origin, &text) {
                Ok(()) => return self.remove_file(origin).map(|()| Location::Keychain),
                Err(e) => tracing::warn!("{e}; storing the key in a file instead"),
            }
        }
        self.ensure_dir()?;
        let path = self.path(origin);
        write_private(&path, text.as_bytes())?;
        if self.use_keychain {
            if let Err(e) = keychain::delete(origin) {
                tracing::warn!("{e}; an older key may remain in the keychain");
            }
        }
        Ok(Location::File(path))
    }

    /// Delete everything stored for `origin`: the file first, then the
    /// keychain item, so a keychain error never leaves the file behind.
    pub fn remove(&self, origin: &str) -> Result<(), StoreError> {
        let file = self.remove_file(origin);
        if self.use_keychain {
            keychain::delete(origin)?;
        }
        file
    }

    fn remove_file(&self, origin: &str) -> Result<(), StoreError> {
        let path = self.path(origin);
        match fs::remove_file(&path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(io_err(&path)(e)),
            _ => Ok(()),
        }
    }

    /// Create the config dir at 0700, or require an existing one to be ours
    /// and tighten it to 0700.
    fn ensure_dir(&self) -> Result<(), StoreError> {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(&self.dir).map_err(io_err(&self.dir))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let meta = fs::metadata(&self.dir).map_err(io_err(&self.dir))?;
            check_owner(meta.uid(), &self.dir)?;
            if meta.permissions().mode() & 0o077 != 0 {
                fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700))
                    .map_err(io_err(&self.dir))?;
            }
        }
        Ok(())
    }
}

/// Parse a record; one saved for another origin counts as absent.
fn parse(text: &str, origin: &str, source: &str) -> Result<Option<StoredDevice>, StoreError> {
    // `toml` errors quote the input, which holds the key: keep none of it.
    let device: StoredDevice =
        toml::from_str(text).map_err(|_| StoreError::Corrupt(source.into()))?;
    Ok((device.origin == origin).then_some(device))
}

#[cfg(unix)]
fn check_owner(owner: u32, path: &Path) -> Result<(), StoreError> {
    // SAFETY: geteuid takes no arguments and cannot fail.
    if owner != unsafe { libc::geteuid() } {
        let why = format!(
            "owned by another user (uid {owner}); set {CONFIG_DIR_ENV} to a directory you own"
        );
        return Err(StoreError::Insecure(path.display().to_string(), why));
    }
    Ok(())
}

/// Refuse a key file another user owns or can read or write.
fn check_private(file: &File, path: &Path) -> Result<(), StoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let meta = file.metadata().map_err(io_err(path))?;
        check_owner(meta.uid(), path)?;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            let why = format!(
                "other users can access it (mode {mode:o}); run `chmod 600` on it or log in again"
            );
            return Err(StoreError::Insecure(path.display().to_string(), why));
        }
    }
    let _ = (file, path);
    Ok(())
}

/// Write a 0600 temp file next to `path`, fsync, rename over `path`, so a
/// crash leaves the old or the new file, never a torn one.
fn write_private(path: &Path, contents: &[u8]) -> Result<(), StoreError> {
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    let _ = fs::remove_file(&tmp);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let result = options.open(&tmp).and_then(|mut file| {
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    });
    result.map_err(|e| {
        let _ = fs::remove_file(&tmp);
        io_err(path)(e)
    })
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod keychain {
    use super::{StoreError, KEYCHAIN_SERVICE};
    use keyring::{Entry, Error};

    pub const AVAILABLE: bool = true;

    fn entry(origin: &str) -> Result<Entry, StoreError> {
        Entry::new(KEYCHAIN_SERVICE, origin).map_err(|e| StoreError::Keychain(e.to_string()))
    }

    pub fn get(origin: &str) -> Result<Option<String>, StoreError> {
        match entry(origin)?.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(Error::NoEntry) => Ok(None),
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
            Ok(()) | Err(Error::NoEntry) => Ok(()),
            Err(e) => Err(StoreError::Keychain(e.to_string())),
        }
    }
}

/// No keychain: `use_keychain` is always false, so these are never called.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod keychain {
    use super::StoreError;

    pub const AVAILABLE: bool = false;

    pub fn get(_: &str) -> Result<Option<String>, StoreError> {
        Ok(None)
    }

    pub fn set(_: &str, _: &str) -> Result<(), StoreError> {
        Err(StoreError::Keychain("no keychain on this platform".into()))
    }

    pub fn delete(_: &str) -> Result<(), StoreError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const API: &str = "https://api.example.com";

    fn device(origin: &str, key: &str) -> StoredDevice {
        StoredDevice {
            origin: origin.into(),
            device_id: "d".into(),
            fingerprint: "f".into(),
            name: "n".into(),
            user_id: "u".into(),
            user_name: "U".into(),
            private_key: SecretString::from(key.to_string()),
        }
    }

    fn file_store() -> (tempfile::TempDir, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::with_dir(tmp.path().join("videocall"), false);
        (tmp, store)
    }

    #[test]
    fn round_trip_per_origin() {
        let (_tmp, store) = file_store();
        assert!(store.load(API).unwrap().is_none());
        store.save(&device(API, "seed-a")).unwrap();
        store
            .save(&device("http://localhost:8081", "seed-b"))
            .unwrap();
        let (a, location) = store.load(API).unwrap().unwrap();
        assert_eq!(a.private_key.expose_secret(), "seed-a");
        assert_eq!(location, Location::File(store.path(API)));
        assert!(!format!("{a:?}").contains("seed-a"));
        store.remove(API).unwrap();
        assert!(store.load(API).unwrap().is_none());
        let (b, _) = store.load("http://localhost:8081").unwrap().unwrap();
        assert_eq!(b.private_key.expose_secret(), "seed-b");

        // Origins that map to the same file name never share a key.
        assert_eq!(
            store.path("https://a-b.example"),
            store.path("https_//a-b.example")
        );
        store
            .save(&device("https://a-b.example", "seed-c"))
            .unwrap();
        assert!(store.load("https_//a-b.example").unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn permissions_and_ownership() {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, store) = file_store();
        fs::create_dir_all(&store.dir).unwrap();
        fs::set_permissions(&store.dir, fs::Permissions::from_mode(0o755)).unwrap();
        store.save(&device(API, "seed-a")).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&store.dir), 0o700, "a loose config dir is tightened");
        assert_eq!(mode(&store.path(API)), 0o600);

        fs::set_permissions(store.path(API), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(store.load(API), Err(StoreError::Insecure(..))));
        store.remove(API).unwrap();
        assert!(!store.path(API).exists(), "logout still works");

        assert!(check_owner(unsafe { libc::geteuid() }.wrapping_add(1), Path::new("/x")).is_err());
    }

    #[test]
    fn corrupt_record_error_hides_contents() {
        let (_tmp, store) = file_store();
        store.save(&device(API, "seed-a")).unwrap();
        fs::write(store.path(API), "private_key = \"seed-a\"\nbroken").unwrap();
        let err = store.load(API).unwrap_err();
        assert!(
            matches!(err, StoreError::Corrupt(_)) && !err.to_string().contains("seed-a"),
            "{err}"
        );
    }

    #[test]
    fn config_dir_resolution() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        let home = Some(PathBuf::from("/home/u"));
        let at = |pairs, windows| config_dir_from(env(pairs), home.clone(), windows);
        assert_eq!(
            at(&[(CONFIG_DIR_ENV, "/x"), ("XDG_CONFIG_HOME", "/y")], false),
            Some("/x".into())
        );
        assert_eq!(
            at(&[("XDG_CONFIG_HOME", "/y")], false),
            Some("/y/videocall".into())
        );
        assert_eq!(
            at(&[("APPDATA", "/a")], true),
            Some(PathBuf::from("/a").join("videocall"))
        );
        assert_eq!(
            at(&[("APPDATA", "/a"), (CONFIG_DIR_ENV, "")], false),
            Some("/home/u/.config/videocall".into())
        );
        assert_eq!(config_dir_from(env(&[]), None, false), None);
    }
}
