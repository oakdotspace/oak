use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::path::Path;

use crate::error::Result;

/// OS-generated metadata files and directories that should never be tracked.
/// These appear on their own — a file manager browsing a directory drops a
/// `.DS_Store`; a mounted volume gets `.fseventsd`/`.Spotlight-V100` written by
/// the OS's volume-bookkeeping daemons; Windows leaves `Thumbs.db`/`desktop.ini`.
/// Versioning them is never what the user wants, so Oak ignores them everywhere,
/// the same way it ignores `.git/`.
const SYSTEM_METADATA_NAMES: &[&str] = &[
    // macOS
    ".DS_Store",
    ".AppleDouble",
    ".LSOverride",
    ".DocumentRevisions-V100",
    ".fseventsd",
    ".Spotlight-V100",
    ".TemporaryItems",
    ".Trashes",
    ".VolumeIcon.icns",
    ".com.apple.timemachine.donotpresent",
    ".AppleDB",
    ".AppleDesktop",
    ".apdisk",
    // Windows
    "Thumbs.db",
    "ehthumbs.db",
    "Desktop.ini",
    "$RECYCLE.BIN",
    // Linux file managers
    ".directory",
];

/// True if `path` (a `/`-separated relative path) is, or lives under, an
/// OS-generated metadata file or directory — including AppleDouble resource-fork
/// files (`._*`). Matching any path *component* means a write to
/// `.fseventsd/fseventsd-uuid` is caught by the `.fseventsd` directory entry.
pub fn is_system_metadata_path(path: &str) -> bool {
    path.split('/').filter(|c| !c.is_empty()).any(|comp| {
        comp.starts_with("._")
            || SYSTEM_METADATA_NAMES
                .iter()
                .any(|n| n.eq_ignore_ascii_case(comp))
    })
}

/// Handles ignore patterns for the repository
pub struct IgnorePatterns {
    gitignore: Option<Gitignore>,
}

impl IgnorePatterns {
    /// Create a new IgnorePatterns from a repository root
    pub fn new(repo_root: &Path) -> Result<Self> {
        let oakignore_path = repo_root.join(".oakignore");
        let gitignore_path = repo_root.join(".gitignore");

        let mut builder = GitignoreBuilder::new(repo_root);

        // Always ignore .oak directory, .oak-branch marker, and .git directory
        builder.add_line(None, ".oak/").ok();
        builder.add_line(None, ".oak-branch").ok();
        builder.add_line(None, ".git/").ok();

        // Load .oakignore if it exists
        if oakignore_path.exists() {
            builder.add(&oakignore_path);
        }

        // Fall back to .gitignore if no .oakignore exists
        if !oakignore_path.exists() && gitignore_path.exists() {
            builder.add(&gitignore_path);
        }

        let gitignore = builder.build().ok();
        Ok(IgnorePatterns { gitignore })
    }

    /// Create IgnorePatterns from a string of newline-separated patterns.
    /// No filesystem access — useful for server-side filtering of uploaded files.
    pub fn from_patterns(patterns: &str) -> Self {
        let synthetic_root = Path::new("/");
        let mut builder = GitignoreBuilder::new(synthetic_root);
        builder.add_line(None, ".oak/").ok();
        builder.add_line(None, ".oak-branch").ok();
        builder.add_line(None, ".git/").ok();
        for line in patterns.lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty() && !trimmed.starts_with('#') {
                builder.add_line(None, trimmed).ok();
            }
        }
        let gitignore = builder.build().ok();
        IgnorePatterns { gitignore }
    }

    /// Create an empty ignore patterns (ignores nothing except .oak, .oak-branch, and .git)
    pub fn empty(repo_root: &Path) -> Self {
        let mut builder = GitignoreBuilder::new(repo_root);
        builder.add_line(None, ".oak/").ok();
        builder.add_line(None, ".oak-branch").ok();
        builder.add_line(None, ".git/").ok();
        let gitignore = builder.build().ok();
        IgnorePatterns { gitignore }
    }

    /// Check if a file path or any of its ancestor directories should be ignored.
    /// Useful for filtering a flat list of file paths (e.g. from a web upload)
    /// where we can't rely on a directory walker to skip ignored directories.
    pub fn is_path_or_ancestors_ignored(&self, path_str: &str) -> bool {
        let path = Path::new(path_str);
        if self.is_ignored(path, false) {
            return true;
        }
        // Check each ancestor directory
        let mut current = path.parent();
        while let Some(ancestor) = current {
            if ancestor.as_os_str().is_empty() {
                break;
            }
            if self.is_ignored(ancestor, true) {
                return true;
            }
            current = ancestor.parent();
        }
        false
    }

    /// Check if a path should be ignored
    pub fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        let path_str = path.to_string_lossy();

        // Allow version-controlled files inside .oak/. These live in the repo
        // metadata directory but are tracked like any other file:
        // - `.oak/attributes`: per-path attributes
        if path_str == ".oak/attributes" {
            return false;
        }

        // - `.oak/workflows/`: native-CI workflow definitions. Like
        //   `.oak/attributes`, these are tracked and read server-side from the
        //   commit (see Oak's native-CI design). Both the directory itself and
        //   everything under it are version-controlled, so the directory stays
        //   walkable and its files are never ignored.
        if path_str == ".oak/workflows"
            || path_str == ".oak/workflows/"
            || path_str.starts_with(".oak/workflows/")
        {
            return false;
        }

        // The `.oak` directory itself must not be ignored as a *directory* so
        // the working-tree walker can descend into it and see the
        // version-controlled entries above. Files directly under `.oak` that
        // aren't explicitly allowed are still ignored below.
        if is_dir && (path_str == ".oak" || path_str.ends_with("/.oak")) {
            return false;
        }

        // Always ignore .oak contents (other than the exceptions above) and
        // the .oak-branch marker file.
        if path.starts_with(".oak") || path_str.contains("/.oak/") {
            return true;
        }
        // Always ignore .git directory
        if path.starts_with(".git") || path.to_string_lossy().contains("/.git/") {
            return true;
        }
        if path
            .file_name()
            .map(|f| f == ".oak-branch")
            .unwrap_or(false)
        {
            return true;
        }

        // OS-generated metadata (.DS_Store, .fseventsd, Thumbs.db, …) is never
        // tracked, regardless of any .oakignore/.gitignore.
        if is_system_metadata_path(&path_str) {
            return true;
        }

        if let Some(ref gitignore) = self.gitignore {
            gitignore.matched(path, is_dir).is_ignore()
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_ignores_oak_directory() {
        let temp = TempDir::new().unwrap();
        let patterns = IgnorePatterns::empty(temp.path());
        // The .oak directory itself is walkable so the commit scanner can
        // descend to pick up version-controlled entries like `.oak/attributes`.
        // Files inside .oak that aren't explicitly allowed are still ignored.
        assert!(!patterns.is_ignored(Path::new(".oak"), true));
        assert!(patterns.is_ignored(Path::new(".oak/oak.db"), false));
        assert!(patterns.is_ignored(Path::new(".oak/internal"), true));
    }

    #[test]
    fn test_tracks_oak_workflows() {
        let temp = TempDir::new().unwrap();
        let patterns = IgnorePatterns::empty(temp.path());
        // CI workflow definitions under `.oak/workflows/` are version-controlled
        // (read server-side from the commit), so the directory stays walkable
        // and its files are not ignored — unlike the rest of `.oak/`.
        assert!(!patterns.is_ignored(Path::new(".oak/workflows"), true));
        assert!(!patterns.is_ignored(Path::new(".oak/workflows/ci.yml"), false));
        assert!(!patterns.is_ignored(Path::new(".oak/workflows/deploy.yaml"), false));
        // Other `.oak/` contents remain ignored.
        assert!(patterns.is_ignored(Path::new(".oak/oak.db"), false));
    }

    #[test]
    fn test_oakignore_patterns() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join(".oakignore"), "*.log\ntarget/\n").unwrap();

        let patterns = IgnorePatterns::new(temp.path()).unwrap();
        assert!(patterns.is_ignored(Path::new("debug.log"), false));
        assert!(patterns.is_ignored(Path::new("target"), true));
        assert!(!patterns.is_ignored(Path::new("src/main.rs"), false));
    }

    #[test]
    fn test_gitignore_fallback() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join(".gitignore"), "node_modules/\n").unwrap();

        let patterns = IgnorePatterns::new(temp.path()).unwrap();
        assert!(patterns.is_ignored(Path::new("node_modules"), true));
    }

    #[test]
    fn test_ignores_git_directory() {
        let temp = TempDir::new().unwrap();
        let patterns = IgnorePatterns::empty(temp.path());
        assert!(patterns.is_ignored(Path::new(".git"), true));
        assert!(patterns.is_ignored(Path::new(".git/config"), false));
    }

    #[test]
    fn test_always_ignores_oak_and_git_with_no_ignore_file() {
        let temp = TempDir::new().unwrap();
        let patterns = IgnorePatterns::new(temp.path()).unwrap();
        // .oak is walkable (for committed subpaths) but its non-versioned
        // contents are still ignored.
        assert!(!patterns.is_ignored(Path::new(".oak"), true));
        assert!(patterns.is_ignored(Path::new(".oak/oak.db"), false));
        assert!(patterns.is_ignored(Path::new(".git"), true));
        assert!(patterns.is_ignored(Path::new(".oak-branch"), false));
        assert!(!patterns.is_ignored(Path::new("src/main.rs"), false));
    }

    #[test]
    fn test_oakignore_takes_precedence() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join(".gitignore"), "*.log\n").unwrap();
        fs::write(temp.path().join(".oakignore"), "*.txt\n").unwrap();

        let patterns = IgnorePatterns::new(temp.path()).unwrap();
        // .oakignore should be used, not .gitignore
        assert!(patterns.is_ignored(Path::new("file.txt"), false));
        assert!(!patterns.is_ignored(Path::new("file.log"), false));
    }

    #[test]
    fn test_oak_attributes_not_ignored() {
        let temp = TempDir::new().unwrap();
        let patterns = IgnorePatterns::empty(temp.path());

        // .oak/attributes should NOT be ignored (it's version-controlled)
        assert!(!patterns.is_ignored(Path::new(".oak/attributes"), false));

        // But other .oak files should still be ignored
        assert!(patterns.is_ignored(Path::new(".oak/oak.db"), false));
    }

    #[test]
    fn test_nested_gitignore_patterns() {
        let temp = TempDir::new().unwrap();
        fs::write(
            temp.path().join(".gitignore"),
            "*.o\n*.so\nbuild/\n!build/keep.txt\n",
        )
        .unwrap();

        let patterns = IgnorePatterns::new(temp.path()).unwrap();
        assert!(patterns.is_ignored(Path::new("main.o"), false));
        assert!(patterns.is_ignored(Path::new("lib.so"), false));
        assert!(patterns.is_ignored(Path::new("build"), true));
        assert!(!patterns.is_ignored(Path::new("src/lib.rs"), false));
    }

    #[test]
    fn test_negation_pattern() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join(".oakignore"), "*.log\n!important.log\n").unwrap();

        let patterns = IgnorePatterns::new(temp.path()).unwrap();
        assert!(patterns.is_ignored(Path::new("debug.log"), false));
        // Negation pattern should un-ignore
        assert!(!patterns.is_ignored(Path::new("important.log"), false));
    }

    #[test]
    fn test_directory_specific_pattern() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join(".oakignore"), "logs/\n").unwrap();

        let patterns = IgnorePatterns::new(temp.path()).unwrap();
        // Directory pattern should match directories
        assert!(patterns.is_ignored(Path::new("logs"), true));
        // But not files named "logs"
        assert!(!patterns.is_ignored(Path::new("logs"), false));
    }

    #[test]
    fn test_oak_branch_marker_always_ignored() {
        let temp = TempDir::new().unwrap();
        let patterns = IgnorePatterns::new(temp.path()).unwrap();
        assert!(patterns.is_ignored(Path::new(".oak-branch"), false));
        assert!(patterns.is_ignored(Path::new("subdir/.oak-branch"), false));
    }

    #[test]
    fn test_empty_ignore_file() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join(".oakignore"), "").unwrap();

        let patterns = IgnorePatterns::new(temp.path()).unwrap();
        // .oak contents are ignored (except the versioned entries). .git is
        // ignored wholesale.
        assert!(patterns.is_ignored(Path::new(".oak/oak.db"), false));
        assert!(patterns.is_ignored(Path::new(".git"), true));
        // But nothing else
        assert!(!patterns.is_ignored(Path::new("file.txt"), false));
    }

    #[test]
    fn test_comment_lines_in_ignore() {
        let temp = TempDir::new().unwrap();
        fs::write(
            temp.path().join(".oakignore"),
            "# This is a comment\n*.log\n# Another comment\n",
        )
        .unwrap();

        let patterns = IgnorePatterns::new(temp.path()).unwrap();
        assert!(patterns.is_ignored(Path::new("debug.log"), false));
        assert!(!patterns.is_ignored(Path::new("# This is a comment"), false));
    }

    #[test]
    fn test_from_patterns_godot() {
        let patterns = IgnorePatterns::from_patterns("# Godot\n.godot/\n*.import\n");
        // Directory patterns match the directory itself
        assert!(patterns.is_ignored(Path::new(".godot"), true));
        // Extension patterns match files
        assert!(patterns.is_ignored(Path::new("icon.svg.import"), false));
        // Normal files pass through
        assert!(!patterns.is_ignored(Path::new("project.godot"), false));
        assert!(!patterns.is_ignored(Path::new("scenes/main.tscn"), false));
        // Built-in ignores still work
        assert!(patterns.is_ignored(Path::new(".oak/oak.db"), false));
        assert!(patterns.is_ignored(Path::new(".git"), true));
    }

    #[test]
    fn test_from_patterns_unity() {
        let patterns = IgnorePatterns::from_patterns(
            "Library/\nTemp/\nLogs/\nUserSettings/\nobj/\nBuilds/\nBuild/\n*.csproj\n*.sln\n",
        );
        // Directory patterns match directories
        assert!(patterns.is_ignored(Path::new("Library"), true));
        assert!(patterns.is_ignored(Path::new("Temp"), true));
        // Extension globs match files
        assert!(patterns.is_ignored(Path::new("Assembly-CSharp.csproj"), false));
        assert!(patterns.is_ignored(Path::new("MyGame.sln"), false));
        // Normal files pass through
        assert!(!patterns.is_ignored(Path::new("Assets/Scripts/Player.cs"), false));
        assert!(!patterns.is_ignored(Path::new("ProjectSettings/ProjectSettings.asset"), false));
    }

    #[test]
    fn test_from_patterns_empty() {
        let patterns = IgnorePatterns::from_patterns("");
        assert!(!patterns.is_ignored(Path::new("src/main.rs"), false));
        // Built-in ignores still work
        assert!(patterns.is_ignored(Path::new(".oak/oak.db"), false));
        assert!(patterns.is_ignored(Path::new(".git"), true));
    }

    #[test]
    fn test_system_metadata_path() {
        // macOS volume bookkeeping (the original `oak mount` report)
        assert!(is_system_metadata_path(".fseventsd"));
        assert!(is_system_metadata_path(".fseventsd/fseventsd-uuid"));
        assert!(is_system_metadata_path(".Spotlight-V100/Store-V2/x"));
        assert!(is_system_metadata_path(".Trashes/501"));
        // .DS_Store anywhere in the tree
        assert!(is_system_metadata_path(".DS_Store"));
        assert!(is_system_metadata_path("src/.DS_Store"));
        // AppleDouble resource forks
        assert!(is_system_metadata_path("._foo"));
        assert!(is_system_metadata_path("dir/._image.png"));
        // Windows junk
        assert!(is_system_metadata_path("Thumbs.db"));
        assert!(is_system_metadata_path("a/b/desktop.ini")); // case-insensitive
                                                             // Normal files pass through
        assert!(!is_system_metadata_path("src/main.rs"));
        assert!(!is_system_metadata_path("README.md"));
        assert!(!is_system_metadata_path("Documents/notes.txt"));
    }

    #[test]
    fn test_is_ignored_covers_system_metadata() {
        let temp = TempDir::new().unwrap();
        let patterns = IgnorePatterns::empty(temp.path());
        assert!(patterns.is_ignored(Path::new(".fseventsd"), true));
        assert!(patterns.is_ignored(Path::new(".fseventsd/fseventsd-uuid"), false));
        assert!(patterns.is_ignored(Path::new("src/.DS_Store"), false));
        assert!(!patterns.is_ignored(Path::new("src/main.rs"), false));
    }

    #[test]
    fn test_from_patterns_is_file_in_ignored_dir() {
        // Demonstrates that callers must check parent directories separately
        // (the CLI walker does this by not descending into ignored dirs)
        let patterns = IgnorePatterns::from_patterns("Library/\n");
        assert!(patterns.is_ignored(Path::new("Library"), true));
        // A file path is checked against patterns; caller must also check ancestors
        assert!(patterns.is_path_or_ancestors_ignored("Library/foo.dll"));
        assert!(!patterns.is_path_or_ancestors_ignored("Assets/main.cs"));
    }
}
