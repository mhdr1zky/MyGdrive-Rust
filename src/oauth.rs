//! Desktop OAuth: PKCE, loopback callback and private, durable token storage.
use crate::{drive::AccessTokenProvider, now_ms, Error, Result};
use async_trait::async_trait;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use fs2::FileExt;
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Mutex,
};

pub const DRIVE_SCOPE: &str = "https://www.googleapis.com/auth/drive";
pub const FILE_SCOPE: &str = "https://www.googleapis.com/auth/drive.file";
const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

#[derive(Clone, Deserialize, Serialize)]
pub struct DesktopClient {
    client_id: String,
    client_secret: String,
}
impl DesktopClient {
    pub fn load(path: &Path) -> Result<Self> {
        #[derive(Deserialize)]
        struct Document {
            installed: DesktopClient,
        }
        let doc: Document = serde_json::from_reader(File::open(path)?)?;
        if !doc
            .installed
            .client_id
            .ends_with(".apps.googleusercontent.com")
            || doc.installed.client_secret.is_empty()
        {
            return Err(Error::Invalid(
                "expected Google Desktop app client JSON".into(),
            ));
        }
        // Endpoints in downloaded JSON are deliberately not trusted for token transmission.
        Ok(doc.installed)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct AccountIdentity {
    pub email: String,
    pub display_name: String,
}
#[derive(Deserialize, Serialize)]
struct StoredToken {
    client: DesktopClient,
    access_token: String,
    refresh_token: String,
    expires_at_ms: i64,
    scope: String,
    drive_user_id: String,
    #[serde(default)]
    identity: Option<AccountIdentity>,
}
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
    token_type: String,
    scope: Option<String>,
}

pub fn token_path(state: &Path, account: &str) -> PathBuf {
    state
        .join("oauth")
        .join(format!("{:x}.json", Sha256::digest(account.as_bytes())))
}
/// Read only the public identity from the private credential store.
pub fn saved_identity(path: &Path) -> Result<Option<AccountIdentity>> {
    Ok(read_token(path)?.identity)
}

/// Enrich older credential stores without requiring the user to sign in again.
pub async fn refresh_identity(path: &Path) -> Result<AccountIdentity> {
    refresh_identity_at(path, "https://www.googleapis.com/drive/v3/about").await
}
async fn refresh_identity_at(path: &Path, endpoint: &str) -> Result<AccountIdentity> {
    #[derive(Deserialize)]
    struct About {
        user: DriveUser,
    }
    let provider = OAuthProvider::new(path.to_path_buf())?;
    let response = http_client()?
        .get(endpoint)
        .query(&[("fields", "user(permissionId,emailAddress,displayName)")])
        .bearer_auth(provider.access_token().await?)
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(Error::AuthRequired);
    }
    let about: About = serde_json::from_slice(&bounded_body(response).await?)?;
    let _lock = auth_lock(path)?;
    let mut saved = read_token(path)?;
    if saved.drive_user_id != about.user.permission_id {
        return Err(Error::AuthRequired);
    }
    let identity = about.user.identity()?;
    saved.identity = Some(identity.clone());
    save_token(path, &saved)?;
    Ok(identity)
}
#[derive(Deserialize)]
struct DriveUser {
    #[serde(rename = "permissionId")]
    permission_id: String,
    #[serde(rename = "emailAddress", default)]
    email: String,
    #[serde(rename = "displayName", default)]
    display_name: String,
}
impl DriveUser {
    fn identity(&self) -> Result<AccountIdentity> {
        if self.email.is_empty() {
            return Err(Error::Invalid(
                "Google tidak mengembalikan email akun.".into(),
            ));
        }
        Ok(AccountIdentity {
            email: self.email.clone(),
            display_name: self.display_name.clone(),
        })
    }
}
fn private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } {
        return Err(Error::Invalid(
            "OAuth directory must be owned by current user and not a symlink".into(),
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
fn auth_lock(path: &Path) -> Result<File> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Invalid("OAuth storage needs parent directory".into()))?;
    private_directory(parent)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path.with_extension("lock"))?;
    lock.try_lock_exclusive()
        .map_err(|_| Error::Invalid("another OAuth operation is running; retry later".into()))?;
    Ok(lock)
}
fn read_token(path: &Path) -> Result<StoredToken> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::AuthRequired
            } else {
                Error::Io(e)
            }
        })?;
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.mode() & 0o077 != 0
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.len() > 65536
    {
        return Err(Error::Invalid(
            "OAuth token file must be private (0600), regular, owned by current user".into(),
        ));
    }
    Ok(serde_json::from_reader(file.take(65537))?)
}
fn save_token(path: &Path, value: &StoredToken) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Invalid("OAuth storage needs parent directory".into()))?;
    private_directory(parent)?;
    let temp = parent.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(&serde_json::to_vec(value)?)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}
fn http_client() -> Result<Client> {
    Ok(Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()?)
}
async fn bounded_body(mut response: reqwest::Response) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > 65536 {
            return Err(Error::Invalid("oversized OAuth response".into()));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}
async fn token_request(
    http: &Client,
    endpoint: &str,
    form: &[(&str, &str)],
) -> Result<TokenResponse> {
    let response = http.post(endpoint).form(form).send().await?;
    let status = response.status();
    if !status.is_success() {
        return Err(match status.as_u16() {
            400 | 401 | 403 => Error::AuthRequired,
            code => Error::Http {
                status: code,
                reason: "OAuth endpoint unavailable".into(),
            },
        });
    }
    let body = bounded_body(response).await?;
    // Parse errors contain no response content or token values.
    let token: TokenResponse = serde_json::from_slice(&body)
        .map_err(|_| Error::Invalid("invalid OAuth response".into()))?;
    if !token.token_type.eq_ignore_ascii_case("Bearer")
        || token.access_token.is_empty()
        || token.expires_in == 0
        || token.expires_in > 31_536_000
    {
        return Err(Error::Invalid("invalid OAuth token properties".into()));
    }
    Ok(token)
}
fn expiry(seconds: u64) -> i64 {
    now_ms().saturating_add((seconds * 1000) as i64)
}
fn has_scope(granted: &str, required: &str) -> bool {
    granted.split_whitespace().any(|s| s == required)
}

pub struct OAuthProvider {
    path: PathBuf,
    http: Client,
    endpoint: String,
    mutex: Mutex<()>,
}
impl OAuthProvider {
    pub fn new(path: PathBuf) -> Result<Self> {
        Ok(Self {
            path,
            http: http_client()?,
            endpoint: TOKEN_URL.into(),
            mutex: Mutex::new(()),
        })
    }
}
#[async_trait]
impl AccessTokenProvider for OAuthProvider {
    async fn access_token(&self) -> Result<String> {
        let _guard = self.mutex.lock().await;
        // Re-read on every request so re-login is visible to an existing daemon.
        let mut saved = read_token(&self.path)?;
        if saved.expires_at_ms > now_ms() + 60_000 {
            return Ok(saved.access_token);
        }
        let _lock = auth_lock(&self.path)?;
        saved = read_token(&self.path)?;
        if saved.expires_at_ms > now_ms() + 60_000 {
            return Ok(saved.access_token);
        }
        let token = token_request(
            &self.http,
            &self.endpoint,
            &[
                ("grant_type", "refresh_token"),
                ("client_id", &saved.client.client_id),
                ("client_secret", &saved.client.client_secret),
                ("refresh_token", &saved.refresh_token),
            ],
        )
        .await?;
        if let Some(scope) = &token.scope {
            if !has_scope(scope, &saved.scope) {
                return Err(Error::AuthRequired);
            }
        }
        saved.access_token = token.access_token;
        if let Some(refresh) = token.refresh_token.filter(|s| !s.is_empty()) {
            saved.refresh_token = refresh;
        }
        saved.expires_at_ms = expiry(token.expires_in);
        save_token(&self.path, &saved)?;
        Ok(saved.access_token)
    }
}

fn nonce() -> String {
    let mut bytes = Vec::from(*uuid::Uuid::new_v4().as_bytes());
    bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    URL_SAFE_NO_PAD.encode(bytes)
}
fn authorization_url(
    client: &DesktopClient,
    redirect: &str,
    scope: &str,
    state: &str,
    verifier: &str,
) -> Url {
    let mut url = Url::parse(AUTH_URL).expect("constant URL");
    url.query_pairs_mut().extend_pairs([
        ("client_id", client.client_id.as_str()),
        ("redirect_uri", redirect),
        ("response_type", "code"),
        ("scope", scope),
        ("state", state),
        (
            "code_challenge",
            &URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
        ),
        ("code_challenge_method", "S256"),
        ("access_type", "offline"),
        ("prompt", "consent"),
    ]);
    url
}
// Returns None for unrelated or malformed requests; they cannot consume a login attempt.
fn callback(request: &str, state: &str, port: u16) -> Option<Result<String>> {
    let mut lines = request.split("\r\n");
    let parts: Vec<_> = lines.next()?.split_whitespace().collect();
    if parts.len() != 3
        || parts[0] != "GET"
        || parts[2] != "HTTP/1.1"
        || !parts[1].starts_with("/oauth/callback?")
    {
        return None;
    }
    let hosts: Vec<_> = lines
        .filter_map(|line| line.split_once(':'))
        .filter(|(key, _)| key.eq_ignore_ascii_case("host"))
        .collect();
    if hosts.len() != 1 || hosts[0].1.trim() != format!("127.0.0.1:{port}") {
        return None;
    }
    let url = Url::parse(&format!("http://127.0.0.1:{}{}", port, parts[1])).ok()?;
    let query: Vec<_> = url.query_pairs().collect();
    let values = |key: &str| {
        query
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.to_string())
            .collect::<Vec<_>>()
    };
    if values("state") != [state] {
        return None;
    }
    if !values("error").is_empty() {
        return Some(Err(Error::AuthRequired));
    }
    let codes = values("code");
    if codes.len() != 1 || codes[0].is_empty() {
        return None;
    }
    Some(Ok(codes[0].clone()))
}
async fn wait_callback(listener: &TcpListener, state: &str) -> Result<String> {
    let port = listener.local_addr()?.port();
    loop {
        let (mut socket, _) = listener.accept().await?;
        let result = tokio::time::timeout(Duration::from_secs(3), async {
            let mut data = Vec::new();
            loop {
                let mut chunk = [0u8; 1024];
                let n = socket.read(&mut chunk).await?;
                if n == 0 || data.len() + n > 8192 { return Ok::<_,std::io::Error>(None); }
                data.extend_from_slice(&chunk[..n]);
                if data.windows(4).any(|w| w == b"\r\n\r\n") { break; }
            }
            let parsed = std::str::from_utf8(&data).ok().and_then(|r| callback(r,state,port));
            let (status, body) = if parsed.is_some() { ("200 OK", "Respons diterima. Kembali ke terminal untuk melihat hasil login.") } else { ("400 Bad Request", "Callback tidak valid.") };
            socket.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nContent-Security-Policy: default-src 'none'\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",body.len()).as_bytes()).await?;
            Ok(parsed)
        }).await;
        if let Ok(Ok(Some(code))) = result {
            return code;
        }
    }
}

/// Starts a system-browser login. The user grants access directly on Google's page.
pub async fn login(
    client_path: &Path,
    path: &Path,
    scope: &str,
    no_browser: bool,
    timeout: Duration,
) -> Result<()> {
    if !(Duration::from_secs(60)..=Duration::from_secs(3600)).contains(&timeout) {
        return Err(Error::Invalid(
            "login timeout must be 60-3600 seconds".into(),
        ));
    }
    if scope != DRIVE_SCOPE && scope != FILE_SCOPE {
        return Err(Error::Invalid("unsupported Drive scope".into()));
    }
    login_flow(
        DesktopClient::load(client_path)?,
        path,
        scope,
        no_browser,
        timeout,
        true,
    )
    .await
}

/// Native UI entry point. Never prints authorization URLs or token data.
/// A missing client path reuses this profile's registered client for reauthentication.
pub async fn login_desktop(client_path: Option<&Path>, path: &Path) -> Result<()> {
    let client = match client_path {
        Some(p) => DesktopClient::load(p)?,
        None => read_token(path)?.client,
    };
    login_flow(
        client,
        path,
        DRIVE_SCOPE,
        false,
        Duration::from_secs(900),
        false,
    )
    .await
}

async fn login_flow(
    client: DesktopClient,
    path: &Path,
    scope: &str,
    no_browser: bool,
    timeout: Duration,
    announce: bool,
) -> Result<()> {
    let _lock = auth_lock(path)?;
    let previous = match read_token(path) {
        Ok(t) => Some(t),
        Err(Error::AuthRequired) => None,
        Err(e) => return Err(e),
    };
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let redirect = format!(
        "http://127.0.0.1:{}/oauth/callback",
        listener.local_addr()?.port()
    );
    let state = nonce();
    let verifier = nonce();
    let url = authorization_url(&client, &redirect, scope, &state, &verifier);
    if announce {
        println!("Buka URL berikut di browser pada komputer ini (berlaku {} menit). Jangan bagikan URL callback atau kode login:\n{url}", timeout.as_secs() / 60);
    }
    if !no_browser {
        match std::process::Command::new("xdg-open")
            .arg(url.as_str())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
            Err(_) if announce => {
                eprintln!("Browser tidak dapat dibuka otomatis; gunakan URL di atas.")
            }
            Err(_) => {
                return Err(Error::Invalid(
                    "Browser tidak dapat dibuka. Periksa aplikasi browser default.".into(),
                ))
            }
        }
    }
    let code = tokio::time::timeout(timeout, wait_callback(&listener, &state))
        .await
        .map_err(|_| Error::Invalid(if announce {
            "login timed out; callback listener closed. Run login again with --timeout-seconds 1800 and use the NEW browser page"
        } else {
            "Waktu login habis. Tekan Hubungkan akun atau Login ulang lagi, lalu gunakan halaman browser yang baru."
        }.into()))??;
    let http = http_client()?;
    let token = token_request(
        &http,
        TOKEN_URL,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("client_id", &client.client_id),
            ("client_secret", &client.client_secret),
            ("redirect_uri", &redirect),
            ("code_verifier", &verifier),
        ],
    )
    .await?;
    if !token.scope.as_deref().is_some_and(|s| has_scope(s, scope)) {
        return Err(Error::AuthRequired);
    }
    let refresh_token = token
        .refresh_token
        .filter(|s| !s.is_empty())
        .ok_or(Error::AuthRequired)?;
    #[derive(Deserialize)]
    struct About {
        user: DriveUser,
    }
    let response = http
        .get("https://www.googleapis.com/drive/v3/about")
        .query(&[("fields", "user(permissionId,emailAddress,displayName)")])
        .bearer_auth(&token.access_token)
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(Error::AuthRequired);
    }
    let about: About =
        serde_json::from_slice(&bounded_body(response).await?).map_err(|_| Error::AuthRequired)?;
    if about.user.permission_id.is_empty()
        || previous
            .as_ref()
            .is_some_and(|p| p.drive_user_id != about.user.permission_id)
    {
        return Err(Error::Invalid(
            if announce {
                "Google account differs from stored login; use a separate --state for another account"
            } else {
                "Akun Google berbeda dari profil ini. Login ulang dengan akun semula, atau hubungkan akun baru melalui profil terpisah."
            }.into(),
        ));
    }
    save_token(
        path,
        &StoredToken {
            client,
            access_token: token.access_token,
            refresh_token,
            expires_at_ms: expiry(token.expires_in),
            scope: scope.into(),
            identity: about.user.identity().ok(),
            drive_user_id: about.user.permission_id,
        },
    )?;
    if announce {
        println!("Login berhasil. Token privat tersimpan; refresh otomatis aktif.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::unix::fs::symlink, sync::Arc};
    use wiremock::{
        matchers::{body_string_contains, method, path},
        Mock, MockServer, ResponseTemplate,
    };

    fn stored() -> StoredToken {
        StoredToken {
            client: DesktopClient {
                client_id: "test.apps.googleusercontent.com".into(),
                client_secret: "client-private".into(),
            },
            access_token: "old-access".into(),
            refresh_token: "old-refresh".into(),
            expires_at_ms: 0,
            scope: DRIVE_SCOPE.into(),
            drive_user_id: "owner".into(),
            identity: None,
        }
    }
    #[tokio::test]
    async fn older_login_identity_is_enriched_privately_and_cannot_switch_accounts() {
        use wiremock::matchers::{header, path as request_path, query_param};
        let server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("oauth/token.json");
        let mut token = stored();
        token.expires_at_ms = now_ms() + 3_600_000;
        save_token(&path, &token).unwrap();
        assert!(saved_identity(&path).unwrap().is_none());
        Mock::given(request_path("/about")).and(header("authorization","Bearer old-access"))
            .and(query_param("fields","user(permissionId,emailAddress,displayName)"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"user":{"permissionId":"owner","emailAddress":"user@example.test","displayName":"User"}})))
            .mount(&server).await;
        let identity = refresh_identity_at(&path, &format!("{}/about", server.uri()))
            .await
            .unwrap();
        assert_eq!(identity.email, "user@example.test");
        assert_eq!(saved_identity(&path).unwrap(), Some(identity));
        assert_eq!(read_token(&path).unwrap().refresh_token, "old-refresh");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        server.reset().await;
        Mock::given(request_path("/about")).respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"user":{"permissionId":"different-owner","emailAddress":"other@example.test","displayName":"Other"}}))).mount(&server).await;
        let before = fs::read(&path).unwrap();
        assert!(
            refresh_identity_at(&path, &format!("{}/about", server.uri()))
                .await
                .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), before);
    }
    fn provider(path: PathBuf, server: &MockServer) -> OAuthProvider {
        let mut p = OAuthProvider::new(path).unwrap();
        p.endpoint = format!("{}/token", server.uri());
        p
    }
    fn response() -> serde_json::Value {
        serde_json::json!({"access_token":"new-access","expires_in":3600,"token_type":"Bearer"})
    }

    #[test]
    fn pkce_matches_rfc7636_and_url_never_contains_verifier_or_secret() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let url = authorization_url(
            &stored().client,
            "http://127.0.0.1:32123/oauth/callback",
            DRIVE_SCOPE,
            "state",
            verifier,
        );
        let query: std::collections::HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(
            query["code_challenge"],
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(query["access_type"], "offline");
        assert!(!url.as_str().contains(verifier));
        assert!(!url.as_str().contains("client-private"));
        assert_eq!(nonce().len(), 43);
        assert_ne!(nonce(), nonce());
    }
    fn request(query: &str) -> String {
        format!("GET /oauth/callback?{query} HTTP/1.1\r\nHost: 127.0.0.1:32123\r\n\r\n")
    }
    #[test]
    fn callback_rejects_csrf_duplicates_and_wrong_host() {
        for query in [
            "code=x",
            "state=wrong&code=x",
            "state=right&state=right&code=x",
            "state=right&code=a&code=b",
            "state=right&code=",
        ] {
            assert!(callback(&request(query), "right", 32123).is_none());
        }
        let good = request("state=right&code=a%2Bb");
        assert_eq!(callback(&good, "right", 32123).unwrap().unwrap(), "a+b");
        assert!(callback(
            &good.replace("Host: 127.0.0.1:32123", "Host: attacker.invalid"),
            "right",
            32123
        )
        .is_none());
        assert!(matches!(
            callback(&request("state=right&error=access_denied"), "right", 32123),
            Some(Err(Error::AuthRequired))
        ));
    }
    #[test]
    fn storage_is_private_atomic_and_rejects_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let file = token_path(temp.path(), "../../account");
        assert_eq!(file.parent().unwrap(), temp.path().join("oauth"));
        save_token(&file, &stored()).unwrap();
        assert_eq!(fs::metadata(&file).unwrap().mode() & 0o777, 0o600);
        assert_eq!(
            fs::metadata(file.parent().unwrap()).unwrap().mode() & 0o777,
            0o700
        );
        let mut next = stored();
        next.access_token = "replacement".into();
        save_token(&file, &next).unwrap();
        assert_eq!(read_token(&file).unwrap().access_token, "replacement");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_token(&file).is_err());
        let link = temp.path().join("symlink.json");
        symlink(&file, &link).unwrap();
        assert!(read_token(&link).is_err());
    }
    #[tokio::test]
    async fn concurrent_refresh_happens_once_and_preserves_existing_refresh_token() {
        let temp = tempfile::tempdir().unwrap();
        let file = token_path(temp.path(), "a");
        save_token(&file, &stored()).unwrap();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("refresh_token=old-refresh"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response()))
            .expect(1)
            .mount(&server)
            .await;
        let p = Arc::new(provider(file.clone(), &server));
        let (a, b) = tokio::join!(p.access_token(), p.access_token());
        assert_eq!(a.unwrap(), "new-access");
        assert_eq!(b.unwrap(), "new-access");
        let saved = read_token(&file).unwrap();
        assert_eq!(saved.refresh_token, "old-refresh");
        assert!(saved.expires_at_ms > now_ms() + 3_500_000);
        // A separately constructed process/provider observes the persisted refreshed token.
        assert_eq!(
            provider(file, &server).access_token().await.unwrap(),
            "new-access"
        );
    }
    #[tokio::test]
    async fn refresh_rotation_is_persisted() {
        let temp = tempfile::tempdir().unwrap();
        let file = token_path(temp.path(), "a");
        save_token(&file, &stored()).unwrap();
        let server = MockServer::start().await;
        let mut json = response();
        json["refresh_token"] = "rotated-refresh".into();
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json))
            .expect(1)
            .mount(&server)
            .await;
        provider(file.clone(), &server)
            .access_token()
            .await
            .unwrap();
        assert_eq!(read_token(&file).unwrap().refresh_token, "rotated-refresh");
    }
    #[tokio::test]
    async fn revoked_or_reduced_scope_does_not_overwrite_stored_tokens() {
        for status in [400, 200, 503] {
            let temp = tempfile::tempdir().unwrap();
            let file = token_path(temp.path(), "a");
            save_token(&file, &stored()).unwrap();
            let server = MockServer::start().await;
            let mut json = response();
            json["scope"] = FILE_SCOPE.into();
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(status).set_body_json(json))
                .expect(1)
                .mount(&server)
                .await;
            let error = provider(file.clone(), &server)
                .access_token()
                .await
                .unwrap_err();
            assert!(!error.to_string().contains("new-access"));
            if status != 503 {
                assert!(matches!(error, Error::AuthRequired));
            }
            assert_eq!(read_token(&file).unwrap().access_token, "old-access");
            assert_eq!(read_token(&file).unwrap().refresh_token, "old-refresh");
        }
    }
    #[tokio::test]
    async fn authorization_code_exchange_sends_pkce_and_does_not_follow_redirects() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("code_verifier=verifier"))
            .and(body_string_contains("grant_type=authorization_code"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response()))
            .expect(1)
            .mount(&server)
            .await;
        token_request(
            &http_client().unwrap(),
            &format!("{}/token", server.uri()),
            &[
                ("grant_type", "authorization_code"),
                ("code_verifier", "verifier"),
            ],
        )
        .await
        .unwrap();
        Mock::given(path("/redirect"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", format!("{}/must-not-follow", server.uri())),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/must-not-follow"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        assert!(token_request(
            &http_client().unwrap(),
            &format!("{}/redirect", server.uri()),
            &[]
        )
        .await
        .is_err());
    }
    #[tokio::test]
    async fn loopback_ignores_invalid_state_then_accepts_correct_callback() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let worker = tokio::spawn(async move { wait_callback(&listener, "nonce").await });
        let http = http_client().unwrap();
        let base = format!("http://127.0.0.1:{port}/oauth/callback");
        assert_eq!(
            http.get(format!("{base}?state=wrong&code=bad"))
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
        let reply = http
            .get(format!("{base}?state=nonce&code=accepted"))
            .send()
            .await
            .unwrap();
        assert_eq!(reply.status(), 200);
        assert_eq!(reply.headers()["cache-control"], "no-store");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), worker)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            "accepted"
        );
    }
}
