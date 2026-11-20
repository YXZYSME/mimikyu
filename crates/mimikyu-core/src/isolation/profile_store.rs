use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use tempfile::{Builder, NamedTempFile, TempDir};

use crate::driver::{now_ms, Cookie, DriverSession, OriginStorage, StorageState};
use crate::error::{MimikyuError, Result};

pub const LOCK_FILE_NAME: &str = ".mimikyu.lock";
pub const SESSION_COOKIES_FILE: &str = ".mimikyu.session_cookies.json";
pub const SESSION_ORIGINS_FILE: &str = ".mimikyu.session_origins.json";

/// Cross-process advisory `flock` lock protecting a persistent profile directory.
pub struct ProfileLock {
    file: File,
    lock_path: PathBuf,
    profile_dir: PathBuf,
}

impl ProfileLock {
    pub fn try_acquire(profile_dir: &Path) -> Result<Self> {
        fs::create_dir_all(profile_dir)?;
        let lock_path = profile_dir.join(LOCK_FILE_NAME);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;

        match file.try_lock_exclusive() {
            Ok(()) => {
                file.set_len(0)?;
                file.seek(SeekFrom::Start(0))?;
                let meta = format!("pid={}\nacquired_at_ms={}\n", std::process::id(), now_ms());
                file.write_all(meta.as_bytes())?;
                file.flush()?;
                Ok(Self {
                    file,
                    lock_path,
                    profile_dir: profile_dir.to_path_buf(),
                })
            }
            Err(e)
                if e.kind() == ErrorKind::WouldBlock
                    || e.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
            {
                let mut owner = String::new();
                if let Ok(mut rf) = File::open(&lock_path) {
                    let _ = rf.read_to_string(&mut owner);
                }
                let owner_info = owner
                    .lines()
                    .find(|l| l.starts_with("pid="))
                    .unwrap_or("unknown owner")
                    .trim();
                Err(MimikyuError::ProfileLocked(format!(
                    "profile '{}' is locked ({owner_info}); close the owning session or use --ephemeral / --virtual",
                    profile_dir.display()
                )))
            }
            Err(e) => Err(MimikyuError::Io(e)),
        }
    }

    pub fn is_locked(profile_dir: &Path) -> Result<bool> {
        let lock_path = profile_dir.join(LOCK_FILE_NAME);
        if !lock_path.exists() {
            return Ok(false);
        }
        let file = match OpenOptions::new().read(true).write(true).open(&lock_path) {
            Ok(f) => f,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(MimikyuError::Io(e)),
        };
        match file.try_lock_exclusive() {
            Ok(()) => {
                let _ = FileExt::unlock(&file);
                Ok(false)
            }
            Err(e)
                if e.kind() == ErrorKind::WouldBlock
                    || e.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
            {
                Ok(true)
            }
            Err(e) => Err(MimikyuError::Io(e)),
        }
    }

    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    pub fn profile_dir(&self) -> &Path {
        &self.profile_dir
    }
}

impl Drop for ProfileLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// RAII temporary browser profile directory removed cleanly on `cleanup()` or `Drop`.
pub struct EphemeralDir {
    inner: Option<TempDir>,
    path: PathBuf,
}

impl EphemeralDir {
    pub fn new() -> Result<Self> {
        let temp = Builder::new().prefix("mimikyu-profile-").tempdir()?;
        let path = temp.path().to_path_buf();
        Ok(Self {
            inner: Some(temp),
            path,
        })
    }

    pub fn new_in(base: &Path) -> Result<Self> {
        fs::create_dir_all(base)?;
        let temp = Builder::new().prefix("mimikyu-profile-").tempdir_in(base)?;
        let path = temp.path().to_path_buf();
        Ok(Self {
            inner: Some(temp),
            path,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn cleanup(&mut self) -> Result<()> {
        if let Some(dir) = self.inner.take() {
            dir.close()?;
        }
        Ok(())
    }
}

impl Drop for EphemeralDir {
    fn drop(&mut self) {
        drop(self.inner.take());
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedProfile {
    pub name: Option<String>,
    pub path: PathBuf,
    pub is_named: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamedProfileInfo {
    pub name: String,
    pub path: PathBuf,
    pub locked: bool,
}

/// Validates a named profile identifier (prevents path traversal, path separators,
/// absolute paths, or flag-like names).
pub fn validate_profile_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 128 {
        return Err(MimikyuError::InvalidConfig(
            "profile name must be 1..=128 characters".to_string(),
        ));
    }
    if name.contains('\0') {
        return Err(MimikyuError::InvalidConfig(
            "profile name cannot contain NUL bytes".to_string(),
        ));
    }
    if name.contains('/') || name.contains('\\') || Path::new(name).is_absolute() {
        return Err(MimikyuError::InvalidConfig(format!(
            "profile name '{name}' cannot be an absolute path or contain path separators ('/' or '\\')"
        )));
    }
    if name == "." || name == ".." || name.contains("..") || name.starts_with('-') {
        return Err(MimikyuError::InvalidConfig(format!(
            "invalid profile name '{name}': path traversal ('..') and flag-like names are not allowed"
        )));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err(MimikyuError::InvalidConfig(format!(
            "profile name '{name}' contains invalid characters (allowed: ASCII alphanumeric, '.', '_', '-')"
        )));
    }
    Ok(())
}

fn contains_parent_traversal(spec: &str) -> bool {
    spec.split(['/', '\\']).any(|seg| seg == "..")
        || Path::new(spec)
            .components()
            .any(|c| matches!(c, Component::ParentDir))
}

fn canonicalize_base_dir(base: &Path) -> Result<PathBuf> {
    if base.exists() {
        let canonical = fs::canonicalize(base)?;
        if !canonical.is_dir() {
            return Err(MimikyuError::InvalidConfig(format!(
                "profile store root '{}' is not a directory",
                base.display()
            )));
        }
        return Ok(canonical);
    }

    let abs = if base.is_absolute() {
        base.to_path_buf()
    } else {
        std::env::current_dir()?.join(base)
    };

    let mut existing = abs.clone();
    let mut missing_tail = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name() else {
            break;
        };
        missing_tail.push(name.to_os_string());
        if !existing.pop() {
            break;
        }
    }

    let mut resolved = if existing.exists() {
        fs::canonicalize(&existing)?
    } else {
        existing
    };
    for seg in missing_tail.into_iter().rev() {
        resolved.push(seg);
    }
    Ok(resolved)
}

fn reject_symlink_leaf(path: &Path, label: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(MimikyuError::InvalidConfig(format!(
            "{label} '{}' cannot be a symlink",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(MimikyuError::Io(e)),
    }
}

/// Manages named and path-based persistent browser profile directories.
#[derive(Debug, Clone)]
pub struct ProfileStore {
    base_dir: PathBuf,
}

impl Default for ProfileStore {
    fn default() -> Self {
        Self::new(Self::default_base_dir())
    }
}

impl ProfileStore {
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    pub fn default_base_dir() -> PathBuf {
        if let Ok(env_dir) = std::env::var("MIMIKYU_PROFILES_DIR") {
            let trimmed = env_dir.trim();
            if !trimmed.is_empty() {
                return PathBuf::from(trimmed);
            }
        }
        if let Some(local) = dirs::data_local_dir() {
            return local.join("mimikyu").join("profiles");
        }
        std::env::temp_dir().join("mimikyu-profiles")
    }

    fn verify_named_confinement(&self, name: &str) -> Result<PathBuf> {
        validate_profile_name(name)?;
        let canonical_base = canonicalize_base_dir(&self.base_dir)?;
        let candidate = self.base_dir.join(name);

        reject_symlink_leaf(&candidate, &format!("named profile '{name}'"))?;

        match fs::symlink_metadata(&candidate) {
            Ok(_) => {
                let canonical_cand = fs::canonicalize(&candidate)?;
                if !canonical_cand.starts_with(&canonical_base)
                    || canonical_cand == canonical_base
                    || canonical_cand.parent() != Some(canonical_base.as_path())
                {
                    return Err(MimikyuError::InvalidConfig(format!(
                        "profile '{name}' escapes profile store root '{}'",
                        canonical_base.display()
                    )));
                }
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {
                let expected = canonical_base.join(name);
                if !expected.starts_with(&canonical_base)
                    || expected == canonical_base
                    || expected.parent() != Some(canonical_base.as_path())
                {
                    return Err(MimikyuError::InvalidConfig(format!(
                        "profile '{name}' escapes profile store root '{}'",
                        canonical_base.display()
                    )));
                }
            }
            Err(e) => return Err(MimikyuError::Io(e)),
        }

        Ok(candidate)
    }

    pub fn resolve(&self, name_or_path: &str) -> Result<ResolvedProfile> {
        let trimmed = name_or_path.trim();
        if trimmed.is_empty() || trimmed.contains('\0') {
            return Err(MimikyuError::InvalidConfig(
                "profile specifier cannot be empty or contain NUL bytes".to_string(),
            ));
        }
        if trimmed == "." || trimmed == ".." || contains_parent_traversal(trimmed) {
            return Err(MimikyuError::InvalidConfig(format!(
                "invalid profile path '{trimmed}': path traversal ('..') and '.' are not allowed"
            )));
        }

        if trimmed == "~" {
            return Err(MimikyuError::InvalidConfig(
                "refusing bare '~' as profile directory; specify a subdirectory like '~/my-profile'"
                    .to_string(),
            ));
        }

        if trimmed.starts_with("~/") || trimmed.starts_with("~\\") {
            let home = dirs::home_dir().ok_or_else(|| {
                MimikyuError::InvalidConfig(
                    "cannot resolve '~' profile path: home directory unknown".to_string(),
                )
            })?;
            let rest = trimmed
                .strip_prefix("~/")
                .or_else(|| trimmed.strip_prefix("~\\"))
                .unwrap_or("");
            let trimmed_rest = rest.trim_end_matches(['/', '\\']);
            if trimmed_rest.is_empty()
                || trimmed_rest == "."
                || trimmed_rest == ".."
                || contains_parent_traversal(trimmed_rest)
            {
                return Err(MimikyuError::InvalidConfig(format!(
                    "invalid '~' profile path '{trimmed}'"
                )));
            }
            let target = home.join(rest);
            reject_symlink_leaf(&target, "profile path")?;
            return Ok(ResolvedProfile {
                name: None,
                path: target,
                is_named: false,
            });
        }

        if trimmed.contains('/')
            || trimmed.contains('\\')
            || trimmed.starts_with("./")
            || trimmed.starts_with("../")
        {
            let p = PathBuf::from(trimmed);
            let abs = if p.is_absolute() {
                p
            } else {
                std::env::current_dir()?.join(p)
            };
            reject_symlink_leaf(&abs, "profile path")?;
            return Ok(ResolvedProfile {
                name: None,
                path: abs,
                is_named: false,
            });
        }

        self.resolve_named(trimmed)
    }

    pub fn resolve_named(&self, name: &str) -> Result<ResolvedProfile> {
        let path = self.verify_named_confinement(name)?;
        Ok(ResolvedProfile {
            name: Some(name.to_string()),
            path,
            is_named: true,
        })
    }

    /// Resolves a strictly named profile confined inside the canonicalized profile store root.
    /// Rejects path traversal (`..`), absolute paths, `~`, path separators (`/` / `\`), and symlinks.
    pub fn resolve_confined_named(&self, name: &str) -> Result<ResolvedProfile> {
        let trimmed = name.trim();
        self.resolve_named(trimmed)
    }

    pub fn prepare_persistent(&self, name_or_path: &str) -> Result<PathBuf> {
        let resolved = self.resolve(name_or_path)?;
        reject_symlink_leaf(&resolved.path, "profile path")?;
        fs::create_dir_all(&resolved.path)?;
        if resolved.is_named {
            let canonical_base = fs::canonicalize(&self.base_dir)?;
            let canonical_path = fs::canonicalize(&resolved.path)?;
            if !canonical_path.starts_with(&canonical_base)
                || canonical_path == canonical_base
                || canonical_path.parent() != Some(canonical_base.as_path())
            {
                return Err(MimikyuError::InvalidConfig(format!(
                    "prepared profile '{}' escapes profile store root '{}'",
                    resolved.path.display(),
                    canonical_base.display()
                )));
            }
        }
        Ok(resolved.path)
    }

    pub fn list_named(&self) -> Result<Vec<NamedProfileInfo>> {
        if !self.base_dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.base_dir)? {
            let entry = entry?;
            let ftype = entry.file_type()?;
            if ftype.is_symlink() || !ftype.is_dir() {
                continue;
            }
            let path = entry.path();
            let Some(name) = entry.file_name().to_str().map(ToString::to_string) else {
                continue;
            };
            if validate_profile_name(&name).is_err() {
                continue;
            }
            let locked = ProfileLock::is_locked(&path)?;
            out.push(NamedProfileInfo { name, path, locked });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    pub fn delete_named(&self, name: &str) -> Result<()> {
        let resolved = self.resolve_named(name)?;
        if !resolved.path.exists() {
            return Err(MimikyuError::NotFound(format!(
                "named profile '{name}' does not exist"
            )));
        }
        let lock = ProfileLock::try_acquire(&resolved.path)?;
        // Rename atomically while holding the exclusive flock (prefix '-' is rejected by
        // validate_profile_name so list_named skips it) before dropping the lock and removing,
        // preventing any concurrent prepare_persistent + try_acquire from racing on .mimikyu.lock.
        let deleting_path = self
            .base_dir
            .join(format!("-deleting-{name}-{}", uuid::Uuid::new_v4()));
        fs::rename(&resolved.path, &deleting_path)?;
        drop(lock);
        fs::remove_dir_all(&deleting_path)?;
        Ok(())
    }
}

/// Atomically persist `StorageState` as pretty JSON with `0o600` permissions on Unix.
pub fn save_storage_state(path: &Path, state: &StorageState) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;

    let json_bytes = serde_json::to_vec_pretty(state)?;
    let mut tmp = NamedTempFile::new_in(parent)?;
    tmp.write_all(&json_bytes)?;
    tmp.as_file().flush()?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o600);
        fs::set_permissions(tmp.path(), perms)?;
    }

    tmp.persist(path).map_err(|e| MimikyuError::Io(e.error))?;
    Ok(())
}

/// Load and validate a `StorageState` JSON file from disk.
pub fn load_storage_state(path: &Path) -> Result<StorageState> {
    let bytes = fs::read(path)?;
    let state: StorageState = serde_json::from_slice(&bytes)?;
    for c in &state.cookies {
        if c.name.trim().is_empty() || c.name.chars().any(char::is_control) {
            return Err(MimikyuError::InvalidConfig(
                "storage state contains a cookie with empty or invalid name".to_string(),
            ));
        }
        if c.domain.trim().is_empty() || c.domain.chars().any(char::is_control) {
            return Err(MimikyuError::InvalidConfig(format!(
                "cookie '{}' has empty or invalid domain",
                c.name
            )));
        }
        if let Some(exp) = c.expires
            && !exp.is_finite()
        {
            return Err(MimikyuError::InvalidConfig(format!(
                "cookie '{}' has non-finite expiry",
                c.name
            )));
        }
    }
    Ok(state)
}

/// Persist non-persistent session cookies (`expires.is_none()`) atomically with `0o600` permissions.
/// Removes the sidecar file if no session cookies remain.
pub fn save_session_cookies(profile_dir: &Path, cookies: &[Cookie]) -> Result<()> {
    let path = profile_dir.join(SESSION_COOKIES_FILE);
    let session_cookies: Vec<&Cookie> = cookies
        .iter()
        .filter(|c| {
            c.expires.is_none() && !c.name.trim().is_empty() && !c.domain.trim().is_empty()
        })
        .collect();
    if session_cookies.is_empty() {
        match fs::remove_file(&path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(MimikyuError::Io(e)),
        }
    }

    fs::create_dir_all(profile_dir)?;
    let json_bytes = serde_json::to_vec_pretty(&session_cookies)?;
    let mut tmp = NamedTempFile::new_in(profile_dir)?;
    tmp.write_all(&json_bytes)?;
    tmp.as_file().flush()?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o600);
        fs::set_permissions(tmp.path(), perms)?;
    }

    tmp.persist(&path).map_err(|e| MimikyuError::Io(e.error))?;
    Ok(())
}

/// Load and validate persisted session cookies for a persistent profile directory.
pub fn load_session_cookies(profile_dir: &Path) -> Result<Vec<Cookie>> {
    let path = profile_dir.join(SESSION_COOKIES_FILE);
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(MimikyuError::Io(e)),
    };
    let cookies: Vec<Cookie> = serde_json::from_slice(&bytes)?;
    for c in &cookies {
        if c.name.trim().is_empty() || c.domain.trim().is_empty() {
            return Err(MimikyuError::InvalidConfig(
                "session cookies file contains a cookie with empty name or domain".to_string(),
            ));
        }
        if let Some(exp) = c.expires
            && !exp.is_finite()
        {
            return Err(MimikyuError::InvalidConfig(format!(
                "cookie '{}' has non-finite expiry",
                c.name
            )));
        }
    }
    Ok(cookies)
}

/// Persist pending per-origin `localStorage`/`sessionStorage` entries atomically with `0o600` permissions.
/// Removes the sidecar file if no pending origins remain.
pub fn save_session_origins(profile_dir: &Path, origins: &[OriginStorage]) -> Result<()> {
    let path = profile_dir.join(SESSION_ORIGINS_FILE);
    let pending: Vec<&OriginStorage> = origins
        .iter()
        .filter(|o| {
            let orig = o.origin.trim();
            !orig.is_empty()
                && orig != "null"
                && (!o.local_storage.is_empty() || !o.session_storage.is_empty())
        })
        .collect();
    if pending.is_empty() {
        match fs::remove_file(&path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(MimikyuError::Io(e)),
        }
    }

    fs::create_dir_all(profile_dir)?;
    let json_bytes = serde_json::to_vec_pretty(&pending)?;
    let mut tmp = NamedTempFile::new_in(profile_dir)?;
    tmp.write_all(&json_bytes)?;
    tmp.as_file().flush()?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o600);
        fs::set_permissions(tmp.path(), perms)?;
    }

    tmp.persist(&path).map_err(|e| MimikyuError::Io(e.error))?;
    Ok(())
}

/// Load and validate persisted pending per-origin storage for a persistent profile directory.
pub fn load_session_origins(profile_dir: &Path) -> Result<Vec<OriginStorage>> {
    let path = profile_dir.join(SESSION_ORIGINS_FILE);
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(MimikyuError::Io(e)),
    };
    let origins: Vec<OriginStorage> = serde_json::from_slice(&bytes)?;
    for o in &origins {
        if o.origin.trim().is_empty() {
            return Err(MimikyuError::InvalidConfig(
                "session origins file contains an entry with empty origin".to_string(),
            ));
        }
    }
    Ok(origins)
}

/// Merge incoming per-origin storage entries into `existing`, updating matching origins in place.
pub fn merge_origin_storage(existing: &mut Vec<OriginStorage>, incoming: Vec<OriginStorage>) {
    for inc in incoming {
        let orig = inc.origin.trim();
        if orig.is_empty()
            || orig == "null"
            || (inc.local_storage.is_empty() && inc.session_storage.is_empty())
        {
            continue;
        }
        if let Some(found) = existing.iter_mut().find(|o| o.origin == inc.origin) {
            found.local_storage.extend(inc.local_storage);
            found.session_storage.extend(inc.session_storage);
        } else {
            existing.push(inc);
        }
    }
}

pub async fn export_storage_state(session: &dyn DriverSession, path: &Path) -> Result<StorageState> {
    let mut state = session.storage_state().await?;
    state.cookies.retain(|c| !c.name.trim().is_empty());
    save_storage_state(path, &state)?;
    Ok(state)
}

pub async fn import_storage_state(session: &dyn DriverSession, path: &Path) -> Result<StorageState> {
    let state = load_storage_state(path)?;
    session.set_storage_state(state.clone()).await?;
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::{Cookie, OriginStorage};
    use std::collections::BTreeMap;

    #[test]
    fn profile_store_resolves_named_and_paths_and_sanitizes() {
        let base = tempfile::tempdir().unwrap();
        let store = ProfileStore::new(base.path().to_path_buf());

        let named = store.resolve("work-1.profile_a").unwrap();
        assert!(named.is_named);
        assert_eq!(named.name.as_deref(), Some("work-1.profile_a"));
        assert_eq!(named.path, base.path().join("work-1.profile_a"));

        let rel = store.resolve("./my-prof").unwrap();
        assert!(!rel.is_named);
        assert!(rel.path.is_absolute());
        assert!(rel.path.ends_with("my-prof"));

        let abs = store.resolve("/tmp/custom-mimikyu-prof").unwrap();
        assert!(!abs.is_named);
        assert_eq!(abs.path, PathBuf::from("/tmp/custom-mimikyu-prof"));

        let tilde = store.resolve("~/mimikyu-test-prof").unwrap();
        assert!(!tilde.is_named);
        assert!(tilde.path.ends_with("mimikyu-test-prof"));

        // Sanitization rejects invalid named profiles & traversal in resolve_named
        for bad in ["../x", ".", "..", "-bad", "a\0b", "foo/bar", ""] {
            assert!(validate_profile_name(bad).is_err(), "should reject {bad:?}");
        }
        assert!(store.resolve(".").is_err());
        assert!(store.resolve("..").is_err());
        assert!(store.resolve("~").is_err());
        assert!(store.resolve("~/").is_err());
        assert!(store.resolve("").is_err());
        assert!(store.resolve("-bad").is_err());
        assert!(store.resolve("a\0b").is_err());
    }

    #[test]
    fn profile_store_delete_named_renames_and_respects_lock() {
        let base = tempfile::tempdir().unwrap();
        let store = ProfileStore::new(base.path().to_path_buf());
        let p_dir = store.prepare_persistent("work-del").unwrap();
        fs::write(p_dir.join("marker.txt"), b"ok").unwrap();

        let lock = ProfileLock::try_acquire(&p_dir).unwrap();
        let err = store
            .delete_named("work-del")
            .expect_err("delete_named must fail while locked");
        assert!(matches!(err, MimikyuError::ProfileLocked(_)));
        drop(lock);

        store.delete_named("work-del").expect("delete unlocked profile");
        assert!(!p_dir.exists());
        assert!(store.list_named().unwrap().is_empty());
        assert_eq!(fs::read_dir(base.path()).unwrap().count(), 0);
    }

    #[test]
    fn profile_lock_enforces_exclusivity_and_releases_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!ProfileLock::is_locked(dir.path()).unwrap());

        let lock1 = ProfileLock::try_acquire(dir.path()).unwrap();
        assert!(ProfileLock::is_locked(dir.path()).unwrap());

        let err = ProfileLock::try_acquire(dir.path())
            .err()
            .expect("second acquire must fail");
        assert!(matches!(err, MimikyuError::ProfileLocked(_)));

        drop(lock1);
        assert!(!ProfileLock::is_locked(dir.path()).unwrap());
        let lock2 = ProfileLock::try_acquire(dir.path()).unwrap();
        drop(lock2);
    }

    #[test]
    fn storage_state_atomic_save_0600_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("sub").join("state.json");

        let mut ls = BTreeMap::new();
        ls.insert("token".to_string(), "abc".to_string());
        let state = StorageState {
            cookies: vec![Cookie {
                name: "sid".to_string(),
                value: "123".to_string(),
                domain: "127.0.0.1".to_string(),
                path: "/".to_string(),
                expires: None,
                http_only: true,
                secure: false,
                same_site: Some("Lax".to_string()),
            }],
            origins: vec![OriginStorage {
                origin: "http://127.0.0.1:8080".to_string(),
                local_storage: ls,
                session_storage: BTreeMap::new(),
            }],
        };

        save_storage_state(&file_path, &state).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&file_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        let loaded = load_storage_state(&file_path).unwrap();
        assert_eq!(loaded.cookies.len(), 1);
        assert_eq!(loaded.cookies[0].name, "sid");
        assert_eq!(loaded.origins.len(), 1);
    }

    #[test]
    fn session_cookies_atomic_save_filter_validate_and_remove() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_session_cookies(dir.path()).unwrap().is_empty());

        let cookies = vec![
            Cookie {
                name: "sess".to_string(),
                value: "s1".to_string(),
                domain: "127.0.0.1".to_string(),
                path: "/".to_string(),
                expires: None,
                http_only: true,
                secure: false,
                same_site: Some("Lax".to_string()),
            },
            Cookie {
                name: "persist".to_string(),
                value: "p1".to_string(),
                domain: "127.0.0.1".to_string(),
                path: "/".to_string(),
                expires: Some(1_900_000_000.0),
                http_only: false,
                secure: false,
                same_site: None,
            },
        ];

        save_session_cookies(dir.path(), &cookies).unwrap();
        let path = dir.path().join(SESSION_COOKIES_FILE);
        assert!(path.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        let loaded = load_session_cookies(dir.path()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "sess");
        assert_eq!(loaded[0].value, "s1");
        assert!(loaded[0].http_only);
        assert!(loaded[0].expires.is_none());

        // Saving with only persistent cookies or empty removes the file
        save_session_cookies(dir.path(), &cookies[1..]).unwrap();
        assert!(!path.exists());
        assert!(load_session_cookies(dir.path()).unwrap().is_empty());

        // Validation rejects empty cookie name and empty cookie domain
        fs::write(
            &path,
            r#"[{"name":" ","value":"v","domain":"127.0.0.1","path":"/","expires":null,"http_only":false,"secure":false,"same_site":null}]"#,
        )
        .unwrap();
        assert!(load_session_cookies(dir.path()).is_err());

        let state_file = dir.path().join("bad_domain_state.json");
        fs::write(
            &state_file,
            r#"{"cookies":[{"name":"c1","value":"v1","domain":"","path":"/","expires":null,"http_only":false,"secure":false,"same_site":null}],"origins":[]}"#,
        )
        .unwrap();
        assert!(
            load_storage_state(&state_file).is_err(),
            "load_storage_state must reject cookie with empty domain"
        );
    }

    #[test]
    fn session_origins_atomic_save_merge_validate_and_remove() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_session_origins(dir.path()).unwrap().is_empty());

        let mut ls1 = BTreeMap::new();
        ls1.insert("k1".to_string(), "v1".to_string());
        let mut ss1 = BTreeMap::new();
        ss1.insert("s1".to_string(), "sv1".to_string());

        let mut origins = vec![OriginStorage {
            origin: "http://127.0.0.1:8080".to_string(),
            local_storage: ls1,
            session_storage: ss1,
        }];
        save_session_origins(dir.path(), &origins).unwrap();
        let path = dir.path().join(SESSION_ORIGINS_FILE);
        assert!(path.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        let mut ls2 = BTreeMap::new();
        ls2.insert("k2".to_string(), "v2".to_string());
        merge_origin_storage(
            &mut origins,
            vec![
                OriginStorage {
                    origin: "http://127.0.0.1:8080".to_string(),
                    local_storage: ls2,
                    session_storage: BTreeMap::new(),
                },
                OriginStorage {
                    origin: "null".to_string(),
                    local_storage: BTreeMap::from([("bad".into(), "x".into())]),
                    session_storage: BTreeMap::new(),
                },
            ],
        );
        save_session_origins(dir.path(), &origins).unwrap();
        let loaded = load_session_origins(dir.path()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].local_storage.get("k1").map(String::as_str), Some("v1"));
        assert_eq!(loaded[0].local_storage.get("k2").map(String::as_str), Some("v2"));
        assert_eq!(loaded[0].session_storage.get("s1").map(String::as_str), Some("sv1"));

        save_session_origins(dir.path(), &[]).unwrap();
        assert!(!path.exists());
        assert!(load_session_origins(dir.path()).unwrap().is_empty());

        fs::write(
            &path,
            r#"[{"origin":" ","local_storage":{"a":"b"},"session_storage":{}}]"#,
        )
        .unwrap();
        assert!(load_session_origins(dir.path()).is_err());
    }

    #[test]
    fn profile_store_blocks_traversal_separators_and_symlink_escapes() {
        let base = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let store = ProfileStore::new(base.path().to_path_buf());

        for bad_named in [
            "",
            ".",
            "..",
            "../escape",
            "a..b",
            "/etc/passwd",
            "foo/bar",
            "foo\\bar",
            "~/home-prof",
            "~",
            "-flag",
            "bad\0name",
        ] {
            assert!(
                validate_profile_name(bad_named).is_err(),
                "validate_profile_name should reject {bad_named:?}"
            );
            assert!(
                store.resolve_named(bad_named).is_err(),
                "resolve_named should reject {bad_named:?}"
            );
            assert!(
                store.resolve_confined_named(bad_named).is_err(),
                "resolve_confined_named should reject {bad_named:?}"
            );
        }

        // Parent traversal must also be rejected by ProfileStore::resolve for path-based specs
        for bad_path in [
            "../escape",
            "./sub/../escape",
            "~/../escape",
            "/tmp/../etc",
            "sub\\..\\escape",
        ] {
            assert!(
                store.resolve(bad_path).is_err(),
                "resolve should reject traversal path {bad_path:?}"
            );
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            // Plant symlink inside profile store pointing outside
            let escape_link = base.path().join("escape_link");
            symlink(outside.path(), &escape_link).unwrap();
            assert!(
                store.resolve("escape_link").is_err(),
                "resolve must reject symlink inside profile store"
            );
            assert!(
                store.resolve_named("escape_link").is_err(),
                "resolve_named must reject symlink inside profile store"
            );
            assert!(
                store.resolve_confined_named("escape_link").is_err(),
                "resolve_confined_named must reject symlink inside profile store"
            );
            assert!(
                store.prepare_persistent("escape_link").is_err(),
                "prepare_persistent must reject symlink inside profile store"
            );
            assert!(
                store.delete_named("escape_link").is_err(),
                "delete_named must reject symlink inside profile store"
            );
            assert!(
                outside.path().exists(),
                "outside directory must not be touched by delete_named"
            );
            assert!(
                store.list_named().unwrap().is_empty(),
                "list_named must ignore symlinks"
            );

            // Dangling symlink inside profile store pointing outside must also be rejected
            let dangling = base.path().join("dangling_link");
            symlink(outside.path().join("nonexistent-target"), &dangling).unwrap();
            assert!(store.resolve_named("dangling_link").is_err());
            assert!(store.prepare_persistent("dangling_link").is_err());

            // Symlink at path-based profile leaf must also be rejected
            let path_link = outside.path().join("path_link");
            symlink(base.path(), &path_link).unwrap();
            assert!(store.resolve(&path_link.to_string_lossy()).is_err());
        }
    }
}
