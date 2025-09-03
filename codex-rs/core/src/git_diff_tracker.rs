use std::path::PathBuf;
use std::process::Command;
use std::time::SystemTime;
use sha1::Digest;

/// Tracks git changes from an initial state through a session and can produce a
/// combined unified diff (committed + uncommitted) plus untracked files created
/// after the tracker was initialized. Mirrors the behavior of the Python
/// GitDiffTracker used elsewhere.
#[derive(Debug)]
pub struct GitDiffTracker {
    enabled: bool,
    cwd: Option<PathBuf>,
    initial_git_hash: Option<String>,
    session_start_time: SystemTime,
    last_diff_hash: Option<String>,
}

impl GitDiffTracker {
    pub fn new(enabled: bool, cwd: Option<PathBuf>) -> Self {
        let mut tracker = Self {
            enabled,
            cwd,
            initial_git_hash: None,
            session_start_time: SystemTime::now(),
            last_diff_hash: None,
        };
        if tracker.enabled {
            tracker.capture_initial_state();
        }
        tracker
    }

    fn capture_initial_state(&mut self) {
        match self.run_git(&["rev-parse", "HEAD"]) {
            Ok(out) if !out.trim().is_empty() => {
                self.initial_git_hash = Some(out.trim().to_string());
            }
            _ => {
                // Not in a git repo or no commits; disable tracking
                self.enabled = false;
            }
        }
    }

    /// Returns Some(diff_text) when tracking is enabled; may be an empty string if
    /// there are no changes. Returns None when disabled (e.g., not in a git repo).
    pub fn get_diff(&mut self) -> Option<String> {
        if !self.enabled {
            return None;
        }

        let mut combined = String::new();
        let exclude_patterns = self.get_worktree_exclusions();

        // Build git diff command
        let mut args: Vec<&str> = Vec::new();
        if let Some(hash) = &self.initial_git_hash {
            args.extend(["diff", hash]);
        } else {
            args.extend(["diff", "HEAD"]);
        }
        // Append exclusions ("--" then patterns)
        if !exclude_patterns.is_empty() {
            args.push("--");
            for p in &exclude_patterns {
                args.push(p);
            }
        }

        if let Ok(out) = self.run_git(&args) {
            let s = out.trim();
            if !s.is_empty() {
                combined.push_str(s);
            }
        }

        // Append untracked files content in a diff-like form
        let untracked = self.get_untracked_files(&exclude_patterns);
        if !untracked.is_empty() {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str(&untracked);
        }

        Some(combined)
    }

    /// Return a diff only if it is non-empty and different from the last one
    /// returned by this method during the session. Uses SHA-1 of the trimmed
    /// diff text to detect changes.
    pub fn get_diff_if_changed(&mut self) -> Option<String> {
        let diff = self.get_diff()?;
        let trimmed = diff.trim().to_string();
        let mut hasher = sha1::Sha1::new();
        hasher.update(trimmed.as_bytes());
        let hash = format!("{:x}", hasher.finalize());
        match &self.last_diff_hash {
            Some(prev) if prev == &hash => None,
            _ => {
                self.last_diff_hash = Some(hash);
                Some(trimmed)
            }
        }
    }

    fn get_worktree_exclusions(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Ok(raw) = self.run_git(&["worktree", "list", "--porcelain"]) {
            let current_dir = self
                .cwd
                .clone()
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            for line in raw.lines() {
                if let Some(rest) = line.strip_prefix("worktree ") {
                    let worktree_path = rest.trim();
                    let worktree = PathBuf::from(worktree_path);
                    if worktree != current_dir
						&& let Ok(rel) = worktree.strip_prefix(&current_dir)
                    {
                        let rels = rel.to_string_lossy().to_string();
                        if !rels.is_empty() {
                            out.push(format!(":(exclude){rels}"));
                        }
                    }
                }
            }
        }
        out
    }

    fn get_untracked_files(&self, exclude_patterns: &[String]) -> String {
        // Build git ls-files to find untracked files
        let mut args: Vec<&str> = vec!["ls-files", "--others", "--exclude-standard"]; 
        if !exclude_patterns.is_empty() {
            args.push("--");
            for p in exclude_patterns {
                args.push(p);
            }
        }

        let Ok(out) = self.run_git(&args) else { return String::new() };
        let files: Vec<&str> = out.lines().filter(|s| !s.trim().is_empty()).collect();
        if files.is_empty() {
            return String::new();
        }

        let base = self
            .cwd
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        let mut buf = String::new();
        for rel in files {
            let abs = base.join(rel);
            // Skip files that existed before the session started.
            match std::fs::metadata(&abs)
                .and_then(|m| m.created().or_else(|_| m.modified()))
            {
                Ok(created) => {
                    if created < self.session_start_time {
                        continue;
                    }
                }
                Err(_) => continue,
            }

            use std::fmt::Write as _;
            let _ = writeln!(buf, "diff --git a/{rel} b/{rel}");
            buf.push_str("new file mode 100644\n");
            buf.push_str("index 0000000..0000000\n");
            buf.push_str("--- /dev/null\n");
            let _ = writeln!(buf, "+++ b/{rel}");

            match std::fs::read_to_string(&abs) {
                Ok(contents) => {
                    let lines: Vec<&str> = contents.lines().collect();
                    let count = lines.len();
                    let _ = writeln!(buf, "@@ -0,0 +1,{count} @@");
                    for line in lines {
                        let _ = writeln!(buf, "+{line}");
                    }
                    if !contents.ends_with('\n') {
                        buf.push_str("\\ No newline at end of file\n");
                    }
                }
                Err(_) => {
                    buf.push_str("@@ -0,0 +1,1 @@\n");
                    buf.push_str("+[Binary or unreadable file]\n");
                }
            }
            buf.push('\n');
        }

        buf
    }

    fn run_git(&self, args: &[&str]) -> std::io::Result<String> {
        let mut cmd = Command::new("git");
        cmd.args(args);
        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }
        // Note: std::process::Command doesn't support a timeout natively.
        // We rely on the fact that these commands are quick in practice.
        let out = cmd.output()?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).to_string())
        } else {
            Ok(String::new())
        }
    }
}
