//! Windows ProjFS-backed virtual filesystem for `oak mount`.
//!
//! Microsoft's *Projected File System* (Windows 10 1809+) is the native
//! callback-driven VFS used by GVFS / Scalar for huge git monorepos. It's a
//! near-perfect fit for Oak: ProjFS asks our process for directory contents
//! and file data on demand, and persists modifications in-place to the
//! mount directory on disk. We don't have to fake an overlay or intercept
//! reads of unmodified files — once a file is fetched and written to disk,
//! ProjFS treats subsequent reads as native and bypasses our callbacks.
//!
//! # Differences from the FUSE backend
//!
//! - **In-place writes.** Modifications materialize directly to the mount
//!   tree (FUSE writes go through a flat overlay dir). Dirty entries set
//!   `DirtyEntry::in_place = true` so `mount::commit` reads from the mount
//!   path on disk rather than the overlay dir.
//! - **No blocking event loop.** `PrjStartVirtualizing` returns once the
//!   provider is registered; ProjFS spins up its own threadpool to invoke
//!   our callbacks. The caller (`mount::start`) parks on Ctrl-C instead.
//! - **Persistent virtualization root.** Marking a directory writes a
//!   reparse point that survives reboots. Re-mounting the same `dest` is a
//!   no-op for the marker; we just call `PrjStartVirtualizing` again.
//! - **OS-managed name comparison.** ProjFS provides `PrjFileNameMatch` and
//!   `PrjFileNameCompare` so we get NTFS-correct case-insensitive matching
//!   without rolling our own.
//!
//! # Setup the user must do once per machine
//!
//! Enable the Windows optional feature (admin PowerShell):
//! ```powershell
//! Enable-WindowsOptionalFeature -Online -FeatureName Client-ProjFS -NoRestart
//! ```
//! Or via Settings → Apps → Optional features → "Windows Projected File System".
//!
//! # Threading
//!
//! ProjFS calls our `extern "system"` callbacks from arbitrary threads in
//! its own pool. Each callback receives an `InstanceContext` pointer we
//! supplied at `PrjStartVirtualizing` time; we use that to find our shared
//! state. All mutable state hides behind a `Mutex` (or per-field locks).
//! The state's lifetime is at least as long as the virtualization context —
//! we leak a `Box` at start time and reclaim it on `stop()`.
//!
//! # What this file is *not*
//!
//! It's not a comprehensive ProjFS provider. We implement the callbacks
//! needed for the active-commit workflow: enumerate the manifest's contents
//! as the mount, fetch blobs on demand, observe modifications, observe
//! deletions, and surface renames. Hard links, alternate data streams,
//! sparse files, and pre-convert-to-full notifications are not handled.
//!
//! # Why this can't be fully validated from a Mac dev box
//!
//! ProjFS only links + runs on Windows. The author cross-checks with
//! `cargo check --target x86_64-pc-windows-{gnu,msvc}` to catch syntactic
//! and type errors, but the runtime correctness of marshaling, callback
//! lifetime, and end-to-end mount/edit/commit loops MUST be tested on a
//! real Windows host. Treat this code as carefully written but
//! unvalidated against a live system on first land.

#![cfg(target_os = "windows")]

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use oak_core::{FileMode, Manifest, ManifestEntry, OakError, Result};
use oak_core::{Repository, SqliteRepository};
use tokio::runtime::Handle;
use windows::core::{GUID, HRESULT, PCWSTR};
use windows::Win32::Foundation::{
    BOOLEAN, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER, S_OK,
};
use windows::Win32::Storage::ProjectedFileSystem::{
    PrjAllocateAlignedBuffer, PrjFileNameCompare, PrjFileNameMatch, PrjFillDirEntryBuffer,
    PrjFreeAlignedBuffer, PrjMarkDirectoryAsPlaceholder, PrjStartVirtualizing, PrjStopVirtualizing,
    PrjWriteFileData, PrjWritePlaceholderInfo, PRJ_CALLBACKS, PRJ_CALLBACK_DATA,
    PRJ_CB_DATA_FLAG_ENUM_RESTART_SCAN, PRJ_DIR_ENTRY_BUFFER_HANDLE, PRJ_FILE_BASIC_INFO,
    PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT, PRJ_NOTIFICATION,
    PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_DELETED,
    PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_MODIFIED, PRJ_NOTIFICATION_FILE_OVERWRITTEN,
    PRJ_NOTIFICATION_FILE_RENAMED, PRJ_NOTIFICATION_MAPPING, PRJ_NOTIFICATION_NEW_FILE_CREATED,
    PRJ_NOTIFICATION_PARAMETERS, PRJ_NOTIFY_FILE_HANDLE_CLOSED_FILE_DELETED,
    PRJ_NOTIFY_FILE_HANDLE_CLOSED_FILE_MODIFIED, PRJ_NOTIFY_FILE_OVERWRITTEN,
    PRJ_NOTIFY_FILE_RENAMED, PRJ_NOTIFY_NEW_FILE_CREATED, PRJ_PLACEHOLDER_INFO,
    PRJ_STARTVIRTUALIZING_OPTIONS,
};

use super::state::{self, DirtyEntry, MountConfig, OverlayMeta};

// Win32 error code constants we want as HRESULT in callbacks. The windows
// crate exposes HRESULT-typed wrappers for some, but not for all the codes
// we care about — wrap them once here.
const HR_E_ACCESSDENIED: HRESULT = HRESULT(0x80070005u32 as i32);
const HR_E_HANDLE: HRESULT = HRESULT(0x80070006u32 as i32);

fn hr_from_win32(code: u32) -> HRESULT {
    // FACILITY_WIN32 layout: 0x80070000 | (code & 0xFFFF)
    HRESULT((0x80070000u32 | (code & 0xFFFF)) as i32)
}

// ===========================================================================
// State held for the lifetime of a mount
// ===========================================================================

/// Per-mount state shared across ProjFS callback threads.
///
/// Wrapped in `Arc<Mutex<…>>` for the mutable bits and accessed from
/// `extern "system"` callbacks via a raw pointer we hand ProjFS in
/// `instanceContext`. The pointer is reclaimed on `stop()`.
struct ProjFsState {
    cfg: MountConfig,
    cache: Arc<SqliteRepository>,
    state_dir: PathBuf,
    /// Tokio handle so blocking callbacks can `block_on` async fetches
    /// against the remote. ProjFS callbacks are sync, but our blob fetcher
    /// is async; the runtime is owned by `mount::start`.
    rt: Handle,
    /// API token for remote blob fetches.
    token: Option<String>,
    /// Tree built from the manifest, indexed for fast lookup. Both fields
    /// guarded by the same lock so directory enumeration sees a consistent
    /// snapshot.
    inner: Mutex<InnerState>,
}

/// Mutable per-mount state behind `ProjFsState::inner`.
struct InnerState {
    /// All directory paths we surface (relative to the mount root, no
    /// leading separator). Used to answer "does this directory exist?".
    dirs: std::collections::HashSet<String>,
    /// path → entry. Files only; directories aren't separately stored
    /// because their contents come from `children`.
    files: HashMap<String, FileEntry>,
    /// dir path → list of immediate child names (files + dirs). Empty key
    /// `""` is the root.
    children: HashMap<String, Vec<ChildEntry>>,
    /// Active enumeration sessions. ProjFS issues a Start/Get*/End triple
    /// keyed by an opaque GUID — we keep per-session cursor state here.
    enumerations: HashMap<GUID, EnumSession>,
    /// In-flight overlay metadata. Persisted to disk via the same
    /// `overlay-meta.json` the FUSE backend uses, but with `in_place: true`
    /// because ProjFS hydrates modifications directly to the mount tree.
    overlay: OverlayMeta,
}

/// File metadata cached in memory for quick `GetPlaceholderInfo` answers.
struct FileEntry {
    blob_hash: String,
    size: u64,
    #[allow(dead_code)] // mode propagates via children, not file lookup
    mode: FileMode,
}

#[derive(Clone)]
struct ChildEntry {
    name: String,
    is_dir: bool,
    /// Size of the file (0 for dirs). Used to fill enumeration buffers.
    size: u64,
    /// Whether the child is a regular file or directory; symlinks present
    /// as plain files from ProjFS's perspective.
    mode: FileMode,
}

/// Per-enumeration cursor.
struct EnumSession {
    /// Children of the directory being enumerated, sorted by ProjFS-
    /// compatible (case-insensitive) ordering. Filled at Start time.
    entries: Vec<ChildEntry>,
    /// Index of the next entry to return on the next Get callback.
    cursor: usize,
    /// Optional search expression from the GetDirectoryEnumeration call.
    /// We re-evaluate against entries with `PrjFileNameMatch` per call.
    search: Option<Vec<u16>>,
}

// ===========================================================================
// Public handle returned by start() — drop / stop() owns the unwind
// ===========================================================================

/// Live mount handle. Drop or `stop()` to halt virtualization.
pub struct ProjFsMount {
    /// Raw pointer to the leaked state Box. We need this to free the box
    /// after `PrjStopVirtualizing`. Boxed so callbacks can dereference via
    /// the `instanceContext` pointer ProjFS hands them.
    state_ptr: *mut ProjFsState,
    /// ProjFS handle — `Send` is fine to assert via the wrapper below.
    ctx: ContextHandle,
}

// `PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT` is a typedef'd `*mut c_void` and
// thus not `Send`. We only ever pass it back to ProjFS APIs that are
// thread-safe per the docs, so wrap it.
struct ContextHandle(PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT);
unsafe impl Send for ContextHandle {}
unsafe impl Sync for ContextHandle {}

unsafe impl Send for ProjFsMount {}
unsafe impl Sync for ProjFsMount {}

impl ProjFsMount {
    /// Initialize the virtualization root, register callbacks, and start
    /// servicing. Returns a handle the caller can drop or `stop()`.
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        cfg: MountConfig,
        cache: Arc<SqliteRepository>,
        manifest: &Manifest,
        sizes: &HashMap<String, u64>,
        token: Option<String>,
        rt: Handle,
        state_dir: PathBuf,
    ) -> Result<Self> {
        // Build the in-memory tree from the manifest.
        let mut inner = build_inner(manifest, sizes)?;
        // Reload any prior overlay so a re-mount of the same dest after a
        // crash sees pending edits the user hadn't committed yet.
        if let Ok(prev) = state::load_overlay_meta(&state_dir) {
            inner.overlay = prev;
        }
        let state = Box::new(ProjFsState {
            cfg: cfg.clone(),
            cache,
            state_dir,
            rt,
            token,
            inner: Mutex::new(inner),
        });
        let state_ptr = Box::into_raw(state);

        // Mark the destination as a virtualization root. This is idempotent —
        // the second call on an already-marked dir returns success.
        // We derive a stable GUID per mount so ProjFS knows it's the same
        // provider across remounts.
        let virt_root_id = mount_guid(&cfg.id);
        let dest_w = path_to_wide(&cfg.mount_point);
        let mark_result = unsafe {
            PrjMarkDirectoryAsPlaceholder(
                PCWSTR(dest_w.as_ptr()),
                PCWSTR::null(),
                None,
                &virt_root_id,
            )
        };
        if let Err(e) = mark_result {
            let _ = unsafe { Box::from_raw(state_ptr) };
            return Err(OakError::Io(std::io::Error::other(format!(
                "PrjMarkDirectoryAsPlaceholder: {}",
                e
            ))));
        }

        // Register the notification mapping: every path under the root gets
        // file-modified / created / deleted / renamed notifications. ProjFS
        // looks up the mapping on each event; an empty path is the root.
        let notify_root = wide_from("");
        // Combine the bits using the type's own BitOr impl — these are
        // PRJ_NOTIFY_TYPES wrapper structs, not raw integers.
        let mask = PRJ_NOTIFY_NEW_FILE_CREATED
            | PRJ_NOTIFY_FILE_OVERWRITTEN
            | PRJ_NOTIFY_FILE_HANDLE_CLOSED_FILE_MODIFIED
            | PRJ_NOTIFY_FILE_HANDLE_CLOSED_FILE_DELETED
            | PRJ_NOTIFY_FILE_RENAMED;
        // The mapping must outlive the call to PrjStartVirtualizing — keep
        // it in a stack local that lives the whole function.
        let mut mappings = [PRJ_NOTIFICATION_MAPPING {
            NotificationBitMask: mask,
            NotificationRoot: PCWSTR(notify_root.as_ptr()),
        }];

        let callbacks = PRJ_CALLBACKS {
            StartDirectoryEnumerationCallback: Some(start_directory_enumeration_cb),
            EndDirectoryEnumerationCallback: Some(end_directory_enumeration_cb),
            GetDirectoryEnumerationCallback: Some(get_directory_enumeration_cb),
            GetPlaceholderInfoCallback: Some(get_placeholder_info_cb),
            GetFileDataCallback: Some(get_file_data_cb),
            QueryFileNameCallback: Some(query_file_name_cb),
            NotificationCallback: Some(notification_cb),
            // CancelCommand is optional — we don't service long-running
            // operations so there's nothing to cancel.
            CancelCommandCallback: None,
        };

        let options = PRJ_STARTVIRTUALIZING_OPTIONS {
            Flags: Default::default(),
            PoolThreadCount: 0,       // 0 = ProjFS picks a default
            ConcurrentThreadCount: 0, // 0 = ProjFS picks a default
            NotificationMappings: mappings.as_mut_ptr(),
            NotificationMappingsCount: mappings.len() as u32,
        };

        let ctx = match unsafe {
            PrjStartVirtualizing(
                PCWSTR(dest_w.as_ptr()),
                &callbacks,
                Some(state_ptr as *const _),
                Some(&options),
            )
        } {
            Ok(c) => c,
            Err(e) => {
                let _ = unsafe { Box::from_raw(state_ptr) };
                return Err(OakError::Io(std::io::Error::other(format!(
                    "PrjStartVirtualizing: {}",
                    e
                ))));
            }
        };

        Ok(ProjFsMount {
            state_ptr,
            ctx: ContextHandle(ctx),
        })
    }

    /// Halt virtualization. Idempotent — calling twice is a no-op.
    pub fn stop(self) -> Result<()> {
        // Drop runs the destructor below.
        drop(self);
        Ok(())
    }
}

impl Drop for ProjFsMount {
    fn drop(&mut self) {
        // Stop virtualization first so no new callbacks can fire against
        // our state, then reclaim the leaked Box.
        if !self.ctx.0.is_invalid() {
            unsafe { PrjStopVirtualizing(self.ctx.0) };
        }
        if !self.state_ptr.is_null() {
            // Persist whatever overlay state is in memory before dropping.
            // Best-effort: ignore IO errors at shutdown.
            unsafe {
                let st = &*self.state_ptr;
                if let Ok(inner) = st.inner.lock() {
                    let _ = state::save_overlay_meta(&st.state_dir, &inner.overlay);
                }
                let _ = Box::from_raw(self.state_ptr);
            }
        }
    }
}

// ===========================================================================
// Building the in-memory tree from a manifest
// ===========================================================================

fn build_inner(manifest: &Manifest, sizes: &HashMap<String, u64>) -> Result<InnerState> {
    let mut dirs = std::collections::HashSet::new();
    dirs.insert(String::new()); // root
    let mut files = HashMap::new();
    let mut children: HashMap<String, Vec<ChildEntry>> = HashMap::new();

    for entry in &manifest.entries {
        let size = sizes.get(entry.blob_hash.as_str()).copied().unwrap_or(0);
        files.insert(
            entry.path.clone(),
            FileEntry {
                blob_hash: entry.blob_hash.as_str().to_string(),
                size,
                mode: entry.mode,
            },
        );

        // Walk every prefix of the path, recording intermediate directories
        // and registering each component as a child of its parent.
        let parts: Vec<&str> = entry.path.split('/').collect();
        let mut cumulative = String::new();
        for (i, part) in parts.iter().enumerate() {
            let parent = cumulative.clone();
            if !cumulative.is_empty() {
                cumulative.push('/');
            }
            cumulative.push_str(part);

            let is_last = i + 1 == parts.len();
            let is_dir = !is_last;
            let entry_size = if is_dir { 0 } else { size };
            let entry_mode = if is_dir {
                FileMode::Regular
            } else {
                entry.mode
            };

            // Record directory paths so we can answer "is this a dir" later.
            if is_dir {
                dirs.insert(cumulative.clone());
            }

            // Append to parent's child list, deduplicating on name.
            let kids = children.entry(parent).or_default();
            if !kids.iter().any(|c| c.name == *part) {
                kids.push(ChildEntry {
                    name: part.to_string(),
                    is_dir,
                    size: entry_size,
                    mode: entry_mode,
                });
            }
        }
    }

    Ok(InnerState {
        dirs,
        files,
        children,
        enumerations: HashMap::new(),
        overlay: OverlayMeta::default(),
    })
}

// ===========================================================================
// Helpers: path / string conversions and HRESULT plumbing
// ===========================================================================

/// Convert a Rust `&Path` to a NUL-terminated wide buffer suitable for
/// `PCWSTR`. The returned `Vec<u16>` must outlive any `PCWSTR` derived
/// from it (callers stash it in a local before passing the pointer in).
fn path_to_wide(p: &std::path::Path) -> Vec<u16> {
    let mut v: Vec<u16> = p.as_os_str().encode_wide().collect();
    v.push(0);
    v
}

fn wide_from(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = OsStr::new(s).encode_wide().collect();
    v.push(0);
    v
}

/// Decode a NUL-terminated UTF-16 PCWSTR to a Rust String. Stops at the
/// first NUL or after a generous safety cap to avoid runaway reads on
/// malformed input.
unsafe fn pcwstr_to_string(p: PCWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    let mut cur = p.as_ptr();
    while !cur.is_null() && unsafe { *cur } != 0 && len < 32 * 1024 {
        len += 1;
        cur = unsafe { cur.add(1) };
    }
    let slice = unsafe { std::slice::from_raw_parts(p.as_ptr(), len) };
    OsString::from_wide(slice).to_string_lossy().into_owned()
}

/// Translate a ProjFS-style relative path (uses `\` on the wire) into the
/// `/`-separated form Oak's manifests use everywhere else.
fn projfs_path_to_oak(s: &str) -> String {
    s.replace('\\', "/")
}

/// Derive a stable per-mount GUID from the mount id (a UUIDv4 simple-form
/// string). We feed the first 32 hex chars into the GUID's data fields.
fn mount_guid(id: &str) -> GUID {
    let bytes: [u8; 16] = uuid_bytes(id);
    GUID::from_values(
        u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        u16::from_be_bytes([bytes[4], bytes[5]]),
        u16::from_be_bytes([bytes[6], bytes[7]]),
        [
            bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
        ],
    )
}

fn uuid_bytes(id: &str) -> [u8; 16] {
    // Mount ids are uuidv4 in simple (no-dash) form. Tolerate either shape.
    let cleaned: String = id.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    let mut bytes = [0u8; 16];
    if cleaned.len() < 32 {
        return bytes;
    }
    for (i, b) in bytes.iter_mut().enumerate() {
        let pair = &cleaned[i * 2..i * 2 + 2];
        *b = u8::from_str_radix(pair, 16).unwrap_or(0);
    }
    bytes
}

// ===========================================================================
// Callback dispatch: each `extern "system"` thunks back into a method
// on `ProjFsState`. Locks are taken per-call; see comments for race notes.
// ===========================================================================

/// Reconstitute the `&ProjFsState` reference from a callback's
/// `instanceContext` pointer. Safe as long as the pointer is alive — we
/// only free the box in `Drop` after `PrjStopVirtualizing` returns, which
/// guarantees no callbacks are in flight.
unsafe fn state_from_data(data: *const PRJ_CALLBACK_DATA) -> &'static ProjFsState {
    let data = unsafe { &*data };
    let ptr = data.InstanceContext as *const ProjFsState;
    unsafe { &*ptr }
}

extern "system" fn start_directory_enumeration_cb(
    callback_data: *const PRJ_CALLBACK_DATA,
    enumeration_id: *const GUID,
) -> HRESULT {
    let st = unsafe { state_from_data(callback_data) };
    let data = unsafe { &*callback_data };
    let dir_path = projfs_path_to_oak(&unsafe { pcwstr_to_string(data.FilePathName) });

    let mut inner = match st.inner.lock() {
        Ok(g) => g,
        Err(_) => return HR_E_ACCESSDENIED,
    };

    let kids = inner.children.get(&dir_path).cloned().unwrap_or_default();
    // ProjFS expects entries sorted by `PrjFileNameCompare`. Fall back to
    // case-insensitive byte compare if the API is unavailable for any reason.
    let mut sorted = kids;
    sorted.sort_by(|a, b| compare_names(&a.name, &b.name));
    let session = EnumSession {
        entries: sorted,
        cursor: 0,
        search: None,
    };
    inner
        .enumerations
        .insert(unsafe { *enumeration_id }, session);
    S_OK
}

extern "system" fn end_directory_enumeration_cb(
    callback_data: *const PRJ_CALLBACK_DATA,
    enumeration_id: *const GUID,
) -> HRESULT {
    let st = unsafe { state_from_data(callback_data) };
    if let Ok(mut inner) = st.inner.lock() {
        inner.enumerations.remove(unsafe { &*enumeration_id });
    }
    S_OK
}

extern "system" fn get_directory_enumeration_cb(
    callback_data: *const PRJ_CALLBACK_DATA,
    enumeration_id: *const GUID,
    search_expression: PCWSTR,
    dir_entry_buffer_handle: PRJ_DIR_ENTRY_BUFFER_HANDLE,
) -> HRESULT {
    let st = unsafe { state_from_data(callback_data) };
    let data = unsafe { &*callback_data };
    // ProjFS sets PRJ_CB_DATA_FLAG_ENUM_RESTART_SCAN when the OS wants
    // enumeration rewound. The flags type is a transparent wrapper, so we
    // bitand-and-compare via the type's own ops.
    let restart = (data.Flags.0 & PRJ_CB_DATA_FLAG_ENUM_RESTART_SCAN.0) != 0;

    let mut inner = match st.inner.lock() {
        Ok(g) => g,
        Err(_) => return HR_E_ACCESSDENIED,
    };

    let session = match inner.enumerations.get_mut(unsafe { &*enumeration_id }) {
        Some(s) => s,
        None => return HR_E_HANDLE,
    };
    if restart {
        session.cursor = 0;
    }
    if !search_expression.is_null() {
        let search_str = unsafe { pcwstr_to_string(search_expression) };
        // Stash a wide copy for PrjFileNameMatch to consume on each entry.
        session.search = Some(wide_from(&search_str));
    }

    while session.cursor < session.entries.len() {
        let entry = &session.entries[session.cursor];
        let name_w = wide_from(&entry.name);
        // Filter by search expression if present. PrjFileNameMatch returns
        // BOOLEAN — `as_bool()` converts to Rust bool.
        if let Some(ref pat) = session.search {
            let m = unsafe { PrjFileNameMatch(PCWSTR(name_w.as_ptr()), PCWSTR(pat.as_ptr())) };
            if !m.as_bool() {
                session.cursor += 1;
                continue;
            }
        }
        let file_info = build_file_basic_info(entry);
        let fill_result = unsafe {
            PrjFillDirEntryBuffer(
                PCWSTR(name_w.as_ptr()),
                Some(&file_info),
                dir_entry_buffer_handle,
            )
        };
        match fill_result {
            Ok(()) => {
                session.cursor += 1;
            }
            Err(e) => {
                // ERROR_INSUFFICIENT_BUFFER means "no room for more this call;
                // ProjFS will call us again with a fresh buffer". We DON'T
                // advance the cursor in that case — return S_OK so ProjFS
                // knows the previous entry was valid, and on next call we'll
                // retry this same entry.
                if e.code() == hr_from_win32(ERROR_INSUFFICIENT_BUFFER.0) {
                    return S_OK;
                }
                return e.code();
            }
        }
    }
    S_OK
}

extern "system" fn get_placeholder_info_cb(callback_data: *const PRJ_CALLBACK_DATA) -> HRESULT {
    let st = unsafe { state_from_data(callback_data) };
    let data = unsafe { &*callback_data };
    let path = projfs_path_to_oak(&unsafe { pcwstr_to_string(data.FilePathName) });

    let inner = match st.inner.lock() {
        Ok(g) => g,
        Err(_) => return HR_E_ACCESSDENIED,
    };

    let (info, found) = if let Some(file) = inner.files.get(&path) {
        (make_placeholder(file.size, false), true)
    } else if inner.dirs.contains(&path) {
        (make_placeholder(0, true), true)
    } else {
        (unsafe { std::mem::zeroed() }, false)
    };

    if !found {
        return hr_from_win32(ERROR_FILE_NOT_FOUND.0);
    }

    let path_w = wide_from(&path);
    match unsafe {
        PrjWritePlaceholderInfo(
            data.NamespaceVirtualizationContext,
            PCWSTR(path_w.as_ptr()),
            &info,
            std::mem::size_of::<PRJ_PLACEHOLDER_INFO>() as u32,
        )
    } {
        Ok(()) => S_OK,
        Err(e) => e.code(),
    }
}

extern "system" fn get_file_data_cb(
    callback_data: *const PRJ_CALLBACK_DATA,
    byte_offset: u64,
    length: u32,
) -> HRESULT {
    let st = unsafe { state_from_data(callback_data) };
    let data = unsafe { &*callback_data };
    let path = projfs_path_to_oak(&unsafe { pcwstr_to_string(data.FilePathName) });

    let blob_hash = {
        let inner = match st.inner.lock() {
            Ok(g) => g,
            Err(_) => return HR_E_ACCESSDENIED,
        };
        match inner.files.get(&path) {
            Some(f) => f.blob_hash.clone(),
            None => return hr_from_win32(ERROR_FILE_NOT_FOUND.0),
        }
    };

    // Fetch from local cache, or remote if missing. Remote fetch is async;
    // bridge to sync via the runtime handle. We grab the slice we need
    // from the full blob — ProjFS may ask for sub-ranges of large files
    // (4GB+ asset files are realistic for game repos).
    let fetch_result: Result<Vec<u8>> = (|| {
        let hash = oak_core::Hash(blob_hash.clone());
        if let Some(blob) = st.cache.get_blob(&hash)? {
            return Ok(blob.content);
        }
        // Pull it down through the shared blob_fetch helper. block_on the
        // future on the runtime owned by `mount::start`; ProjFS will park
        // this thread until the fetch completes, which is correct — the
        // user's read syscall is waiting on this byte range.
        let cache = st.cache.clone();
        let cfg = st.cfg.clone();
        let token = st.token.clone();
        let hash_for_fetch = hash.clone();
        st.rt.block_on(async move {
            crate::commands::blob_fetch::ensure_blobs_local(
                cache.as_ref(),
                &cfg.remote_url,
                &cfg.owner,
                &cfg.repo,
                token.as_deref(),
                std::slice::from_ref(&hash_for_fetch),
            )
            .await
        })?;
        let blob = st
            .cache
            .get_blob(&hash)?
            .ok_or_else(|| OakError::BlobNotFound(blob_hash.clone()))?;
        Ok(blob.content)
    })();
    let bytes = match fetch_result {
        Ok(b) => b,
        // Content the server withheld under path-based permissions reads as
        // access-denied (the fix is an org-admin grant); anything else keeps
        // the historical not-found mapping.
        Err(OakError::RestrictedContent(_)) => return hr_from_win32(ERROR_ACCESS_DENIED.0),
        Err(_) => return hr_from_win32(ERROR_FILE_NOT_FOUND.0),
    };

    // Trim to the (offset, length) ProjFS asked for. Out-of-range requests
    // are an error.
    let start = byte_offset as usize;
    let end = start.saturating_add(length as usize);
    if start >= bytes.len() {
        return hr_from_win32(ERROR_INSUFFICIENT_BUFFER.0);
    }
    let slice = &bytes[start..end.min(bytes.len())];

    // PrjWriteFileData requires aligned memory — use ProjFS's own allocator.
    unsafe {
        let buf = PrjAllocateAlignedBuffer(data.NamespaceVirtualizationContext, slice.len());
        if buf.is_null() {
            return hr_from_win32(ERROR_INSUFFICIENT_BUFFER.0);
        }
        std::ptr::copy_nonoverlapping(slice.as_ptr(), buf as *mut u8, slice.len());
        let r = PrjWriteFileData(
            data.NamespaceVirtualizationContext,
            &data.DataStreamId,
            buf,
            byte_offset,
            slice.len() as u32,
        );
        PrjFreeAlignedBuffer(buf);
        match r {
            Ok(()) => S_OK,
            Err(e) => e.code(),
        }
    }
}

extern "system" fn query_file_name_cb(callback_data: *const PRJ_CALLBACK_DATA) -> HRESULT {
    // Default behavior: tell ProjFS the file exists if we can find it
    // (case-insensitively) in our manifest. Returning ERROR_FILE_NOT_FOUND
    // tells ProjFS the path is virtual-not-present, so the OS gets a clean
    // "not found" instead of waiting for a placeholder fetch.
    let st = unsafe { state_from_data(callback_data) };
    let data = unsafe { &*callback_data };
    let path = projfs_path_to_oak(&unsafe { pcwstr_to_string(data.FilePathName) });
    let inner = match st.inner.lock() {
        Ok(g) => g,
        Err(_) => return HR_E_ACCESSDENIED,
    };
    if inner.files.contains_key(&path) || inner.dirs.contains(&path) {
        S_OK
    } else {
        hr_from_win32(ERROR_FILE_NOT_FOUND.0)
    }
}

extern "system" fn notification_cb(
    callback_data: *const PRJ_CALLBACK_DATA,
    is_directory: BOOLEAN,
    notification: PRJ_NOTIFICATION,
    destination_file_name: PCWSTR,
    _operation_parameters: *mut PRJ_NOTIFICATION_PARAMETERS,
) -> HRESULT {
    let st = unsafe { state_from_data(callback_data) };
    let data = unsafe { &*callback_data };
    let src_path = projfs_path_to_oak(&unsafe { pcwstr_to_string(data.FilePathName) });
    let dest_path = if destination_file_name.is_null() {
        String::new()
    } else {
        projfs_path_to_oak(&unsafe { pcwstr_to_string(destination_file_name) })
    };

    let mut inner = match st.inner.lock() {
        Ok(g) => g,
        Err(_) => return HR_E_ACCESSDENIED,
    };

    // We only care about file-level events. Directory notifications still
    // arrive (mkdir / rmdir) but the user-facing model is files;
    // directories are derived from manifest entries, so we leave dir
    // events to be reflected on commit by the new file paths.
    if is_directory.as_bool() {
        return S_OK;
    }

    match notification {
        PRJ_NOTIFICATION_NEW_FILE_CREATED
        | PRJ_NOTIFICATION_FILE_OVERWRITTEN
        | PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_MODIFIED => {
            inner.overlay.dirty.insert(
                src_path.clone(),
                DirtyEntry {
                    overlay_file: String::new(),
                    mode: "regular".into(),
                    in_place: true,
                },
            );
        }
        PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_DELETED => {
            inner.overlay.deletions.push(src_path.clone());
            inner.overlay.dirty.remove(&src_path);
        }
        PRJ_NOTIFICATION_FILE_RENAMED => {
            if !dest_path.is_empty() {
                inner.overlay.renames.insert(src_path.clone(), dest_path);
            }
        }
        _ => {}
    }
    // Persist on every change so a crash mid-session preserves state.
    let _ = state::save_overlay_meta(&st.state_dir, &inner.overlay);
    S_OK
}

// ===========================================================================
// File-info / placeholder builders
// ===========================================================================

fn build_file_basic_info(entry: &ChildEntry) -> PRJ_FILE_BASIC_INFO {
    let mut info: PRJ_FILE_BASIC_INFO = unsafe { std::mem::zeroed() };
    info.IsDirectory = BOOLEAN::from(entry.is_dir);
    info.FileSize = entry.size as i64;
    // Timestamps left zero: NTFS will pick up real mtimes once files
    // hydrate. Setting fake creation dates would only mislead users.
    if !entry.is_dir {
        info.FileAttributes = match entry.mode {
            FileMode::Executable | FileMode::Regular => 0x80, // FILE_ATTRIBUTE_NORMAL
            FileMode::Symlink => 0x400,                       // FILE_ATTRIBUTE_REPARSE_POINT
        };
    } else {
        info.FileAttributes = 0x10; // FILE_ATTRIBUTE_DIRECTORY
    }
    info
}

fn make_placeholder(size: u64, is_dir: bool) -> PRJ_PLACEHOLDER_INFO {
    let mut info: PRJ_PLACEHOLDER_INFO = unsafe { std::mem::zeroed() };
    info.FileBasicInfo.IsDirectory = BOOLEAN::from(is_dir);
    info.FileBasicInfo.FileSize = size as i64;
    info.FileBasicInfo.FileAttributes = if is_dir { 0x10 } else { 0x80 };
    info
}

// ===========================================================================
// Name comparison fallback (PrjFileNameCompare returns int — < 0, 0, > 0)
// ===========================================================================

fn compare_names(a: &str, b: &str) -> std::cmp::Ordering {
    let aw = wide_from(a);
    let bw = wide_from(b);
    let r = unsafe { PrjFileNameCompare(PCWSTR(aw.as_ptr()), PCWSTR(bw.as_ptr())) };
    match r {
        n if n < 0 => std::cmp::Ordering::Less,
        0 => std::cmp::Ordering::Equal,
        _ => std::cmp::Ordering::Greater,
    }
}

#[cfg(test)]
mod tests {
    // The real ProjFS surface only links on Windows, so logic that *can*
    // be unit-tested cross-platform lives in pure helpers. Tests in this
    // module run on Windows targets and exercise GUID derivation +
    // path/string conversions; runtime behavior of callbacks needs a
    // real ProjFS-enabled host.
    use super::*;

    #[test]
    fn projfs_path_to_oak_normalizes_separators() {
        assert_eq!(projfs_path_to_oak("src\\main.rs"), "src/main.rs");
        assert_eq!(projfs_path_to_oak("a\\b\\c.txt"), "a/b/c.txt");
        assert_eq!(projfs_path_to_oak("README.md"), "README.md");
    }

    #[test]
    fn mount_guid_is_stable_per_id() {
        let id = "0123456789abcdef0123456789abcdef";
        let g1 = mount_guid(id);
        let g2 = mount_guid(id);
        assert_eq!(g1, g2);
    }

    #[test]
    fn mount_guid_differs_across_ids() {
        let g1 = mount_guid("0123456789abcdef0123456789abcdef");
        let g2 = mount_guid("ffffffffffffffffffffffffffffffff");
        assert_ne!(g1, g2);
    }
}
