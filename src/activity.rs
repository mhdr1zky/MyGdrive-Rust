//! Track actual Drive transfers/mutations, excluding routine metadata polling.
use crate::{
    drive::{ByteStream, DriveClient},
    model::*,
    Result,
};
use async_trait::async_trait;
use futures_util::StreamExt;
use std::sync::{
    atomic::{AtomicI64, AtomicU64, Ordering},
    Arc,
};

#[derive(Default)]
pub(crate) struct Activity {
    active: AtomicU64,
    last_ms: AtomicI64,
}
impl Activity {
    pub(crate) fn snapshot(&self) -> (u64, i64) {
        (
            self.active.load(Ordering::Acquire),
            self.last_ms.load(Ordering::Acquire),
        )
    }
    fn begin(self: &Arc<Self>) -> Guard {
        self.active.fetch_add(1, Ordering::AcqRel);
        self.last_ms.store(crate::now_ms(), Ordering::Release);
        Guard(self.clone())
    }
}
struct Guard(Arc<Activity>);
impl Drop for Guard {
    fn drop(&mut self) {
        self.0.last_ms.store(crate::now_ms(), Ordering::Release);
        self.0.active.fetch_sub(1, Ordering::AcqRel);
    }
}
pub(crate) struct TrackedDrive {
    pub inner: Arc<dyn DriveClient>,
    pub activity: Arc<Activity>,
}
#[async_trait]
impl DriveClient for TrackedDrive {
    async fn start_page_token(&self) -> Result<String> {
        self.inner.start_page_token().await
    }
    async fn list_files(&self, token: Option<&str>) -> Result<FilePage> {
        self.inner.list_files(token).await
    }
    async fn changes(&self, token: &str) -> Result<ChangePage> {
        self.inner.changes(token).await
    }
    async fn get(&self, id: &str) -> Result<Option<RemoteFile>> {
        self.inner.get(id).await
    }
    async fn list_children(&self, parent: &str) -> Result<Vec<RemoteFile>> {
        self.inner.list_children(parent).await
    }
    async fn reserve_id(&self) -> Result<String> {
        self.inner.reserve_id().await
    }
    fn conditional_mutations(&self) -> bool {
        self.inner.conditional_mutations()
    }
    async fn download(&self, id: &str) -> Result<ByteStream> {
        let guard = self.activity.begin();
        let stream = self.inner.download(id).await?;
        // Keep activity alive through body consumption, including cancellation/error paths.
        Ok(Box::pin(futures_util::stream::unfold(
            (stream, guard),
            |(mut stream, guard)| async move {
                stream.next().await.map(|chunk| (chunk, (stream, guard)))
            },
        )))
    }
    async fn create(
        &self,
        id: &str,
        name: &str,
        parent: &str,
        snapshot: &Snapshot,
    ) -> Result<RemoteFile> {
        let _activity = self.activity.begin();
        self.inner.create(id, name, parent, snapshot).await
    }
    async fn create_folder(&self, id: &str, name: &str, parent: &str) -> Result<RemoteFile> {
        let _activity = self.activity.begin();
        self.inner.create_folder(id, name, parent).await
    }
    async fn update(&self, remote: &RemoteFile, snapshot: &Snapshot) -> Result<RemoteFile> {
        let _activity = self.activity.begin();
        self.inner.update(remote, snapshot).await
    }
    async fn trash(&self, remote: &RemoteFile) -> Result<RemoteFile> {
        let _activity = self.activity.begin();
        self.inner.trash(remote).await
    }
    async fn trash_empty_folder(&self, remote: &RemoteFile) -> Result<RemoteFile> {
        let _activity = self.activity.begin();
        self.inner.trash_empty_folder(remote).await
    }
    async fn rename(&self, remote: &RemoteFile, name: &str, parent: &str) -> Result<RemoteFile> {
        let _activity = self.activity.begin();
        self.inner.rename(remote, name, parent).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fake;
    #[async_trait]
    impl DriveClient for Fake {
        async fn start_page_token(&self) -> Result<String> {
            Ok("token".into())
        }
        async fn list_files(&self, _: Option<&str>) -> Result<FilePage> {
            unreachable!()
        }
        async fn changes(&self, _: &str) -> Result<ChangePage> {
            unreachable!()
        }
        async fn get(&self, _: &str) -> Result<Option<RemoteFile>> {
            Ok(None)
        }
        async fn reserve_id(&self) -> Result<String> {
            Ok("reserved".into())
        }
        async fn download(&self, id: &str) -> Result<ByteStream> {
            if id == "error" {
                return Err(crate::Error::Changed);
            }
            Ok(Box::pin(futures_util::stream::iter([Ok(
                bytes::Bytes::from_static(b"content"),
            )])))
        }
        async fn create(&self, _: &str, _: &str, _: &str, _: &Snapshot) -> Result<RemoteFile> {
            Err(crate::Error::Changed)
        }
    }
    #[tokio::test]
    async fn polling_stays_idle_and_download_releases_activity_on_eof_error_or_cancel() {
        let activity = Arc::new(Activity::default());
        let drive = TrackedDrive {
            inner: Arc::new(Fake),
            activity: activity.clone(),
        };
        drive.start_page_token().await.unwrap();
        drive.get("file").await.unwrap();
        assert_eq!(
            activity.snapshot(),
            (0, 0),
            "metadata checks must not trigger badge"
        );
        let mut stream = drive.download("file").await.unwrap();
        assert_eq!(activity.snapshot().0, 1);
        assert!(stream.next().await.unwrap().is_ok());
        assert_eq!(activity.snapshot().0, 1, "count the whole response body");
        assert!(stream.next().await.is_none());
        assert_eq!(activity.snapshot().0, 0);
        let stream = drive.download("file").await.unwrap();
        assert_eq!(activity.snapshot().0, 1);
        drop(stream);
        assert_eq!(
            activity.snapshot().0,
            0,
            "cancelled body must not leave badge stuck"
        );
        assert!(drive.download("error").await.is_err());
        assert_eq!(activity.snapshot().0, 0);
        assert!(drive.create_folder("id", "name", "root").await.is_err());
        assert_eq!(
            activity.snapshot().0,
            0,
            "failed mutation must not leave badge stuck"
        );
    }
}
