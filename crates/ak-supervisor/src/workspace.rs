//! One git worktree per agent, outside the human's checkout.
//!
//! The worktree lives at `<base>/<handle>` where the base defaults to a
//! sibling of the repository named `<repo>-agents`. It is created once on the
//! branch `agent/<handle>` and reused across runs. Per-ticket branches are the
//! agent's own business, as the conventions prompt explains.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::SupervisorError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub path: PathBuf,
    pub branch: String,
    pub dirty_files: usize,
}

/// `<parent of repo>/<repo name>-agents`.
pub fn default_base(repo: &Path) -> PathBuf {
    let name = repo
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".into());
    repo.parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| repo.to_path_buf())
        .join(format!("{name}-agents"))
}

/// Creates the worktree if needed and reports its branch and dirtiness.
pub async fn ensure(repo: &Path, base: &Path, handle: &str) -> Result<Workspace, SupervisorError> {
    let path = base.join(handle);
    if !path.join(".git").exists() {
        std::fs::create_dir_all(base).map_err(|e| SupervisorError::io(base, e))?;
        let branch = format!("agent/{handle}");
        let path_str = path.to_string_lossy().to_string();
        // New branch first; if it already exists, check it out instead.
        let created = git(repo, &["worktree", "add", &path_str, "-b", &branch]).await;
        if let Err(first) = created {
            git(repo, &["worktree", "add", &path_str, &branch])
                .await
                .map_err(|second| {
                    SupervisorError::Git(format!(
                        "could not create worktree {path_str}: {first}; then: {second}"
                    ))
                })?;
        }
    }
    status(&path).await
}

/// Branch name and number of changed files.
pub async fn status(path: &Path) -> Result<Workspace, SupervisorError> {
    let out = git(path, &["status", "--porcelain=v1", "--branch"]).await?;
    let mut branch = String::from("(detached)");
    let mut dirty = 0;
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            // "## main...origin/main [ahead 1]" or "## HEAD (no branch)"
            branch = rest.split("...").next().unwrap_or(rest).trim().to_string();
        } else if !line.trim().is_empty() {
            dirty += 1;
        }
    }
    Ok(Workspace {
        path: path.to_path_buf(),
        branch,
        dirty_files: dirty,
    })
}

pub async fn git(cwd: &Path, args: &[&str]) -> Result<String, SupervisorError> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let output = cmd
        .output()
        .await
        .map_err(|e| SupervisorError::Git(format!("could not run git: {e}")))?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(SupervisorError::Git(format!(
            "git {} failed: {}",
            args.join(" "),
            if err.is_empty() {
                output.status.to_string()
            } else {
                err
            }
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn repo() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("proj");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]).await.unwrap();
        git(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ],
        )
        .await
        .unwrap();
        (tmp, repo)
    }

    #[tokio::test]
    async fn creates_and_reuses_a_worktree_per_agent() {
        let (_tmp, repo) = repo().await;
        let base = default_base(&repo);
        assert!(base.ends_with("proj-agents"));

        let ws = ensure(&repo, &base, "backend").await.unwrap();
        assert_eq!(ws.branch, "agent/backend");
        assert_eq!(ws.dirty_files, 0);
        assert!(ws.path.join(".git").exists());

        // Dirty the worktree and call again: same place, reported dirty.
        std::fs::write(ws.path.join("note.txt"), "x").unwrap();
        let again = ensure(&repo, &base, "backend").await.unwrap();
        assert_eq!(again.path, ws.path);
        assert_eq!(again.dirty_files, 1);

        // A second agent gets its own worktree and branch.
        let qa = ensure(&repo, &base, "qa").await.unwrap();
        assert_eq!(qa.branch, "agent/qa");
        assert_ne!(qa.path, ws.path);

        // The human's checkout is untouched.
        let main = status(&repo).await.unwrap();
        assert_eq!(main.branch, "main");
        assert_eq!(main.dirty_files, 0);
    }

    #[tokio::test]
    async fn recreates_a_deleted_worktree_on_an_existing_branch() {
        let (_tmp, repo) = repo().await;
        let base = default_base(&repo);
        let ws = ensure(&repo, &base, "ops").await.unwrap();
        std::fs::remove_dir_all(&ws.path).unwrap();
        git(&repo, &["worktree", "prune"]).await.unwrap();
        let again = ensure(&repo, &base, "ops").await.unwrap();
        assert_eq!(again.branch, "agent/ops");
    }
}
