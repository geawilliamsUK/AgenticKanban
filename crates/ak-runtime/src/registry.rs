//! The project registry: the list of repositories this machine manages, and
//! the open handle to each one's store and policy.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use ak_core::config::{HomeConfig, ProjectEntry, ProjectPolicy};
use ak_core::ids::ProjectId;
use ak_core::Actor;
use ak_store::{HumanIdentity, SqliteStore, Store};

use crate::agents::{self, AgentFiles};
use crate::config;
use crate::error::RuntimeError;
use crate::paths::HomePaths;

/// A project whose store is open.
pub struct OpenProject {
    pub entry: ProjectEntry,
    /// Replaced in place when the policy is saved from Settings.
    pub policy: RwLock<ProjectPolicy>,
    pub store: Arc<SqliteStore>,
    pub human: Actor,
    pub agents: AgentFiles,
}

impl OpenProject {
    pub fn id(&self) -> ProjectId {
        self.entry.id
    }

    /// A snapshot of the current policy.
    pub fn policy(&self) -> ProjectPolicy {
        self.policy.read().expect("policy lock").clone()
    }

    pub fn store(&self) -> Arc<dyn Store> {
        self.store.clone()
    }
}

pub struct ProjectRegistry {
    paths: HomePaths,
    config: HomeConfig,
    open: HashMap<ProjectId, Arc<OpenProject>>,
}

impl ProjectRegistry {
    /// Loads the home config, creating the home directory if needed.
    pub fn load(paths: HomePaths) -> Result<Self, RuntimeError> {
        std::fs::create_dir_all(paths.root()).map_err(|e| RuntimeError::io(paths.root(), e))?;
        let config = config::load_home_config(&paths.config_file())?;
        Ok(Self {
            paths,
            config,
            open: HashMap::new(),
        })
    }

    pub fn paths(&self) -> &HomePaths {
        &self.paths
    }

    pub fn config(&self) -> &HomeConfig {
        &self.config
    }

    pub fn projects(&self) -> &[ProjectEntry] {
        &self.config.projects
    }

    pub fn entry(&self, id: ProjectId) -> Option<&ProjectEntry> {
        self.config.project(id)
    }

    pub fn update_settings(
        &mut self,
        settings: ak_core::config::GlobalSettings,
    ) -> Result<(), RuntimeError> {
        self.config.settings = settings;
        self.save_config()
    }

    /// Registers a repository. Creates the project directory and a default
    /// policy. Does not touch the repository.
    pub fn add_project(
        &mut self,
        path: &Path,
        name: Option<String>,
    ) -> Result<ProjectId, RuntimeError> {
        let path = normalise(path)?;
        if !is_git_repository(&path) {
            return Err(RuntimeError::NotAGitRepository(path));
        }
        if let Some(existing) = self.config.project_by_path(&path) {
            return Err(RuntimeError::AlreadyRegistered(path, existing.name.clone()));
        }
        let name = name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| {
            path.file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_else(|| "project".into())
        });
        let id = ProjectId::new();
        let dir = self.paths.project_dir(id);
        std::fs::create_dir_all(self.paths.agents_dir(id))
            .map_err(|e| RuntimeError::io(&dir, e))?;
        std::fs::create_dir_all(self.paths.runs_dir(id)).map_err(|e| RuntimeError::io(&dir, e))?;
        config::save_policy(
            &self.paths.project_policy_file(id),
            &ProjectPolicy::new(&name),
        )?;
        self.config.projects.push(ProjectEntry {
            id,
            name,
            path,
            worktree_base: None,
        });
        self.save_config()?;
        Ok(id)
    }

    /// Unregisters a project. With `delete_data`, closes its store and removes
    /// its home directory too. The repository is never touched.
    pub async fn remove_project(
        &mut self,
        id: ProjectId,
        delete_data: bool,
    ) -> Result<(), RuntimeError> {
        let before = self.config.projects.len();
        self.config.projects.retain(|p| p.id != id);
        if self.config.projects.len() == before {
            return Err(RuntimeError::UnknownProject(id.to_string()));
        }
        if let Some(project) = self.open.remove(&id) {
            project.store.close().await;
        }
        self.save_config()?;
        if delete_data {
            let dir = self.paths.project_dir(id);
            if dir.exists() {
                remove_dir_with_retry(&dir).await?;
            }
        }
        Ok(())
    }

    /// Opens a project's store, bootstrapping it on first open. Idempotent.
    pub async fn open(&mut self, id: ProjectId) -> Result<Arc<OpenProject>, RuntimeError> {
        if let Some(p) = self.open.get(&id) {
            return Ok(p.clone());
        }
        let entry = self
            .entry(id)
            .cloned()
            .ok_or_else(|| RuntimeError::UnknownProject(id.to_string()))?;
        let policy = config::load_policy(&self.paths.project_policy_file(id))?;
        let dir = self.paths.project_dir(id);
        std::fs::create_dir_all(&dir).map_err(|e| RuntimeError::io(&dir, e))?;
        let store = Arc::new(SqliteStore::open(&self.paths.project_db_file(id)).await?);
        let identity = HumanIdentity {
            handle: self.config.settings.human_handle.clone(),
            display_name: self.config.settings.human_display_name.clone(),
        };
        let human = store.bootstrap(&policy, &identity).await?;
        let agent_files = AgentFiles::new(self.paths.agents_dir(id));
        let report = agents::reconcile(&agent_files, store.as_ref()).await?;
        for (handle, message) in &report.errors {
            tracing::warn!(project = %id, handle, "agent definition skipped: {message}");
        }
        let project = Arc::new(OpenProject {
            entry,
            policy: RwLock::new(policy),
            store,
            human,
            agents: agent_files,
        });
        self.open.insert(id, project.clone());
        Ok(project)
    }

    /// Writes the policy file and updates the open project, if any.
    pub fn save_project_policy(
        &self,
        id: ProjectId,
        policy: &ProjectPolicy,
    ) -> Result<(), RuntimeError> {
        if self.entry(id).is_none() {
            return Err(RuntimeError::UnknownProject(id.to_string()));
        }
        config::save_policy(&self.paths.project_policy_file(id), policy)?;
        if let Some(p) = self.open.get(&id) {
            *p.policy.write().expect("policy lock") = policy.clone();
        }
        Ok(())
    }

    /// Renames the project in the home config. The policy's own name is
    /// saved separately through `save_project_policy`.
    pub fn rename_project(&mut self, id: ProjectId, name: &str) -> Result<(), RuntimeError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(RuntimeError::Serialize(
                "project name cannot be empty".into(),
            ));
        }
        let entry = self
            .config
            .projects
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or_else(|| RuntimeError::UnknownProject(id.to_string()))?;
        entry.name = name.to_string();
        self.save_config()
    }

    pub fn opened(&self, id: ProjectId) -> Option<Arc<OpenProject>> {
        self.open.get(&id).cloned()
    }

    pub fn close(&mut self, id: ProjectId) {
        self.open.remove(&id);
    }

    fn save_config(&self) -> Result<(), RuntimeError> {
        config::save_home_config(&self.paths.config_file(), &self.config)
    }
}

fn normalise(path: &Path) -> Result<PathBuf, RuntimeError> {
    let canonical = std::fs::canonicalize(path).map_err(|e| RuntimeError::io(path, e))?;
    Ok(strip_verbatim_prefix(canonical))
}

/// `canonicalize` on Windows returns `\\?\C:\...`, which confuses tools and
/// users alike. Strip it when the remainder is a plain drive path.
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        if rest.len() >= 2 && rest.as_bytes()[1] == b':' {
            return PathBuf::from(rest);
        }
    }
    path
}

/// Windows can report a file as still in use for a moment after the handle
/// closes. A few short retries cover it.
async fn remove_dir_with_retry(dir: &Path) -> Result<(), RuntimeError> {
    let mut last = None;
    for _ in 0..5 {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = Some(e);
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
    Err(RuntimeError::io(dir, last.unwrap()))
}

fn is_git_repository(path: &Path) -> bool {
    // `.git` is a directory for a normal checkout and a file for a worktree.
    path.is_dir() && path.join(".git").exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> (tempfile::TempDir, ProjectRegistry, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let home = HomePaths::at(tmp.path().join("home"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let registry = ProjectRegistry::load(home).unwrap();
        (tmp, registry, repo)
    }

    #[test]
    fn rejects_non_repositories_and_duplicates() {
        let (tmp, mut registry, repo) = fresh();
        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert!(matches!(
            registry.add_project(&plain, None).unwrap_err(),
            RuntimeError::NotAGitRepository(_)
        ));

        registry.add_project(&repo, Some("Repo".into())).unwrap();
        assert!(matches!(
            registry.add_project(&repo, None).unwrap_err(),
            RuntimeError::AlreadyRegistered(..)
        ));
    }

    #[test]
    fn add_persists_config_and_policy() {
        let (_tmp, mut registry, repo) = fresh();
        let id = registry.add_project(&repo, None).unwrap();
        let entry = registry.entry(id).unwrap();
        assert_eq!(entry.name, "repo");
        assert!(registry.paths().project_policy_file(id).exists());

        let reloaded = ProjectRegistry::load(registry.paths().clone()).unwrap();
        assert_eq!(reloaded.projects().len(), 1);
        assert_eq!(reloaded.projects()[0].id, id);
        let policy = config::load_policy(&reloaded.paths().project_policy_file(id)).unwrap();
        assert_eq!(policy.name, "repo");
        assert_eq!(policy.key_prefix, "REP");
    }

    #[tokio::test]
    async fn open_bootstraps_and_remove_can_delete_data() {
        let (_tmp, mut registry, repo) = fresh();
        let id = registry
            .add_project(&repo, Some("Demo Project".into()))
            .unwrap();
        let project = registry.open(id).await.unwrap();
        assert_eq!(project.human.handle, "me");
        assert_eq!(project.store.columns().await.unwrap().len(), 5);
        assert!(registry.paths().project_db_file(id).exists());

        // Second open returns the same handle.
        let again = registry.open(id).await.unwrap();
        assert!(Arc::ptr_eq(&project, &again));

        drop(again);
        drop(project);
        registry.remove_project(id, true).await.unwrap();
        assert!(registry.projects().is_empty());
        assert!(!registry.paths().project_dir(id).exists());
        assert!(repo.exists(), "the repository is never touched");
    }
}
