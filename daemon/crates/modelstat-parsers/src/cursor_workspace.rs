//! Which FOLDER each Cursor conversation was held in.
//!
//! Cursor's chat store is ONE global key/value DB and it names no folder: a
//! bubble record carries its text, its ids and its timestamp, and nothing about
//! where the work happened. So [`crate::cursor`] shipped `cwd: None` on every
//! event from the day it was written — and `cwd` is the ONE input
//! `resolve_authoritative_git` needs. With no cwd it never runs, no folder is
//! ever probed for its remote, and no Cursor session has ever reached the server
//! carrying a repository — not one, on any device, for the parser's whole
//! life.
//!
//! Cursor does record the folder, one level up from the chat store. Every folder
//! the editor opens gets a `workspaceStorage/<hash>/` directory holding
//!
//!   * `workspace.json` — `{"folder": "file:///abs/path"}`, the folder itself;
//!   * `state.vscdb` — whose `composer.composerData` key lists `allComposers[]`,
//!     the conversations that folder holds.
//!
//! Joining those two IS the map, and it is Cursor's OWN bookkeeping rather than
//! an inference: nothing here reads a directory name, a path shape, or a word of
//! any message. Measured against a real Cursor store 2026-09-01 — 59 of 69
//! conversations, 11,439 of 11,761 bubbles (97%), and not one conversation
//! claimed by two folders.
//!
//! ── What this module refuses to decide ──
//!
//! A conversation two folders both claim gets NO folder. One of the two is
//! wrong, nothing here can say which, and placing the session in a repository it
//! never ran in is exactly the failure the server's repository identity was
//! rebuilt to end. It is counted instead.
//!
//! An unplaced conversation ships `cwd: None`, precisely as before: an honest
//! "not known", never a stand-in. What must NOT happen is the third case — an
//! index this module can no longer read looking identical to a machine that has
//! no index. That silence is how Cursor's `ai_code_hashes` table sat dead for
//! weeks, so [`WorkspaceScan`] separates the two and [`WorkspaceFolders::report`]
//! says which one fired.
//!
//! ── The grain this can reach, and no finer ──
//!
//! One folder per CONVERSATION, stamped on every event of it. A conversation
//! that genuinely worked across two repositories will therefore be attributed
//! entirely to one, and the task's `repo_ids` array — which exists precisely
//! because a task does touch more than one checkout — will hold a single entry
//! for it.
//!
//! That is Cursor's limit, not a choice made here. Its index records the folder
//! a WINDOW was opened on, and a conversation lives in one window; nothing in
//! the store says where an individual message went. Checked against a real
//! store: `relevantFiles`, `recentlyViewedFiles`, `attachedFolders` and
//! `gitDiffs` are empty on every one of its 11,761 bubbles, so there is no
//! finer signal to read even best-effort.
//!
//! The transcript parsers have no such ceiling — Claude Code and Codex state a
//! cwd per LINE, so their arrays fill honestly. A Cursor session's does not, and
//! a reader comparing the two should know which is which.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};

/// Cursor's `ItemTable` key for the conversation list a folder holds.
const COMPOSER_DATA_KEY: &str = "composer.composerData";

/// Cursor's conversation → folder map, plus what reading it could not decide.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceFolders {
    by_conversation: BTreeMap<String, String>,
    scan: WorkspaceScan,
}

/// What one read of the workspace index saw. Every field exists to keep two
/// different silences apart — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceScan {
    /// `workspaceStorage/` was there to read at all. FALSE means Cursor has
    /// never opened a folder on this machine, which must never read like an
    /// index we read and understood nothing in.
    pub storage_present: bool,
    /// Per-folder directories walked.
    pub workspaces: u64,
    /// …of which named exactly one local folder.
    pub folders: u64,
    /// …of which named none: an empty editor window, or a multi-root
    /// `.code-workspace` holding several folders at once, for which there is no
    /// single honest answer.
    pub folderless: u64,
    /// Conversations placed in exactly one folder — the map's size.
    pub placed: u64,
    /// Conversations two or more folders both claimed. Dropped, never guessed.
    pub ambiguous: u64,
    /// Directories whose `state.vscdb` is present but yielded no conversation
    /// list. The `ai_code_hashes` shape: the artefact is there and we understood
    /// none of it.
    pub unreadable: u64,
}

impl WorkspaceFolders {
    /// The folder Cursor recorded this conversation under.
    ///
    /// `None` is an honest "the index does not place it" — the caller ships no
    /// cwd, exactly as it did before this module existed.
    #[must_use]
    pub fn folder(&self, conversation_id: &str) -> Option<&str> {
        self.by_conversation
            .get(conversation_id)
            .map(String::as_str)
    }

    #[must_use]
    pub fn scan(&self) -> &WorkspaceScan {
        &self.scan
    }

    /// Say what the index could not answer, given how many conversations the
    /// chat store actually held.
    ///
    /// `conversations_in_store` is what makes the loudest check possible: an
    /// index that placed NONE of a non-empty store is a schema move, not an
    /// empty machine, and only the caller knows the store's side of that.
    pub fn report(&self, chat_store: &str, conversations_in_store: u64) {
        let s = &self.scan;
        if !s.storage_present {
            warn_once(
                chat_store,
                "no-workspace-storage",
                &format!(
                    "cursor: no workspaceStorage beside the chat store at {chat_store} — no \
                     conversation can be given a folder, so no Cursor session will carry a \
                     repository"
                ),
            );
            return;
        }
        if conversations_in_store > 0 && s.placed == 0 {
            warn_once(
                chat_store,
                "placed-none",
                &format!(
                    "cursor: the workspace index at {chat_store} placed NONE of \
                     {conversations_in_store} conversations ({} directories, {} naming a folder) \
                     — Cursor has most likely moved its storage schema",
                    s.workspaces, s.folders
                ),
            );
        }
        if s.unreadable > 0 {
            warn_once(
                chat_store,
                "unreadable-stores",
                &format!(
                    "cursor: {} of {} workspace directories beside {chat_store} hold a \
                     state.vscdb with no readable `{COMPOSER_DATA_KEY}` — their conversations \
                     carry no folder",
                    s.unreadable, s.workspaces
                ),
            );
        }
        if s.ambiguous > 0 {
            warn_once(
                chat_store,
                "ambiguous",
                &format!(
                    "cursor: {} conversations in {chat_store} are claimed by more than one \
                     folder — each ships with no folder rather than a guessed one",
                    s.ambiguous
                ),
            );
        }
    }
}

/// Read Cursor's workspace index beside `chat_store` — its global
/// `<data-dir>/User/globalStorage/state.vscdb`.
#[must_use]
pub fn read(chat_store: &str) -> WorkspaceFolders {
    let mut scan = WorkspaceScan::default();
    let Some(dir) = workspace_storage_dir(chat_store) else {
        return WorkspaceFolders {
            by_conversation: BTreeMap::new(),
            scan,
        };
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return WorkspaceFolders {
            by_conversation: BTreeMap::new(),
            scan,
        };
    };
    scan.storage_present = true;

    // Every folder that claims a conversation, so a second claim is VISIBLE
    // rather than silently overwriting the first.
    let mut claims: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for entry in entries.flatten() {
        let ws = entry.path();
        if !ws.is_dir() {
            continue;
        }
        scan.workspaces += 1;
        let Some(folder) = folder_of(&ws) else {
            scan.folderless += 1;
            continue;
        };
        scan.folders += 1;
        let store = ws.join("state.vscdb");
        if !store.is_file() {
            continue;
        }
        let Some(conversations) = conversations_of(&store) else {
            scan.unreadable += 1;
            continue;
        };
        for id in conversations {
            claims.entry(id).or_default().insert(folder.clone());
        }
    }

    let mut by_conversation = BTreeMap::new();
    for (id, folders) in claims {
        let mut it = folders.into_iter();
        match (it.next(), it.next()) {
            (Some(only), None) => {
                by_conversation.insert(id, only);
            }
            // Two folders, two different answers, no way to choose. Counted.
            _ => scan.ambiguous += 1,
        }
    }
    scan.placed = by_conversation.len() as u64;
    WorkspaceFolders {
        by_conversation,
        scan,
    }
}

/// Cursor's per-folder storage, a sibling of its global chat store:
/// `<data-dir>/User/globalStorage/state.vscdb` ↔ `<data-dir>/User/workspaceStorage`.
fn workspace_storage_dir(chat_store: &str) -> Option<PathBuf> {
    let user = Path::new(chat_store).parent()?.parent()?;
    Some(user.join("workspaceStorage"))
}

/// The single local folder a workspace directory names, or `None`.
///
/// `None` covers an empty editor window (no `workspace.json`) and a multi-root
/// `.code-workspace` (a `"workspace"` key, several folders at once). Both are
/// real states with no single answer, and inventing one would put a session in a
/// folder it never ran in.
fn folder_of(ws: &Path) -> Option<String> {
    let text = std::fs::read_to_string(ws.join("workspace.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    path_from_file_uri(json.get("folder")?.as_str()?)
}

/// The conversation ids a workspace's own store lists, or `None` when it holds
/// no readable list at all.
///
/// The two are DIFFERENT states and folding them together would hide the one
/// that matters: an empty list is a folder that was opened and never chatted in,
/// while an unreadable one is an artefact this build no longer understands.
///
/// Read through the same allowlisted single-key probe the auth detection uses —
/// never a prefix scan, because `cursorAuth/accessToken` lives in this very
/// table and a sweep would pull live credentials into memory on its way past.
fn conversations_of(store: &Path) -> Option<Vec<String>> {
    let raw = crate::cursor::read_item_table(&store.to_string_lossy(), &[COMPOSER_DATA_KEY]);
    let json: serde_json::Value = serde_json::from_str(raw.get(COMPOSER_DATA_KEY)?).ok()?;
    Some(
        json.get("allComposers")?
            .as_array()?
            .iter()
            .filter_map(|c| c.get("composerId")?.as_str())
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// The absolute path a `file://` URI names.
///
/// Cursor writes the folder as a URI, so a path holding a space, a `#`, or any
/// non-ASCII character arrives percent-encoded (`file:///Users/me/My%20Code`).
/// Decoding is not cosmetic: the undecoded string is a DIFFERENT path, one that
/// exists on no machine, and probing it would find no `.git` and report no
/// repository — a wrong answer wearing an absence's clothes.
///
/// `None` for anything that is not a local path: another scheme, or a non-empty
/// authority (`file://server/share`, a network location with no local form).
///
/// Public so the daemon's end-to-end test can build a URI and assert it reads
/// back as the path it was built from — the Windows arm of that round trip is
/// the one a macOS or Linux run would otherwise never exercise.
pub fn path_from_file_uri(uri: &str) -> Option<String> {
    let path = percent_decode(uri.strip_prefix("file://")?.strip_prefix('/')?)?;
    // A Windows URI carries its drive in the first segment
    // (`file:///c%3A/Users/…` → `c:/Users/…`) and is already absolute. A POSIX
    // one had its leading slash consumed above, and needs it back.
    let bytes = path.as_bytes();
    let windows_drive =
        bytes.first().is_some_and(u8::is_ascii_alphabetic) && bytes.get(1) == Some(&b':');
    Some(if windows_drive {
        path
    } else {
        format!("/{path}")
    })
}

/// Percent-decode a URI path.
///
/// `None` on a malformed escape rather than a half-decoded string: a path
/// decoded partway is a different path, and this module's whole job is to hand
/// out a folder that is either right or absent.
fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            out.push(u8::from_str_radix(s.get(i + 1..i + 3)?, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// One warn per (store, condition) per process.
///
/// Per PROCESS, not per parse: the daemon re-scans on a timer, so a per-parse
/// warn would reprint the same line every cycle forever and train its reader to
/// ignore the log. Same shape, and the same reason, as `skips::warn_new_kind`.
fn warn_once(chat_store: &str, condition: &str, message: &str) {
    static SEEN: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(BTreeSet::new()));
    let mut guard = seen.lock().unwrap_or_else(PoisonError::into_inner);
    if guard.insert(format!("{chat_store}\u{0}{condition}")) {
        modelstat_log::log_warn!("{message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    /// A Cursor data directory, laid out exactly as the editor lays one out —
    /// the global chat store and the per-folder index as siblings under `User`.
    struct Install(PathBuf);

    impl Install {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("modelstat-cursor-ws-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("User/globalStorage")).unwrap();
            std::fs::write(root.join("User/globalStorage/state.vscdb"), b"").unwrap();
            Self(root)
        }

        fn chat_store(&self) -> String {
            self.0
                .join("User/globalStorage/state.vscdb")
                .to_string_lossy()
                .into_owned()
        }

        /// One workspace directory: the `workspace.json` body it carries (None =
        /// none at all), and the conversation ids its own store lists (None = no
        /// store; `Some(&[])` = a store that lists none).
        fn workspace(&self, name: &str, workspace_json: Option<&str>, held: Option<&[&str]>) {
            let ws = self.0.join("User/workspaceStorage").join(name);
            std::fs::create_dir_all(&ws).unwrap();
            if let Some(body) = workspace_json {
                std::fs::write(ws.join("workspace.json"), body).unwrap();
            }
            let Some(ids) = held else { return };
            let conn = Connection::open(ws.join("state.vscdb")).unwrap();
            conn.execute(
                "CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value BLOB)",
                [],
            )
            .unwrap();
            let all: Vec<_> = ids
                .iter()
                .map(|id| serde_json::json!({ "composerId": id }))
                .collect();
            conn.execute(
                "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
                rusqlite::params![
                    COMPOSER_DATA_KEY,
                    serde_json::json!({ "allComposers": all }).to_string()
                ],
            )
            .unwrap();
        }

        /// A `workspace.json` naming one folder by absolute path.
        fn folder(path: &str) -> String {
            format!(r#"{{"folder": "file://{path}"}}"#)
        }
    }

    impl Drop for Install {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_folder_names_every_conversation_it_holds() {
        let i = Install::new("basic");
        i.workspace(
            "w1",
            Some(&Install::folder("/src/api")),
            Some(&["conv-a", "conv-b"]),
        );
        i.workspace("w2", Some(&Install::folder("/src/web")), Some(&["conv-c"]));
        let f = read(&i.chat_store());

        assert_eq!(f.folder("conv-a"), Some("/src/api"));
        assert_eq!(f.folder("conv-b"), Some("/src/api"));
        assert_eq!(f.folder("conv-c"), Some("/src/web"));
        // A conversation the index never mentions is absent, not defaulted.
        assert_eq!(f.folder("conv-unknown"), None);
        assert_eq!(
            *f.scan(),
            WorkspaceScan {
                storage_present: true,
                workspaces: 2,
                folders: 2,
                folderless: 0,
                placed: 3,
                ambiguous: 0,
                unreadable: 0,
            }
        );
    }

    #[test]
    fn a_conversation_two_folders_claim_is_placed_nowhere() {
        let i = Install::new("ambiguous");
        i.workspace("w1", Some(&Install::folder("/src/api")), Some(&["shared"]));
        i.workspace("w2", Some(&Install::folder("/src/web")), Some(&["shared"]));
        let f = read(&i.chat_store());

        // One of the two is wrong and nothing here can say which. Naming either
        // would put the session in a repository it never ran in.
        assert_eq!(f.folder("shared"), None);
        assert_eq!(f.scan().ambiguous, 1);
        assert_eq!(f.scan().placed, 0);
    }

    #[test]
    fn a_missing_index_never_reads_like_an_empty_one() {
        let i = Install::new("no-index");
        let f = read(&i.chat_store());
        // Cursor has opened no folder on this machine: nothing COULD have been
        // found. The distinction is the whole point of the flag.
        assert!(!f.scan().storage_present);
        assert_eq!(f.scan().workspaces, 0);

        let j = Install::new("empty-index");
        j.workspace("w1", Some(&Install::folder("/src/api")), Some(&[]));
        let g = read(&j.chat_store());
        // A folder opened and never chatted in: read fine, held nothing.
        assert!(g.scan().storage_present);
        assert_eq!(g.scan().workspaces, 1);
        assert_eq!(g.scan().unreadable, 0);
        assert_eq!(g.scan().placed, 0);
    }

    #[test]
    fn a_store_whose_list_cannot_be_read_is_counted_apart() {
        let i = Install::new("unreadable");
        let ws = i.0.join("User/workspaceStorage/w1");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("workspace.json"), Install::folder("/src/api")).unwrap();
        // A store that is present and holds no list this build understands —
        // the `ai_code_hashes` shape, which must never pass as an empty machine.
        std::fs::write(ws.join("state.vscdb"), b"not a database").unwrap();

        let f = read(&i.chat_store());
        assert_eq!(f.scan().unreadable, 1);
        assert_eq!(f.scan().folders, 1);
        assert_eq!(f.scan().placed, 0);
    }

    #[test]
    fn a_window_with_no_single_folder_names_none() {
        let i = Install::new("folderless");
        // An empty editor window: a directory with no `workspace.json` at all.
        i.workspace("w1", None, Some(&["conv-a"]));
        // A multi-root `.code-workspace`: several folders at once, so there is
        // no single honest answer and none is invented.
        i.workspace(
            "w2",
            Some(r#"{"workspace": "file:///src/all.code-workspace"}"#),
            Some(&["conv-b"]),
        );
        let f = read(&i.chat_store());

        assert_eq!(f.folder("conv-a"), None);
        assert_eq!(f.folder("conv-b"), None);
        assert_eq!(f.scan().folderless, 2);
        assert_eq!(f.scan().folders, 0);
    }

    #[test]
    fn a_percent_encoded_folder_is_decoded_to_the_path_it_names() {
        let i = Install::new("encoded");
        i.workspace(
            "w1",
            Some(r#"{"folder": "file:///Users/me/My%20Code/caf%C3%A9"}"#),
            Some(&["conv-a"]),
        );
        // Undecoded, this names a directory on no machine — the probe would find
        // no `.git` and report no repository, a wrong answer in absence's dress.
        assert_eq!(
            read(&i.chat_store()).folder("conv-a"),
            Some("/Users/me/My Code/café")
        );
    }

    #[test]
    fn a_uri_this_module_cannot_turn_into_a_local_path_names_nothing() {
        // A Windows URI carries its drive in the first segment and is already
        // absolute; a POSIX one gets its leading slash back.
        assert_eq!(
            path_from_file_uri("file:///c%3A/Users/me/api").as_deref(),
            Some("c:/Users/me/api")
        );
        // The colon unencoded, which editors also write.
        assert_eq!(
            path_from_file_uri("file:///C:/Users/me/api").as_deref(),
            Some("C:/Users/me/api")
        );
        assert_eq!(
            path_from_file_uri("file:///src/api").as_deref(),
            Some("/src/api")
        );
        // A network share has no local path, and another scheme is not a path.
        assert_eq!(path_from_file_uri("file://server/share/api"), None);
        assert_eq!(path_from_file_uri("vscode-remote:///src/api"), None);
        // A half-decoded path is a DIFFERENT path, so a bad escape yields none.
        assert_eq!(path_from_file_uri("file:///src/a%zz"), None);
        assert_eq!(path_from_file_uri("file:///src/a%2"), None);
    }
}
