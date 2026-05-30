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

use std::fs;
use std::path::Path;
use std::path::PathBuf;

use assert_matches::assert_matches;
use itertools::Itertools as _;
use jj_lib::backend::Backend;
use jj_lib::backend::FileId;
use jj_lib::git_backend::GitBackend;
use jj_lib::git_backend_consumingchaos::WorkspaceAttributes;
use jj_lib::git_backend_consumingchaos::clear_current_workspace_root;
use jj_lib::git_backend_consumingchaos::set_current_workspace_root;
use jj_lib::repo_path::RepoPath;
use jj_lib::working_copy::SnapshotOptions;
use jj_lib::working_copy::UntrackedReason;
use pollster::FutureExt as _;
use tempfile::TempDir;
use testutils::TestResult;
use testutils::TestWorkspace;
use testutils::empty_snapshot_options;
use testutils::new_temp_dir;
use testutils::user_settings;
use tokio::io::AsyncReadExt as _;

/// Initializes an empty internal-Git store at `<workspace_root>/.jj/repo/store`
/// and returns the workspace root.
fn init_store(temp_dir: &TempDir) -> PathBuf {
    let workspace_root = temp_dir.path().to_owned();
    let store_path = workspace_root.join(".jj/repo/store");
    fs::create_dir_all(&store_path).unwrap();
    let settings = user_settings();
    GitBackend::init_internal(&settings, &store_path).unwrap();
    fs::write(store_path.join("type"), GitBackend::name()).unwrap();
    workspace_root
}

fn load_backend(workspace_root: &Path) -> GitBackend {
    let store_path = workspace_root.join(".jj/repo/store");
    let settings = user_settings();
    GitBackend::load(&settings, &store_path).unwrap()
}

fn write_attributes(workspace_root: &Path, contents: &str) {
    fs::write(workspace_root.join(".jjattributes"), contents).unwrap();
}

fn repo_path(s: &str) -> &RepoPath {
    RepoPath::from_internal_string(s).unwrap()
}

fn write_file(backend: &GitBackend, path: &RepoPath, bytes: &[u8]) -> FileId {
    backend
        .write_file(path, &mut &bytes[..])
        .block_on()
        .unwrap()
}

fn read_file(backend: &GitBackend, path: &RepoPath, id: &FileId) -> Vec<u8> {
    let mut reader = backend.read_file(path, id).block_on().unwrap();
    let mut buf = Vec::new();
    reader.read_to_end(&mut buf).block_on().unwrap();
    buf
}

// ----- WorkspaceAttributes::applies_to -----

#[test]
fn applies_to_default_matches_nothing() {
    let settings = WorkspaceAttributes::default();
    assert!(!settings.applies_to(repo_path("foo")));
    assert!(!settings.applies_to(repo_path("data/file.bin")));
}

#[test]
fn applies_to_matches_configured_patterns() {
    let settings = WorkspaceAttributes::parse(
        b"data/**         storage=cdc\n\
          media/large/**  storage=cdc\n",
    );
    assert!(settings.applies_to(repo_path("data/file.bin")));
    assert!(settings.applies_to(repo_path("data/nested/file.bin")));
    assert!(settings.applies_to(repo_path("media/large/img.png")));
    assert!(!settings.applies_to(repo_path("src/main.rs")));
    assert!(!settings.applies_to(repo_path("media/small.png")));
    // `dataset` shares a literal prefix but is not under the `data` directory.
    assert!(!settings.applies_to(repo_path("dataset/foo")));
}

#[test]
fn applies_to_ignores_non_cdc_storage_values() {
    let settings = WorkspaceAttributes::parse(b"data/** storage=other\n");
    assert!(!settings.applies_to(repo_path("data/file.bin")));
}

#[test]
fn applies_to_respects_attribute_overrides() {
    // Later patterns override earlier ones for overlapping paths.
    let settings = WorkspaceAttributes::parse(
        b"data/**     storage=cdc\n\
          data/skip/* -storage\n",
    );
    assert!(settings.applies_to(repo_path("data/file.bin")));
    assert!(!settings.applies_to(repo_path("data/skip/file.bin")));
}

// ----- load -----

#[test]
fn load_succeeds_when_thread_local_unset() {
    clear_current_workspace_root();
    let dir = new_temp_dir();
    let workspace_root = init_store(&dir);
    // No `.jjattributes` lookup attempted — settings are default. We still get
    // a working backend (this is the path upstream lib consumers and
    // pre-checkout `jj git clone` rely on).
    let backend = load_backend(&workspace_root);
    // Default settings → no path is treated as CDC at write time.
    let id = write_file(&backend, repo_path("any/file.bin"), b"hi");
    assert_eq!(read_file(&backend, repo_path("any/file.bin"), &id), b"hi");
}

#[test]
fn load_succeeds_when_file_missing() {
    let dir = new_temp_dir();
    let workspace_root = init_store(&dir);
    set_current_workspace_root(workspace_root.clone());
    // No `.jjattributes` file at the workspace root → default settings.
    let backend = load_backend(&workspace_root);
    let id = write_file(&backend, repo_path("any/file.bin"), b"hi");
    assert_eq!(read_file(&backend, repo_path("any/file.bin"), &id), b"hi");
}

#[test]
fn load_accepts_empty_file() {
    let dir = new_temp_dir();
    let workspace_root = init_store(&dir);
    write_attributes(&workspace_root, "");
    set_current_workspace_root(workspace_root.clone());
    // Empty file parses as default settings; the construction succeeding is
    // the property under test.
    load_backend(&workspace_root);
}

#[test]
fn load_tolerates_malformed_lines() {
    let dir = new_temp_dir();
    let workspace_root = init_store(&dir);
    // gitattributes parsing is lenient — invalid lines are skipped rather
    // than aborting load. The valid `data/**` line must still apply.
    write_attributes(
        &workspace_root,
        "??? invalid garbage\ndata/** storage=cdc\n",
    );
    set_current_workspace_root(workspace_root.clone());
    let backend = load_backend(&workspace_root);
    let payload = small_payload();
    let matching = write_file(&backend, repo_path("data/x.bin"), &payload);
    let non_matching = write_file(&backend, repo_path("src/x.bin"), &payload);
    assert_ne!(matching, non_matching);
}

// ----- write_file / read_file round-trips -----

fn small_payload() -> Vec<u8> {
    b"hello world".to_vec()
}

/// Deterministic pseudo-random bytes. Using an LCG so the same seed always
/// yields the same content — chunk counts and OIDs are stable across runs.
fn pseudo_random_payload(target_size: usize) -> Vec<u8> {
    let mut state: u64 = 0x9E3779B97F4A7C15;
    let mut bytes = Vec::with_capacity(target_size);
    while bytes.len() < target_size {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        bytes.extend_from_slice(&state.to_le_bytes());
    }
    bytes.truncate(target_size);
    bytes
}

/// Sized to land well above 10 chunks (at the 64 KB avg chunk size used for
/// inputs in this range), so the round-trip test exercises chunk indices past
/// the 9→10 boundary where a missing zero-padding would reorder entries.
fn large_payload() -> Vec<u8> {
    pseudo_random_payload(1024 * 1024)
}

/// Counts loose Git objects under the bare repo at
/// `<workspace>/.jj/repo/store/git/objects/<aa>/<rest>`. Loose-object subdirs
/// have two-character lowercase-hex names; `info` and `pack` are skipped.
fn count_loose_objects(workspace_root: &Path) -> usize {
    let objects_dir = workspace_root.join(".jj/repo/store/git/objects");
    let Ok(entries) = fs::read_dir(&objects_dir) else {
        return 0;
    };
    let mut count = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if name.len() != 2 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let Ok(inner) = fs::read_dir(&path) else {
            continue;
        };
        count += inner
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .count();
    }
    count
}

#[test]
fn round_trip_matching_path() {
    let dir = new_temp_dir();
    let workspace_root = init_store(&dir);
    write_attributes(&workspace_root, "data/** storage=cdc\n");
    set_current_workspace_root(workspace_root.clone());
    let backend = load_backend(&workspace_root);

    let path = repo_path("data/blob.bin");
    let payload = small_payload();
    let id = write_file(&backend, path, &payload);
    assert_eq!(read_file(&backend, path, &id), payload);
}

#[test]
fn round_trip_non_matching_path() {
    let dir = new_temp_dir();
    let workspace_root = init_store(&dir);
    write_attributes(&workspace_root, "data/** storage=cdc\n");
    set_current_workspace_root(workspace_root.clone());
    let backend = load_backend(&workspace_root);

    let path = repo_path("src/main.rs");
    let payload = small_payload();
    let id = write_file(&backend, path, &payload);
    assert_eq!(read_file(&backend, path, &id), payload);
}

#[test]
fn matching_and_non_matching_paths_produce_different_ids() {
    let dir = new_temp_dir();
    let workspace_root = init_store(&dir);
    write_attributes(&workspace_root, "data/** storage=cdc\n");
    set_current_workspace_root(workspace_root.clone());
    let backend = load_backend(&workspace_root);

    let payload = small_payload();
    let matching = write_file(&backend, repo_path("data/x.bin"), &payload);
    let non_matching = write_file(&backend, repo_path("src/x.rs"), &payload);
    // CDC-wrapped content produces a tree OID; plain content produces a blob
    // OID — they must not collide.
    assert_ne!(matching, non_matching);
}

#[test]
fn empty_file_round_trips_through_cdc() {
    let dir = new_temp_dir();
    let workspace_root = init_store(&dir);
    write_attributes(&workspace_root, "data/** storage=cdc\n");
    set_current_workspace_root(workspace_root.clone());
    let backend = load_backend(&workspace_root);

    let path = repo_path("data/empty.bin");
    let id = write_file(&backend, path, b"");
    assert_eq!(read_file(&backend, path, &id), Vec::<u8>::new());
}

#[test]
fn large_file_round_trips_through_cdc() {
    let dir = new_temp_dir();
    let workspace_root = init_store(&dir);
    write_attributes(&workspace_root, "data/** storage=cdc\n");
    set_current_workspace_root(workspace_root.clone());
    let backend = load_backend(&workspace_root);

    let path = repo_path("data/big.bin");
    let payload = large_payload();
    let id = write_file(&backend, path, &payload);
    assert_eq!(read_file(&backend, path, &id), payload);
}

/// Returns true if a `git` binary is reachable. Mirrors the gating in
/// `test_git_backend::test_gc`.
fn git_available() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[test]
fn git_gc_preserves_cdc_chunks_via_reachability() {
    if !git_available() {
        eprintln!("Skipping: git binary not available");
        return;
    }

    use jj_lib::backend::ChangeId;
    use jj_lib::backend::Commit;
    use jj_lib::backend::CopyId;
    use jj_lib::backend::MillisSinceEpoch;
    use jj_lib::backend::Signature;
    use jj_lib::backend::Timestamp;
    use jj_lib::backend::Tree;
    use jj_lib::backend::TreeValue;
    use jj_lib::merge::Merge;
    use jj_lib::object_id::ObjectId as _;
    use jj_lib::repo_path::RepoPathComponentBuf;

    let dir = new_temp_dir();
    let workspace_root = init_store(&dir);
    write_attributes(&workspace_root, "data/** storage=cdc\n");
    set_current_workspace_root(workspace_root.clone());
    let backend = load_backend(&workspace_root);

    let payload = pseudo_random_payload(256 * 1024);
    let file_id = write_file(&backend, repo_path("data/big.bin"), &payload);

    // Build a parent tree referencing the CDC file. With the corrected mode
    // logic, `write_tree` encodes this entry as Tree (not Blob) so `git gc`
    // recurses into the CDC tree and keeps its chunks reachable.
    let entries = vec![(
        RepoPathComponentBuf::new("big.bin".to_owned()).unwrap(),
        TreeValue::File {
            id: file_id.clone(),
            executable: false,
            copy_id: CopyId::placeholder(),
        },
    )];
    let parent_tree_id = backend
        .write_tree(repo_path("data"), &Tree::from_sorted_entries(entries))
        .block_on()
        .unwrap();

    // Wrap the parent tree in a commit; `write_commit` creates a
    // `refs/jj/keep/<commit>` no-gc ref, making the commit (and everything
    // reachable from its root tree) a gc root.
    let timestamp = Timestamp {
        timestamp: MillisSinceEpoch(0),
        tz_offset: 0,
    };
    let signature = Signature {
        name: "Test".to_owned(),
        email: "test@example.com".to_owned(),
        timestamp,
    };
    let mut change_id_bytes = [0u8; 16];
    change_id_bytes[0] = 0x42;
    let commit = Commit {
        parents: vec![backend.root_commit_id().clone()],
        predecessors: vec![],
        root_tree: Merge::resolved(parent_tree_id),
        conflict_labels: Merge::resolved(String::new()),
        change_id: ChangeId::from_bytes(&change_id_bytes),
        description: "test commit".to_owned(),
        author: signature.clone(),
        committer: signature,
        secure_sig: None,
    };
    backend.write_commit(commit, None).block_on().unwrap();

    // Collect the OIDs we expect to survive gc: the marker blob and every
    // chunk blob inside the CDC tree.
    let cdc_tree_oid = gix::ObjectId::from_bytes_or_panic(file_id.as_bytes());
    let expected_oids: Vec<gix::ObjectId> = {
        let repo = backend.git_repo();
        let cdc_tree = repo
            .find_object(cdc_tree_oid)
            .unwrap()
            .try_into_tree()
            .unwrap();
        cdc_tree
            .iter()
            .map(|e| e.unwrap().oid().to_owned())
            .collect()
    };
    assert!(
        expected_oids.len() >= 2,
        "expected at least marker + one chunk, got {}",
        expected_oids.len()
    );

    // Run `git gc --prune=now` against the bare repo. Without the mode fix,
    // git would not recurse through the CDC tree and these objects would be
    // pruned.
    let git_dir = workspace_root.join(".jj/repo/store/git");
    let status = std::process::Command::new("git")
        .env("GIT_DIR", &git_dir)
        .args(["gc", "--prune=now"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git gc failed: {status:?}");

    // After gc, the CDC tree itself, the marker blob, and every chunk blob
    // must still be findable.
    let repo = backend.git_repo();
    assert!(
        repo.find_object(cdc_tree_oid).is_ok(),
        "CDC tree {cdc_tree_oid} was pruned",
    );
    for oid in &expected_oids {
        assert!(
            repo.find_object(*oid).is_ok(),
            "object {oid} (referenced by the CDC tree) was pruned",
        );
    }

    // And the file still reassembles correctly when read back.
    assert_eq!(
        read_file(&backend, repo_path("data/big.bin"), &file_id),
        payload
    );
}

#[test]
fn incremental_edits_share_chunks_across_versions() {
    // Store A: write v1 then v2; count how many new objects v2 actually adds.
    let dir_a = new_temp_dir();
    let root_a = init_store(&dir_a);
    write_attributes(&root_a, "data/** storage=cdc\n");
    set_current_workspace_root(root_a.clone());
    let backend_a = load_backend(&root_a);

    let v1 = pseudo_random_payload(1024 * 1024);
    let mut v2 = v1.clone();
    // Small middle-of-file insertion. CDC boundaries should resync within a
    // chunk or two after the edit, leaving most chunks shared with v1.
    let insert_at = v2.len() / 2;
    v2.splice(insert_at..insert_at, b"PATCH".iter().copied());

    let path = repo_path("data/big.bin");
    write_file(&backend_a, path, &v1);
    let after_v1 = count_loose_objects(&root_a);
    write_file(&backend_a, path, &v2);
    let after_v2 = count_loose_objects(&root_a);
    let incremental = after_v2 - after_v1;

    // Store B: write v2 alone, so we have a no-dedup baseline to compare to.
    let dir_b = new_temp_dir();
    let root_b = init_store(&dir_b);
    write_attributes(&root_b, "data/** storage=cdc\n");
    set_current_workspace_root(root_b.clone());
    let backend_b = load_backend(&root_b);
    write_file(&backend_b, path, &v2);
    let fresh = count_loose_objects(&root_b);

    // A small edit must add fewer objects than half of a full rewrite —
    // anything close to `fresh` would mean dedup didn't happen at all.
    assert!(
        incremental * 2 < fresh,
        "expected CDC dedup; incremental={incremental}, fresh={fresh}",
    );
}

#[test]
fn fresh_load_reads_existing_cdc_content_via_tree_traversal() {
    // Simulates `jj git clone` against a remote that already contains CDC
    // content. JJ has no FileIds stashed at clone time — it reads the
    // commit's root tree from the Git store, the tree decodes file entries
    // by Git mode, and only then is `read_file` called on the discovered
    // FileId. This test reproduces that flow: write a CDC file and a parent
    // tree referencing it, then have a *fresh* backend reach the file by
    // traversing the tree.
    use jj_lib::backend::CopyId;
    use jj_lib::backend::Tree;
    use jj_lib::backend::TreeValue;
    use jj_lib::repo_path::RepoPathComponentBuf;

    let dir = new_temp_dir();
    let workspace_root = init_store(&dir);
    write_attributes(&workspace_root, "data/** storage=cdc\n");
    set_current_workspace_root(workspace_root.clone());

    let payload = pseudo_random_payload(256 * 1024);
    let file_path = repo_path("data/big.bin");
    let parent_path = repo_path("data");
    let tree_id = {
        let writer = load_backend(&workspace_root);
        let file_id = write_file(&writer, file_path, &payload);
        // The "data" directory's tree, as it would land in the Git store once
        // a commit referencing the CDC file was fetched.
        let entries = vec![(
            RepoPathComponentBuf::new("big.bin".to_owned()).unwrap(),
            TreeValue::File {
                id: file_id,
                executable: false,
                copy_id: CopyId::placeholder(),
            },
        )];
        let tree = Tree::from_sorted_entries(entries);
        writer.write_tree(parent_path, &tree).block_on().unwrap()
    };

    // Fresh backend: load against the same on-disk store, then traverse the
    // tree to discover the FileId, then read. Attributes never gate reads, so
    // it doesn't matter whether `.jjattributes` is present or not for the
    // read path.
    clear_current_workspace_root();
    let reader = load_backend(&workspace_root);
    let tree = reader.read_tree(parent_path, &tree_id).block_on().unwrap();
    let entry_name = RepoPathComponentBuf::new("big.bin".to_owned()).unwrap();
    let TreeValue::File {
        id: discovered_id, ..
    } = tree.value(entry_name.as_ref()).unwrap()
    else {
        panic!("expected file entry in tree");
    };
    assert_eq!(read_file(&reader, file_path, discovered_id), payload);
}

#[test]
fn identical_payload_at_different_matching_paths_yields_same_id() {
    let dir = new_temp_dir();
    let workspace_root = init_store(&dir);
    write_attributes(&workspace_root, "data/** storage=cdc\n");
    set_current_workspace_root(workspace_root.clone());
    let backend = load_backend(&workspace_root);

    let payload = small_payload();
    let a = write_file(&backend, repo_path("data/a.bin"), &payload);
    let b = write_file(&backend, repo_path("data/b.bin"), &payload);
    // CDC tree is content-addressed; identical bytes → identical OID.
    assert_eq!(a, b);
}

#[test]
fn snapshot_size_limit_bypassed_for_cdc_paths() -> TestResult {
    let mut test_workspace = TestWorkspace::init();
    let workspace_root = test_workspace.workspace.workspace_root().to_owned();
    fs::write(
        workspace_root.join(".jjattributes"),
        "data/** storage=cdc\n",
    )?;
    set_current_workspace_root(workspace_root.clone());

    let limit: usize = 1024;
    let cdc_path = repo_path("data/big.bin");
    let plain_path = repo_path("plain.bin");
    fs::create_dir(workspace_root.join("data"))?;
    fs::write(
        cdc_path.to_fs_path_unchecked(&workspace_root),
        vec![0; limit * 4],
    )?;
    fs::write(
        plain_path.to_fs_path_unchecked(&workspace_root),
        vec![0; limit * 4],
    )?;
    let options = SnapshotOptions {
        max_new_file_size: limit as u64,
        ..empty_snapshot_options()
    };
    let (_tree, stats) = test_workspace.snapshot_with_options(&options)?;

    // The CDC-flagged path should slip past the size limit; only the
    // unflagged path should be reported as too large.
    assert_eq!(
        stats
            .untracked_paths
            .keys()
            .map(AsRef::as_ref)
            .collect_vec(),
        [plain_path],
    );
    assert_matches!(
        stats.untracked_paths.values().next().unwrap(),
        UntrackedReason::FileTooLarge { .. }
    );

    clear_current_workspace_root();
    Ok(())
}
