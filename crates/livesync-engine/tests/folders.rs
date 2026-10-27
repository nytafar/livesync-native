//! Listing the remote's top-level folders for the folder-scope picker:
//! original case, dot-folders, deleted documents, root files, and never a
//! chunk or an `i:`/`ix:` document read; read-only.

mod common;

use common::{Mock, DB};
use livesync_engine::{list_remote_folders, local_top_folders, EngineError, FolderEntry};
use livesync_model::gate::Refusal;
use livesync_remote::{Couch, RemoteConfig};
use serde_json::json;

fn couch(m: &Mock) -> Couch {
    Couch::new(&RemoteConfig {
        url: m.url.clone(),
        database: DB.into(),
        username: Some("u".into()),
        password: Some("p".into()),
    })
    .unwrap()
}

fn entry(name: &str, files: u64, bytes: u64) -> FolderEntry {
    FolderEntry {
        name: name.into(),
        files,
        bytes,
        remote: true,
        local: false,
    }
}

#[tokio::test]
async fn the_listing_keeps_case_and_leaves_out_dot_folders_deleted_docs_chunks_and_internal_docs() {
    let m = Mock::start().await;
    // Two spellings of one folder (IDs are lower-cased): the commoner one names it.
    m.put_text("Notes/A.md", &["aaaa"]);
    m.put_text("Notes/sub/B.md", &["bb"]);
    m.put_text("notes/c.md", &["c"]);
    // An older client's mixed-case ID keeps its own range.
    m.put(
        json!({"_id": "Archive/Old.md", "path": "Archive/Old.md", "type": "plain", "children": [],
        "ctime": 1, "mtime": 1, "size": 7, "eden": {}}),
    );
    m.put_binary("inputs/media/photo.jpg", &[1u8; 300], 100);
    // Folders whose names sort next to the chunk and internal prefixes.
    m.put_text("h1/x.md", &["x"]);
    m.put_text("ixx/y.md", &["y"]);
    m.put_text("psy/z.md", &["z"]);
    m.put_text("_templates/t.md", &["tt"]);
    // Root files, one of them a dotfile and one a flag file.
    m.put_text("readme.md", &["hello"]);
    m.put_text(".hidden.md", &["no"]);
    m.put_text("redflag.md", &["no"]);
    // Dot-folders, at the top and further down.
    m.put_text(".obsidian/app.json", &["{}"]);
    m.put_text(".trash/old.md", &["old"]);
    m.put_text("Notes/.hidden/x.md", &["no"]);
    // Deleted: soft-deleted, a tombstone, and a whole folder gone.
    m.put_text("Notes/gone.md", &["gone"]);
    m.soft_delete("Notes/gone.md");
    m.put_text("Notes/tomb.md", &["t"]);
    m.tombstone("Notes/tomb.md");
    m.put_text("Old/only.md", &["o"]);
    m.soft_delete("Old/only.md");
    // Hidden-file and customisation sync documents.
    m.put(
        json!({"_id": "i:.obsidian/plugins/x.json", "path": "i:.obsidian/plugins/x.json", "type": "newnote",
        "children": [], "ctime": 1, "mtime": 1, "size": 5, "eden": {}}),
    );
    m.put(json!({"_id": "ix:Work/x.md", "path": "ix:Work/x.md", "type": "plain",
        "children": [], "ctime": 1, "mtime": 1, "size": 5, "eden": {}}));
    m.put(json!({"_id": "ps:Work/y.md", "path": "ps:Work/y.md", "type": "plain",
        "children": [], "ctime": 1, "mtime": 1, "size": 5, "eden": {}}));

    let l = list_remote_folders(&couch(&m)).await.unwrap();
    assert_eq!(
        l.folders,
        vec![
            entry("_templates", 1, 2),
            entry("Archive", 1, 7),
            entry("h1", 1, 1),
            entry("inputs", 1, 300),
            entry("ixx", 1, 1),
            entry("Notes", 3, 7),
            entry("psy", 1, 1),
        ]
    );
    assert_eq!((l.root_files, l.root_bytes), (1, 5));
    assert_eq!(l.files, 10);
    assert!(l.bytes_fetched > 0 && l.docs_read > 0);

    let served = m.served_bodies();
    assert!(!served.is_empty());
    for id in &served {
        assert!(
            !(id.starts_with("h:") || id.starts_with("i:") || id.starts_with("ix:") || id.starts_with("ps:")),
            "read {id}"
        );
    }
    assert!(m.writes().is_empty(), "the listing wrote: {:?}", m.writes());
}

#[tokio::test]
async fn the_listing_pages_through_many_documents() {
    let m = Mock::start().await;
    for i in 0..4500 {
        m.put(
            json!({"_id": format!("bulk/{i:05}.md"), "path": format!("Bulk/{i:05}.md"), "type": "plain",
            "children": [], "ctime": 1, "mtime": 1, "size": 2, "eden": {}}),
        );
    }
    m.put_text("zz/last.md", &["l"]);
    let l = list_remote_folders(&couch(&m)).await.unwrap();
    assert_eq!(l.folders, vec![entry("Bulk", 4500, 9000), entry("zz", 1, 1)]);
}

#[tokio::test]
async fn encrypted_paths_are_refused_clearly() {
    let m = Mock::start().await;
    m.put_text("Notes/a.md", &["a"]);
    m.put(json!({"_id": "f:0123abcd", "path": "/\\:%=encrypted", "type": "plain",
        "children": [], "ctime": 1, "mtime": 1, "size": 5, "eden": {}}));
    match list_remote_folders(&couch(&m)).await {
        Err(EngineError::Refused(Refusal::EncryptedContent(msg))) => assert!(msg.contains("type them"), "{msg}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn an_empty_database_lists_nothing() {
    let m = Mock::start().await;
    let l = list_remote_folders(&couch(&m)).await.unwrap();
    assert!(l.folders.is_empty());
    assert_eq!((l.root_files, l.files), (0, 0));
}

#[test]
fn local_top_folders_skip_files_and_dot_folders() {
    let d = tempfile::tempdir().unwrap();
    for f in ["Notes", "drafts", ".obsidian", ".trash"] {
        std::fs::create_dir(d.path().join(f)).unwrap();
    }
    std::fs::write(d.path().join("readme.md"), b"x").unwrap();
    assert_eq!(local_top_folders(d.path()), vec!["drafts", "Notes"]);
    assert!(local_top_folders(&d.path().join("missing")).is_empty());
}
