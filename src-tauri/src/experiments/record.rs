//! The git-tracked `.ire/experiments/<NNN>-<slug>/` folder created when an
//! experiment starts. `EXPERIMENT.md` inside it is the durable, human-readable
//! record of what ran and why — the `wake_prompt` goal/context lives only in
//! `local.db` otherwise, so it does not survive clearing the cache or reach
//! anyone reading the repository. The folder is also the home for the
//! experiment's own artifacts; raw logs stay in `.ire/cache/experiments/<uuid>/`.

use std::fs;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};

use crate::ire::store::atomic_write;

/// Serializes prefix allocation so two experiments starting at once can't claim
/// the same number. A workspace is single-instance (see `workspace::lock`), so
/// an in-process lock covers every writer.
static ALLOC_LOCK: Mutex<()> = Mutex::new(());

const DIR: &str = ".ire/experiments";

pub struct RecordArgs<'a> {
    pub uuid: &'a str,
    pub name: &'a str,
    pub command: &'a str,
    pub working_dir: &'a str,
    pub wake_prompt: &'a str,
    pub started_at: &'a str,
}

/// Create `.ire/experiments/<NNN>-<slug>/EXPERIMENT.md`, returning the folder
/// path relative to the workspace root. Creates `.ire/experiments/` on demand,
/// so workspaces initialized before this existed pick it up on their next run.
pub fn create(workspace_root: &Path, args: RecordArgs<'_>) -> Result<String> {
    let root = workspace_root.join(DIR);
    let _guard = ALLOC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    fs::create_dir_all(&root).with_context(|| format!("create {}", root.display()))?;

    let name = format!("{:03}-{}", next_prefix(&root), slugify(args.name));
    let dir = root.join(&name);
    fs::create_dir(&dir).with_context(|| format!("create {}", dir.display()))?;
    atomic_write(&dir.join("EXPERIMENT.md"), &render(&args))?;
    Ok(format!("{DIR}/{name}"))
}

/// Undo [`create`] when the experiment never started. Best-effort: a failure is
/// logged, not propagated — the spawn error is what the caller reports.
pub fn remove(workspace_root: &Path, rel_dir: &str) {
    let dir = workspace_root.join(rel_dir);
    if let Err(e) = fs::remove_dir_all(&dir) {
        tracing::warn!(error = %e, dir = %dir.display(), "remove experiment record failed");
    }
}

/// One past the highest `NNN-` prefix present. Gaps are left alone: numbering
/// only moves forward, so deleting a folder never reissues its number.
fn next_prefix(root: &Path) -> u32 {
    let Ok(entries) = fs::read_dir(root) else {
        return 1;
    };
    let highest = entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| {
            let name = e.file_name();
            let (prefix, _) = name.to_str()?.split_once('-')?;
            prefix.parse::<u32>().ok()
        })
        .max();
    highest.unwrap_or(0) + 1
}

/// Filesystem-safe title slug: ASCII alphanumerics lowercased, every other run
/// of characters collapsed to a single `-`.
fn slugify(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let slug: String = out.trim_matches('-').chars().take(60).collect();
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        "experiment".to_string()
    } else {
        slug.to_string()
    }
}

fn render(args: &RecordArgs<'_>) -> String {
    let fence = fence_for(args.command);
    format!(
        "# {name}\n\n\
         - **uuid**: `{uuid}`\n\
         - **started**: {started_at}\n\
         - **working dir**: `{working_dir}`\n\n\
         ## Goal & context\n\n\
         {wake_prompt}\n\n\
         ## Command\n\n\
         {fence}sh\n{command}\n{fence}\n\n\
         ---\n\n\
         Artifacts belonging to this experiment — scripts, result files, notes — go in\n\
         this folder. Raw stdout/stderr stay in `.ire/cache/experiments/{uuid}/`.\n",
        name = args.name.trim(),
        uuid = args.uuid,
        started_at = args.started_at,
        working_dir = args.working_dir,
        wake_prompt = args.wake_prompt.trim(),
        command = args.command,
    )
}

/// A fence long enough that a command containing backticks can't break out.
fn fence_for(command: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in command.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    "`".repeat(longest.max(2) + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args<'a>(name: &'a str, command: &'a str) -> RecordArgs<'a> {
        RecordArgs {
            uuid: "11111111-2222-3333-4444-555555555555",
            name,
            command,
            working_dir: "/tmp/project",
            wake_prompt: "Check whether lr=1e-4 beats the baseline.",
            started_at: "2026-08-11T10:00:00+02:00",
        }
    }

    #[test]
    fn first_experiment_allocates_001() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = create(tmp.path(), args("LR ablation", "echo hi")).unwrap();
        assert_eq!(dir, ".ire/experiments/001-lr-ablation");
        assert!(tmp.path().join(&dir).join("EXPERIMENT.md").exists());
    }

    #[test]
    fn allocation_continues_past_existing_dirs_and_gaps() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(DIR);
        for existing in ["001-first", "004-fourth", "not-numbered"] {
            fs::create_dir_all(root.join(existing)).unwrap();
        }
        // Highest is 004 despite the 002/003 gap, and a loose file is ignored.
        fs::write(root.join("009-a-file-not-a-dir"), "").unwrap();
        let dir = create(tmp.path(), args("fifth", "echo hi")).unwrap();
        assert_eq!(dir, ".ire/experiments/005-fifth");
    }

    #[test]
    fn missing_experiments_dir_is_created_for_old_workspaces() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!tmp.path().join(DIR).exists());
        create(tmp.path(), args("first", "echo hi")).unwrap();
        assert!(tmp.path().join(DIR).join("001-first").is_dir());
    }

    #[test]
    fn slugify_normalizes_titles() {
        assert_eq!(slugify("LR Ablation"), "lr-ablation");
        assert_eq!(slugify("  spaced  out  "), "spaced-out");
        assert_eq!(slugify("a/b\\c:d*e?"), "a-b-c-d-e");
        assert_eq!(slugify("__leading and trailing__"), "leading-and-trailing");
        assert_eq!(slugify("!!!"), "experiment");
        assert_eq!(slugify(""), "experiment");
        assert_eq!(slugify("..hidden"), "hidden");
        assert_eq!(slugify(&"x".repeat(100)).len(), 60);
        // A truncation landing on a separator doesn't leave a trailing dash.
        assert!(!slugify(&format!("{} tail", "x".repeat(59))).ends_with('-'));
    }

    #[test]
    fn record_carries_goal_command_and_shared_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = create(tmp.path(), args("LR ablation", "python run.py --lr 1e-4")).unwrap();
        let md = fs::read_to_string(tmp.path().join(dir).join("EXPERIMENT.md")).unwrap();

        assert!(md.starts_with("# LR ablation\n"));
        assert!(md.contains("11111111-2222-3333-4444-555555555555"));
        assert!(md.contains("2026-08-11T10:00:00+02:00"));
        assert!(md.contains("/tmp/project"));
        assert!(md.contains("Check whether lr=1e-4 beats the baseline."));
        assert!(md.contains("```sh\npython run.py --lr 1e-4\n```"));
    }

    #[test]
    fn command_containing_backticks_cannot_break_the_fence() {
        let tmp = tempfile::tempdir().unwrap();
        let command = "echo ```nested``` && echo $(date)";
        let dir = create(tmp.path(), args("fences", command)).unwrap();
        let md = fs::read_to_string(tmp.path().join(dir).join("EXPERIMENT.md")).unwrap();
        assert!(md.contains(&format!("````sh\n{command}\n````")));
    }

    #[test]
    fn concurrent_starts_get_distinct_numbers() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let dirs: Vec<String> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8)
                .map(|i| {
                    let root = root.clone();
                    s.spawn(move || {
                        let name = format!("run {i}");
                        create(&root, args(&name, "echo hi")).unwrap()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        let mut prefixes: Vec<&str> = dirs
            .iter()
            .map(|d| &d[".ire/experiments/".len()..][..3])
            .collect();
        prefixes.sort_unstable();
        prefixes.dedup();
        assert_eq!(prefixes.len(), 8, "duplicate prefix allocated: {dirs:?}");
        assert_eq!(prefixes, ["001", "002", "003", "004", "005", "006", "007", "008"]);
    }

    #[test]
    fn remove_deletes_the_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = create(tmp.path(), args("doomed", "echo hi")).unwrap();
        assert!(tmp.path().join(&dir).exists());
        remove(tmp.path(), &dir);
        assert!(!tmp.path().join(&dir).exists());
        // The parent survives so the next allocation still sees history.
        assert!(tmp.path().join(DIR).is_dir());
    }
}
