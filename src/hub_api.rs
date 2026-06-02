use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};
use xet_client::cas_client::auth::{AuthError, TokenInfo, TokenRefresher};

use crate::error::{Error, Result, is_retryable_status};

// ── HubOps trait ──────────────────────────────────────────────────────

/// Trait abstracting the Hub API operations used by VirtualFs and FlushManager.
/// Production code uses `HubApiClient`; tests inject mocks.
#[async_trait::async_trait]
pub trait HubOps: Send + Sync {
    async fn list_tree(&self, prefix: &str) -> Result<Vec<TreeEntry>>;
    async fn head_file(&self, path: &str) -> Result<Option<HeadFileInfo>>;
    async fn batch_operations(&self, ops: &[BatchOp]) -> Result<()>;
    async fn download_file_http(&self, path: &str, dest: &Path) -> Result<()>;
    fn default_mtime(&self) -> SystemTime;
    fn source(&self) -> &SourceKind;
    fn is_repo(&self) -> bool;

    /// Cheap probe returning an opaque "revision" token that changes whenever
    /// the source content changes. For repos this is the commit head SHA; for
    /// buckets it is `updatedAt`. The poll loop uses this to skip the full
    /// tree-listing fan-out when nothing has changed.
    ///
    /// Errors fall through to a full fan-out — the probe is an optimization,
    /// not a gate. The default impl errors out so mocks that don't care about
    /// the probe path keep doing full polls.
    async fn probe_revision(&self) -> Result<String> {
        Err(Error::hub("probe_revision not implemented"))
    }
}

// ── Repo / Bucket types ───────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum RepoType {
    Model,
    Dataset,
    Space,
}

impl RepoType {
    /// Path segment used in `/api/{type}/{id}/…` API routes.
    pub fn api_prefix(&self) -> &'static str {
        match self {
            Self::Model => "models",
            Self::Dataset => "datasets",
            Self::Space => "spaces",
        }
    }

    /// Path segment used in `/{type}/{id}/resolve/…` user-facing routes.
    pub fn resolve_prefix(&self) -> &'static str {
        // Models don't have a type prefix in resolve URLs (e.g. /user/repo/resolve/main/file)
        // Datasets and spaces do (e.g. /datasets/user/repo/resolve/main/file)
        match self {
            Self::Model => "",
            Self::Dataset => "datasets/",
            Self::Space => "spaces/",
        }
    }
}

impl std::str::FromStr for RepoType {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "model" => Ok(Self::Model),
            "dataset" => Ok(Self::Dataset),
            "space" => Ok(Self::Space),
            _ => Err(format!("unknown repo type: {s} (expected model, dataset, or space)")),
        }
    }
}

impl std::fmt::Display for RepoType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Model => f.write_str("model"),
            Self::Dataset => f.write_str("dataset"),
            Self::Space => f.write_str("space"),
        }
    }
}

/// Identifies whether we're talking to a bucket or a repo.
/// Also serves as the clap subcommand for the CLI.
#[derive(Debug, Clone)]
pub enum SourceKind {
    Bucket {
        bucket_id: String,
    },
    Repo {
        repo_id: String,
        repo_type: RepoType,
        revision: String,
    },
}

impl std::fmt::Display for SourceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bucket { bucket_id } => write!(f, "bucket/{bucket_id}"),
            Self::Repo {
                repo_id,
                repo_type,
                revision,
            } => write!(f, "{repo_type}/{repo_id}/{revision}"),
        }
    }
}

// ── Shared data types ─────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum BatchOp {
    #[serde(rename_all = "camelCase")]
    AddFile {
        path: String,
        xet_hash: String,
        mtime: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        content_type: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    DeleteFile { path: String },
}

/// Unified tree entry exposed to the rest of the codebase.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TreeEntry {
    pub path: String,
    #[serde(rename = "type")]
    pub entry_type: String,
    pub size: Option<u64>,
    pub xet_hash: Option<String>,
    /// Git blob OID (same value as ETag on resolve endpoint).
    #[serde(default)]
    pub oid: Option<String>,
    pub mtime: Option<String>,
}

/// Raw tree entry from the repo `/tree` API (different shape from bucket tree).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepoTreeEntry {
    path: String,
    #[serde(rename = "type")]
    entry_type: String,
    size: Option<u64>,
    #[serde(default)]
    oid: Option<String>,
    #[serde(default)]
    xet_hash: Option<String>,
    #[serde(default)]
    lfs: Option<LfsInfo>,
    #[serde(default)]
    last_commit: Option<CommitInfo>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommitInfo {
    date: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LfsInfo {
    size: u64,
    #[allow(dead_code)]
    pointer_size: Option<u64>,
}

/// Metadata returned by HEAD on the resolve endpoint
#[derive(Debug)]
pub struct HeadFileInfo {
    pub xet_hash: Option<String>,
    pub etag: Option<String>,
    pub size: Option<u64>,
    pub last_modified: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CasTokenInfo {
    pub cas_url: String,
    pub exp: u64,
    pub access_token: String,
}

// ── HubApiClient ──────────────────────────────────────────────────────

/// How often the token file is re-read from disk.
const TOKEN_FILE_REFRESH: std::time::Duration = std::time::Duration::from_secs(30);

pub struct HubApiClient {
    client: Client,
    /// Client that does NOT follow redirects — used for HEAD requests where we
    /// need response headers from the Hub (not from the CAS redirect target).
    head_client: Client,
    endpoint: String,
    token: Option<String>,
    /// Path to a file containing the API token. Re-read periodically so
    /// the CSI driver can refresh credentials without remounting.
    /// Takes precedence over `token` when the file exists and is non-empty.
    token_file: Option<PathBuf>,
    /// Cached token from `token_file`, refreshed every 30s.
    token_file_cache: std::sync::Mutex<Option<(std::time::Instant, String)>>,
    source: SourceKind,
    /// Last modification time (from repo/bucket info endpoint).
    /// Used as default mtime when per-file mtime is unavailable.
    last_modified: SystemTime,
    /// Optional subfolder prefix. When non-empty, all API calls transparently
    /// prepend this to outgoing paths and strip it from incoming TreeEntry paths.
    path_prefix: String,
    /// When set, every outgoing path is encrypted and every incoming listing path
    /// is decrypted at this boundary, so plaintext names never reach the remote.
    #[cfg(feature = "encrypt")]
    path_cipher: Option<std::sync::Arc<crate::encryption::path::PathCipher>>,
}

/// A bucket-absolute path on its way to the remote.
///
/// The only way to build one is [`HubApiClient::prefixed_path`], which encrypts
/// it whenever a path cipher is configured. Request builders accept *only* this
/// type, so a plaintext name cannot reach a URL or request body by accident.
struct RemotePath {
    path: String,
    /// True when `path` is ciphertext and must be percent-encoded for URLs.
    #[cfg(feature = "encrypt")]
    encrypted: bool,
}

impl RemotePath {
    /// The path as it travels in a JSON request body: raw, not URL-escaped.
    fn as_body(&self) -> &str {
        &self.path
    }

    /// The path interpolated into a request URL. Encrypted paths are
    /// percent-encoded per segment; plaintext paths are left untouched to
    /// preserve existing behavior.
    fn as_url(&self) -> std::borrow::Cow<'_, str> {
        #[cfg(feature = "encrypt")]
        if self.encrypted {
            return std::borrow::Cow::Owned(crate::encryption::url::encode_path_segments(&self.path));
        }
        std::borrow::Cow::Borrowed(&self.path)
    }

    /// Returns `true` when the stored path is empty.
    ///
    /// For encrypted clients this is always `false` because `prefixed_path`
    /// prepends `.enc/`, so even the bucket root yields a non-empty result.
    /// This is intentional: `list_tree_bucket` and `list_tree_repo` branch on
    /// this flag to choose between the bare `/tree` URL (empty) and the
    /// path-ful `/tree/{path}` variant — encrypted clients must always take
    /// the latter.
    fn is_empty(&self) -> bool {
        self.path.is_empty()
    }
}

/// Parse a repo ID, extracting the type from an optional prefix.
/// "datasets/user/ds" → (Dataset, "user/ds")
/// "spaces/user/app" → (Space, "user/app")
/// "user/model" → (Model, "user/model")
pub fn parse_repo_id(repo_id: &str) -> (RepoType, String) {
    if let Some(rest) = repo_id.strip_prefix("datasets/") {
        (RepoType::Dataset, rest.to_string())
    } else if let Some(rest) = repo_id.strip_prefix("spaces/") {
        (RepoType::Space, rest.to_string())
    } else {
        (RepoType::Model, repo_id.to_string())
    }
}

/// Split a raw identifier into `(id, path_prefix)` after the first 2 `/`-separated segments.
/// E.g. `split_path_prefix("user/bucket/a/b")` → `("user/bucket", "a/b")`.
/// Trailing slashes are trimmed, and `.`/`..` components in the prefix are rejected.
pub fn split_path_prefix(raw: &str) -> std::result::Result<(&str, &str), &'static str> {
    let raw = raw.trim_end_matches('/');
    let mut end = 0;
    let mut count = 0;
    for (i, ch) in raw.char_indices() {
        if ch == '/' {
            count += 1;
            if count == 2 {
                end = i;
                break;
            }
        }
    }
    if count < 2 {
        // Not enough segments — entire string is the ID, no prefix.
        Ok((raw, ""))
    } else {
        let prefix = &raw[end + 1..];
        if prefix.split('/').any(|s| s == "." || s == "..") {
            return Err("path prefix must not contain '.' or '..' components");
        }
        Ok((&raw[..end], prefix))
    }
}

fn retry_delay(attempt: u32) -> std::time::Duration {
    debug_assert!(attempt > 0, "retry_delay called with attempt=0");
    std::time::Duration::from_millis(500 * 2u64.pow(attempt - 1))
}

/// Parse the IETF `RateLimit` header for `t=<seconds>` (time until window reset), capped at 30s.
/// Format: `"resource_type";r=<remaining>;t=<seconds_until_reset>`
/// This is what moon-landing sends on 429 responses.
fn parse_retry_delay(headers: &reqwest::header::HeaderMap) -> Option<std::time::Duration> {
    let value = headers.get("ratelimit")?.to_str().ok()?;
    for part in value.split(';') {
        let part = part.trim();
        if let Some(secs_str) = part.strip_prefix("t=")
            && let Ok(secs) = secs_str.parse::<u64>()
        {
            return Some(std::time::Duration::from_secs(secs.min(30)));
        }
    }
    None
}

/// Build an authenticated GET request during client initialization (before HubApiClient exists).
fn init_auth_get(
    client: &Client,
    url: &str,
    token: Option<&str>,
    token_file: &Option<PathBuf>,
) -> reqwest::RequestBuilder {
    let file_token = if token.is_none() {
        token_file
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    } else {
        None
    };
    let effective_token = token.or(file_token.as_deref());
    match effective_token {
        Some(t) => client.get(url).bearer_auth(t),
        None => client.get(url),
    }
}

/// Best-effort probe: does `id` resolve as a repo (model, dataset, or space)?
/// Returns the matching repo type if a 200 response is seen. Any error or
/// non-200 status is treated as "not this kind" — used only to enrich error
/// messages, never on a hot path.
async fn probe_repo(
    client: &Client,
    endpoint: &str,
    id: &str,
    token: Option<&str>,
    token_file: &Option<PathBuf>,
) -> Option<RepoType> {
    for repo_type in [RepoType::Model, RepoType::Dataset, RepoType::Space] {
        let url = format!("{endpoint}/api/{}/{id}", repo_type.api_prefix());
        let Ok(resp) = init_auth_get(client, &url, token, token_file).send().await else {
            continue;
        };
        if resp.status().is_success() {
            return Some(repo_type);
        }
    }
    None
}

/// Send an HTTP request with automatic retry on transient errors (408, 429, 5xx, timeouts).
/// Uses the IETF RateLimit header's t= parameter when present, falls back to exponential backoff (2 retries max).
/// Set `accept_redirects` to treat 3xx as success (needed for HEAD on /resolve/ endpoints
/// where the redirect response itself carries metadata headers).
async fn send_with_retry(
    build_request: impl Fn() -> reqwest::RequestBuilder,
    context: &str,
    accept_redirects: bool,
) -> Result<reqwest::Response> {
    const MAX_RETRIES: u32 = 2;
    let mut attempt = 0;
    loop {
        attempt += 1;
        match build_request().send().await {
            Ok(resp) if resp.status().is_success() || (accept_redirects && resp.status().is_redirection()) => {
                return Ok(resp);
            }
            Ok(resp) => {
                let status = resp.status().as_u16();
                if is_retryable_status(status) && attempt <= MAX_RETRIES {
                    let delay = parse_retry_delay(resp.headers()).unwrap_or_else(|| retry_delay(attempt));
                    warn!("{context}: transient error ({status}), retry {attempt}/{MAX_RETRIES} in {delay:?}");
                    tokio::time::sleep(delay).await;
                    continue;
                }
                let body = resp.text().await.unwrap_or_default();
                return Err(Error::hub_status(status, format!("{context}: {status} {body}")));
            }
            Err(err) if (err.is_timeout() || err.is_connect()) && attempt <= MAX_RETRIES => {
                let delay = retry_delay(attempt);
                warn!("{context}: transient error, retry {attempt}/{MAX_RETRIES} in {delay:?}: {err}");
                tokio::time::sleep(delay).await;
            }
            Err(err) => return Err(Error::Http(err)),
        }
    }
}

fn make_clients(backend: &str) -> (Client, Client) {
    let user_agent = format!("hf-mount/{}; fs/{}", env!("CARGO_PKG_VERSION"), backend);
    // Idle pool / keep-alive shared across both clients so a hung Hub doesn't
    // freeze the poll loop and TLS handshakes are amortized across rounds.
    let base = || {
        reqwest::Client::builder()
            .user_agent(&user_agent)
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Some(Duration::from_secs(60)))
            .connect_timeout(Duration::from_secs(10))
    };
    let client = base()
        .timeout(Duration::from_secs(60))
        .build()
        .expect("failed to build client");
    let head_client = base()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .expect("failed to build head_client");
    (client, head_client)
}

impl HubApiClient {
    /// Create a client from a `SourceKind` (bucket or repo).
    /// Create a client from a source kind. For repos, resolves aliases
    /// (e.g. "gpt2" → "openai-community/gpt2") and fetches repo metadata.
    pub async fn from_source(
        endpoint: &str,
        token: Option<&str>,
        token_file: Option<PathBuf>,
        source: SourceKind,
        path_prefix: String,
        backend: &str,
    ) -> Result<Arc<Self>> {
        let (client, head_client) = make_clients(backend);
        let endpoint = endpoint.trim_end_matches('/').to_string();

        let (source, last_modified) = match source {
            SourceKind::Repo {
                repo_id,
                repo_type,
                revision,
            } => {
                let url = format!("{}/api/{}/{}", endpoint, repo_type.api_prefix(), repo_id);
                let context = format!("resolve repo {repo_id}");
                let resp =
                    send_with_retry(|| init_auth_get(&client, &url, token, &token_file), &context, false).await?;
                let body: serde_json::Value = resp.json().await?;
                let resolved_id = body["id"]
                    .as_str()
                    .ok_or_else(|| Error::hub("repo info missing 'id' field"))?;
                if resolved_id != repo_id {
                    info!("Resolved repo alias: {} → {}", repo_id, resolved_id);
                }
                let last_modified = body["lastModified"].as_str().map(mtime_from_str).unwrap_or(UNIX_EPOCH);
                (
                    SourceKind::Repo {
                        repo_id: resolved_id.to_string(),
                        repo_type,
                        revision,
                    },
                    last_modified,
                )
            }
            SourceKind::Bucket { bucket_id } => {
                let url = format!("{}/api/buckets/{}", endpoint, bucket_id);
                let context = format!("resolve bucket {bucket_id}");
                let resp = match send_with_retry(|| init_auth_get(&client, &url, token, &token_file), &context, false)
                    .await
                {
                    Ok(r) => r,
                    Err(err) => {
                        // Common mistake: user passed a repo id to `bucket`. Probe the
                        // repo APIs and, if one matches, surface a hint instead of the
                        // raw 401 from the bucket endpoint.
                        if let Some(repo_type) = probe_repo(&client, &endpoint, &bucket_id, token, &token_file).await {
                            return Err(Error::hub(format!(
                                "{bucket_id} is not a bucket, but it exists as a {repo_type}. \
                                 Use `repo {bucket_id}` (read-only) instead of `bucket {bucket_id}`."
                            )));
                        }
                        return Err(err);
                    }
                };
                let body: serde_json::Value = resp.json().await?;
                let last_modified = body["updatedAt"].as_str().map(mtime_from_str).unwrap_or(UNIX_EPOCH);
                (SourceKind::Bucket { bucket_id }, last_modified)
            }
        };

        Ok(Arc::new(Self {
            client,
            head_client,
            endpoint,
            token: token.map(|t| t.to_string()),
            token_file,
            token_file_cache: std::sync::Mutex::new(None),
            source,
            last_modified,
            path_prefix,
            #[cfg(feature = "encrypt")]
            path_cipher: None,
        }))
    }

    /// Create a client for a HuggingFace bucket.
    pub fn new(endpoint: &str, token: Option<&str>, bucket_id: &str, backend: &str) -> Arc<Self> {
        let (client, head_client) = make_clients(backend);
        Arc::new(Self {
            client,
            head_client,
            endpoint: endpoint.trim_end_matches('/').to_string(),
            token: token.map(|t| t.to_string()),
            token_file: None,
            token_file_cache: std::sync::Mutex::new(None),
            source: SourceKind::Bucket {
                bucket_id: bucket_id.to_string(),
            },
            last_modified: UNIX_EPOCH,
            path_prefix: String::new(),
            #[cfg(feature = "encrypt")]
            path_cipher: None,
        })
    }

    /// Attach bearer auth to a request if a token is configured.
    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(path) = &self.token_file {
            // Check TTL under lock, but do file I/O outside it to avoid
            // blocking concurrent requests on slow storage.
            let need_refresh = {
                let cache = self.token_file_cache.lock().expect("token_file_cache poisoned");
                match &*cache {
                    Some((at, _)) => at.elapsed() >= TOKEN_FILE_REFRESH,
                    None => true,
                }
            };
            if need_refresh && let Ok(contents) = std::fs::read_to_string(path) {
                let token = contents.trim().to_string();
                if !token.is_empty() {
                    let mut cache = self.token_file_cache.lock().expect("token_file_cache poisoned");
                    *cache = Some((std::time::Instant::now(), token));
                }
            }
            let cache = self.token_file_cache.lock().expect("token_file_cache poisoned");
            if let Some((_, ref token)) = *cache {
                return req.bearer_auth(token);
            }
        }
        match &self.token {
            Some(t) => req.bearer_auth(t),
            None => req,
        }
    }

    pub fn source(&self) -> &SourceKind {
        &self.source
    }

    /// Default mtime when per-file mtime is unavailable.
    pub fn default_mtime(&self) -> SystemTime {
        self.last_modified
    }

    pub fn is_repo(&self) -> bool {
        matches!(self.source, SourceKind::Repo { .. })
    }

    /// Return the path prefix (subfolder) this client is scoped to.
    pub fn path_prefix(&self) -> &str {
        &self.path_prefix
    }

    /// Join `path_prefix` and `path` into the bucket-absolute path, encrypting it
    /// when a path cipher is configured. This is the single chokepoint through
    /// which every outgoing path passes before it reaches a request.
    fn prefixed_path(&self, path: &str) -> Result<RemotePath> {
        let joined = if self.path_prefix.is_empty() {
            path.to_string()
        } else if path.is_empty() {
            self.path_prefix.clone()
        } else {
            format!("{}/{}", self.path_prefix, path)
        };

        #[cfg(feature = "encrypt")]
        if let Some(cipher) = &self.path_cipher {
            use crate::encryption::ENC_ROOT;

            let encrypted = cipher
                .encrypt_path(&joined)
                .map_err(|e| Error::hub(format!("encrypt path: {e}")))?;
            // Prepend the .enc/ wrapper directory so all ciphertext lives in one
            // bucket-root subdirectory. The empty path (bucket root) maps to
            // ".enc" itself, making is_empty() always false for encrypted clients —
            // which is correct: the root listing must hit /tree/.enc, not /tree.
            let path = if encrypted.is_empty() {
                ENC_ROOT.to_string()
            } else {
                format!("{ENC_ROOT}/{encrypted}")
            };
            return Ok(RemotePath { path, encrypted: true });
        }

        Ok(RemotePath {
            path: joined,
            #[cfg(feature = "encrypt")]
            encrypted: false,
        })
    }

    /// Decrypt the names in a listing back to plaintext bucket-absolute paths,
    /// dropping any entry that doesn't decrypt. No-op when no cipher is set.
    #[cfg(feature = "encrypt")]
    fn decrypt_listing(&self, entries: &mut Vec<TreeEntry>) {
        use crate::encryption::ENC_ROOT;

        let Some(cipher) = &self.path_cipher else {
            return;
        };
        // All stored paths now live under .enc/. Strip that prefix before
        // decrypting; drop anything that doesn't match (defense in depth),
        // including a bare `.enc` entry and lookalikes such as `.encrypted`.
        entries.retain_mut(|e| {
            let Some(stored) = e.path.strip_prefix(ENC_ROOT).and_then(|p| p.strip_prefix('/')) else {
                return false;
            };
            match cipher.decrypt_path(stored) {
                Some(plain) => {
                    e.path = plain;
                    true
                }
                None => false,
            }
        });
    }

    /// Attach a path cipher, turning this into an encrypting client. Called once
    /// at setup on the freshly-built (uniquely-owned) client.
    #[cfg(feature = "encrypt")]
    pub fn with_path_cipher(
        self: std::sync::Arc<Self>,
        cipher: std::sync::Arc<crate::encryption::path::PathCipher>,
    ) -> std::sync::Arc<Self> {
        let mut this =
            std::sync::Arc::try_unwrap(self).unwrap_or_else(|_| panic!("with_path_cipher: client already shared"));
        this.path_cipher = Some(cipher);
        std::sync::Arc::new(this)
    }

    /// Strip `path_prefix` from the beginning of `full`. Returns `None` if the
    /// path doesn't start with the prefix (shouldn't happen for correctly scoped results).
    fn strip_path_prefix<'a>(&self, full: &'a str) -> Option<&'a str> {
        if self.path_prefix.is_empty() {
            return Some(full);
        }
        if full == self.path_prefix {
            // The prefix directory itself — caller should filter this out.
            return Some("");
        }
        full.strip_prefix(&self.path_prefix)
            .and_then(|rest| rest.strip_prefix('/'))
    }

    /// Validate that the path prefix exists on the remote.
    /// Calls list_tree("") which internally prepends the prefix.
    pub async fn validate_path_prefix(&self) -> Result<()> {
        if self.path_prefix.is_empty() {
            return Ok(());
        }
        let entries = self.list_tree("").await.map_err(|e| {
            Error::hub(format!(
                "subfolder '{}' not found in {}: {e}",
                self.path_prefix, self.source
            ))
        })?;
        if entries.is_empty() {
            return Err(Error::hub(format!(
                "subfolder '{}' is empty or does not exist in {}",
                self.path_prefix, self.source,
            )));
        }
        Ok(())
    }

    /// Cheap probe: fetch `/api/{type}/{id}` or `/api/buckets/{id}` and return
    /// an opaque revision token. The poll loop calls this once per round and
    /// skips the full tree fan-out when the token matches the previous one.
    ///
    /// Repos: prefer `sha` (commit head) — changes on every push and only on
    /// pushes. Buckets: `updatedAt` — set by Hub on every bucket mutation.
    /// Errors if the Hub response omits the expected field (should not happen
    /// against the production Hub).
    pub async fn probe_revision(&self) -> Result<String> {
        let url = match &self.source {
            SourceKind::Repo { repo_id, repo_type, .. } => {
                format!("{}/api/{}/{}", self.endpoint, repo_type.api_prefix(), repo_id)
            }
            SourceKind::Bucket { bucket_id } => {
                format!("{}/api/buckets/{}", self.endpoint, bucket_id)
            }
        };
        let resp = send_with_retry(|| self.auth(self.client.get(&url)), "revision probe", false).await?;
        let probe: RevisionProbe = resp.json().await?;
        match &self.source {
            SourceKind::Repo { .. } => probe
                .sha
                .or(probe.last_modified)
                .ok_or_else(|| Error::hub("revision probe: repo response missing sha and lastModified")),
            SourceKind::Bucket { .. } => probe
                .updated_at
                .ok_or_else(|| Error::hub("revision probe: bucket response missing updatedAt")),
        }
    }

    /// List tree entries at the given prefix (single directory level).
    /// Follows `Link` header pagination. For repos, includes `expand=true`
    /// to get per-file lastCommit (mtime). For buckets, passes `recursive=false`.
    pub async fn list_tree(&self, prefix: &str) -> Result<Vec<TreeEntry>> {
        let api_prefix = self.prefixed_path(prefix)?;

        // Capture the dispatch result so we can intercept 404 for encrypted root
        // listings (the .enc/ directory doesn't exist until the first write).
        let result: Result<Vec<TreeEntry>> = match &self.source {
            SourceKind::Bucket { bucket_id } => self.list_tree_bucket(bucket_id, &api_prefix).await,
            SourceKind::Repo {
                repo_id,
                repo_type,
                revision,
            } => self.list_tree_repo(repo_id, *repo_type, revision, &api_prefix).await,
        };

        // On a fresh bucket/repo with encryption, .enc/ doesn't exist yet so the
        // API returns 404. Map that to an empty listing — but only for the true
        // bucket root (prefix == "") on a top-level mount (no path_prefix). A
        // subfolder mount calling list_tree("") must still get its 404 propagated
        // so validate_path_prefix can detect a missing subfolder.
        #[cfg(feature = "encrypt")]
        let result = match result {
            Err(Error::Hub { status: Some(404), .. })
                if self.path_cipher.is_some() && prefix.is_empty() && self.path_prefix.is_empty() =>
            {
                Ok(Vec::new())
            }
            other => other,
        };

        let mut entries = result?;

        // Decrypt encrypted names back to plaintext bucket-absolute paths, dropping
        // any entry that doesn't decrypt (this is what lets encrypted and plaintext
        // objects coexist in one bucket).
        #[cfg(feature = "encrypt")]
        self.decrypt_listing(&mut entries);

        // Strip path prefix from returned entries and filter out the prefix dir itself.
        if !self.path_prefix.is_empty() {
            entries.retain_mut(|e| {
                match self.strip_path_prefix(&e.path) {
                    Some(stripped) if !stripped.is_empty() => {
                        e.path = stripped.to_string();
                        true
                    }
                    _ => false, // filter out prefix dir itself or unrelated entries
                }
            });
        }

        Ok(entries)
    }

    async fn list_tree_bucket(&self, bucket_id: &str, prefix: &RemotePath) -> Result<Vec<TreeEntry>> {
        let mut all_entries = Vec::new();
        let recursive_param = "?recursive=false&limit=5000";
        let mut url = if prefix.is_empty() {
            format!("{}/api/buckets/{}/tree{recursive_param}", self.endpoint, bucket_id)
        } else {
            format!(
                "{}/api/buckets/{}/tree/{}{recursive_param}",
                self.endpoint,
                bucket_id,
                prefix.as_url()
            )
        };

        loop {
            let resp = send_with_retry(|| self.auth(self.client.get(&url)), "tree listing", false).await?;

            let next_url = resp
                .headers()
                .get("link")
                .and_then(|v| v.to_str().ok())
                .and_then(parse_link_next);

            let entries: Vec<TreeEntry> = resp.json().await?;
            all_entries.extend(entries);

            match next_url {
                Some(next) => url = next,
                None => break,
            }
        }

        Ok(all_entries)
    }

    async fn list_tree_repo(
        &self,
        repo_id: &str,
        repo_type: RepoType,
        revision: &str,
        prefix: &RemotePath,
    ) -> Result<Vec<TreeEntry>> {
        let mut all_entries = Vec::new();
        let params = "?limit=1000";
        let mut url = if prefix.is_empty() {
            format!(
                "{}/api/{}/{}/tree/{}{params}",
                self.endpoint,
                repo_type.api_prefix(),
                repo_id,
                revision,
            )
        } else {
            format!(
                "{}/api/{}/{}/tree/{}/{}{params}",
                self.endpoint,
                repo_type.api_prefix(),
                repo_id,
                revision,
                prefix.as_url(),
            )
        };

        loop {
            let resp = send_with_retry(|| self.auth(self.client.get(&url)), "repo tree listing", false).await?;

            let next_url = resp
                .headers()
                .get("link")
                .and_then(|v| v.to_str().ok())
                .and_then(parse_link_next);

            let raw_entries: Vec<RepoTreeEntry> = resp.json().await?;
            for raw in raw_entries {
                // For LFS files, the top-level `size` is the pointer size;
                // the real file size lives in `lfs.size`.
                let size = if let Some(ref lfs) = raw.lfs {
                    Some(lfs.size)
                } else {
                    raw.size
                };

                all_entries.push(TreeEntry {
                    path: raw.path,
                    entry_type: raw.entry_type,
                    size,
                    xet_hash: raw.xet_hash,
                    oid: raw.oid,
                    mtime: raw.last_commit.and_then(|c| c.date),
                });
            }

            match next_url {
                Some(next) => url = next,
                None => break,
            }
        }

        Ok(all_entries)
    }

    /// Fetch metadata for a single file via HEAD on the resolve endpoint.
    /// Returns `None` if 404 (file does not exist remotely).
    pub async fn head_file(&self, path: &str) -> Result<Option<HeadFileInfo>> {
        let api_path = self.prefixed_path(path)?;
        let url = match &self.source {
            // Buckets: /buckets/{id}/resolve/{path} (no /api/ prefix)
            SourceKind::Bucket { bucket_id } => {
                format!("{}/buckets/{}/resolve/{}", self.endpoint, bucket_id, api_path.as_url())
            }
            // Repos: /{resolve_prefix}{id}/resolve/{revision}/{path}
            SourceKind::Repo {
                repo_id,
                repo_type,
                revision,
            } => {
                format!(
                    "{}/{}{}/resolve/{}/{}",
                    self.endpoint,
                    repo_type.resolve_prefix(),
                    repo_id,
                    revision,
                    api_path.as_url(),
                )
            }
        };
        let resp = send_with_retry(|| self.auth(self.head_client.head(&url)), "head_file", true).await;
        let resp = match resp {
            Ok(r) => r,
            Err(Error::Hub { status: Some(404), .. }) => return Ok(None),
            Err(err) => return Err(err),
        };

        let headers = resp.headers();
        let xet_hash = headers
            .get("x-xet-hash")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        // Prefer x-linked-size (LFS pointer's true blob size). For non-LFS files
        // the resolve endpoint serves the file directly, so content-length is the
        // real size.
        let size = headers
            .get("x-linked-size")
            .or_else(|| headers.get("content-length"))
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        let etag = headers
            .get("x-linked-etag")
            .or_else(|| headers.get("etag"))
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim_matches('"').to_string());
        let last_modified = headers
            .get("last-modified")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        Ok(Some(HeadFileInfo {
            xet_hash,
            etag,
            size,
            last_modified,
        }))
    }

    /// Get a CAS read token.
    pub async fn get_cas_token(&self) -> Result<CasTokenInfo> {
        let url = match &self.source {
            SourceKind::Bucket { bucket_id } => {
                format!("{}/api/buckets/{}/xet-read-token", self.endpoint, bucket_id)
            }
            SourceKind::Repo {
                repo_id,
                repo_type,
                revision,
            } => {
                format!(
                    "{}/api/{}/{}/xet-read-token/{}",
                    self.endpoint,
                    repo_type.api_prefix(),
                    repo_id,
                    revision,
                )
            }
        };

        let resp = send_with_retry(|| self.auth(self.client.get(&url)), "CAS token request", false).await?;
        let info: CasTokenInfo = resp.json().await?;
        Ok(info)
    }

    /// Get a CAS write token (buckets only).
    pub async fn get_cas_write_token(&self) -> Result<CasTokenInfo> {
        let bucket_id = match &self.source {
            SourceKind::Bucket { bucket_id } => bucket_id,
            SourceKind::Repo { .. } => {
                return Err(Error::hub("write tokens not supported for repos"));
            }
        };
        let url = format!("{}/api/buckets/{}/xet-write-token", self.endpoint, bucket_id);

        let resp = send_with_retry(|| self.auth(self.client.get(&url)), "CAS write token request", false).await?;
        let info: CasTokenInfo = resp.json().await?;
        Ok(info)
    }

    /// Execute batch operations (add/delete files) on the bucket.
    pub async fn batch_operations(&self, ops: &[BatchOp]) -> Result<()> {
        let bucket_id = match &self.source {
            SourceKind::Bucket { bucket_id } => bucket_id,
            SourceKind::Repo { .. } => {
                return Err(Error::hub("batch operations not supported for repos"));
            }
        };
        let url = format!("{}/api/buckets/{}/batch", self.endpoint, bucket_id);

        // Build the NDJSON body. Every path goes through `prefixed_path`, which
        // joins the subfolder prefix and encrypts when a cipher is configured; the
        // result is used raw (JSON-escaped, not URL-escaped).
        let mut body = String::new();
        for op in ops {
            let transformed = match op {
                BatchOp::AddFile {
                    path,
                    xet_hash,
                    mtime,
                    content_type,
                } => BatchOp::AddFile {
                    path: self.prefixed_path(path)?.as_body().to_string(),
                    xet_hash: xet_hash.clone(),
                    mtime: *mtime,
                    content_type: content_type.clone(),
                },
                BatchOp::DeleteFile { path } => BatchOp::DeleteFile {
                    path: self.prefixed_path(path)?.as_body().to_string(),
                },
            };
            body.push_str(&serde_json::to_string(&transformed)?);
            body.push('\n');
        }

        let body = bytes::Bytes::from(body);
        send_with_retry(
            || {
                self.auth(self.client.post(&url))
                    .header("content-type", "application/x-ndjson")
                    .body(body.clone())
            },
            "batch operation",
            false,
        )
        .await?;

        Ok(())
    }

    /// Download a file via HTTP GET on the resolve endpoint and write it to `dest`.
    /// Used for non-Xet files in repos (no xet hash).
    ///
    /// Supports ETag-based conditional requests: if `dest` already exists and a
    /// sidecar `{dest}.etag` file is present, sends `If-None-Match`. On 304 the
    /// existing cached file is kept as-is.
    pub async fn download_file_http(&self, path: &str, dest: &Path) -> Result<()> {
        let api_path = self.prefixed_path(path)?;
        let url = match &self.source {
            SourceKind::Bucket { bucket_id } => {
                format!("{}/buckets/{}/resolve/{}", self.endpoint, bucket_id, api_path.as_url())
            }
            SourceKind::Repo {
                repo_id,
                repo_type,
                revision,
            } => {
                format!(
                    "{}/{}{}/resolve/{}/{}",
                    self.endpoint,
                    repo_type.resolve_prefix(),
                    repo_id,
                    revision,
                    api_path.as_url(),
                )
            }
        };

        // Read cached ETag only if dest exists — otherwise an orphan sidecar
        // (e.g. user manually deleted dest) could trick us into a 304 and
        // `return Ok(())` without dest actually being on disk.
        let etag_path = dest.with_extension("etag");
        let cached_etag = if tokio::fs::try_exists(dest).await.unwrap_or(false) {
            tokio::fs::read_to_string(&etag_path).await.ok()
        } else {
            None
        };

        info!("HTTP download: {} → {:?}", path, dest);
        let resp = send_with_retry(
            || {
                let mut r = self.auth(self.client.get(&url));
                if let Some(ref etag) = cached_etag {
                    r = r.header("If-None-Match", format!("\"{}\"", etag.trim()));
                }
                r
            },
            "HTTP download",
            false,
        )
        .await;
        let resp = match resp {
            Ok(r) => r,
            Err(Error::Hub { status: Some(304), .. }) => {
                info!("HTTP cache hit (304): {}", path);
                return Ok(());
            }
            Err(err) => return Err(err),
        };

        let new_etag = resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim_matches('"').to_string());

        // Stream response body to a temp file, then atomic-rename to dest.
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let tmp = dest.with_extension(format!("tmp.{}", std::process::id()));
        let result: std::result::Result<(), Error> = async {
            let mut file = tokio::fs::File::create(&tmp).await?;
            let mut stream = resp.bytes_stream();
            use futures::StreamExt;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                file.write_all(&chunk).await?;
            }
            file.shutdown().await?;
            drop(file);
            tokio::fs::rename(&tmp, dest).await?;
            if let Some(etag) = &new_etag {
                tokio::fs::write(&etag_path, etag).await.ok();
            } else {
                tokio::fs::remove_file(&etag_path).await.ok();
            }
            Ok(())
        }
        .await;
        if result.is_err() {
            tokio::fs::remove_file(&tmp).await.ok();
        }
        result
    }

    /// Create a token refresher for this source.
    /// Uses a write token when `read_only` is false (write tokens can also read).
    pub fn token_refresher(self: &Arc<Self>, read_only: bool) -> Arc<HubTokenRefresher> {
        let kind = if read_only { TokenKind::Read } else { TokenKind::Write };
        Arc::new(HubTokenRefresher {
            hub_client: self.clone(),
            kind,
        })
    }
}

pub fn mtime_from_str(s: &str) -> SystemTime {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .and_then(|dt| u64::try_from(dt.timestamp()).ok())
        .map(|secs| UNIX_EPOCH + std::time::Duration::from_secs(secs))
        .unwrap_or(UNIX_EPOCH)
}

/// Parse HTTP-date format (e.g. "Sat, 28 Feb 2026 14:52:39 GMT") from Last-Modified header.
pub fn mtime_from_http_date(s: &str) -> SystemTime {
    chrono::DateTime::parse_from_rfc2822(s)
        .ok()
        .and_then(|dt| u64::try_from(dt.timestamp()).ok())
        .map(|secs| UNIX_EPOCH + std::time::Duration::from_secs(secs))
        .unwrap_or(UNIX_EPOCH)
}

#[async_trait::async_trait]
impl HubOps for HubApiClient {
    async fn list_tree(&self, prefix: &str) -> Result<Vec<TreeEntry>> {
        self.list_tree(prefix).await
    }
    async fn head_file(&self, path: &str) -> Result<Option<HeadFileInfo>> {
        self.head_file(path).await
    }
    async fn batch_operations(&self, ops: &[BatchOp]) -> Result<()> {
        self.batch_operations(ops).await
    }
    async fn download_file_http(&self, path: &str, dest: &Path) -> Result<()> {
        self.download_file_http(path, dest).await
    }
    fn default_mtime(&self) -> SystemTime {
        self.default_mtime()
    }
    fn source(&self) -> &SourceKind {
        self.source()
    }
    fn is_repo(&self) -> bool {
        self.is_repo()
    }
    async fn probe_revision(&self) -> Result<String> {
        self.probe_revision().await
    }
}

/// Fields read from `/api/{type}/{id}` for the cheap-probe path. For repos,
/// `sha` is the commit head and changes on every push; `last_modified` is a
/// fallback when `sha` is absent (older Hub responses). For buckets,
/// `updated_at` is the only signal.
#[derive(serde::Deserialize)]
struct RevisionProbe {
    #[serde(default)]
    sha: Option<String>,
    #[serde(default, rename = "lastModified")]
    last_modified: Option<String>,
    #[serde(default, rename = "updatedAt")]
    updated_at: Option<String>,
}

/// Parse `Link` header to extract the URL with `rel="next"`.
/// Format: `<https://example.com/page2>; rel="next", <...>; rel="prev"`
fn parse_link_next(header: &str) -> Option<String> {
    for part in header.split(',') {
        let part = part.trim();
        if part.contains("rel=\"next\"")
            && let Some(start) = part.find('<')
            && let Some(end) = part.find('>')
        {
            return Some(part[start + 1..end].to_string());
        }
    }
    None
}

// ── Token refresh ─────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum TokenKind {
    Read,
    Write,
}

pub struct HubTokenRefresher {
    hub_client: Arc<HubApiClient>,
    kind: TokenKind,
}

impl HubTokenRefresher {
    pub async fn fetch_initial(&self) -> Result<CasTokenInfo> {
        match self.kind {
            TokenKind::Read => self.hub_client.get_cas_token().await,
            TokenKind::Write => self.hub_client.get_cas_write_token().await,
        }
    }
}

#[async_trait::async_trait]
impl TokenRefresher for HubTokenRefresher {
    async fn refresh(&self) -> std::result::Result<TokenInfo, AuthError> {
        let jwt = self
            .fetch_initial()
            .await
            .map_err(|e| AuthError::TokenRefreshFailure(e.to_string()))?;
        Ok((jwt.access_token, jwt.exp))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_batch_op_add_file_serialization() {
        let op = BatchOp::AddFile {
            path: "data/file.bin".to_string(),
            xet_hash: "abc123def456".to_string(),
            mtime: 1700000000000,
            content_type: Some("application/octet-stream".to_string()),
        };

        let json = serde_json::to_string(&op).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        // Verify tag is camelCase "addFile"
        assert_eq!(parsed["type"], "addFile");
        assert_eq!(parsed["path"], "data/file.bin");
        assert_eq!(parsed["xetHash"], "abc123def456");
        assert_eq!(parsed["mtime"], 1700000000000u64);
        assert_eq!(parsed["contentType"], "application/octet-stream");
    }

    #[test]
    fn test_batch_op_delete_file_serialization() {
        let op = BatchOp::DeleteFile {
            path: "old/file.txt".to_string(),
        };

        let json = serde_json::to_string(&op).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["type"], "deleteFile");
        assert_eq!(parsed["path"], "old/file.txt");

        // Should not contain addFile-specific fields
        assert!(parsed.get("xetHash").is_none());
        assert!(parsed.get("mtime").is_none());
    }

    #[test]
    fn test_batch_op_add_file_no_content_type() {
        let op = BatchOp::AddFile {
            path: "readme.txt".to_string(),
            xet_hash: "hash999".to_string(),
            mtime: 1234567890000,
            content_type: None,
        };

        let json = serde_json::to_string(&op).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        // contentType should be absent (skip_serializing_if = "Option::is_none")
        assert!(
            parsed.get("contentType").is_none(),
            "contentType should be omitted when None"
        );

        // Other fields should still be present
        assert_eq!(parsed["type"], "addFile");
        assert_eq!(parsed["path"], "readme.txt");
        assert_eq!(parsed["xetHash"], "hash999");
        assert_eq!(parsed["mtime"], 1234567890000u64);
    }

    #[test]
    fn test_parse_link_next_picks_next_relation() {
        let header = r#"<https://api.example.com/page=1>; rel="prev", <https://api.example.com/page=3>; rel="next""#;
        assert_eq!(
            parse_link_next(header),
            Some("https://api.example.com/page=3".to_string())
        );
    }

    #[test]
    fn test_parse_link_next_returns_none_when_missing() {
        let header = r#"<https://api.example.com/page=1>; rel="prev", <https://api.example.com/page=2>; rel="last""#;
        assert_eq!(parse_link_next(header), None);
    }

    #[test]
    fn test_mtime_parsers_fallback_to_unix_epoch_on_invalid_input() {
        assert_eq!(mtime_from_str("not-a-date"), UNIX_EPOCH);
        assert_eq!(mtime_from_http_date("still-not-a-date"), UNIX_EPOCH);
    }

    #[test]
    fn test_mtime_parsers_support_valid_formats() {
        let rfc3339 = mtime_from_str("2026-02-28T14:52:39Z");
        let http_date = mtime_from_http_date("Sat, 28 Feb 2026 14:52:39 GMT");
        assert!(rfc3339 > UNIX_EPOCH);
        assert!(http_date > UNIX_EPOCH);
        assert_eq!(rfc3339, http_date);
    }

    #[test]
    fn test_repo_type_from_str() {
        assert_eq!("model".parse::<RepoType>().unwrap(), RepoType::Model);
        assert_eq!("dataset".parse::<RepoType>().unwrap(), RepoType::Dataset);
        assert_eq!("space".parse::<RepoType>().unwrap(), RepoType::Space);
        assert!("unknown".parse::<RepoType>().is_err());
    }

    #[test]
    fn test_repo_type_api_prefix() {
        assert_eq!(RepoType::Model.api_prefix(), "models");
        assert_eq!(RepoType::Dataset.api_prefix(), "datasets");
        assert_eq!(RepoType::Space.api_prefix(), "spaces");
    }

    #[test]
    fn test_repo_type_resolve_prefix() {
        assert_eq!(RepoType::Model.resolve_prefix(), "");
        assert_eq!(RepoType::Dataset.resolve_prefix(), "datasets/");
        assert_eq!(RepoType::Space.resolve_prefix(), "spaces/");
    }

    // ── split_path_prefix tests ───────────────────────────────────────

    #[test]
    fn test_split_path_prefix_bucket_no_subfolder() {
        let (id, prefix) = split_path_prefix("user/bucket").unwrap();
        assert_eq!(id, "user/bucket");
        assert_eq!(prefix, "");
    }

    #[test]
    fn test_split_path_prefix_bucket_with_subfolder() {
        let (id, prefix) = split_path_prefix("user/bucket/a/b").unwrap();
        assert_eq!(id, "user/bucket");
        assert_eq!(prefix, "a/b");
    }

    #[test]
    fn test_split_path_prefix_bucket_single_subfolder() {
        let (id, prefix) = split_path_prefix("user/bucket/checkpoints").unwrap();
        assert_eq!(id, "user/bucket");
        assert_eq!(prefix, "checkpoints");
    }

    #[test]
    fn test_split_path_prefix_single_segment() {
        let (id, prefix) = split_path_prefix("gpt2").unwrap();
        assert_eq!(id, "gpt2");
        assert_eq!(prefix, "");
    }

    #[test]
    fn test_split_path_prefix_repo_with_subfolder() {
        let (id, prefix) = split_path_prefix("user/model/ckpt/v2").unwrap();
        assert_eq!(id, "user/model");
        assert_eq!(prefix, "ckpt/v2");
    }

    #[test]
    fn test_split_path_prefix_trailing_slash() {
        let (id, prefix) = split_path_prefix("user/bucket/checkpoints/").unwrap();
        assert_eq!(id, "user/bucket");
        assert_eq!(prefix, "checkpoints");
    }

    #[test]
    fn test_split_path_prefix_rejects_dotdot() {
        assert!(split_path_prefix("user/bucket/../other").is_err());
    }

    #[test]
    fn test_split_path_prefix_rejects_dot() {
        assert!(split_path_prefix("user/bucket/./foo").is_err());
    }

    // ── prefixed_path / strip_path_prefix tests ───────────────────────

    fn make_test_client(prefix: &str, token_file: Option<PathBuf>) -> HubApiClient {
        let (client, head_client) = make_clients("test");
        HubApiClient {
            client,
            head_client,
            endpoint: "https://huggingface.co".to_string(),
            token: Some("static-token".to_string()),
            token_file,
            token_file_cache: std::sync::Mutex::new(None),
            source: SourceKind::Bucket {
                bucket_id: "user/bucket".to_string(),
            },
            last_modified: UNIX_EPOCH,
            path_prefix: prefix.to_string(),
            #[cfg(feature = "encrypt")]
            path_cipher: None,
        }
    }

    #[test]
    fn test_prefixed_path_empty_prefix() {
        let c = make_test_client("", None);
        assert_eq!(c.prefixed_path("file.txt").unwrap().as_body(), "file.txt");
        assert_eq!(c.prefixed_path("a/b").unwrap().as_body(), "a/b");
        assert_eq!(c.prefixed_path("").unwrap().as_body(), "");
    }

    #[test]
    fn test_prefixed_path_with_prefix() {
        let c = make_test_client("sub/dir", None);
        assert_eq!(c.prefixed_path("file.txt").unwrap().as_body(), "sub/dir/file.txt");
        assert_eq!(c.prefixed_path("a/b").unwrap().as_body(), "sub/dir/a/b");
        assert_eq!(c.prefixed_path("").unwrap().as_body(), "sub/dir");
    }

    #[test]
    fn plaintext_paths_pass_through_url_unchanged() {
        // A non-encrypted client must not alter or percent-encode paths.
        let c = make_test_client("", None);
        let rp = c.prefixed_path("a b?c.txt").unwrap();
        assert_eq!(rp.as_body(), "a b?c.txt");
        assert_eq!(rp.as_url(), "a b?c.txt");
    }

    #[cfg(feature = "encrypt")]
    mod encryption_boundary {
        use super::*;
        use crate::encryption::ENC_ROOT;
        use crate::encryption::path::PathCipher;
        use std::sync::Arc;

        fn encrypted_client(prefix: &str) -> HubApiClient {
            let mut c = make_test_client(prefix, None);
            c.path_cipher = Some(Arc::new(PathCipher::new([0x11u8; 16])));
            c
        }

        /// Assert the stored path carries the `.enc/` wrapper and return what's
        /// underneath it.
        fn strip_enc_root(path: &str) -> &str {
            path.strip_prefix(ENC_ROOT)
                .and_then(|p| p.strip_prefix('/'))
                .unwrap_or_else(|| panic!("stored path must start with {ENC_ROOT}/, got: {path}"))
        }

        /// An encrypted client pointed at a one-shot mock server returning 404,
        /// for the list_tree 404→empty guard tests.
        async fn encrypted_client_with_404_mock(prefix: &str) -> HubApiClient {
            // The guard tests never inspect the request, and the mock server
            // ignores send failures, so the capture receiver can be dropped.
            let (mock_url, _rx) = capture_requests(vec![404]).await;
            let mut c = make_test_client(prefix, None);
            c.client = reqwest::Client::new();
            c.endpoint = mock_url;
            c.path_cipher = Some(Arc::new(PathCipher::new([0x11u8; 16])));
            c
        }

        fn entry(path: &str) -> TreeEntry {
            TreeEntry {
                path: path.to_string(),
                entry_type: "file".to_string(),
                size: Some(10),
                xet_hash: None,
                oid: None,
                mtime: None,
            }
        }

        #[test]
        fn outgoing_path_is_ciphertext_and_round_trips() {
            let c = encrypted_client("");
            let cipher = c.path_cipher.clone().unwrap();
            let rp = c.prefixed_path("dir/super-secret.txt").unwrap();
            // The JSON body form starts with .enc/ and is ciphertext — no plaintext leak.
            assert!(!rp.as_body().contains("secret"));
            assert!(!rp.as_body().contains("dir"));
            // Strip the wrapper, then decrypt back to the bucket-absolute plaintext path.
            let stored = strip_enc_root(rp.as_body());
            assert_eq!(cipher.decrypt_path(stored), Some("dir/super-secret.txt".to_string()));
        }

        #[test]
        fn prefix_is_encrypted_into_the_bucket_absolute_path() {
            let c = encrypted_client("sub/dir");
            let cipher = c.path_cipher.clone().unwrap();
            let rp = c.prefixed_path("a.txt").unwrap();
            let stored = strip_enc_root(rp.as_body());
            assert_eq!(cipher.decrypt_path(stored), Some("sub/dir/a.txt".to_string()));
        }

        #[test]
        fn empty_path_maps_to_enc_root() {
            let c = encrypted_client("");
            let rp = c.prefixed_path("").unwrap();
            assert_eq!(rp.as_body(), ENC_ROOT);
            assert!(!rp.is_empty());
        }

        #[test]
        fn url_form_is_encoded_and_leak_free() {
            let c = encrypted_client("");
            let rp = c.prefixed_path("super-secret-config.json").unwrap();
            let url = rp.as_url();
            assert!(!url.contains("secret"));
            // Only URL-safe bytes reach the request URL.
            assert!(
                url.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/' | b'%')),
                "url contains unescaped reserved characters: {url}"
            );
        }

        #[test]
        fn listing_decrypts_known_and_drops_the_rest() {
            let c = encrypted_client("");
            let cipher = c.path_cipher.clone().unwrap();
            let encrypted = cipher.encrypt_path("dir/file.txt").unwrap();
            let stored_path = format!("{ENC_ROOT}/{encrypted}");
            let mut entries = vec![
                entry(&stored_path),               // valid ciphertext under .enc → decrypted
                entry(ENC_ROOT),                   // bare .enc directory → dropped (no trailing /)
                entry("plaintext.txt"),            // plaintext at root → dropped
                entry("AAAAAAAAAAAAAAAAAAAAAAAA"), // garbage outside .enc → dropped
            ];
            c.decrypt_listing(&mut entries);
            let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
            assert_eq!(paths, vec!["dir/file.txt"]);
        }

        #[test]
        fn listing_drops_ciphertext_outside_enc() {
            let c = encrypted_client("");
            let cipher = c.path_cipher.clone().unwrap();
            // A valid ciphertext component that lives outside .enc should be dropped.
            let encrypted = cipher.encrypt_path("secret.txt").unwrap();
            let mut entries = vec![
                entry(&format!("{ENC_ROOT}/{encrypted}")), // inside .enc → kept
                entry(&encrypted),                         // at root (outside .enc) → dropped
            ];
            c.decrypt_listing(&mut entries);
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].path, "secret.txt");
        }

        #[test]
        fn non_encrypted_client_omits_enc_prefix() {
            let c = make_test_client("", None);
            #[cfg(feature = "encrypt")]
            assert!(c.path_cipher.is_none());
            let rp = c.prefixed_path("file.txt").unwrap();
            assert_eq!(rp.as_body(), "file.txt");

            let rp_root = c.prefixed_path("").unwrap();
            assert!(rp_root.is_empty());
        }

        // ── 404→empty listing tests (async, use mock HTTP) ─────────────

        #[tokio::test]
        async fn encrypted_client_404_on_root_becomes_empty() {
            // .enc/ doesn't exist → API returns 404 for /tree/.enc, which the
            // root listing maps to empty (all three guard legs true).
            let c = encrypted_client_with_404_mock("").await;
            let entries = c.list_tree("").await.unwrap();
            assert!(entries.is_empty());
        }

        #[tokio::test]
        async fn encrypted_client_404_on_subdir_still_errors() {
            // A non-root prefix also gets a 404 → must NOT be mapped to empty.
            let c = encrypted_client_with_404_mock("").await;
            assert!(c.list_tree("some-dir").await.is_err());
        }

        #[tokio::test]
        async fn encrypted_subfolder_mount_404_not_swallowed() {
            // A subfolder mount calling list_tree("") must get its 404 propagated
            // so validate_path_prefix can detect a missing subfolder: path_prefix
            // is non-empty, so the 404 guard's third leg fails → error.
            let c = encrypted_client_with_404_mock("sub/dir").await;
            assert!(c.list_tree("").await.is_err());
        }
    }

    #[test]
    fn test_strip_path_prefix_empty_prefix() {
        let c = make_test_client("", None);
        assert_eq!(c.strip_path_prefix("file.txt"), Some("file.txt"));
        assert_eq!(c.strip_path_prefix("a/b/c"), Some("a/b/c"));
    }

    #[test]
    fn test_strip_path_prefix_with_prefix() {
        let c = make_test_client("sub/dir", None);
        assert_eq!(c.strip_path_prefix("sub/dir/file.txt"), Some("file.txt"));
        assert_eq!(c.strip_path_prefix("sub/dir/a/b"), Some("a/b"));
        // The prefix directory itself → empty string
        assert_eq!(c.strip_path_prefix("sub/dir"), Some(""));
        // Unrelated path → None
        assert_eq!(c.strip_path_prefix("other/file.txt"), None);
    }

    // ── token file cache tests ────────────────────────────────────────

    fn cached_token(client: &HubApiClient) -> Option<String> {
        client.token_file_cache.lock().unwrap().as_ref().map(|(_, t)| t.clone())
    }

    fn test_token_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hf-mount-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn token_file_populates_cache_on_first_auth() {
        let dir = test_token_dir();
        let path = dir.join("token-pop");
        std::fs::write(&path, "file-token\n").unwrap();

        let client = make_test_client("", Some(path.clone()));
        let req = client.client.get("http://example.com");
        let _ = client.auth(req);

        assert_eq!(cached_token(&client).as_deref(), Some("file-token"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn token_file_cache_reused_within_ttl() {
        let dir = test_token_dir();
        let path = dir.join("token-ttl");
        std::fs::write(&path, "token-v1").unwrap();

        let client = make_test_client("", Some(path.clone()));
        let req = client.client.get("http://example.com");
        let _ = client.auth(req);
        assert_eq!(cached_token(&client).as_deref(), Some("token-v1"));

        // Update file, but cache should still serve old value.
        std::fs::write(&path, "token-v2").unwrap();
        let req = client.client.get("http://example.com");
        let _ = client.auth(req);
        assert_eq!(cached_token(&client).as_deref(), Some("token-v1"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn token_file_refreshes_after_ttl() {
        let dir = test_token_dir();
        let path = dir.join("token-refresh");
        std::fs::write(&path, "token-v1").unwrap();

        let client = make_test_client("", Some(path.clone()));
        let req = client.client.get("http://example.com");
        let _ = client.auth(req);

        // Force cache expiry by backdating the timestamp.
        {
            let mut cache = client.token_file_cache.lock().unwrap();
            if let Some((ref mut at, _)) = *cache {
                *at -= TOKEN_FILE_REFRESH + std::time::Duration::from_secs(1);
            }
        }

        std::fs::write(&path, "token-v2").unwrap();
        let req = client.client.get("http://example.com");
        let _ = client.auth(req);
        assert_eq!(cached_token(&client).as_deref(), Some("token-v2"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn token_file_missing_falls_back_to_static() {
        let path = PathBuf::from("/tmp/nonexistent-token-file-test");
        let client = make_test_client("", Some(path));
        let req = client.client.get("http://example.com");
        let _ = client.auth(req);
        assert!(cached_token(&client).is_none());
    }

    // ── retry / error helpers ─────────────────────────────────────────

    #[test]
    fn retry_delay_exponential_backoff() {
        assert_eq!(retry_delay(1), std::time::Duration::from_millis(500));
        assert_eq!(retry_delay(2), std::time::Duration::from_millis(1000));
        assert_eq!(retry_delay(3), std::time::Duration::from_millis(2000));
    }

    #[test]
    fn is_retryable_status_covers_expected_codes() {
        use crate::error::is_retryable_status;
        assert!(is_retryable_status(408));
        assert!(is_retryable_status(429));
        assert!(is_retryable_status(500));
        assert!(is_retryable_status(502));
        assert!(is_retryable_status(503));
        assert!(is_retryable_status(504));
        assert!(!is_retryable_status(200));
        assert!(!is_retryable_status(301));
        assert!(!is_retryable_status(400));
        assert!(!is_retryable_status(401));
        assert!(!is_retryable_status(403));
        assert!(!is_retryable_status(404));
    }

    #[test]
    fn parse_retry_delay_extracts_t_value() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("ratelimit", r#""hub_api";r=0;t=45"#.parse().unwrap());
        let duration = parse_retry_delay(&headers).unwrap();
        assert_eq!(duration, std::time::Duration::from_secs(30)); // capped
    }

    #[test]
    fn parse_retry_delay_small_value() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("ratelimit", r#""hub_api";r=0;t=5"#.parse().unwrap());
        let duration = parse_retry_delay(&headers).unwrap();
        assert_eq!(duration, std::time::Duration::from_secs(5));
    }

    #[test]
    fn parse_retry_delay_missing_header() {
        let headers = reqwest::header::HeaderMap::new();
        assert!(parse_retry_delay(&headers).is_none());
    }

    #[test]
    fn parse_retry_delay_no_t_param() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("ratelimit", r#""hub_api";r=50"#.parse().unwrap());
        assert!(parse_retry_delay(&headers).is_none());
    }

    #[test]
    fn error_hub_constructors() {
        let err = Error::hub("test error");
        assert!(matches!(err, Error::Hub { status: None, .. }));
        assert!(err.to_string().contains("test error"));

        let err = Error::hub_status(404, "not found");
        assert!(matches!(err, Error::Hub { status: Some(404), .. }));
        assert!(err.to_string().contains("404"));
        assert!(err.to_string().contains("not found"));
    }

    // ── send_with_retry integration tests ─────────────────────────────

    /// Minimal HTTP server that responds with a given sequence of status codes.
    async fn mock_server(responses: Vec<u16>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");

        tokio::spawn(async move {
            for status in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let reason = match status {
                    200 => "OK",
                    304 => "Not Modified",
                    404 => "Not Found",
                    429 => "Too Many Requests",
                    500 => "Internal Server Error",
                    503 => "Service Unavailable",
                    _ => "Unknown",
                };
                let body = format!("status {status}");
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.ok();
                stream.shutdown().await.ok();
            }
        });

        url
    }

    /// Like `mock_server`, but captures each full HTTP request (request line +
    /// headers + body) and hands it back over a channel for assertions.
    #[cfg(feature = "encrypt")]
    async fn capture_requests(responses: Vec<u16>) -> (String, tokio::sync::mpsc::Receiver<Vec<u8>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        let (tx, rx) = tokio::sync::mpsc::channel(8);

        tokio::spawn(async move {
            for status in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                // Read headers, then `Content-Length` bytes of body.
                let mut data = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    data.extend_from_slice(&buf[..n]);
                    if let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&data[..pos]).to_lowercase();
                        let content_length = headers
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if data.len() >= pos + 4 + content_length {
                            break;
                        }
                    }
                }
                let _ = tx.send(data).await;
                let body = "[]";
                let response = format!(
                    "HTTP/1.1 {status} OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.ok();
                stream.shutdown().await.ok();
            }
        });

        (url, rx)
    }

    /// HTTP-level proof of the boundary: an encrypted GET URL carries `.enc/` +
    /// percent-encoded ciphertext, while batch NDJSON carries the raw stored path
    /// (`.enc/` + raw ciphertext) — and neither ever carries the plaintext.
    #[cfg(feature = "encrypt")]
    #[tokio::test]
    async fn http_get_url_is_encoded_ciphertext_and_batch_body_is_raw() {
        use crate::encryption::ENC_ROOT;
        use crate::encryption::path::PathCipher;
        use std::sync::Arc;

        let cipher = Arc::new(PathCipher::new([0x11u8; 16]));
        let encrypted = cipher.encrypt_path("secret-dir/secret.txt").unwrap();
        let stored = format!("{ENC_ROOT}/{encrypted}");
        // `.enc` is all URI-unreserved characters, so the encoded form keeps it
        // verbatim: ".enc/" + percent-encoded ciphertext.
        let url_form = crate::encryption::url::encode_path_segments(&stored);
        assert!(url_form.starts_with(&format!("{ENC_ROOT}/")));

        let (mock_url, mut rx) = capture_requests(vec![404, 200]).await;
        let client = HubApiClient::new(&mock_url, Some("tok"), "user/bucket", "test").with_path_cipher(cipher);

        // GET (head_file): the resolve URL path is `.enc/` + percent-encoded ciphertext.
        let _ = client.head_file("secret-dir/secret.txt").await;
        let req = String::from_utf8_lossy(&rx.recv().await.unwrap()).into_owned();
        let request_line = req.lines().next().unwrap_or("");
        assert!(
            request_line.contains(&format!("/resolve/{url_form}")),
            "GET URL must contain .enc/ + the percent-encoded ciphertext, got: {request_line}"
        );
        assert!(!req.contains("secret"), "no plaintext name in the request");

        // POST (batch): NDJSON body carries the raw stored path, not URL-encoded.
        client
            .batch_operations(&[BatchOp::AddFile {
                path: "secret-dir/secret.txt".to_string(),
                xet_hash: "deadbeef".to_string(),
                mtime: 0,
                content_type: None,
            }])
            .await
            .unwrap();
        let req = String::from_utf8_lossy(&rx.recv().await.unwrap()).into_owned();
        // The NDJSON body's `path` (JSON-decoded) starts with `.enc/`.
        let json_line = req.split("\r\n\r\n").nth(1).unwrap_or("").lines().next().unwrap_or("");
        let op: serde_json::Value = serde_json::from_str(json_line).expect("batch body is NDJSON");
        assert_eq!(
            op["path"].as_str().unwrap(),
            stored,
            "batch path must be the raw .enc/ + ciphertext"
        );
        assert!(!req.contains("secret"), "no plaintext name in the batch body");
    }

    #[tokio::test]
    async fn send_with_retry_success_on_first_try() {
        let url = mock_server(vec![200]).await;
        let client = Client::new();
        let resp = send_with_retry(|| client.get(&url), "test", false).await.unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn send_with_retry_retries_on_503_then_succeeds() {
        let url = mock_server(vec![503, 200]).await;
        let client = Client::new();
        let resp = send_with_retry(|| client.get(&url), "test", false).await.unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn send_with_retry_retries_on_429_then_succeeds() {
        let url = mock_server(vec![429, 200]).await;
        let client = Client::new();
        let resp = send_with_retry(|| client.get(&url), "test", false).await.unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn send_with_retry_gives_up_after_max_retries() {
        let url = mock_server(vec![503, 503, 503]).await;
        let client = Client::new();
        let result = send_with_retry(|| client.get(&url), "test", false).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, Error::Hub { status: Some(503), .. }));
    }

    #[tokio::test]
    async fn send_with_retry_no_retry_on_404() {
        let url = mock_server(vec![404]).await;
        let client = Client::new();
        let result = send_with_retry(|| client.get(&url), "test", false).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Error::Hub { status: Some(404), .. }));
    }

    #[tokio::test]
    async fn send_with_retry_304_returned_as_error() {
        let url = mock_server(vec![304]).await;
        let client = Client::new();
        let result = send_with_retry(|| client.get(&url), "test", false).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Error::Hub { status: Some(304), .. }));
    }

    #[tokio::test]
    async fn send_with_retry_accepts_redirects_when_enabled() {
        let url = mock_server(vec![302]).await;
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let resp = send_with_retry(|| client.get(&url), "test", true).await.unwrap();
        assert_eq!(resp.status(), 302);
    }

    #[tokio::test]
    async fn send_with_retry_rejects_redirects_when_disabled() {
        let url = mock_server(vec![302]).await;
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let result = send_with_retry(|| client.get(&url), "test", false).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Error::Hub { status: Some(302), .. }));
    }

    // ── probe_repo tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn probe_repo_identifies_model() {
        // probe_repo hits /api/models first → 200 means it's a model.
        let url = mock_server(vec![200]).await;
        let client = Client::new();
        let result = probe_repo(&client, &url, "user/thing", None, &None).await;
        assert_eq!(result, Some(RepoType::Model));
    }

    #[tokio::test]
    async fn probe_repo_falls_through_to_dataset() {
        // models 404 → datasets 200 → identified as dataset.
        let url = mock_server(vec![404, 200]).await;
        let client = Client::new();
        let result = probe_repo(&client, &url, "user/thing", None, &None).await;
        assert_eq!(result, Some(RepoType::Dataset));
    }

    #[tokio::test]
    async fn probe_repo_returns_none_when_no_endpoint_matches() {
        // All three repo endpoints return 404 → not a repo.
        let url = mock_server(vec![404, 404, 404]).await;
        let client = Client::new();
        let result = probe_repo(&client, &url, "user/thing", None, &None).await;
        assert_eq!(result, None);
    }
}
