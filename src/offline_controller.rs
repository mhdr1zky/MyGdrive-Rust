//! Offline browser worker: no GTK-thread disk, hashing, network or SQLite writes.
use crate::{
    db::Database,
    offline::{Graph, Page, Request as OfflineRequest},
    service::{self, Action, Request},
    Error, Result,
};
use std::{
    path::{Path, PathBuf},
    sync::mpsc,
    time::Duration,
};
use tokio_util::sync::CancellationToken;
pub enum Command {
    List {
        parent: String,
        offset: usize,
    },
    Set {
        target: String,
        pinned: bool,
        parent: String,
        offset: usize,
    },
}
pub enum Event {
    Page {
        page: Box<Page>,
        live: bool,
        revision: u64,
    },
    Error {
        message: String,
        revision: u64,
    },
}
pub struct Job {
    pub command: Command,
    pub revision: u64,
}
pub struct Controller {
    pub commands: tokio::sync::mpsc::Sender<Job>,
    pub events: mpsc::Receiver<Event>,
    stop: CancellationToken,
}
impl Controller {
    pub fn stop(&self) {
        self.stop.cancel();
    }
}
impl Drop for Controller {
    fn drop(&mut self) {
        self.stop();
    }
}
pub async fn browse(state: &Path, parent: &str, offset: usize) -> Result<(Page, bool)> {
    if let Ok(status) = service::send(state, &Request::new(Action::Status)).await {
        if status.accepted {
            let mut request = Request::new(Action::OfflineList);
            request.session_id = Some(status.status.session_id);
            request.offline = Some(OfflineRequest::List {
                parent: parent.into(),
                offset,
            });
            let reply = service::send(state, &request).await?;
            if reply.accepted {
                return Ok((
                    reply.offline.ok_or_else(|| {
                        Error::Invalid("Restart service untuk mengaktifkan browser offline.".into())
                    })?,
                    true,
                ));
            }
            return Err(Error::Invalid(
                "Folder berubah atau service perlu dimulai ulang. Muat ulang inventaris.".into(),
            ));
        }
    }
    let config = service::load_config(state)?;
    let db = Database::read_only(&state.join("state.sqlite3"), &config.account).await?;
    let result = Graph::load(&db, true).await?.page(parent, offset);
    db.pool.close().await;
    Ok((result?, false))
}
pub async fn select(state: &Path, target: &str, pinned: bool) -> Result<()> {
    let status = service::send(state, &Request::new(Action::Status)).await?;
    if !status.accepted {
        return Err(Error::Invalid("Service belum tersedia.".into()));
    }
    let mut request = Request::new(Action::OfflineSet);
    request.session_id = Some(status.status.session_id);
    request.offline = Some(OfflineRequest::Set {
        target: target.into(),
        pinned,
    });
    let reply = match service::send(state, &request).await {
        Ok(r) => r,
        Err(_) => service::send(state, &request).await?,
    };
    if !reply.accepted {
        return Err(Error::Invalid(
            "Pilihan belum terkonfirmasi. Muat ulang folder sebelum mencoba lagi.".into(),
        ));
    }
    Ok(())
}
#[derive(Default)]
struct Observer {
    db: Option<Database>,
    stamp: Option<(i64, Option<String>)>,
}
impl Observer {
    async fn changed(&mut self, state: &Path) -> bool {
        let session = service::send(state, &Request::new(Action::Status))
            .await
            .ok()
            .filter(|r| r.accepted)
            .map(|r| r.status.session_id);
        if self.db.is_none() {
            if let Ok(config) = service::load_config(state) {
                self.db = Database::read_only(&state.join("state.sqlite3"), &config.account)
                    .await
                    .ok();
            }
        }
        let version = match &self.db {
            Some(db) => sqlx::query_scalar::<_, i64>("PRAGMA data_version")
                .fetch_one(&db.pool)
                .await
                .ok(),
            None => None,
        };
        let Some(version) = version else {
            self.stamp = None;
            return true;
        };
        let stamp = (version, session);
        let changed = self.stamp.as_ref() != Some(&stamp);
        self.stamp = Some(stamp);
        changed
    }
}
async fn run_worker(
    state: PathBuf,
    mut input: tokio::sync::mpsc::Receiver<Job>,
    events: mpsc::Sender<Event>,
    cancel: CancellationToken,
) {
    let mut revision = 0;
    let mut parent = "root".to_string();
    let mut offset = 0;
    let mut observer = Observer::default();
    let mut poll = tokio::time::interval(Duration::from_secs(5));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let command = tokio::select! {_ = cancel.cancelled()=>break,c=input.recv()=>match c{Some(Job{command,revision:r})=>{revision=r;Some(command)},None=>break},_ = poll.tick()=>None};
        let changed = tokio::select! {_ = cancel.cancelled()=>break,v=observer.changed(&state)=>v};
        // This persistent read-only connection observes commits by the engine's
        // separate connection. Idle inventory browsing must not repeatedly parse
        // the entire subtree or stat all pinned files while nothing has changed.
        if command.is_none() && !changed {
            continue;
        }
        let result = tokio::select! {_ = cancel.cancelled()=>break,r=execute(&state,command,&mut parent,&mut offset)=>r};
        if result.is_err() {
            observer.stamp = None;
        }
        let event=match result {
            Ok((page,live))=>Event::Page{page:Box::new(page),live,revision},
            Err(Error::Invalid(message))=>Event::Error{message,revision},
            Err(_)=>Event::Error{message:"Inventaris belum dapat dibaca. Mulai atau perbarui service lalu muat ulang; pilihan yang belum terkonfirmasi perlu diperiksa kembali.".into(),revision},
        };
        if events.send(event).is_err() {
            break;
        }
    }
    if let Some(db) = observer.db {
        db.pool.close().await;
    }
}
pub fn spawn(state: PathBuf) -> Result<Controller> {
    let (commands, input) = tokio::sync::mpsc::channel(8);
    let (events, receiver) = mpsc::channel();
    let stop = CancellationToken::new();
    let cancel = stop.clone();
    std::thread::Builder::new()
        .name("mygdrive-offline".into())
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                let _ = events.send(Event::Error {
                    message: "Worker offline tidak dapat dimulai.".into(),
                    revision: 0,
                });
                return;
            };
            runtime.block_on(run_worker(state, input, events, cancel));
        })?;
    Ok(Controller {
        commands,
        events: receiver,
        stop,
    })
}
async fn execute(
    state: &Path,
    command: Option<Command>,
    parent: &mut String,
    offset: &mut usize,
) -> Result<(Page, bool)> {
    match command {
        Some(Command::List {
            parent: p,
            offset: o,
        }) => {
            *parent = p;
            *offset = o;
        }
        Some(Command::Set {
            target,
            pinned,
            parent: p,
            offset: o,
        }) => {
            select(state, &target, pinned).await?;
            *parent = p;
            *offset = o;
        }
        None => {}
    }
    browse(state, parent, *offset).await
}
