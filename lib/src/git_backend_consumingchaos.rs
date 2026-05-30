// Copyright 2026 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![expect(missing_docs)]

use std::cell::RefCell;
use std::fs;
use std::io;
use std::path::PathBuf;

use bstr::BStr;
use thiserror::Error;

use crate::backend::BackendError;
use crate::backend::BackendResult;
use crate::backend::FileId;
use crate::git_backend::GitBackend;
use crate::object_id::ObjectId as _;
use crate::repo_path::RepoPath;

/// Name of the per-workspace attributes file consulted by the fork. Uses
/// `.gitattributes` syntax (pattern + `name=value` assignments), parsed via
/// `gix::attrs`.
const ATTRIBUTES_FILE: &str = ".jjattributes";

/// Attribute name whose value selects the storage strategy for matching
/// files. Currently the only recognized value is [`CDC_VALUE`].
const STORAGE_ATTR: &str = "storage";

/// Value of [`STORAGE_ATTR`] that opts a path into CDC storage.
const CDC_VALUE: &str = "cdc";

/// Name of the marker entry inside a CDC tree. Its presence at the top level of
/// a tree distinguishes our chunked representation from a regular Git tree that
/// happens to have been stored under a file's `FileId`. The blob it points at
/// is empty, so all CDC trees share the same well-known marker OID. `.cdc`
/// (0x2E) sorts before any digit (0x30), so the marker is always the first
/// entry in the tree.
const CDC_MARKER_NAME: &str = ".cdc";

thread_local! {
    /// Side-channel set by the fork's CLI before triggering workspace load.
    /// `load_workspace_attributes` consults this to find the *current*
    /// workspace root (which may be a secondary workspace, distinct from the
    /// primary that owns the shared `.jj/repo/store`).
    static CURRENT_WORKSPACE_ROOT: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// Records the workspace root for any subsequent `GitBackend` construction on
/// this thread. Intended to be called from the CLI once the workspace path is
/// known and before `Workspace::load` runs.
pub fn set_current_workspace_root(path: PathBuf) {
    CURRENT_WORKSPACE_ROOT.with(|cell| *cell.borrow_mut() = Some(path));
}

/// Clears the thread-local workspace root. Useful for test isolation.
pub fn clear_current_workspace_root() {
    CURRENT_WORKSPACE_ROOT.with(|cell| *cell.borrow_mut() = None);
}

fn current_workspace_root() -> Option<PathBuf> {
    CURRENT_WORKSPACE_ROOT.with(|cell| cell.borrow().clone())
}

#[derive(Debug, Error)]
pub enum WorkspaceAttributesError {
    #[error("Failed to read `.jjattributes`")]
    Io(#[source] io::Error),
}

#[derive(Debug, Clone, Default)]
pub struct WorkspaceAttributes {
    search: gix::attrs::Search,
    collection: gix::attrs::search::MetadataCollection,
}

impl WorkspaceAttributes {
    /// Returns `true` when `path` is assigned `storage=cdc` by `.jjattributes`.
    /// Empty attributes (no file, or no matching pattern) returns `false`.
    pub fn applies_to(&self, path: &RepoPath) -> bool {
        if self.search.num_pattern_lists() == 0 {
            return false;
        }
        let mut outcome = gix::attrs::search::Outcome::default();
        outcome.initialize_with_selection(&self.collection, [STORAGE_ATTR]);
        let path_str = path.as_internal_file_string();
        self.search.pattern_matching_relative_path(
            BStr::new(path_str.as_bytes()),
            gix::glob::pattern::Case::Sensitive,
            None,
            &mut outcome,
        );
        outcome
            .iter_selected()
            .any(|m| m.assignment.state.as_bstr() == Some(BStr::new(CDC_VALUE.as_bytes())))
    }

    /// Parses a `.jjattributes` buffer. Lenient: malformed lines are skipped
    /// (with a `gix-trace` warning) rather than returning an error.
    pub fn parse(bytes: &[u8]) -> Self {
        let mut search = gix::attrs::Search::default();
        let mut collection = gix::attrs::search::MetadataCollection::default();
        // Parsing is lenient: invalid lines are logged via gix-trace and
        // skipped, mirroring how Git treats malformed `.gitattributes` entries.
        search.add_patterns_buffer(bytes, ATTRIBUTES_FILE.into(), None, &mut collection, true);
        Self { search, collection }
    }
}

/// Loads `WorkspaceAttributes` from the thread-local workspace root.
///
/// Returns [`WorkspaceAttributes::default`] when the thread-local is unset
/// or no `.jjattributes` file is present at the workspace root. Returns an
/// error only when the file exists but cannot be read.
pub fn load_workspace_attributes() -> Result<WorkspaceAttributes, WorkspaceAttributesError> {
    let Some(workspace_root) = current_workspace_root() else {
        return Ok(WorkspaceAttributes::default());
    };
    let attributes_path = workspace_root.join(ATTRIBUTES_FILE);
    let bytes = match fs::read(&attributes_path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok(WorkspaceAttributes::default());
        }
        Err(err) => return Err(WorkspaceAttributesError::Io(err)),
    };
    Ok(WorkspaceAttributes::parse(&bytes))
}

/// Returns true if `oid` points at a tree object whose first entry is the
/// `.cdc` marker — i.e. a CDC-wrapped file. Used by `GitBackend::write_tree` to
/// choose between `EntryKind::Blob` and `EntryKind::Tree` for the parent
/// entry, and by `GitBackend::read_tree` to decide whether a tree-mode entry
/// should be reported as `TreeValue::File` (CDC) or `TreeValue::Tree`
/// (ordinary subdirectory).
pub(crate) fn is_cdc_tree(repo: &gix::Repository, oid: &gix::oid) -> BackendResult<bool> {
    let header = repo
        .find_header(oid)
        .map_err(|err| BackendError::ObjectNotFound {
            object_type: "object".to_owned(),
            hash: oid.to_string(),
            source: Box::new(err),
        })?;
    if header.kind() != gix::object::Kind::Tree {
        return Ok(false);
    }
    let tree = repo
        .find_object(oid)
        .map_err(|err| BackendError::ObjectNotFound {
            object_type: "tree".to_owned(),
            hash: oid.to_string(),
            source: Box::new(err),
        })?
        .try_into_tree()
        .map_err(|err| BackendError::ReadObject {
            object_type: "tree".to_owned(),
            hash: oid.to_string(),
            source: Box::new(err),
        })?;
    let Some(first) = tree.iter().next() else {
        return Ok(false);
    };
    let first = first.map_err(|err| BackendError::ReadObject {
        object_type: "tree".to_owned(),
        hash: oid.to_string(),
        source: Box::new(err),
    })?;
    Ok(first.filename() == CDC_MARKER_NAME.as_bytes())
}

/// Reads a file stored as a CDC tree. Returns `Some(data)` when `id` points at
/// a CDC tree we wrote, or `None` when the caller should fall back to reading
/// `id` as a regular Git blob.
pub(crate) fn read_cdc_file(backend: &GitBackend, id: &FileId) -> BackendResult<Option<Vec<u8>>> {
    let oid = git_oid_from_file_id(id)?;
    let repo = backend.git_repo();
    let object = repo
        .find_object(oid)
        .map_err(|err| BackendError::ObjectNotFound {
            object_type: "file".to_owned(),
            hash: id.hex(),
            source: Box::new(err),
        })?;

    if object.kind != gix::object::Kind::Tree {
        return Ok(None);
    }

    let tree = object
        .try_into_tree()
        .map_err(|err| BackendError::ReadObject {
            object_type: "file".to_owned(),
            hash: id.hex(),
            source: Box::new(err),
        })?;

    // Tree entries are returned in Git's sort order, so `.cdc` (0x2E) is
    // always the first entry of a CDC tree we wrote.
    let mut entries = tree.iter();
    let first = entries
        .next()
        .ok_or_else(|| BackendError::ReadObject {
            object_type: "file".to_owned(),
            hash: id.hex(),
            source: "expected a CDC tree".into(),
        })?
        .map_err(|err| BackendError::ReadObject {
            object_type: "file".to_owned(),
            hash: id.hex(),
            source: Box::new(err),
        })?;
    if first.filename() != CDC_MARKER_NAME.as_bytes() {
        return Err(BackendError::ReadObject {
            object_type: "file".to_owned(),
            hash: id.hex(),
            source: "expected a CDC tree".into(),
        });
    }

    let mut data = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| BackendError::ReadObject {
            object_type: "file".to_owned(),
            hash: id.hex(),
            source: Box::new(err),
        })?;
        let chunk_oid = entry.oid().to_owned();
        let blob = repo
            .find_object(chunk_oid)
            .map_err(|err| BackendError::ObjectNotFound {
                object_type: "cdc chunk".to_owned(),
                hash: chunk_oid.to_string(),
                source: Box::new(err),
            })?
            .try_into_blob()
            .map_err(|err| BackendError::ReadObject {
                object_type: "cdc chunk".to_owned(),
                hash: chunk_oid.to_string(),
                source: Box::new(err),
            })?;
        data.extend_from_slice(&blob.data);
    }
    Ok(Some(data))
}

/// Stores `bytes` as a CDC tree (marker entry plus one blob per chunk emitted
/// by fastcdc) and returns the tree OID as a `FileId`.
pub(crate) fn write_cdc_file(backend: &GitBackend, bytes: &[u8]) -> BackendResult<FileId> {
    let repo = backend.git_repo();

    let marker_oid = repo
        .write_blob(b"")
        .map_err(|err| BackendError::WriteObject {
            object_type: "cdc marker",
            source: Box::new(err),
        })?
        .detach();

    let mut entries = vec![gix::objs::tree::Entry {
        mode: gix::object::tree::EntryKind::Blob.into(),
        filename: CDC_MARKER_NAME.into(),
        oid: marker_oid,
    }];

    let (min_size, avg_size, max_size) = cdc_parameters(bytes.len());
    let chunker = fastcdc::v2020::FastCDC::new(
        bytes,
        min_size as usize,
        avg_size as usize,
        max_size as usize,
    );
    for (index, chunk) in chunker.enumerate() {
        let chunk_bytes = &bytes[chunk.offset..chunk.offset + chunk.length];
        let chunk_oid = repo
            .write_blob(chunk_bytes)
            .map_err(|err| BackendError::WriteObject {
                object_type: "cdc chunk",
                source: Box::new(err),
            })?
            .detach();
        entries.push(gix::objs::tree::Entry {
            mode: gix::object::tree::EntryKind::Blob.into(),
            filename: format!("{index:08}").into(),
            oid: chunk_oid,
        });
    }

    let tree_oid = repo
        .write_object(gix::objs::Tree { entries })
        .map_err(|err| BackendError::WriteObject {
            object_type: "cdc tree",
            source: Box::new(err),
        })?
        .detach();

    Ok(FileId::from_bytes(tree_oid.as_bytes()))
}

fn git_oid_from_file_id(id: &FileId) -> BackendResult<gix::ObjectId> {
    gix::ObjectId::try_from(id.as_bytes()).map_err(|_| BackendError::InvalidHashLength {
        expected: gix::hash::Kind::Sha1.len_in_bytes(),
        actual: id.as_bytes().len(),
        object_type: "file".to_owned(),
        hash: id.hex(),
    })
}

/// Returns `(min, avg, max)` FastCDC-style chunk-size parameters for an input
/// of the given size.
fn cdc_parameters(size: usize) -> (u32, u32, u32) {
    match size {
        0..=131_072 => (8 * 1024, 16 * 1024, 64 * 1024),
        131_073..=16_777_216 => (32 * 1024, 64 * 1024, 256 * 1024),
        16_777_217..=1_073_741_824 => (64 * 1024, 256 * 1024, 1024 * 1024),
        _ => (512 * 1024, 1024 * 1024, 4 * 1024 * 1024),
    }
}
