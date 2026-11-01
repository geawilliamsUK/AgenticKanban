//! A ticket branch as the human reviews it before it is merged: its commits
//! and its changes against the branch it will be merged into.
//!
//! Agent worktrees share the project repository's refs, so `ticket/KEY` is
//! read from the human's own checkout without touching any worktree.

use std::path::Path;

use crate::workspace::git;
use crate::SupervisorError;

/// Lines of patch kept per file, and in all, before the rest is cut.
const FILE_LINES: usize = 1500;
const TOTAL_LINES: usize = 20_000;

#[derive(Debug, Clone, PartialEq)]
pub struct BranchDiff {
    pub branch: String,
    /// What it is compared with: the integration branch, or the checkout's.
    pub base: String,
    /// The branch's head commit, abbreviated.
    pub head: String,
    /// Commits on the branch and not on the base, newest first:
    /// abbreviated hash and subject.
    pub commits: Vec<(String, String)>,
    pub files: Vec<FileDiff>,
    /// Some patches were cut short to keep the view usable.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileDiff {
    pub path: String,
    /// None for a binary file.
    pub added: Option<u32>,
    pub removed: Option<u32>,
    /// The file's part of the patch, from its `diff --git` line.
    pub patch: String,
}

impl BranchDiff {
    pub fn added(&self) -> u32 {
        self.files.iter().filter_map(|f| f.added).sum()
    }

    pub fn removed(&self) -> u32 {
        self.files.iter().filter_map(|f| f.removed).sum()
    }
}

/// The branch a ticket branch merges into: the configured integration
/// branch, or the branch the human's checkout is on.
pub async fn base_branch(repo: &Path, integration: Option<&str>) -> Result<String, SupervisorError> {
    if let Some(b) = integration.filter(|b| !b.trim().is_empty()) {
        return Ok(b.trim().to_string());
    }
    Ok(git(repo, &["rev-parse", "--abbrev-ref", "HEAD"])
        .await?
        .trim()
        .to_string())
}

/// `branch` against `base`: what merging it would bring in. None when the
/// branch does not exist.
pub async fn branch_diff(
    repo: &Path,
    base: &str,
    branch: &str,
) -> Result<Option<BranchDiff>, SupervisorError> {
    let full = format!("refs/heads/{branch}");
    if git(repo, &["rev-parse", "--verify", "--quiet", &full])
        .await
        .is_err()
    {
        return Ok(None);
    }
    let head = git(repo, &["rev-parse", "--short", &full])
        .await?
        .trim()
        .to_string();
    let range = format!("{base}..{branch}");
    let commits = git(repo, &["log", "--no-color", "--format=%h %s", &range])
        .await?
        .lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(h, s)| (h.to_string(), s.to_string()))
        .collect();
    // Three dots: the changes since the branch left the base, which is what
    // a merge brings in, not what the base has done since.
    let patch = git(
        repo,
        &[
            "diff",
            "--no-color",
            "--no-ext-diff",
            "-M",
            &format!("{base}...{branch}"),
        ],
    )
    .await?;
    let (files, truncated) = split_patch(&patch);
    Ok(Some(BranchDiff {
        branch: branch.to_string(),
        base: base.to_string(),
        head,
        commits,
        files,
        truncated,
    }))
}

/// One `FileDiff` per `diff --git` section, with its added and removed
/// lines counted from its hunks.
fn split_patch(patch: &str) -> (Vec<FileDiff>, bool) {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut lines_in_file = 0;
    let mut cut = false;
    let mut total = 0;
    let mut truncated = false;
    let mut in_hunk = false;
    for line in patch.lines() {
        if let Some(header) = line.strip_prefix("diff --git ") {
            // "a/old b/new": the new path follows the last " b/".
            let path = header
                .rfind(" b/")
                .map(|i| header[i + 3..].to_string())
                .unwrap_or_else(|| header.to_string());
            files.push(FileDiff {
                path,
                added: Some(0),
                removed: Some(0),
                patch: String::new(),
            });
            lines_in_file = 0;
            cut = false;
            in_hunk = false;
        }
        let Some(file) = files.last_mut() else { continue };
        if line.starts_with("@@") {
            in_hunk = true;
        } else if line.starts_with("Binary files ") {
            file.added = None;
            file.removed = None;
        } else if in_hunk && line.starts_with('+') {
            file.added = file.added.map(|n| n + 1);
        } else if in_hunk && line.starts_with('-') {
            file.removed = file.removed.map(|n| n + 1);
        }
        if lines_in_file < FILE_LINES && total < TOTAL_LINES {
            file.patch.push_str(line);
            file.patch.push('\n');
            total += 1;
        } else if !cut {
            file.patch.push_str("… (cut here; see the branch itself for the rest)\n");
            cut = true;
            truncated = true;
        }
        lines_in_file += 1;
    }
    (files, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn commit(repo: &Path, file: &str, body: &str, msg: &str) {
        std::fs::write(repo.join(file), body).unwrap();
        git(repo, &["add", "-A"]).await.unwrap();
        git(
            repo,
            &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", msg],
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_ticket_branch_diff_shows_only_what_the_merge_brings() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        git(repo, &["init", "-q", "-b", "main"]).await.unwrap();
        commit(repo, "a.txt", "one\ntwo\nthree\n", "init").await;
        git(repo, &["checkout", "-q", "-b", "ticket/WEB-1"]).await.unwrap();
        commit(repo, "a.txt", "one\nTWO\nthree\nfour\n", "WEB-1 change a").await;
        commit(repo, "b.bin", "\0\x01\x02", "WEB-1 add a binary").await;
        git(repo, &["checkout", "-q", "main"]).await.unwrap();
        // Work on the base since is not part of the review.
        commit(repo, "c.txt", "later\n", "unrelated").await;

        assert_eq!(base_branch(repo, None).await.unwrap(), "main");
        assert_eq!(base_branch(repo, Some("develop")).await.unwrap(), "develop");
        assert!(branch_diff(repo, "main", "ticket/NOPE-1").await.unwrap().is_none());

        let d = branch_diff(repo, "main", "ticket/WEB-1").await.unwrap().unwrap();
        assert_eq!(d.commits.len(), 2);
        assert_eq!(d.commits[0].1, "WEB-1 add a binary");
        let paths: Vec<&str> = d.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["a.txt", "b.bin"]);
        assert_eq!((d.files[0].added, d.files[0].removed), (Some(2), Some(1)));
        assert!(d.files[0].patch.contains("+TWO"));
        assert_eq!(d.files[1].added, None);
        assert_eq!((d.added(), d.removed()), (2, 1));
        assert!(!d.truncated);
        assert!(!d.head.is_empty());
    }

    #[test]
    fn long_patches_are_cut() {
        let body: String = (0..FILE_LINES + 10).map(|i| format!("+{i}\n")).collect();
        let patch = format!("diff --git a/x b/x\n@@ -0,0 +1 @@\n{body}");
        let (files, truncated) = split_patch(&patch);
        assert!(truncated);
        assert_eq!(files[0].added, Some((FILE_LINES + 10) as u32));
        assert_eq!(files[0].patch.lines().count(), FILE_LINES + 1);
    }
}
