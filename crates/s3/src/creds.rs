//! Disk cache for resolved AWS SSO credentials.
//!
//! The Rust SDK resolves SSO role credentials on every invocation (~550ms
//! against the WAN); unlike the AWS CLI it never persists the derived
//! credentials. This provider caches the resolved credentials under the user
//! cache dir — the same trust model as the CLI's `~/.aws/cli/cache` — and
//! falls through to the real chain on a miss or near expiry, so long-running
//! builds still refresh.
//!
//! Persistence is scoped to SSO-configured profiles because SSO is the only
//! source whose identity inputs are all keyable files: the shared config
//! files select the account and role, and the SSO token cache holds the
//! login. Environment credentials, container roles, instance metadata, and
//! `credential_process` can all change identity without touching a keyable
//! input — and they resolve locally and near-instantly, so a disk cache buys
//! them nothing. Cache reads are self-healing: an unreadable, corrupt,
//! oversized, or stale entry is a miss, never an error.
//!
//! The SDK's in-memory identity cache refreshes ahead of the credentials'
//! real expiry by re-invoking this provider; the disk margin below only
//! decides whether a *new* process trusts an entry, so the two buffers
//! compose rather than conflict.

use anyhow::{Context, Result};
use aws_credential_types::provider::{self, future, ProvideCredentials, SharedCredentialsProvider};
use aws_credential_types::Credentials;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Refuse cached credentials this close to expiry: enough for clock skew and
/// an in-flight request; anything longer just re-resolves early.
const EXPIRY_MARGIN: Duration = Duration::from_secs(120);

/// Cached entries are a few hundred bytes; anything larger is not ours.
const MAX_ENTRY_BYTES: u64 = 64 * 1024;

#[derive(Serialize, Deserialize)]
struct StoredCredentials {
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
    account_id: Option<String>,
    expires_unix_secs: u64,
}

#[derive(Debug)]
pub(crate) struct DiskCachedProvider {
    inner: SharedCredentialsProvider,
    #[cfg(test)]
    path_override: Option<PathBuf>,
}

impl DiskCachedProvider {
    pub(crate) fn new(inner: SharedCredentialsProvider) -> Self {
        Self {
            inner,
            #[cfg(test)]
            path_override: None,
        }
    }

    #[cfg(test)]
    fn with_path(inner: SharedCredentialsProvider, path: PathBuf) -> Self {
        Self {
            inner,
            path_override: Some(path),
        }
    }

    fn path(&self) -> Option<PathBuf> {
        #[cfg(test)]
        if let Some(path) = &self.path_override {
            return Some(path.clone());
        }
        cache_path()
    }

    async fn load(&self) -> provider::Result {
        // The key is derived fresh per lookup so a mid-process `aws sso
        // login` moves it and the stale entry stops matching.
        let path = self.path();
        if let Some(path) = &path {
            if let Some(credentials) = read_cached(path, SystemTime::now()) {
                return Ok(credentials);
            }
        }
        let credentials = self.inner.provide_credentials().await?;
        if let (Some(path), Some(_)) = (&path, credentials.expiry()) {
            // Re-derive the key: if any keyed input changed while the chain
            // resolved, these credentials belong to the new key, not the one
            // we read. A failed cache write must not fail the request.
            if self.path() == Some(path.to_path_buf()) {
                let _ = write_cached(path, &credentials);
            }
        }
        Ok(credentials)
    }
}

impl ProvideCredentials for DiskCachedProvider {
    fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
    where
        Self: 'a,
    {
        future::ProvideCredentials::new(self.load())
    }
}

/// Reads a keyed input file. Absent is a legitimate state (hashed as empty);
/// any other read error disables caching rather than mis-keying the entry.
fn read_keyed_file(path: &Path) -> Option<Vec<u8>> {
    match std::fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Some(Vec::new()),
        Err(_) => None,
    }
}

/// True when the active profile section configures SSO, directly or via an
/// `sso_session` reference.
fn profile_uses_sso(config: &str, profile: &str) -> bool {
    let mut in_profile = false;
    for line in config.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            let header = line.trim_start_matches('[').trim_end_matches(']').trim();
            in_profile = header == format!("profile {profile}")
                || (profile == "default" && header == "default");
        } else if in_profile
            && (line.starts_with("sso_session") || line.starts_with("sso_start_url"))
        {
            return true;
        }
    }
    false
}

/// The cache key covers every input SSO resolution reads to pick an
/// identity: the profile selection, both shared config files, and the SSO
/// token cache. Any change — a config edit, a fresh `aws sso login` — moves
/// the key, so a stale entry can never serve another identity. Environments
/// that resolve credentials some other way return `None`: no cache.
fn cache_path() -> Option<PathBuf> {
    for var in [
        "AWS_ACCESS_KEY_ID",
        "AWS_WEB_IDENTITY_TOKEN_FILE",
        "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
        "AWS_CONTAINER_CREDENTIALS_FULL_URI",
    ] {
        if std::env::var_os(var).is_some() {
            return None;
        }
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    let home = Path::new(&home);
    let profile = std::env::var("AWS_PROFILE")
        .or_else(|_| std::env::var("AWS_DEFAULT_PROFILE"))
        .unwrap_or_else(|_| "default".to_owned());
    let config_path = std::env::var("AWS_CONFIG_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.join(".aws/config"));
    let credentials_path = std::env::var("AWS_SHARED_CREDENTIALS_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.join(".aws/credentials"));
    let config = read_keyed_file(&config_path)?;
    if !profile_uses_sso(&String::from_utf8_lossy(&config), &profile) {
        return None;
    }
    let mut hasher = Sha256::new();
    hasher.update(&profile);
    hasher.update([0]);
    hasher.update(&config);
    hasher.update([0]);
    hasher.update(read_keyed_file(&credentials_path)?);
    hasher.update([0]);
    for (name, bytes) in sso_token_cache_state(&home.join(".aws/sso/cache"))? {
        hasher.update(&name);
        hasher.update([0]);
        hasher.update(&bytes);
        hasher.update([0]);
    }
    let key = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let mut path = holys3_core::cache_home().ok()?;
    path.push("holys3");
    path.push("credentials");
    path.push(format!("{key}.json"));
    Some(path)
}

/// The SSO token cache contents, sorted by file name. A missing directory is
/// a legitimate (logged-out) state; a read error disables caching.
fn sso_token_cache_state(dir: &Path) -> Option<Vec<(String, Vec<u8>)>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Some(Vec::new()),
        Err(_) => return None,
    };
    let mut state = Vec::new();
    for entry in entries {
        let entry = entry.ok()?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".json") {
            continue;
        }
        state.push((name, std::fs::read(entry.path()).ok()?));
    }
    state.sort();
    Some(state)
}

fn read_cached(path: &Path, now: SystemTime) -> Option<Credentials> {
    if std::fs::metadata(path).ok()?.len() > MAX_ENTRY_BYTES {
        return None;
    }
    let stored: StoredCredentials = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let expiry =
        SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(stored.expires_unix_secs))?;
    if expiry <= now.checked_add(EXPIRY_MARGIN)? {
        return None;
    }
    let mut builder = Credentials::builder()
        .access_key_id(stored.access_key_id)
        .secret_access_key(stored.secret_access_key)
        .expiry(expiry)
        .provider_name("holys3-disk-cache");
    builder.set_session_token(stored.session_token);
    builder.set_account_id(stored.account_id.map(Into::into));
    Some(builder.build())
}

fn write_cached(path: &Path, credentials: &Credentials) -> Result<()> {
    let expiry = credentials
        .expiry()
        .context("only expiring credentials are cached")?;
    let stored = StoredCredentials {
        access_key_id: credentials.access_key_id().to_owned(),
        secret_access_key: credentials.secret_access_key().to_owned(),
        session_token: credentials.session_token().map(str::to_owned),
        account_id: credentials.account_id().map(|id| id.as_str().to_owned()),
        expires_unix_secs: expiry
            .duration_since(SystemTime::UNIX_EPOCH)
            .context("credential expiry predates the unix epoch")?
            .as_secs(),
    };
    let dir = path.parent().context("cache path has no parent")?;
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    serde_json::to_writer(&mut file, &stored)?;
    file.persist(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn expiring_credentials(expiry: SystemTime) -> Credentials {
        Credentials::new("AKID", "SECRET", Some("TOKEN".into()), Some(expiry), "test")
    }

    #[test]
    fn cached_credentials_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let now = SystemTime::now();
        let expiry = now + Duration::from_secs(3600);
        let credentials = Credentials::builder()
            .access_key_id("AKID")
            .secret_access_key("SECRET")
            .session_token("TOKEN")
            .account_id("381235349110")
            .expiry(expiry)
            .provider_name("test")
            .build();
        write_cached(&path, &credentials).unwrap();

        let cached = read_cached(&path, now).expect("fresh entry must hit");
        assert_eq!(cached.access_key_id(), "AKID");
        assert_eq!(cached.secret_access_key(), "SECRET");
        assert_eq!(cached.session_token(), Some("TOKEN"));
        assert_eq!(
            cached.account_id().map(|id| id.as_str()),
            Some("381235349110"),
            "account id must survive the round trip"
        );
        let cached_expiry = cached.expiry().expect("expiry survives the round trip");
        assert_eq!(
            cached_expiry
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            expiry
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "cache file must be private");
        }
    }

    #[test]
    fn near_expiry_corrupt_and_hostile_entries_miss() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let now = SystemTime::now();

        write_cached(&path, &expiring_credentials(now + Duration::from_secs(60))).unwrap();
        assert!(
            read_cached(&path, now).is_none(),
            "an entry inside the expiry margin must miss"
        );

        std::fs::write(&path, b"not json").unwrap();
        assert!(
            read_cached(&path, now).is_none(),
            "corrupt entries must miss"
        );

        let overflow = serde_json::json!({
            "access_key_id": "AKID",
            "secret_access_key": "SECRET",
            "session_token": null,
            "account_id": null,
            "expires_unix_secs": u64::MAX,
        });
        std::fs::write(&path, serde_json::to_vec(&overflow).unwrap()).unwrap();
        assert!(
            read_cached(&path, now).is_none(),
            "an overflowing expiry must miss, not panic"
        );

        std::fs::write(&path, vec![b' '; (MAX_ENTRY_BYTES + 1) as usize]).unwrap();
        assert!(
            read_cached(&path, now).is_none(),
            "oversized entries must miss unread"
        );
        assert!(read_cached(dir.path().join("absent.json").as_path(), now).is_none());
    }

    #[test]
    fn only_sso_profiles_enable_the_cache() {
        let sso = "[default]\nsso_session = corp\nregion = us-east-2\n\n[sso-session corp]\nsso_start_url = https://corp.awsapps.com/start\n";
        assert!(profile_uses_sso(sso, "default"));
        assert!(!profile_uses_sso(sso, "other"));

        let legacy = "[profile books]\nsso_start_url = https://corp.awsapps.com/start\n";
        assert!(profile_uses_sso(legacy, "books"));

        let iam = "[default]\nregion = us-east-2\n\n[profile books]\ncredential_process = /usr/bin/vault-helper\n";
        assert!(!profile_uses_sso(iam, "default"));
        assert!(!profile_uses_sso(iam, "books"));
        assert!(!profile_uses_sso("", "default"));
    }

    #[derive(Debug)]
    struct CountingChain {
        calls: Arc<AtomicUsize>,
        expiry: SystemTime,
    }

    impl ProvideCredentials for CountingChain {
        fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
        where
            Self: 'a,
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            future::ProvideCredentials::ready(Ok(expiring_credentials(self.expiry)))
        }
    }

    #[test]
    fn provider_serves_repeat_lookups_without_the_chain() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let calls = Arc::new(AtomicUsize::new(0));
        let chain = SharedCredentialsProvider::new(CountingChain {
            calls: calls.clone(),
            expiry: SystemTime::now() + Duration::from_secs(3600),
        });
        let provider = DiskCachedProvider::with_path(chain, path.clone());

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let first = rt.block_on(provider.provide_credentials()).unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "first lookup resolves the chain"
        );
        assert!(path.exists(), "resolved credentials must be cached");

        let second = rt.block_on(provider.provide_credentials()).unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "repeat lookup must not hit the chain"
        );
        assert_eq!(second.access_key_id(), first.access_key_id());
        assert_eq!(second.session_token(), first.session_token());
    }
}
