//! `oak tree inspect` (fb-519) and `oak export --tree-only` (fb-522): one
//! pinned commit's whole tree, verified from the local SQLite snapshot.
//! Neither command reads the working copy, changes refs, or uses the network.
use oak_core::sqlite::file_inspect::InspectionStatus;
use oak_core::sqlite::tree_inspect::{
    TreeFileEvidence, TreeFileSink, TreeInspection, TreeInspectionLimits,
};
use oak_core::{FileMode, OakError, Result, SqliteRepository};
use serde::Serialize;
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

fn open_snapshot(cwd: &Path, op: &'static str) -> Result<(PathBuf, SqliteRepository)> {
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    if super::mount::mount_dest_for(cwd)?.is_some() {
        return Err(OakError::Unsupported {
            backend: "mount",
            op,
        });
    }
    let ctx = crate::resolve::resolve(cwd)?;
    if !matches!(ctx.backend, crate::resolve::Backend::Sqlite) {
        return Err(OakError::Unsupported { backend: "git", op });
    }
    let repo = SqliteRepository::open_read_only(&ctx.db_path()?)?;
    Ok((ctx.work_tree, repo))
}

fn larger_budget_command(inspection: &TreeInspection, base: &str) -> Option<String> {
    let truncation = inspection.truncation.as_ref()?;
    let commit = inspection.commit.as_deref()?;
    Some(match truncation.limit {
        "max_files" => format!(
            "{base} --at {commit} --max-files {} --max-bytes {}",
            (inspection.max_files.saturating_mul(2))
                .min(oak_core::sqlite::tree_inspect::MAX_TREE_MAX_FILES),
            inspection.max_bytes
        ),
        _ => format!(
            "{base} --at {commit} --max-files {} --max-bytes {}",
            inspection.max_files,
            (inspection.logical_bytes + truncation.next_declared_size.unwrap_or(0))
                .max(inspection.max_bytes.saturating_mul(2))
                .min(oak_core::sqlite::tree_inspect::MAX_TREE_MAX_BYTES)
        ),
    })
}

/// Returns true (exit 0) only when the listing is complete and verified.
pub fn inspect(
    cwd: &Path,
    revision: &str,
    limits: TreeInspectionLimits,
    json: bool,
) -> Result<bool> {
    let (work_tree, repo) = open_snapshot(cwd, "tree inspect")?;
    let inspection = repo.inspect_pinned_tree(revision, limits)?;
    let complete = inspection.complete;

    #[derive(Serialize)]
    struct Output {
        schema_version: u32,
        kind: &'static str,
        repository_root: PathBuf,
        backend: &'static str,
        evidence: TreeInspection,
        recommended_next_commands: Vec<String>,
    }
    let mut next = Vec::new();
    if let Some(command) = larger_budget_command(&inspection, "oak tree inspect --json") {
        next.push(command);
    }
    if complete {
        if let Some(commit) = inspection.commit.as_deref() {
            next.push(format!("oak export --tree-only --at {commit} DEST --json"));
        }
    }
    if json {
        crate::output::print_json(&Output {
            schema_version: 1,
            kind: "tree_inspection",
            repository_root: work_tree,
            backend: "sqlite",
            evidence: inspection,
            recommended_next_commands: next,
        })?;
    } else {
        for file in &inspection.files {
            let status = serde_json::to_value(file.status)?;
            crate::output::print_line(&format!(
                "{} {:>10} {} {}",
                mode_label(file.mode),
                file.size
                    .map(|size| size.to_string())
                    .unwrap_or_else(|| "-".into()),
                file.sha256
                    .as_deref()
                    .unwrap_or(status.as_str().unwrap_or("unverified")),
                file.path
            ));
        }
        crate::output::print_line(&format!(
            "{} file(s), {} logical byte(s) at {}{}",
            inspection.file_count,
            inspection.logical_bytes,
            inspection.commit.as_deref().unwrap_or(revision),
            if complete { "" } else { " — INCOMPLETE" }
        ));
        if let Some(truncation) = &inspection.truncation {
            crate::output::print_line(&format!(
                "truncated at {} ({}); next path: {}",
                truncation.limit, "budget", truncation.next_path
            ));
        }
        if let Some(reason) = &inspection.reason {
            crate::output::print_line(reason);
        }
        for command in &next {
            crate::output::print_line(&format!("next: {command}"));
        }
    }
    Ok(complete)
}

fn mode_label(mode: FileMode) -> &'static str {
    match mode {
        FileMode::Regular => "100644",
        FileMode::Executable => "100755",
        FileMode::Symlink => "120000",
    }
}

#[derive(Serialize)]
struct ExportReceipt {
    schema_version: u32,
    kind: &'static str,
    requested_revision: String,
    commit: Option<String>,
    root_tree: Option<String>,
    destination: PathBuf,
    materialized: bool,
    history_replayed: bool,
    status: InspectionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    complete: bool,
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    truncation: Option<oak_core::sqlite::tree_inspect::TreeTruncation>,
    files_written: u64,
    symlinks_written: u64,
    /// On platforms without symlink support, symlink entries are written as
    /// regular files holding the target text.
    symlinks_as_files: bool,
    /// `synced_before_publish`: staged files and directories were flushed
    /// with one barrier before the atomic rename, and the parent afterwards.
    /// `not_synced`: no barrier on this platform. `not_published` otherwise.
    durability: &'static str,
    /// Staging directories left by earlier exports whose process is gone
    /// (e.g. interrupted by SIGINT), removed before this export started.
    stale_staging_removed: u64,
    logical_bytes: u64,
    max_files: u64,
    max_bytes: u64,
    recommended_next_commands: Vec<String>,
}

/// Materialize one pinned tree into `dest`. Every file is hash-verified
/// before it is written to a private staging directory beside `dest`; the
/// staging directory is renamed into place only when the whole tree
/// verified within budget. Returns false (exit 1) when nothing was published.
pub fn export(
    cwd: &Path,
    revision: &str,
    dest: &Path,
    limits: TreeInspectionLimits,
    json: bool,
) -> Result<()> {
    let (work_tree, repo) = open_snapshot(cwd, "export --tree-only")?;
    let dest = super::file::lexical_absolute(cwd, dest);
    if dest.file_name().is_none() {
        return Err(OakError::InvalidArgument(
            "export DEST must name a directory".into(),
        ));
    }
    let dest_existed = match std::fs::symlink_metadata(&dest) {
        Ok(metadata) if metadata.is_dir() => {
            if std::fs::read_dir(&dest)?.next().is_some() {
                return Err(OakError::InvalidArgument(format!(
                    "export DEST {} is not empty; --tree-only never writes into existing content",
                    dest.display()
                )));
            }
            true
        }
        Ok(_) => {
            return Err(OakError::InvalidArgument(format!(
                "export DEST {} exists and is not a directory",
                dest.display()
            )))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    let parent = dest.parent().unwrap_or(Path::new("."));
    let canonical_parent = std::fs::canonicalize(parent).map_err(|error| {
        OakError::InvalidArgument(format!(
            "export DEST parent {} is not an existing directory: {error}",
            parent.display()
        ))
    })?;
    let metadata_dir = std::fs::canonicalize(&work_tree)
        .unwrap_or(work_tree)
        .join(".oak");
    if canonical_parent.starts_with(&metadata_dir) {
        return Err(OakError::InvalidArgument(
            "export DEST must not be inside the repository's .oak metadata directory".into(),
        ));
    }
    let staging_prefix = format!(
        ".{}.oak-export-",
        dest.file_name().unwrap_or_default().to_string_lossy()
    );
    let stale_staging_removed = remove_stale_staging(&canonical_parent, &staging_prefix);
    let staging = tempfile::Builder::new()
        .prefix(&format!("{staging_prefix}{}-", std::process::id()))
        .tempdir_in(&canonical_parent)?;
    let root = staging.path().to_path_buf();
    let mut staging = Some(staging);
    let mut writer = StagingWriter {
        root: root.clone(),
        dirs: HashSet::new(),
        symlinks: Vec::new(),
        files_written: 0,
        current: None,
    };
    let inspection = repo.visit_pinned_tree(revision, limits, &mut writer)?;
    let mut receipt = ExportReceipt {
        schema_version: 1,
        kind: "tree_export",
        requested_revision: revision.to_string(),
        commit: inspection.commit.clone(),
        root_tree: inspection.root_tree.clone(),
        destination: dest.clone(),
        materialized: false,
        history_replayed: false,
        status: inspection.status,
        reason: inspection.reason.clone(),
        complete: inspection.complete,
        truncated: inspection.truncated,
        truncation: None,
        files_written: 0,
        symlinks_written: 0,
        symlinks_as_files: !cfg!(unix),
        durability: "not_published",
        stale_staging_removed,
        logical_bytes: inspection.logical_bytes,
        max_files: inspection.max_files,
        max_bytes: inspection.max_bytes,
        recommended_next_commands: Vec::new(),
    };
    if let Some(command) = larger_budget_command(&inspection, "oak tree inspect --json") {
        receipt.recommended_next_commands.push(command);
    }
    receipt.truncation = inspection.truncation;
    if receipt.complete {
        let symlinks = writer.finish_symlinks()?;
        receipt.durability = writer.durable_barrier()?;
        if dest_existed {
            // Fails (and publishes nothing) if content raced into DEST.
            std::fs::remove_dir(&dest).map_err(|error| {
                OakError::InvalidArgument(format!(
                    "export DEST {} changed during export: {error}",
                    dest.display()
                ))
            })?;
        }
        let kept = staging.take().expect("staging present").keep();
        #[cfg(unix)]
        {
            // tempdir creates 0700; give DEST ordinary directory permissions.
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&kept, std::fs::Permissions::from_mode(0o755))?;
        }
        if let Err(error) = rename_noreplace(&kept, &dest) {
            let _ = std::fs::remove_dir_all(&kept);
            return Err(error);
        }
        // Make the publishing rename itself durable.
        if receipt.durability == "synced_before_publish" {
            sync_dir(&canonical_parent)?;
        }
        receipt.materialized = true;
        receipt.files_written = writer.files_written;
        receipt.symlinks_written = symlinks;
    }
    // A failed or truncated export removes its staging directory now:
    // `exit_process` below skips destructors.
    drop(staging.take());
    let materialized = receipt.materialized;
    if json {
        crate::output::print_json(&receipt)?;
    } else if materialized {
        crate::output::success(&format!(
            "Exported tree of {} ({} file(s), {} byte(s)) into {}",
            receipt.commit.as_deref().unwrap_or(revision),
            receipt.files_written + receipt.symlinks_written,
            receipt.logical_bytes,
            dest.display()
        ));
    } else {
        crate::output::error(&format!(
            "Nothing exported: the tree at {} did not fully verify within budget ({}){}",
            receipt.commit.as_deref().unwrap_or(revision),
            receipt.reason.as_deref().unwrap_or(if receipt.truncated {
                "truncated"
            } else {
                "incomplete"
            }),
            receipt
                .recommended_next_commands
                .first()
                .map(|command| format!("; inspect with: {command}"))
                .unwrap_or_default()
        ));
    }
    if !materialized {
        crate::output::exit_process(1);
    }
    Ok(())
}

/// Remove `<prefix><pid>-XXXX` staging directories whose owning process is
/// provably gone (fb-522 QA L1: SIGINT skips destructors). A live or unknown
/// owner, a symlink, or an unparsable name is left alone.
fn remove_stale_staging(parent: &Path, prefix: &str) -> u64 {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(rest) = name.to_str().and_then(|name| name.strip_prefix(prefix)) else {
            continue;
        };
        let Some(pid) = rest
            .split('-')
            .next()
            .and_then(|pid| pid.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == std::process::id()
            || !matches!(
                crate::workdir_lock::process_liveness(pid),
                crate::workdir_lock::ProcessLiveness::Dead
            )
        {
            continue;
        }
        let path = entry.path();
        if std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir())
            && std::fs::remove_dir_all(&path).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let handle = std::fs::File::open(dir)?;
        if unsafe { libc::fsync(handle.as_raw_fd()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        #[cfg(target_os = "macos")]
        if unsafe { libc::fcntl(handle.as_raw_fd(), libc::F_FULLFSYNC) } == -1 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// Rename a finished staging directory to a path that does not exist.
fn rename_noreplace(from: &Path, to: &Path) -> Result<()> {
    if std::fs::symlink_metadata(to).is_ok() {
        return Err(OakError::InvalidArgument(format!(
            "export DEST {} appeared during export; refusing to replace it",
            to.display()
        )));
    }
    std::fs::rename(from, to)?;
    Ok(())
}

struct StagingWriter {
    root: PathBuf,
    /// Directories this export created, by exact repo-relative path.
    dirs: HashSet<String>,
    /// Created last so no file write can ever traverse a written symlink.
    symlinks: Vec<(String, Vec<u8>)>,
    files_written: u64,
    /// The file currently being streamed (bytes not yet verified).
    current: Option<(String, Staged)>,
}

enum Staged {
    File(std::io::BufWriter<std::fs::File>),
    Symlink(Vec<u8>),
}

/// Symlink targets are buffered (they are created last); real targets are
/// far below this.
const MAX_SYMLINK_TARGET_BYTES: usize = 64 * 1024;

impl StagingWriter {
    fn collision(path: &str) -> OakError {
        OakError::InvalidArgument(format!(
            "tree path '{path}' collides with another path on this filesystem (case or Unicode folding); nothing was exported"
        ))
    }

    fn ensure_parents(&mut self, path: &str) -> Result<()> {
        let mut prefix = String::new();
        let components: Vec<&str> = path.split('/').collect();
        for component in &components[..components.len() - 1] {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            if self.dirs.contains(&prefix) {
                continue;
            }
            match std::fs::create_dir(self.root.join(&prefix)) {
                Ok(()) => {
                    self.dirs.insert(prefix.clone());
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    return Err(Self::collision(&prefix))
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn check_path(path: &str) -> Result<()> {
        // Tree entry names are validated when trees are decoded; re-check the
        // joined path so a staging write can never leave the staging root.
        if path.is_empty()
            || path.starts_with('/')
            || path.contains('\\')
            || path.contains('\0')
            || path
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
        {
            return Err(OakError::InvalidPath(format!(
                "refusing unsafe tree path '{path}'"
            )));
        }
        Ok(())
    }

    fn file_mode(mode: FileMode) -> u32 {
        if mode == FileMode::Executable {
            0o755
        } else {
            0o644
        }
    }

    /// Make everything staged durable with ONE expensive barrier instead of
    /// a full-device flush per file (fb-522 QA M1). The rename that follows
    /// is the publish point, so per-file flushes add nothing to atomicity.
    ///
    /// - Linux: `syncfs` on the staging filesystem (files and directories).
    /// - macOS: files were `fsync`ed as they closed (cheap: no drive-cache
    ///   flush); directories are `fsync`ed here, then one `F_FULLFSYNC`
    ///   flushes the drive cache for all of it.
    /// - Elsewhere: no barrier (reported as `durability: "not_synced"`).
    fn durable_barrier(&self) -> Result<&'static str> {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::io::AsRawFd;
            let root = std::fs::File::open(&self.root)?;
            if unsafe { libc::syncfs(root.as_raw_fd()) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok("synced_before_publish")
        }
        #[cfg(target_os = "macos")]
        {
            use std::os::unix::io::AsRawFd;
            for dir in &self.dirs {
                let handle = std::fs::File::open(self.root.join(dir))?;
                if unsafe { libc::fsync(handle.as_raw_fd()) } != 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
            }
            let root = std::fs::File::open(&self.root)?;
            if unsafe { libc::fsync(root.as_raw_fd()) } != 0
                || unsafe { libc::fcntl(root.as_raw_fd(), libc::F_FULLFSYNC) } == -1
            {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok("synced_before_publish")
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            Ok("not_synced")
        }
    }

    fn finish_symlinks(&mut self) -> Result<u64> {
        let mut written = 0;
        for (path, target) in std::mem::take(&mut self.symlinks) {
            let link = self.root.join(&path);
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStrExt;
                if target.contains(&0) {
                    return Err(OakError::InvalidPath(format!(
                        "symlink '{path}' has a NUL byte in its target"
                    )));
                }
                match std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(&target), &link) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        return Err(Self::collision(&path))
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            #[cfg(not(unix))]
            {
                let mut handle = match std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&link)
                {
                    Ok(handle) => handle,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        return Err(Self::collision(&path))
                    }
                    Err(error) => return Err(error.into()),
                };
                handle.write_all(&target)?;
            }
            written += 1;
        }
        Ok(written)
    }
}

impl TreeFileSink for StagingWriter {
    fn begin(&mut self, path: &str, mode: FileMode) -> Result<()> {
        Self::check_path(path)?;
        self.ensure_parents(path)?;
        let staged = if mode == FileMode::Symlink {
            Staged::Symlink(Vec::new())
        } else {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(Self::file_mode(mode));
            }
            match options.open(self.root.join(path)) {
                Ok(handle) => Staged::File(std::io::BufWriter::with_capacity(256 * 1024, handle)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    return Err(Self::collision(path))
                }
                Err(error) => return Err(error.into()),
            }
        };
        self.current = Some((path.to_string(), staged));
        Ok(())
    }

    fn write_chunk(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        match self.current.as_mut() {
            Some((_, Staged::File(writer))) => writer.write_all(bytes),
            Some((_, Staged::Symlink(target))) => {
                if target.len() + bytes.len() > MAX_SYMLINK_TARGET_BYTES {
                    return Err(std::io::Error::other(
                        "symlink target exceeds the 64 KiB export budget",
                    ));
                }
                target.extend_from_slice(bytes);
                Ok(())
            }
            None => Err(std::io::Error::other("no staged file is open")),
        }
    }

    fn finish(&mut self, file: &TreeFileEvidence) -> Result<()> {
        let Some((path, staged)) = self.current.take() else {
            // Cached/metadata-only entries never reach a sink.
            return Ok(());
        };
        if file.status != InspectionStatus::Verified {
            // Unverified bytes never survive; the export will not publish.
            if let Staged::File(writer) = staged {
                drop(writer);
                let _ = std::fs::remove_file(self.root.join(&path));
            }
            return Ok(());
        }
        match staged {
            Staged::Symlink(target) => self.symlinks.push((path, target)),
            Staged::File(writer) => {
                let handle = writer.into_inner().map_err(|error| error.into_error())?;
                #[cfg(unix)]
                {
                    // `mode` at open is filtered by the umask; set it exactly.
                    use std::os::unix::fs::PermissionsExt;
                    handle.set_permissions(std::fs::Permissions::from_mode(Self::file_mode(
                        file.mode,
                    )))?;
                }
                #[cfg(target_os = "macos")]
                {
                    // Plain fsync hands the data to the drive without the
                    // per-file drive-cache flush `sync_all` (F_FULLFSYNC)
                    // costs; `durable_barrier` issues one F_FULLFSYNC.
                    use std::os::unix::io::AsRawFd;
                    if unsafe { libc::fsync(handle.as_raw_fd()) } != 0 {
                        return Err(std::io::Error::last_os_error().into());
                    }
                }
                self.files_written += 1;
            }
        }
        Ok(())
    }
}
