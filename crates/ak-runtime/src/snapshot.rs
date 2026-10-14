//! Export and Import: the repository's `.agentic/` snapshot of a project's
//! agents and policy. The home directory stays canonical. The snapshot is a
//! publication the human stages and commits; agents never write it, and
//! runs never read it. Never `.claude/agents/`: Claude Code would load that
//! as subagents inside the agents' own runs.

use std::path::{Path, PathBuf};
use std::process::Command;

use ak_core::agent::AgentDefinition;
use ak_core::config::ProjectPolicy;

use crate::agents::AgentFiles;
use crate::config;
use crate::error::RuntimeError;

pub const SNAPSHOT_DIR: &str = ".agentic";
const AGENTS_DIR: &str = "agents";
const POLICY_FILE: &str = "policy.toml";

/// Where a repository keeps its snapshot.
#[derive(Debug, Clone)]
pub struct SnapshotPaths {
    root: PathBuf,
}

impl SnapshotPaths {
    pub fn new(repo: &Path) -> Self {
        Self {
            root: repo.join(SNAPSHOT_DIR),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn agents_dir(&self) -> PathBuf {
        self.root.join(AGENTS_DIR)
    }

    pub fn policy_file(&self) -> PathBuf {
        self.root.join(POLICY_FILE)
    }

    /// Whether the repository has a snapshot at all.
    pub fn exists(&self) -> bool {
        self.agents_dir().is_dir()
    }

    /// The snapshot's agents, in the same layout as the home directory.
    pub fn files(&self) -> AgentFiles {
        AgentFiles::new(self.agents_dir())
    }
}

#[derive(Debug, Clone)]
pub struct ExportOptions {
    /// Memory is this machine's experience; off by default.
    pub include_memory: bool,
    pub include_policy: bool,
    /// `git add` the snapshot afterwards. The app never commits.
    pub stage: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            include_memory: false,
            include_policy: true,
            stage: true,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ExportReport {
    pub root: PathBuf,
    pub exported: Vec<String>,
    /// Handles that were in the snapshot but are no longer on the team.
    pub removed: Vec<String>,
    pub policy_written: bool,
    pub staged: bool,
    pub stage_error: Option<String>,
}

/// Writes the project's agents (and policy) into the repository's snapshot,
/// removing snapshot agents that are no longer on the team.
pub fn export(
    home: &AgentFiles,
    policy: &ProjectPolicy,
    repo: &Path,
    opts: &ExportOptions,
) -> Result<ExportReport, RuntimeError> {
    let snapshot = SnapshotPaths::new(repo);
    let target = snapshot.files();
    std::fs::create_dir_all(target.dir()).map_err(|e| RuntimeError::io(target.dir(), e))?;
    let mut report = ExportReport {
        root: snapshot.root().to_path_buf(),
        ..Default::default()
    };
    let handles = home.handles()?;
    for handle in &handles {
        let def = home.read_definition(handle)?;
        target.write_definition(&def)?;
        if opts.include_memory {
            target.write_memory(handle, &home.read_memory(handle)?)?;
        } else {
            let memory = target.memory_path(handle);
            if memory.is_file() {
                std::fs::remove_file(&memory).map_err(|e| RuntimeError::io(&memory, e))?;
            }
        }
        report.exported.push(handle.clone());
    }
    for stale in target.handles()? {
        if !handles.contains(&stale) {
            target.remove(&stale)?;
            report.removed.push(stale);
        }
    }
    if opts.include_policy {
        config::save_policy(&snapshot.policy_file(), policy)?;
        report.policy_written = true;
    }
    if opts.stage {
        match stage(repo) {
            Ok(()) => report.staged = true,
            Err(e) => report.stage_error = Some(e),
        }
    }
    Ok(report)
}

/// `git add` on the snapshot directory, in the human's own checkout.
fn stage(repo: &Path) -> Result<(), String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(repo)
        .args(["add", "-A", "--", SNAPSHOT_DIR]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let output = cmd.output().map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportStatus {
    New,
    Changed,
    Identical,
}

impl ImportStatus {
    pub fn label(&self) -> &'static str {
        match self {
            ImportStatus::New => "new",
            ImportStatus::Changed => "changed",
            ImportStatus::Identical => "identical",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ImportCandidate {
    pub handle: String,
    pub status: ImportStatus,
    pub incoming: AgentDefinition,
    pub current: Option<AgentDefinition>,
    /// Memory present in the snapshot, offered separately.
    pub incoming_memory: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ImportReview {
    /// The repository the snapshot was read from.
    pub source: PathBuf,
    pub policy: Option<ProjectPolicy>,
    /// Whether the snapshot's policy carries this project's shared id.
    pub same_project: Option<bool>,
    pub candidates: Vec<ImportCandidate>,
    pub errors: Vec<(String, String)>,
}

/// Reads a repository's snapshot and compares each agent with the project's
/// own definition.
pub fn review_import(
    source_repo: &Path,
    home: &AgentFiles,
    current_policy: &ProjectPolicy,
) -> Result<ImportReview, RuntimeError> {
    let snapshot = SnapshotPaths::new(source_repo);
    if !snapshot.exists() {
        return Err(RuntimeError::Snapshot(format!(
            "{} has no {SNAPSHOT_DIR}/{AGENTS_DIR} directory",
            source_repo.display()
        )));
    }
    let files = snapshot.files();
    let mut candidates = Vec::new();
    let mut errors = Vec::new();
    for handle in files.handles()? {
        let incoming = match files.read_definition(&handle) {
            Ok(d) => d,
            Err(e) => {
                errors.push((handle, e.to_string()));
                continue;
            }
        };
        let current = if home.definition_path(&handle).is_file() {
            match home.read_definition(&handle) {
                Ok(d) => Some(d),
                Err(e) => {
                    errors.push((
                        handle.clone(),
                        format!("the current definition is unreadable: {e}"),
                    ));
                    None
                }
            }
        } else {
            None
        };
        let status = match &current {
            None => ImportStatus::New,
            Some(c) if c.render() == incoming.render() => ImportStatus::Identical,
            Some(_) => ImportStatus::Changed,
        };
        let incoming_memory = if files.memory_path(&handle).is_file() {
            files
                .read_memory(&handle)
                .ok()
                .filter(|m| !m.trim().is_empty())
        } else {
            None
        };
        candidates.push(ImportCandidate {
            handle,
            status,
            incoming,
            current,
            incoming_memory,
        });
    }
    let (policy, same_project) = if snapshot.policy_file().is_file() {
        match config::load_policy(&snapshot.policy_file()) {
            Ok(p) => {
                let same = p.shared_id == current_policy.shared_id;
                (Some(p), Some(same))
            }
            Err(e) => {
                errors.push(("policy".into(), e.to_string()));
                (None, None)
            }
        }
    } else {
        (None, None)
    };
    Ok(ImportReview {
        source: source_repo.to_path_buf(),
        policy,
        same_project,
        candidates,
        errors,
    })
}

#[derive(Debug, Clone, Default)]
pub struct ImportSelection {
    pub handles: Vec<String>,
    pub memory_for: Vec<String>,
    pub policy: bool,
}

/// Writes the chosen definitions and memories into the home directory and
/// returns the handles written plus the policy to apply, if chosen.
/// Reconciliation picks the files up.
pub fn apply_import(
    review: &ImportReview,
    selection: &ImportSelection,
    home: &AgentFiles,
) -> Result<(Vec<String>, Option<ProjectPolicy>), RuntimeError> {
    let mut written = Vec::new();
    for c in &review.candidates {
        if selection.handles.contains(&c.handle) {
            home.write_definition(&c.incoming)?;
            written.push(c.handle.clone());
        }
        if selection.memory_for.contains(&c.handle) {
            if let Some(m) = &c.incoming_memory {
                home.write_memory(&c.handle, m)?;
            }
        }
    }
    let policy = if selection.policy {
        review.policy.clone()
    } else {
        None
    };
    Ok((written, policy))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriftStatus {
    /// Both exist and differ.
    Changed,
    /// In the repository's snapshot, not on the team here.
    OnlyInRepository,
    /// On the team here, not exported yet.
    OnlyInHome,
}

impl DriftStatus {
    pub fn label(&self) -> &'static str {
        match self {
            DriftStatus::Changed => "differs from the repository",
            DriftStatus::OnlyInRepository => "available from the repository",
            DriftStatus::OnlyInHome => "not exported yet",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriftEntry {
    pub handle: String,
    pub status: DriftStatus,
}

/// How the home directory and the repository's snapshot differ. Empty when
/// the repository has no snapshot.
pub fn drift(home: &AgentFiles, repo: &Path) -> Result<Vec<DriftEntry>, RuntimeError> {
    let snapshot = SnapshotPaths::new(repo);
    if !snapshot.exists() {
        return Ok(Vec::new());
    }
    let files = snapshot.files();
    let repo_handles = files.handles()?;
    let home_handles = home.handles()?;
    let mut out = Vec::new();
    for h in &repo_handles {
        if home_handles.contains(h) {
            let theirs = files
                .read_definition(h)
                .map(|d| d.render())
                .unwrap_or_default();
            let ours = home
                .read_definition(h)
                .map(|d| d.render())
                .unwrap_or_default();
            if theirs != ours {
                out.push(DriftEntry {
                    handle: h.clone(),
                    status: DriftStatus::Changed,
                });
            }
        } else {
            out.push(DriftEntry {
                handle: h.clone(),
                status: DriftStatus::OnlyInRepository,
            });
        }
    }
    for h in &home_handles {
        if !repo_handles.contains(h) {
            out.push(DriftEntry {
                handle: h.clone(),
                status: DriftStatus::OnlyInHome,
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(repo: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    fn repo(tmp: &Path) -> PathBuf {
        let repo = tmp.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        repo
    }

    fn def(handle: &str, description: &str) -> AgentDefinition {
        let mut d = AgentDefinition::template(handle, "role");
        d.frontmatter.description = description.into();
        d
    }

    #[test]
    fn export_writes_the_snapshot_prunes_and_stages() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = repo(tmp.path());
        let home = AgentFiles::new(tmp.path().join("home").join("agents"));
        home.write_definition(&def("backend", "API")).unwrap();
        home.write_definition(&def("frontend", "UI")).unwrap();
        home.write_memory("backend", "- remember this").unwrap();
        let policy = ProjectPolicy::new("Demo");

        let report = export(&home, &policy, &repo, &ExportOptions::default()).unwrap();
        assert_eq!(report.exported, vec!["backend", "frontend"]);
        assert!(report.policy_written);
        assert!(report.staged, "{:?}", report.stage_error);
        let snapshot = SnapshotPaths::new(&repo);
        assert!(snapshot.exists());
        assert!(snapshot.files().definition_path("backend").is_file());
        assert!(
            !snapshot.files().memory_path("backend").exists(),
            "memory off by default"
        );
        let exported_policy = config::load_policy(&snapshot.policy_file()).unwrap();
        assert_eq!(exported_policy.shared_id, policy.shared_id);
        let staged = git(&repo, &["diff", "--cached", "--name-only"]);
        assert!(
            staged.contains(".agentic/agents/backend/agent.md"),
            "{staged}"
        );
        assert!(staged.contains(".agentic/policy.toml"), "{staged}");
        assert!(!repo.join(".claude").exists(), "never .claude");

        // A retired agent leaves the snapshot; memory can be included.
        home.remove("frontend").unwrap();
        let report = export(
            &home,
            &policy,
            &repo,
            &ExportOptions {
                include_memory: true,
                include_policy: false,
                stage: false,
            },
        )
        .unwrap();
        assert_eq!(report.exported, vec!["backend"]);
        assert_eq!(report.removed, vec!["frontend"]);
        assert!(!report.staged);
        assert!(!snapshot.files().definition_path("frontend").exists());
        assert_eq!(
            snapshot.files().read_memory("backend").unwrap(),
            "- remember this"
        );
    }

    #[test]
    fn review_classifies_apply_writes_and_drift_reports() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = repo(tmp.path());
        let theirs = AgentFiles::new(tmp.path().join("theirs"));
        theirs.write_definition(&def("backend", "API v2")).unwrap();
        theirs.write_definition(&def("qa", "Tests")).unwrap();
        theirs.write_definition(&def("same", "Same")).unwrap();
        theirs.write_memory("qa", "- qa memory").unwrap();
        let their_policy = ProjectPolicy::new("Demo");
        export(
            &theirs,
            &their_policy,
            &repo,
            &ExportOptions {
                include_memory: true,
                include_policy: true,
                stage: false,
            },
        )
        .unwrap();

        let home = AgentFiles::new(tmp.path().join("home"));
        home.write_definition(&def("backend", "API v1")).unwrap();
        home.write_definition(&def("same", "Same")).unwrap();
        home.write_definition(&def("local", "Only here")).unwrap();
        let my_policy = ProjectPolicy::new("Demo");

        let review = review_import(&repo, &home, &my_policy).unwrap();
        assert!(review.errors.is_empty(), "{:?}", review.errors);
        assert_eq!(review.same_project, Some(false), "different shared id");
        let status = |h: &str| {
            review
                .candidates
                .iter()
                .find(|c| c.handle == h)
                .unwrap()
                .status
        };
        assert_eq!(status("backend"), ImportStatus::Changed);
        assert_eq!(status("qa"), ImportStatus::New);
        assert_eq!(status("same"), ImportStatus::Identical);
        let qa = review.candidates.iter().find(|c| c.handle == "qa").unwrap();
        assert_eq!(qa.incoming_memory.as_deref(), Some("- qa memory"));
        assert!(review.candidates.iter().all(|c| c.handle != "local"));

        let drift_before = drift(&home, &repo).unwrap();
        assert!(drift_before.contains(&DriftEntry {
            handle: "backend".into(),
            status: DriftStatus::Changed
        }));
        assert!(drift_before.contains(&DriftEntry {
            handle: "qa".into(),
            status: DriftStatus::OnlyInRepository
        }));
        assert!(drift_before.contains(&DriftEntry {
            handle: "local".into(),
            status: DriftStatus::OnlyInHome
        }));
        assert!(drift_before.iter().all(|d| d.handle != "same"));

        let (written, policy) = apply_import(
            &review,
            &ImportSelection {
                handles: vec!["qa".into(), "backend".into()],
                memory_for: vec!["qa".into()],
                policy: true,
            },
            &home,
        )
        .unwrap();
        assert_eq!(written, vec!["backend", "qa"]);
        assert_eq!(policy.map(|p| p.shared_id), Some(their_policy.shared_id));
        assert_eq!(
            home.read_definition("backend")
                .unwrap()
                .frontmatter
                .description,
            "API v2"
        );
        assert_eq!(home.read_memory("qa").unwrap(), "- qa memory");

        let again = review_import(&repo, &home, &their_policy).unwrap();
        assert_eq!(again.same_project, Some(true));
        assert!(again
            .candidates
            .iter()
            .all(|c| c.status == ImportStatus::Identical));
        let drift_after = drift(&home, &repo).unwrap();
        assert_eq!(
            drift_after,
            vec![DriftEntry {
                handle: "local".into(),
                status: DriftStatus::OnlyInHome
            }]
        );

        // No snapshot: nothing to review, nothing drifts.
        let bare = tmp.path().join("bare");
        std::fs::create_dir_all(&bare).unwrap();
        assert!(matches!(
            review_import(&bare, &home, &my_policy),
            Err(RuntimeError::Snapshot(_))
        ));
        assert!(drift(&home, &bare).unwrap().is_empty());
    }
}
