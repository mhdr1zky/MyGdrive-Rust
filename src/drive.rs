use crate::{model::*, Error, Result};
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use reqwest::{Client, Response, Url};
use std::{
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::Duration,
};
use tokio_util::io::ReaderStream;

pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>;
pub const FILE_FIELDS: &str = "id,name,mimeType,version,size,sha256Checksum,md5Checksum,headRevisionId,parents,trashed,capabilities(canDownload,canEdit,canTrash)";

#[async_trait]
pub trait AccessTokenProvider: Send + Sync {
    async fn access_token(&self) -> Result<String>;
}

/// The OAuth layer may replace this private file atomically after refreshing the token.
pub struct TokenFile(pub PathBuf);
#[async_trait]
impl AccessTokenProvider for TokenFile {
    async fn access_token(&self) -> Result<String> {
        let value = tokio::fs::read_to_string(&self.0)
            .await
            .map_err(|_| Error::AuthRequired)?;
        let value = value.trim();
        if value.is_empty() || value.contains(['\r', '\n']) {
            return Err(Error::AuthRequired);
        }
        Ok(value.to_string())
    }
}

#[async_trait]
pub trait DriveClient: Send + Sync {
    async fn start_page_token(&self) -> Result<String>;
    async fn list_files(&self, page_token: Option<&str>) -> Result<FilePage>;
    async fn changes(&self, page_token: &str) -> Result<ChangePage>;
    async fn get(&self, id: &str) -> Result<Option<RemoteFile>>;
    async fn list_children(&self, _parent: &str) -> Result<Vec<RemoteFile>> {
        Err(Error::Unsupported(
            "folder listing unavailable in this adapter".into(),
        ))
    }
    async fn download(&self, id: &str) -> Result<ByteStream>;
    async fn reserve_id(&self) -> Result<String>;
    async fn create(
        &self,
        id: &str,
        name: &str,
        parent: &str,
        snapshot: &Snapshot,
    ) -> Result<RemoteFile>;
    async fn create_folder(&self, _id: &str, _name: &str, _parent: &str) -> Result<RemoteFile> {
        Err(Error::Unsupported(
            "folder creation unavailable in this adapter".into(),
        ))
    }
    /// Only an adapter with verified server-side compare-and-swap may return true.
    fn conditional_mutations(&self) -> bool {
        false
    }
    async fn update(&self, _remote: &RemoteFile, _snapshot: &Snapshot) -> Result<RemoteFile> {
        Err(Error::Unsupported(
            "conditional update is not verified for this adapter".into(),
        ))
    }
    async fn trash(&self, _remote: &RemoteFile) -> Result<RemoteFile> {
        Err(Error::Unsupported(
            "conditional Trash is not verified for this adapter".into(),
        ))
    }
    /// Empty-folder Trash requires an adapter-level membership race guard.
    async fn trash_empty_folder(&self, remote: &RemoteFile) -> Result<RemoteFile> {
        if !remote.folder() || !self.list_children(&remote.id).await?.is_empty() {
            return Err(Error::Precondition);
        }
        self.trash(remote).await
    }
    async fn rename(&self, _remote: &RemoteFile, _name: &str, _parent: &str) -> Result<RemoteFile> {
        Err(Error::Unsupported(
            "conditional rename/move is not verified for this adapter".into(),
        ))
    }
}

pub struct HttpDriveClient {
    http: Client,
    base: Url,
    upload_base: Url,
    token: Arc<dyn AccessTokenProvider>,
}
impl HttpDriveClient {
    pub fn new(token: Arc<dyn AccessTokenProvider>) -> Result<Self> {
        Self::with_endpoints(
            token,
            "https://www.googleapis.com/drive/v3/",
            "https://www.googleapis.com/upload/drive/v3/",
        )
    }
    /// Custom endpoints permit a local mock server; never accept arbitrary sync metadata URLs.
    pub fn with_endpoints(
        token: Arc<dyn AccessTokenProvider>,
        base: &str,
        upload: &str,
    ) -> Result<Self> {
        let parse = |s: &str| -> Result<Url> {
            Url::parse(s).map_err(|_| Error::Invalid("invalid API endpoint".into()))
        };
        let base = parse(base)?;
        let upload_base = parse(upload)?;
        for url in [&base, &upload_base] {
            if url.scheme() != "https"
                && !(url.scheme() == "http"
                    && matches!(url.host_str(), Some("127.0.0.1" | "localhost")))
            {
                return Err(Error::Invalid(
                    "API endpoints must use HTTPS (except localhost tests)".into(),
                ));
            }
        }
        Ok(Self {
            http: Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(300))
                .tcp_keepalive(Duration::from_secs(30))
                .pool_idle_timeout(Duration::from_secs(120))
                .pool_max_idle_per_host(4)
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            base,
            upload_base,
            token,
        })
    }
    fn endpoint(&self, path: &str, id: Option<&str>) -> Result<Url> {
        let mut url = self.base.clone();
        let mut parts = url
            .path_segments_mut()
            .map_err(|_| Error::Invalid("API base".into()))?;
        parts.pop_if_empty().push(path);
        if let Some(id) = id {
            parts.push(id);
        }
        drop(parts);
        Ok(url)
    }
    async fn auth(&self, request: reqwest::RequestBuilder) -> Result<reqwest::RequestBuilder> {
        Ok(request.bearer_auth(self.token.access_token().await?))
    }
    /// Drive v3 ignores If-Match on writes. The v2 mutation API exposes a
    /// strong JSON ETag and rejects a stale validator at the commit itself.
    fn mutation_endpoint(&self, upload: bool, id: &str) -> Result<Url> {
        let mut url = if upload {
            self.upload_base.clone()
        } else {
            self.base.clone()
        };
        let path = url
            .path()
            .trim_end_matches('/')
            .strip_suffix("/v3")
            .ok_or_else(|| Error::Invalid("Drive mutation endpoint needs a v3 base".into()))?;
        let path = format!("{path}/v2");
        url.set_path(&path);
        url.path_segments_mut()
            .map_err(|_| Error::Invalid("Drive mutation base".into()))?
            .push("files")
            .push(id);
        Ok(url)
    }

    async fn get_mutation_remote(&self, id: &str) -> Result<(RemoteFile, Option<String>)> {
        let response = self.auth(self.http.get(self.mutation_endpoint(false, id)?)
            .query(&[("fields", "id,title,mimeType,version,fileSize,md5Checksum,headRevisionId,parents(id),labels(trashed),etag")]))
            .await?.send().await?;
        let value: serde_json::Value = Self::check(response).await?.json().await?;
        let tag = value["etag"].as_str().map(String::from);
        let parents: Vec<&str> = value["parents"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|parent| parent["id"].as_str())
            .collect();
        let remote = serde_json::from_value(serde_json::json!({
            "id":value["id"],"name":value["title"],"mimeType":value["mimeType"],
            "version":value["version"],"size":value["fileSize"],
            "md5Checksum":value["md5Checksum"],"headRevisionId":value["headRevisionId"],
            "parents":parents,"trashed":value["labels"]["trashed"].as_bool().unwrap_or(false)
        }))?;
        Ok((remote, tag))
    }

    fn mutation_tag(
        expected: &RemoteFile,
        current: &RemoteFile,
        tag: Option<String>,
    ) -> Result<String> {
        if expected.version.is_empty()
            || current.version != expected.version
            || current.trashed
            || current.unavailable
        {
            return Err(Error::Precondition);
        }
        // A weak or absent validator cannot authorize a destructive mutation.
        tag.filter(|value| value.starts_with('"') && value.ends_with('"') && value.len() > 2)
            .ok_or_else(|| {
                Error::Unsupported("Drive returned no strong ETag; mutation refused".into())
            })
    }

    /// Inherited Trash must not hide a child introduced during parent Trash.
    /// Query without `trashed = false`: inherited children otherwise disappear.
    async fn folder_has_nonexplicit_child(&self, parent: &str) -> Result<bool> {
        let parent = parent.replace('\\', "\\\\").replace('\'', "\\'");
        let q = format!("'{parent}' in parents");
        let mut token: Option<String> = None;
        let mut seen = std::collections::HashSet::new();
        loop {
            let mut request = self.http.get(self.endpoint("files", None)?).query(&[
                ("q", q.as_str()),
                ("pageSize", "1000"),
                ("fields", "nextPageToken,files(id,explicitlyTrashed)"),
            ]);
            if let Some(value) = &token {
                request = request.query(&[("pageToken", value)]);
            }
            let page: serde_json::Value = Self::check(self.auth(request).await?.send().await?)
                .await?
                .json()
                .await?;
            let files = page["files"]
                .as_array()
                .ok_or_else(|| Error::Invalid("missing folder membership list".into()))?;
            if files
                .iter()
                .any(|file| file["explicitlyTrashed"].as_bool() != Some(true))
            {
                return Ok(true);
            }
            match page.get("nextPageToken") {
                Some(serde_json::Value::String(next)) if seen.insert(next.clone()) => {
                    token = Some(next.clone())
                }
                Some(serde_json::Value::String(_)) => {
                    return Err(Error::Invalid("repeated folder listing cursor".into()))
                }
                None | Some(serde_json::Value::Null) => return Ok(false),
                _ => return Err(Error::Invalid("invalid folder membership cursor".into())),
            }
        }
    }

    async fn restore_folder_after_membership_race(&self, id: &str) -> Result<()> {
        let (current, tag) = self.get_mutation_remote(id).await?;
        if !current.folder() || current.unavailable {
            return Err(Error::Precondition);
        }
        if !current.trashed {
            return Ok(());
        }
        let tag = tag
            .filter(|value| value.starts_with('"') && value.ends_with('"') && value.len() > 2)
            .ok_or_else(|| {
                Error::Unsupported("Drive returned no strong ETag; restore refused".into())
            })?;
        let response = self
            .auth(
                self.http
                    .patch(self.mutation_endpoint(false, id)?)
                    .header(reqwest::header::IF_MATCH, tag)
                    .json(&serde_json::json!({"labels":{"trashed":false}})),
            )
            .await?
            .send()
            .await?;
        Self::check(response).await?;
        let restored = self.get(id).await?.ok_or(Error::Precondition)?;
        if restored.trashed || !restored.folder() {
            return Err(Error::Precondition);
        }
        Ok(())
    }

    async fn check(response: Response) -> Result<Response> {
        let status = response.status().as_u16();
        if response.status().is_success() {
            return Ok(response);
        }
        if status == 401 {
            return Err(Error::AuthRequired);
        }
        if status == 412 {
            return Err(Error::Precondition);
        }
        let body: serde_json::Value = response.json().await.unwrap_or_default();
        let raw = body
            .pointer("/error/errors/0/reason")
            .and_then(|v| v.as_str())
            .unwrap_or("requestFailed");
        let reason = match raw {
            "rateLimitExceeded"
            | "userRateLimitExceeded"
            | "invalidPageToken"
            | "pageTokenExpired"
            | "invalid"
            | "storageQuotaExceeded"
            | "insufficientFilePermissions"
            | "notFound" => raw,
            _ => "requestFailed",
        }
        .to_string();
        Err(Error::Http { status, reason })
    }
}

#[async_trait]
impl DriveClient for HttpDriveClient {
    async fn create_folder(&self, id: &str, name: &str, parent: &str) -> Result<RemoteFile> {
        if let Some(existing) = self.get(id).await? {
            return if existing.folder()
                && !existing.trashed
                && existing.name == name
                && existing.parents == [parent]
            {
                Ok(existing)
            } else {
                Err(Error::Precondition)
            };
        }
        let response = self.auth(self.http.post(self.endpoint("files",None)?).query(&[("fields",FILE_FIELDS)])
            .json(&serde_json::json!({"id":id,"name":name,"mimeType":"application/vnd.google-apps.folder","parents":[parent]}))).await?.send().await?;
        Ok(Self::check(response).await?.json().await?)
    }
    fn conditional_mutations(&self) -> bool {
        // Verified against live Drive v2: stale rename and media writes return 412.
        true
    }

    async fn update(&self, expected: &RemoteFile, snapshot: &Snapshot) -> Result<RemoteFile> {
        let (current, tag) = self.get_mutation_remote(&expected.id).await?;
        // Replaying a journal after an ambiguous committed response must not
        // upload twice or reject the already completed operation.
        if !current.trashed
            && !current.unavailable
            && current.matches(&snapshot.digest) == Some(true)
        {
            return Ok(current);
        }
        if current.folder() || current.workspace() {
            return Err(Error::Unsupported(
                "native Drive documents cannot be replaced with file bytes".into(),
            ));
        }
        let tag = Self::mutation_tag(expected, &current, tag)?;
        let mut url = self.mutation_endpoint(true, &expected.id)?;
        url.query_pairs_mut().append_pair("uploadType", "media");
        let file = tokio::fs::File::open(&snapshot.path).await?;
        // One conditional request guards the actual content commit. Checking an
        // ETag only when initializing a resumable session is insufficient.
        let response = self
            .auth(
                self.http
                    .put(url)
                    .header(reqwest::header::IF_MATCH, tag)
                    .header(
                        reqwest::header::CONTENT_TYPE,
                        if current.mime_type.is_empty() {
                            "application/octet-stream"
                        } else {
                            &current.mime_type
                        },
                    )
                    .header(reqwest::header::CONTENT_LENGTH, snapshot.digest.size)
                    .body(reqwest::Body::wrap_stream(ReaderStream::new(file))),
            )
            .await?
            .send()
            .await?;
        Self::check(response).await?;
        let updated = self.get(&expected.id).await?.ok_or(Error::Precondition)?;
        if updated.id != expected.id || updated.matches(&snapshot.digest) != Some(true) {
            return Err(Error::Precondition);
        }
        Ok(updated)
    }

    async fn trash(&self, expected: &RemoteFile) -> Result<RemoteFile> {
        let (current, tag) = self.get_mutation_remote(&expected.id).await?;
        if current.trashed {
            return Ok(current);
        }
        let tag = Self::mutation_tag(expected, &current, tag)?;
        let response = self
            .auth(
                self.http
                    .patch(self.mutation_endpoint(false, &expected.id)?)
                    .header(reqwest::header::IF_MATCH, tag)
                    .json(&serde_json::json!({"labels":{"trashed":true}})),
            )
            .await?
            .send()
            .await?;
        Self::check(response).await?;
        let updated = self.get(&expected.id).await?.ok_or(Error::Precondition)?;
        if updated.id != expected.id || !updated.trashed {
            return Err(Error::Precondition);
        }
        Ok(updated)
    }

    async fn trash_empty_folder(&self, expected: &RemoteFile) -> Result<RemoteFile> {
        if !expected.folder()
            || (!expected.trashed && !self.list_children(&expected.id).await?.is_empty())
        {
            return Err(Error::Precondition);
        }
        let trashed = self.trash(expected).await?;
        // Also runs when Trash already committed before a process crash.
        // Parent ETags do not change when children are added, so an ETag alone
        // cannot guard membership. Preserve the journal until this postcheck.
        if self.folder_has_nonexplicit_child(&expected.id).await? {
            self.restore_folder_after_membership_race(&expected.id)
                .await?;
            return Err(Error::Precondition);
        }
        Ok(trashed)
    }

    async fn rename(&self, expected: &RemoteFile, name: &str, parent: &str) -> Result<RemoteFile> {
        let (current, tag) = self.get_mutation_remote(&expected.id).await?;
        if !current.trashed
            && !current.unavailable
            && current.name == name
            && current.parents == [parent]
        {
            return Ok(current);
        }
        let tag = Self::mutation_tag(expected, &current, tag)?;
        let mut request = self
            .http
            .patch(self.mutation_endpoint(false, &expected.id)?)
            .header(reqwest::header::IF_MATCH, tag)
            .json(&serde_json::json!({"title":name}));
        if current.parents != [parent] {
            request = request.query(&[("addParents", parent)]);
            let removed: Vec<&str> = current
                .parents
                .iter()
                .map(String::as_str)
                .filter(|existing| *existing != parent)
                .collect();
            if !removed.is_empty() {
                request = request.query(&[("removeParents", removed.join(","))]);
            }
        }
        let response = self.auth(request).await?.send().await?;
        Self::check(response).await?;
        let updated = self.get(&expected.id).await?.ok_or(Error::Precondition)?;
        if updated.id != expected.id || updated.name != name || updated.parents != [parent] {
            return Err(Error::Precondition);
        }
        Ok(updated)
    }

    async fn start_page_token(&self) -> Result<String> {
        let response = self
            .auth(
                self.http
                    .get(self.endpoint("changes", Some("startPageToken"))?),
            )
            .await?
            .send()
            .await?;
        let body: serde_json::Value = Self::check(response).await?.json().await?;
        body["startPageToken"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| Error::Invalid("missing startPageToken".into()))
    }
    async fn list_files(&self, token: Option<&str>) -> Result<FilePage> {
        let fields = format!("nextPageToken,files({FILE_FIELDS})");
        let mut request = self.http.get(self.endpoint("files", None)?).query(&[
            ("spaces", "drive"),
            ("q", "trashed = false"),
            ("pageSize", "1000"),
            ("fields", fields.as_str()),
        ]);
        if let Some(token) = token {
            request = request.query(&[("pageToken", token)]);
        }
        Ok(Self::check(self.auth(request).await?.send().await?)
            .await?
            .json()
            .await?)
    }
    async fn changes(&self, token: &str) -> Result<ChangePage> {
        let fields =
            format!("nextPageToken,newStartPageToken,changes(fileId,removed,file({FILE_FIELDS}))");
        let response = self
            .auth(self.http.get(self.endpoint("changes", None)?).query(&[
                ("pageToken", token),
                ("spaces", "drive"),
                ("includeRemoved", "true"),
                ("pageSize", "1000"),
                ("fields", fields.as_str()),
            ]))
            .await?
            .send()
            .await?;
        Ok(Self::check(response).await?.json().await?)
    }
    async fn get(&self, id: &str) -> Result<Option<RemoteFile>> {
        let response = self
            .auth(
                self.http
                    .get(self.endpoint("files", Some(id))?)
                    .query(&[("fields", FILE_FIELDS)]),
            )
            .await?
            .send()
            .await?;
        if response.status().as_u16() == 404 {
            return Ok(None);
        }
        Ok(Some(Self::check(response).await?.json().await?))
    }
    async fn list_children(&self, parent: &str) -> Result<Vec<RemoteFile>> {
        // Drive query string values require escaping independently of URL encoding.
        let parent = parent.replace('\\', "\\\\").replace('\'', "\\'");
        let q = format!("'{parent}' in parents and trashed = false");
        let fields = format!("nextPageToken,files({FILE_FIELDS})");
        let mut token: Option<String> = None;
        let mut seen = std::collections::HashSet::new();
        let mut files = Vec::new();
        loop {
            let mut request = self.http.get(self.endpoint("files", None)?).query(&[
                ("q", q.as_str()),
                ("pageSize", "1000"),
                ("fields", fields.as_str()),
            ]);
            if let Some(value) = &token {
                request = request.query(&[("pageToken", value)]);
            }
            let page: FilePage = Self::check(self.auth(request).await?.send().await?)
                .await?
                .json()
                .await?;
            files.extend(page.files);
            match page.next_page_token {
                Some(next) if seen.insert(next.clone()) => token = Some(next),
                Some(_) => return Err(Error::Invalid("repeated folder listing cursor".into())),
                None => break,
            }
        }
        Ok(files)
    }
    async fn download(&self, id: &str) -> Result<ByteStream> {
        let response = self
            .auth(
                self.http
                    .get(self.endpoint("files", Some(id))?)
                    .query(&[("alt", "media")]),
            )
            .await?
            .send()
            .await?;
        let response = Self::check(response).await?;
        Ok(Box::pin(
            response.bytes_stream().map(|r| r.map_err(Error::from)),
        ))
    }
    async fn reserve_id(&self) -> Result<String> {
        let response = self
            .auth(
                self.http
                    .get(self.endpoint("files", Some("generateIds"))?)
                    .query(&[("count", "1"), ("space", "drive"), ("type", "files")]),
            )
            .await?
            .send()
            .await?;
        let body: serde_json::Value = Self::check(response).await?.json().await?;
        body["ids"][0]
            .as_str()
            .map(String::from)
            .ok_or_else(|| Error::Invalid("missing generated ID".into()))
    }
    async fn create(
        &self,
        id: &str,
        name: &str,
        parent: &str,
        snapshot: &Snapshot,
    ) -> Result<RemoteFile> {
        // A prior timeout may have occurred after the server committed this fixed ID.
        if let Some(existing) = self.get(id).await? {
            if !existing.trashed
                && !existing.unavailable
                && existing.name == name
                && existing.parents == [parent]
                && existing.matches(&snapshot.digest) == Some(true)
            {
                return Ok(existing);
            }
            return Err(Error::Precondition);
        }
        let mut url = self
            .upload_base
            .join("files")
            .map_err(|_| Error::Invalid("upload endpoint".into()))?;
        url.query_pairs_mut()
            .append_pair("uploadType", "resumable")
            .append_pair("fields", FILE_FIELDS);
        let response = self
            .auth(
                self.http
                    .post(url)
                    .header("X-Upload-Content-Type", "application/octet-stream")
                    .header("X-Upload-Content-Length", snapshot.digest.size)
                    .json(&serde_json::json!({"id":id,"name":name,"parents":[parent]})),
            )
            .await?
            .send()
            .await?;
        let response = Self::check(response).await?;
        let location = response
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| Error::Invalid("missing resumable Location".into()))?;
        let location =
            Url::parse(location).map_err(|_| Error::Invalid("upload Location".into()))?;
        if location.origin() != self.upload_base.origin() {
            return Err(Error::Invalid("unexpected upload origin".into()));
        }
        let file = tokio::fs::File::open(&snapshot.path).await?;
        let body = reqwest::Body::wrap_stream(ReaderStream::new(file));
        let response = self
            .auth(
                self.http
                    .put(location)
                    .header("Content-Type", "application/octet-stream")
                    .header("Content-Length", snapshot.digest.size)
                    .body(body),
            )
            .await?
            .send()
            .await?;
        Ok(Self::check(response).await?.json().await?)
    }
}

pub fn file_name(path: &Path) -> Result<&str> {
    path.file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| Error::Invalid("invalid file name".into()))
}
