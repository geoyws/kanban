//! Where a command was run, as git sees it.
//!
//! The ledger already had `repo_path`, `branch`, `head_sha` and
//! `dirty_summary` on checkpoints and handoffs. Measured 2026-08-21 across the
//! twelve live boards, **0 of 20 checkpoints and 0 of 3 handoffs carried a HEAD
//! sha**: the columns existed and were empty, because filling them meant the
//! caller passing `--repo --branch --head --dirty` by hand and nobody ever did.
//! A resuming agent got `branch: null` and had to guess. A field that says
//! something and holds nothing is the defect this project exists to refuse, so
//! this is captured rather than requested.
//!
//! **Asking git rather than reading `.git`.** Parsing the directory looks easy
//! until the layouts on a real machine disagree with the tutorial: on this box a
//! submodule has a real `.git` directory while its superproject's `.git` is a
//! file, a linked worktree redirects through `gitdir:`, refs may be packed, and
//! HEAD may be detached. Getting any of that subtly wrong writes *wrong*
//! provenance into a durable record, which is worse than writing none. git is
//! the authority on its own layout, so it is asked — the same reason the MCP
//! server runs the CLI instead of reimplementing it.
//!
//! Every failure degrades to `None`. A directory that is not a repository, or a
//! machine with no git, records no provenance and never fails a command that
//! was not about git in the first place.

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;

/// How deep a superproject chain is followed before giving up.
///
/// A submodule inside a submodule is ordinary here — this repository sits in
/// the dotfiles, which sit in the journal — but a cycle would not be, and a
/// bounded walk cannot hang on one.
const MAX_SUPERPROJECT_DEPTH: usize = 8;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitContext {
    /// The checkout the caller was standing in, which for a driver lane is the
    /// lane itself rather than the repository it belongs to.
    pub worktree: String,
    /// `main` for an ordinary checkout, `linked` for a `git worktree` lane.
    pub worktree_kind: &'static str,
    /// Absent when HEAD is detached.
    pub branch: Option<String>,
    pub head: String,
    /// Count of `git status --porcelain` lines. Zero is clean.
    pub dirty: i64,
    /// The outermost superproject, when this checkout is nested inside one.
    ///
    /// A submodule's own commit says nothing about which revision of the whole
    /// tree it was part of, and the answer to "what was checked out" is the
    /// root's commit. The chain is followed to the end rather than one level,
    /// because nesting is not limited to one level.
    pub root_worktree: Option<String>,
    pub root_branch: Option<String>,
    pub root_head: Option<String>,
}

/// Run a git command in `dir`, or `None` if git fails for any reason.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_owned())
    }
}

/// The outermost repository this one is nested inside, if any.
fn root_of(worktree: &Path) -> Option<PathBuf> {
    let mut outermost: Option<PathBuf> = None;
    let mut current = worktree.to_path_buf();
    for _ in 0..MAX_SUPERPROJECT_DEPTH {
        match git(&current, &["rev-parse", "--show-superproject-working-tree"]) {
            Some(parent) => {
                let parent = PathBuf::from(parent);
                // A parent that is not new ends the walk rather than looping.
                if Some(&parent) == outermost.as_ref() {
                    break;
                }
                current = parent.clone();
                outermost = Some(parent);
            }
            None => break,
        }
    }
    outermost
}

/// Resolve the git context of `dir`, or `None` when it is not a repository.
pub fn resolve(dir: &Path) -> Option<GitContext> {
    let worktree = git(dir, &["rev-parse", "--show-toplevel"])?;
    let worktree_path = PathBuf::from(&worktree);
    let head = git(&worktree_path, &["rev-parse", "HEAD"])?;
    // A lane is a linked worktree, which git tells apart by its git-dir not
    // being the shared one. This is the field that answers "which lane".
    let worktree_kind = match (
        git(&worktree_path, &["rev-parse", "--absolute-git-dir"]),
        git(
            &worktree_path,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        ),
    ) {
        (Some(own), Some(shared)) if own != shared => "linked",
        _ => "main",
    };
    let dirty = git(&worktree_path, &["status", "--porcelain"])
        .map(|text| text.lines().filter(|line| !line.trim().is_empty()).count() as i64)
        .unwrap_or(0);
    let root = root_of(&worktree_path);
    let (root_worktree, root_branch, root_head) = match root {
        Some(root) => (
            Some(root.to_string_lossy().into_owned()),
            git(&root, &["branch", "--show-current"]),
            git(&root, &["rev-parse", "HEAD"]),
        ),
        None => (None, None, None),
    };
    Some(GitContext {
        worktree,
        worktree_kind,
        branch: git(&worktree_path, &["branch", "--show-current"]),
        head,
        dirty,
        root_worktree,
        root_branch,
        root_head,
    })
}

/// A one-line summary of the working tree, for the `dirty_summary` column.
pub fn dirty_summary(context: &GitContext) -> String {
    match context.dirty {
        0 => "clean".to_owned(),
        1 => "1 file changed".to_owned(),
        n => format!("{n} files changed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_that_is_not_a_repository_records_nothing() {
        // Provenance is captured opportunistically: a command run outside a
        // repository is not an error, it simply has no git context to record.
        assert!(resolve(Path::new("/")).is_none());
        assert!(resolve(Path::new("/nonexistent-path-for-a-test")).is_none());
    }

    #[test]
    fn a_dirty_summary_reads_as_english() {
        let mut context = GitContext {
            worktree: "/tmp/x".into(),
            worktree_kind: "main",
            branch: None,
            head: "abc".into(),
            dirty: 0,
            root_worktree: None,
            root_branch: None,
            root_head: None,
        };
        assert_eq!(dirty_summary(&context), "clean");
        context.dirty = 1;
        assert_eq!(dirty_summary(&context), "1 file changed");
        context.dirty = 4;
        assert_eq!(dirty_summary(&context), "4 files changed");
    }
}
