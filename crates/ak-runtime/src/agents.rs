//! Agent definition files on disk, reconciliation of the store with them,
//! and a watcher that reports changes.
//!
//! Layout under a project's home directory:
//!
//! ```text
//! agents/<handle>/agent.md    definition, Claude Code subagent format
//! agents/<handle>/memory.md   memory
//! ```
//!
//! The directory name is the handle. The frontmatter `name` must match it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Utc;
use notify_debouncer_mini::notify::RecursiveMode;
use notify_debouncer_mini::{new_debouncer, DebounceEventResult, Debouncer};
use tokio::sync::mpsc;

use ak_core::agent::{Agent, AgentDefinition, DEFINITION_FILE, MEMORY_FILE};
use ak_core::ids::ActorId;
use ak_core::{Actor, ActorKind};
use ak_store::Store;

use crate::error::RuntimeError;

#[derive(Debug, Clone)]
pub struct AgentFiles {
    dir: PathBuf,
}

impl AgentFiles {
    pub fn new(agents_dir: PathBuf) -> Self {
        Self { dir: agents_dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn agent_dir(&self, handle: &str) -> PathBuf {
        self.dir.join(handle)
    }

    pub fn definition_path(&self, handle: &str) -> PathBuf {
        self.agent_dir(handle).join(DEFINITION_FILE)
    }

    pub fn memory_path(&self, handle: &str) -> PathBuf {
        self.agent_dir(handle).join(MEMORY_FILE)
    }

    /// Handles of every directory that contains a definition file, sorted.
    pub fn handles(&self) -> Result<Vec<String>, RuntimeError> {
        let mut out = Vec::new();
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(RuntimeError::io(&self.dir, e)),
        };
        for entry in entries {
            let entry = entry.map_err(|e| RuntimeError::io(&self.dir, e))?;
            let path = entry.path();
            if path.is_dir() && path.join(DEFINITION_FILE).is_file() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    out.push(name.to_string());
                }
            }
        }
        out.sort();
        Ok(out)
    }

    pub fn read_definition(&self, handle: &str) -> Result<AgentDefinition, RuntimeError> {
        let path = self.definition_path(handle);
        let text = std::fs::read_to_string(&path).map_err(|e| RuntimeError::io(&path, e))?;
        Ok(AgentDefinition::parse(&text)?)
    }

    /// Writes the definition under the directory named by its handle and
    /// makes sure an (empty) memory file exists beside it.
    pub fn write_definition(&self, def: &AgentDefinition) -> Result<PathBuf, RuntimeError> {
        Actor::validate_handle(def.handle())?;
        let dir = self.agent_dir(def.handle());
        std::fs::create_dir_all(&dir).map_err(|e| RuntimeError::io(&dir, e))?;
        let path = self.definition_path(def.handle());
        write_atomic(&path, &def.render())?;
        let memory = self.memory_path(def.handle());
        if !memory.exists() {
            write_atomic(&memory, "")?;
        }
        Ok(path)
    }

    pub fn read_memory(&self, handle: &str) -> Result<String, RuntimeError> {
        let path = self.memory_path(handle);
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(RuntimeError::io(&path, e)),
        }
    }

    pub fn write_memory(&self, handle: &str, text: &str) -> Result<(), RuntimeError> {
        let dir = self.agent_dir(handle);
        std::fs::create_dir_all(&dir).map_err(|e| RuntimeError::io(&dir, e))?;
        write_atomic(&self.memory_path(handle), text)
    }

    /// Removes the agent's directory. Reconciliation then retires the agent.
    pub fn remove(&self, handle: &str) -> Result<(), RuntimeError> {
        let dir = self.agent_dir(handle);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(|e| RuntimeError::io(&dir, e))?;
        }
        Ok(())
    }
}

fn write_atomic(path: &Path, text: &str) -> Result<(), RuntimeError> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text).map_err(|e| RuntimeError::io(&tmp, e))?;
    std::fs::rename(&tmp, path).map_err(|e| RuntimeError::io(path, e))?;
    Ok(())
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReconcileReport {
    pub added: Vec<String>,
    pub updated: Vec<String>,
    pub retired: Vec<String>,
    /// Handle and message for every directory that could not be used.
    pub errors: Vec<(String, String)>,
}

impl ReconcileReport {
    pub fn changed(&self) -> bool {
        !(self.added.is_empty() && self.updated.is_empty() && self.retired.is_empty())
    }
}

/// Aligns the store's agent rows with the files on disk. New directories add
/// agents, edited files refresh cached fields, missing directories retire
/// agents. Runtime state (lifecycle, current run) is never touched.
pub async fn reconcile(
    files: &AgentFiles,
    store: &dyn Store,
) -> Result<ReconcileReport, RuntimeError> {
    let mut report = ReconcileReport::default();
    let on_disk = files.handles()?;
    let existing = store.agents().await?;

    for handle in &on_disk {
        let def = match files.read_definition(handle) {
            Ok(d) => d,
            Err(e) => {
                report.errors.push((handle.clone(), e.to_string()));
                continue;
            }
        };
        if def.handle() != handle {
            report.errors.push((
                handle.clone(),
                format!(
                    "frontmatter name {:?} does not match the directory name",
                    def.handle()
                ),
            ));
            continue;
        }
        let def_path = files.definition_path(handle);
        let mem_path = files.memory_path(handle);

        match existing.iter().find(|a| &a.handle == handle) {
            Some(prev) => {
                let mut agent = Agent::from_definition(prev.actor_id, &def, def_path, mem_path);
                agent.state = prev.state;
                agent.current_run_id = prev.current_run_id;
                agent.cooldown_until = prev.cooldown_until;
                agent.missing_since = None;
                if agent != *prev {
                    store.upsert_agent(&agent).await?;
                    report.updated.push(handle.clone());
                }
            }
            None => {
                let actor_id = match store.actor_by_handle(handle).await? {
                    Some(a) if a.kind == ActorKind::Agent => a.id,
                    Some(a) => {
                        report.errors.push((
                            handle.clone(),
                            format!(
                                "handle is already used by {} @{}",
                                a.kind.as_str(),
                                a.handle
                            ),
                        ));
                        continue;
                    }
                    None => ActorId::new(),
                };
                let agent = Agent::from_definition(actor_id, &def, def_path, mem_path);
                store.upsert_agent(&agent).await?;
                report.added.push(handle.clone());
            }
        }
    }

    for prev in existing
        .iter()
        .filter(|a| !on_disk.contains(&a.handle) && a.missing_since.is_none())
    {
        store
            .set_agent_missing(prev.actor_id, Some(Utc::now()))
            .await?;
        report.retired.push(prev.handle.clone());
    }

    Ok(report)
}

/// Watches an agents directory and sends `()` on the channel after each
/// debounced burst of changes. Dropping the watcher stops it.
pub struct AgentWatcher {
    _debouncer: Debouncer<notify_debouncer_mini::notify::RecommendedWatcher>,
}

/// Like `watch`, but never creates the directory: `None` when it is absent.
pub fn watch_if_exists(
    dir: &Path,
    tx: mpsc::UnboundedSender<()>,
) -> Result<Option<AgentWatcher>, RuntimeError> {
    if !dir.is_dir() {
        return Ok(None);
    }
    watch(dir, tx).map(Some)
}

pub fn watch(dir: &Path, tx: mpsc::UnboundedSender<()>) -> Result<AgentWatcher, RuntimeError> {
    std::fs::create_dir_all(dir).map_err(|e| RuntimeError::io(dir, e))?;
    let mut debouncer = new_debouncer(
        Duration::from_millis(300),
        move |res: DebounceEventResult| {
            if res.is_ok() {
                let _ = tx.send(());
            }
        },
    )
    .map_err(|e| RuntimeError::Watch(e.to_string()))?;
    debouncer
        .watcher()
        .watch(dir, RecursiveMode::Recursive)
        .map_err(|e| RuntimeError::Watch(e.to_string()))?;
    Ok(AgentWatcher {
        _debouncer: debouncer,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ak_core::config::ProjectPolicy;
    use ak_store::{HumanIdentity, SqliteStore};

    async fn store() -> SqliteStore {
        let s = SqliteStore::open_in_memory().await.unwrap();
        s.bootstrap(&ProjectPolicy::new("Demo"), &HumanIdentity::default())
            .await
            .unwrap();
        s
    }

    #[tokio::test]
    async fn reconcile_adds_updates_and_retires() {
        let tmp = tempfile::tempdir().unwrap();
        let files = AgentFiles::new(tmp.path().join("agents"));
        let store = store().await;

        // Empty directory: nothing happens.
        let r = reconcile(&files, &store).await.unwrap();
        assert!(!r.changed());

        files
            .write_definition(&AgentDefinition::template("backend", "Backend developer"))
            .unwrap();
        files
            .write_definition(&AgentDefinition::template("qa", "QA"))
            .unwrap();
        assert!(
            files.memory_path("backend").exists(),
            "memory file is created alongside"
        );

        let r = reconcile(&files, &store).await.unwrap();
        assert_eq!(r.added, vec!["backend", "qa"]);
        let agents = store.agents().await.unwrap();
        assert_eq!(agents.len(), 2);
        let backend_id = agents
            .iter()
            .find(|a| a.handle == "backend")
            .unwrap()
            .actor_id;

        // Edit the file: cached fields refresh, identity is stable.
        let mut def = files.read_definition("backend").unwrap();
        def.frontmatter.description = "Owns the API.".into();
        def.frontmatter.model = Some("opus".into());
        files.write_definition(&def).unwrap();
        let r = reconcile(&files, &store).await.unwrap();
        assert_eq!(r.updated, vec!["backend"]);
        let backend = store.agent(backend_id).await.unwrap();
        assert_eq!(backend.description, "Owns the API.");
        assert_eq!(backend.model.as_deref(), Some("opus"));

        // Unchanged files are not rewritten.
        let r = reconcile(&files, &store).await.unwrap();
        assert!(!r.changed());

        // Remove a directory: the agent is retired, not deleted.
        files.remove("qa").unwrap();
        let r = reconcile(&files, &store).await.unwrap();
        assert_eq!(r.retired, vec!["qa"]);
        let qa = store.agent_by_handle("qa").await.unwrap().unwrap();
        assert!(qa.is_retired());

        // Put it back: same actor id, no longer retired.
        files
            .write_definition(&AgentDefinition::template("qa", "QA"))
            .unwrap();
        let r = reconcile(&files, &store).await.unwrap();
        assert_eq!(r.updated, vec!["qa"]);
        let qa_again = store.agent_by_handle("qa").await.unwrap().unwrap();
        assert_eq!(qa_again.actor_id, qa.actor_id);
        assert!(!qa_again.is_retired());
    }

    #[tokio::test]
    async fn reconcile_reports_bad_files_without_failing() {
        let tmp = tempfile::tempdir().unwrap();
        let files = AgentFiles::new(tmp.path().join("agents"));
        let store = store().await;

        // Directory name and frontmatter name disagree.
        std::fs::create_dir_all(files.agent_dir("ops")).unwrap();
        std::fs::write(files.definition_path("ops"), "---\nname: sre\n---\nbody\n").unwrap();
        // Garbage frontmatter.
        std::fs::create_dir_all(files.agent_dir("broken")).unwrap();
        std::fs::write(files.definition_path("broken"), "no frontmatter here\n").unwrap();
        // Handle taken by the human.
        std::fs::create_dir_all(files.agent_dir("me")).unwrap();
        std::fs::write(files.definition_path("me"), "---\nname: me\n---\nbody\n").unwrap();
        // One good one.
        files
            .write_definition(&AgentDefinition::template("dba", "DBA"))
            .unwrap();

        let r = reconcile(&files, &store).await.unwrap();
        assert_eq!(r.added, vec!["dba"]);
        let bad: Vec<&str> = r.errors.iter().map(|(h, _)| h.as_str()).collect();
        assert_eq!(bad, vec!["broken", "me", "ops"]);
        assert_eq!(store.agents().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn watcher_reports_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("agents");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _watcher = watch(&dir, tx).unwrap();

        let files = AgentFiles::new(dir.clone());
        files
            .write_definition(&AgentDefinition::template("backend", "Backend"))
            .unwrap();

        let got = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        assert!(
            matches!(got, Ok(Some(()))),
            "expected a change notification"
        );
    }
}
