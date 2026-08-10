//! ProjectsSync — owner-side upkeep of project rows (git presence) plus the
//! orphan-chat repair sweep.
//!
//! A project is a synced (device, folder) pair; the folder need NOT be a git
//! repo. This service watches the workspace `projects` rows owned by THIS device
//! and keeps their `gitDetected`/`checkoutId` stamps truthful:
//!
//! - recheck on boot / when a project row is first observed;
//! - a non-recursive `notify` watcher on the project folder — `.git` appearing or
//!   vanishing (git init / de-git) kicks a recheck;
//! - a slow 2-minute repair tick (native watchers coalesce/drop events).
//!
//! Stamps are written ONLY on change, so steady state never grows the oplog.
//! Remote devices read `project.git_detected` straight from the doc — branch
//! pickers and the diff sidebar gate on it with zero RPCs.
//!
//! The repair tick also runs the orphan sweep: a chat created concurrently
//! with a `deleteProject` on another device can sync in after the cascade ran,
//! leaving a dangling `projectId`. The HOST device deletes its own such chats
//! (writer discipline — we never touch other devices' rows).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use tokio::sync::{mpsc, watch};

use nova_proto::Project;

use crate::repos::Repos;
use crate::workspace_host::WorkspaceHost;

/// Trailing debounce after a filesystem event burst.
const WATCH_DEBOUNCE: Duration = Duration::from_millis(500);
/// Slow repair pass: recheck every owned project + orphan sweep.
const REPAIR_INTERVAL: Duration = Duration::from_secs(120);

struct ProjectEntry {
    path: PathBuf,
    kick_tx: mpsc::UnboundedSender<()>,
    /// Keeps the folder watcher alive; dropped on entry close.
    _watcher: Option<notify::RecommendedWatcher>,
}

struct ProjectsSyncInner {
    repos: Repos,
    workspace: WorkspaceHost,
    device_id: String,
    entries: Mutex<HashMap<String, Arc<ProjectEntry>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
pub struct ProjectsSync {
    inner: Arc<ProjectsSyncInner>,
}

impl ProjectsSync {
    /// Build and start the sync loop: follows the workspace projects watch and
    /// runs the repair tick. Requires a tokio runtime.
    pub fn start(repos: Repos, workspace: WorkspaceHost, device_id: &str) -> Self {
        let sync = Self {
            inner: Arc::new(ProjectsSyncInner {
                repos,
                workspace: workspace.clone(),
                device_id: device_id.to_string(),
                entries: Mutex::new(HashMap::new()),
            }),
        };
        tokio::spawn(projects_task(
            Arc::downgrade(&sync.inner),
            workspace.watch_projects(),
        ));
        sync
    }

    /// Reconcile + recheck now (tests / opportunistic callers).
    pub async fn reconcile_now(&self) {
        let projects = self.inner.workspace.watch_projects().borrow().clone();
        reconcile(&self.inner, &projects);
        for entry in lock(&self.inner.entries).values() {
            let _ = entry.kick_tx.send(());
        }
    }
}

/// (Re)build the entry set for the projects THIS device owns.
fn reconcile(inner: &Arc<ProjectsSyncInner>, projects: &[Project]) {
    let owned: HashMap<&str, &Project> = projects
        .iter()
        .filter(|s| s.device_id == inner.device_id)
        .map(|s| (s.id.as_str(), s))
        .collect();

    let mut entries = lock(&inner.entries);
    entries.retain(|id, _| owned.contains_key(id.as_str()));
    for (id, project) in owned {
        if entries.contains_key(id) {
            continue; // deviceId/path are immutable — nothing to refresh
        }
        let (kick_tx, kick_rx) = mpsc::unbounded_channel();
        // Non-recursive watcher on the project folder: `.git` appearing/vanishing
        // among the direct children is exactly the signal we need. Watch
        // failures are fine — the repair tick still converges.
        let watcher = {
            let tx = kick_tx.clone();
            let result =
                notify::recommended_watcher(move |event: Result<notify::Event, notify::Error>| {
                    let Ok(event) = event else { return };
                    if event
                        .paths
                        .iter()
                        .any(|p| p.file_name().is_some_and(|n| n == ".git"))
                    {
                        let _ = tx.send(());
                    }
                });
            match result {
                Ok(mut watcher) => {
                    use notify::Watcher as _;
                    match watcher.watch(
                        Path::new(&project.path),
                        notify::RecursiveMode::NonRecursive,
                    ) {
                        Ok(()) => Some(watcher),
                        Err(err) => {
                            tracing::debug!(path = %project.path, error = %err, "projects: watch failed");
                            None
                        }
                    }
                }
                Err(err) => {
                    tracing::debug!(error = %err, "projects: watcher create failed");
                    None
                }
            }
        };
        let entry = Arc::new(ProjectEntry {
            path: PathBuf::from(&project.path),
            kick_tx: kick_tx.clone(),
            _watcher: watcher,
        });
        entries.insert(id.to_string(), entry.clone());
        tokio::spawn(entry_task(
            Arc::downgrade(inner),
            id.to_string(),
            Arc::downgrade(&entry),
            kick_rx,
        ));
        let _ = kick_tx.send(()); // initial check (boot / first observed)
    }
}

/// Per-project task: trailing-debounce kicks, then recheck git presence.
async fn entry_task(
    inner: Weak<ProjectsSyncInner>,
    project_id: String,
    entry: Weak<ProjectEntry>,
    mut kick_rx: mpsc::UnboundedReceiver<()>,
) {
    while kick_rx.recv().await.is_some() {
        loop {
            match tokio::time::timeout(WATCH_DEBOUNCE, kick_rx.recv()).await {
                Ok(Some(())) => continue,
                Ok(None) => return, // entry closed mid-burst
                Err(_) => break,
            }
        }
        let (Some(inner), Some(entry)) = (inner.upgrade(), entry.upgrade()) else {
            return;
        };
        check_project(&inner, &project_id, &entry.path).await;
    }
}

/// Probe git presence and stamp the row — write only on change.
async fn check_project(inner: &Arc<ProjectsSyncInner>, project_id: &str, path: &Path) {
    let detected = inner.repos.is_repo(path).await;
    let checkout_id = if detected {
        match inner.repos.checkout_identity(path).await {
            Ok(identity) => Some(identity.id),
            Err(err) => {
                tracing::debug!(project = %project_id, error = %err, "projects: checkout identity failed");
                None
            }
        }
    } else {
        None
    };
    let current = match inner.workspace.read_projects() {
        Ok(projects) => projects.into_iter().find(|s| s.id == project_id),
        Err(err) => {
            tracing::warn!(project = %project_id, error = %err, "projects: row read failed");
            return;
        }
    };
    let Some(current) = current else {
        return; // deleted while checking
    };
    if current.git_detected == detected && current.checkout_id == checkout_id {
        return; // unchanged — no oplog growth
    }
    match inner
        .workspace
        .set_project_git(project_id, detected, checkout_id.as_deref())
    {
        Ok(_) => {
            tracing::info!(project = %project_id, git = detected, "project git presence updated");
        }
        Err(err) => {
            tracing::warn!(project = %project_id, error = %err, "projects: git stamp failed");
        }
    }
}

/// Host-side repair: delete OUR chats whose `projectId` dangles (create-vs-delete
/// race). Chats hosted by other devices are left alone.
fn sweep_orphans(inner: &Arc<ProjectsSyncInner>) {
    let projects = inner.workspace.watch_projects().borrow().clone();
    let live: std::collections::HashSet<&str> = projects.iter().map(|s| s.id.as_str()).collect();
    let chats = inner.workspace.watch_chats().borrow().clone();
    for chat in chats {
        if chat.device_id != inner.device_id {
            continue;
        }
        let Some(project_id) = chat.project_id.as_deref() else {
            continue;
        };
        if live.contains(project_id) {
            continue;
        }
        tracing::info!(chat = %chat.id, project = %project_id, "deleting orphaned chat (project gone)");
        if let Err(err) = inner.workspace.delete_chat(&chat.id) {
            tracing::warn!(chat = %chat.id, error = %err, "projects: orphan delete failed");
        }
    }
}

/// Projects-watch follower + repair tick. Weak handles so dropping the service
/// tears the loop down.
async fn projects_task(
    inner: Weak<ProjectsSyncInner>,
    mut projects_rx: watch::Receiver<Vec<Project>>,
) {
    let mut repair = tokio::time::interval(REPAIR_INTERVAL);
    repair.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    repair.tick().await; // consume the immediate first tick
    {
        let Some(inner) = inner.upgrade() else { return };
        let projects = projects_rx.borrow().clone();
        reconcile(&inner, &projects);
    }
    loop {
        tokio::select! {
            changed = projects_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                let Some(inner) = inner.upgrade() else { break };
                let projects = projects_rx.borrow_and_update().clone();
                reconcile(&inner, &projects);
            }
            _ = repair.tick() => {
                let Some(inner) = inner.upgrade() else { break };
                let projects = projects_rx.borrow().clone();
                reconcile(&inner, &projects);
                for entry in lock(&inner.entries).values() {
                    let _ = entry.kick_tx.send(());
                }
                sweep_orphans(&inner);
            }
        }
    }
}
