use farflow_core::*;
use rusqlite::Connection;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};

#[test]
fn shared_backup_file_operations_are_bounded_private_and_reject_links() {
    let dir = tempfile::tempdir().unwrap();
    regular_directory(dir.path()).unwrap();
    let path = dir.path().join("metadata.json");
    write_json(&path, &json!({"正文":"私有备份"})).unwrap();
    assert!(write_json(&path, &json!({})).is_err());
    let bytes = fs::read(&path).unwrap();
    assert_eq!(read_file(&path, bytes.len() as u64).unwrap(), bytes);
    assert!(read_file(&path, bytes.len() as u64 - 1).is_err());
    assert_eq!(
        hash_file(&path).unwrap(),
        format!("{:x}", Sha256::digest(&bytes))
    );
    assert!(valid_hash(&hash_file(&path).unwrap()));
    for invalid in [
        "",
        "../escape",
        &"a".repeat(63),
        &"A".repeat(64),
        &"g".repeat(64),
    ] {
        assert!(!valid_hash(invalid));
    }
    assert!(regular_directory(&path).is_err());
    assert!(read_file(dir.path(), 1).is_err());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let link = dir.path().join("linked.json");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(read_file(&link, 4096).is_err());
        assert!(hash_file(&link).is_err());
        let linked_dir = dir.path().join("linked-dir");
        std::os::unix::fs::symlink(dir.path(), &linked_dir).unwrap();
        assert!(regular_directory(&linked_dir).is_err());
        assert!(hash_file(&linked_dir.join("metadata.json")).is_err());
    }
}

struct F {
    directory: tempfile::TempDir,
    workspace: LocalWorkspace,
    owner: Member,
}

impl F {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let owner = member("owner", Role::Owner);
        let workspace = LocalWorkspace::create(
            directory.path().join("workspace"),
            Id::new_v4(),
            "磁盘工作空间".into(),
            owner.clone(),
        )
        .unwrap();
        Self {
            directory,
            workspace,
            owner,
        }
    }
    fn root(&self) -> PathBuf {
        self.directory.path().join("workspace")
    }
    fn state(&self) -> State {
        self.workspace.snapshot(self.owner.id).unwrap()
    }
    fn request(&self, command: Command) -> Request {
        Request {
            id: Id::new_v4(),
            actor_id: self.owner.id,
            at: 100,
            command,
        }
    }
    fn ok(&mut self, command: Command) -> Receipt {
        self.workspace.execute(self.request(command)).unwrap()
    }
    fn document(&mut self, title: &str) -> Id {
        let item = document(title);
        let id = item.id;
        self.ok(Command::Create { item });
        id
    }
    fn upload_request(&self, item: Id) -> Upload {
        Upload {
            operation_id: Id::new_v4(),
            actor_id: self.owner.id,
            at: 101,
            id: Id::new_v4(),
            item_id: item,
            name: "验收证据.txt".into(),
            mime: "text/plain".into(),
        }
    }
    fn blob(&self, bytes: &[u8]) -> PathBuf {
        let hash = format!("{:x}", Sha256::digest(bytes));
        self.root().join("attachments").join(&hash[..2]).join(hash)
    }
    fn backup(&self) -> PathBuf {
        let path = self.directory.path().join("backup");
        self.workspace.backup(self.owner.id, &path).unwrap();
        path
    }
}

fn member(account: &str, role: Role) -> Member {
    Member {
        id: Id::new_v4(),
        account: account.into(),
        name: account.into(),
        roles: BTreeSet::from([role]),
        department: "研发".into(),
        active: true,
    }
}

fn document(title: &str) -> NewItem {
    serde_json::from_value(
        json!({"id":Id::new_v4(),"kind":"document","title":title,"description":"中文正文"}),
    )
    .unwrap()
}

fn value(state: State) -> Value {
    serde_json::to_value(state).unwrap()
}

fn rewrite_manifest(path: &Path, change: impl FnOnce(&mut Value)) {
    let file = path.join("backup.json");
    let mut data: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    change(&mut data);
    fs::write(file, serde_json::to_vec(&data).unwrap()).unwrap();
}

#[test]
fn shared_attachment_byte_functions_validate_content_and_metadata() {
    let f = F::new();
    let bytes = "公共附件接口真实内容".as_bytes();
    let hash = format!("{:x}", Sha256::digest(bytes));
    write_attachment_bytes(&f.root(), &hash, bytes).unwrap();
    sync_directory(&f.root()).unwrap();
    #[cfg(unix)]
    assert!(sync_directory(&f.root().join("missing")).is_err());
    let mut attachment = Attachment {
        id: Id::new_v4(),
        item_id: Id::new_v4(),
        name: "证据.txt".into(),
        mime: "text/plain".into(),
        sha256: hash.clone(),
        size: bytes.len() as u64,
        actor_id: f.owner.id,
        at: 100,
        deleted: false,
    };
    assert_eq!(
        read_attachment_bytes(&f.root(), &attachment).unwrap(),
        bytes
    );
    assert!(write_attachment_bytes(&f.root(), "invalid", bytes).is_err());
    assert!(write_attachment_bytes(&f.root(), &hash, b"different").is_err());
    attachment.size += 1;
    assert!(read_attachment_bytes(&f.root(), &attachment).is_err());
    attachment.sha256 = "invalid".into();
    assert!(read_attachment_bytes(&f.root(), &attachment).is_err());
}

#[test]
fn local_workspace_survives_cold_open_with_indexed_objects_and_journal() {
    let mut f = F::new();
    let id = f.document("需求交付说明");
    assert_eq!(f.workspace.info().name, "磁盘工作空间");
    assert_eq!(f.workspace.info().mode, Mode::Local);
    let info: WorkspaceInfo =
        serde_json::from_value(serde_json::to_value(f.workspace.info()).unwrap()).unwrap();
    assert_eq!(&info, f.workspace.info());
    let snapshot = value(f.state());
    let root = f.root();
    let actor = f.owner.id;
    let connection = Connection::open(root.join("data.sqlite")).unwrap();
    let row: (String, String, u64) = connection
        .query_row(
            "SELECT kind,status,version FROM objects WHERE id=?1",
            [id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(row, ("document".into(), "active".into(), 1));
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM operations", [], |r| r
                .get::<_, u64>(0))
            .unwrap(),
        1
    );
    drop(connection);
    drop(f.workspace);
    let reopened = LocalWorkspace::open(root).unwrap();
    assert_eq!(value(reopened.snapshot(actor).unwrap()), snapshot);
}

#[test]
fn two_connections_use_fresh_state_and_preserve_idempotency_after_reopen() {
    let mut f = F::new();
    let id = f.document("原文");
    let root = f.root();
    let mut other = LocalWorkspace::open(&root).unwrap();
    let request = f.request(Command::Update {
        id,
        version: 1,
        patch: Patch {
            title: Some("第一位修改".into()),
            ..Default::default()
        },
    });
    let receipt = f.workspace.execute(request.clone()).unwrap();
    let stale = f.request(Command::Update {
        id,
        version: 1,
        patch: Patch {
            title: Some("过期覆盖".into()),
            ..Default::default()
        },
    });
    assert!(matches!(other.execute(stale), Err(Error::Conflict(_))));
    let another = f.request(Command::Create {
        item: document("第二位创建"),
    });
    other.execute(another).unwrap();
    let before = value(f.state());
    assert_eq!(other.execute(request.clone()).unwrap(), receipt);
    assert_eq!(value(f.state()), before);
    drop(other);
    let mut reopened = LocalWorkspace::open(root).unwrap();
    assert_eq!(reopened.execute(request.clone()).unwrap(), receipt);
    let mut changed = request;
    changed.at += 1;
    assert!(matches!(reopened.execute(changed), Err(Error::Conflict(_))));
    assert_eq!(reopened.snapshot(f.owner.id).unwrap().items.len(), 2);
}

#[test]
fn simultaneous_stale_writes_have_one_winner_and_one_explicit_conflict() {
    let mut f = F::new();
    let id = f.document("并发文档");
    let root = f.root();
    let actor = f.owner.id;
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|index| {
            let root = root.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut workspace = LocalWorkspace::open(root).unwrap();
                barrier.wait();
                workspace.execute(Request {
                    id: Id::new_v4(),
                    actor_id: actor,
                    at: 100,
                    command: Command::Update {
                        id,
                        version: 1,
                        patch: Patch {
                            description: Some(format!("并发内容 {index}")),
                            ..Default::default()
                        },
                    },
                })
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, Err(Error::Conflict(_))))
            .count(),
        1
    );
    assert_eq!(f.state().items[&id].version, 2);
    assert_eq!(f.state().history.len(), 2);
}

#[test]
fn uploads_store_real_bytes_deduplicate_and_enforce_read_write_permissions() {
    let mut f = F::new();
    let doc = f.document("附件文档");
    let viewer = member("reader", Role::Viewer);
    f.ok(Command::AddMember {
        member: viewer.clone(),
    });
    let bytes = "附件真实内容".as_bytes();
    let upload = f.upload_request(doc);
    let roundtrip: Upload = serde_json::from_value(serde_json::to_value(&upload).unwrap()).unwrap();
    assert_eq!(roundtrip.id, upload.id);
    let receipt = f.workspace.upload(upload.clone(), bytes).unwrap();
    let state = f.state();
    let attachment = &state.attachments[&upload.id];
    assert_eq!(attachment.size, bytes.len() as u64);
    assert_eq!(attachment.sha256, format!("{:x}", Sha256::digest(bytes)));
    assert_eq!(f.workspace.upload(upload.clone(), bytes).unwrap(), receipt);
    assert!(matches!(
        f.workspace.upload(upload.clone(), b"changed"),
        Err(Error::Conflict(_))
    ));
    assert_eq!(
        f.workspace.read_attachment(viewer.id, upload.id).unwrap(),
        bytes
    );
    assert!(f
        .workspace
        .read_attachment(Id::new_v4(), upload.id)
        .is_err());
    let mut denied = f.upload_request(doc);
    denied.actor_id = viewer.id;
    assert!(matches!(
        f.workspace.upload(denied, bytes),
        Err(Error::Forbidden(_))
    ));
    let another = f.upload_request(doc);
    f.workspace.upload(another.clone(), bytes).unwrap();
    assert_eq!(
        fs::read_dir(f.blob(bytes).parent().unwrap())
            .unwrap()
            .count(),
        1
    );
    let comment = Id::new_v4();
    f.ok(Command::AddComment {
        id: comment,
        item_id: doc,
        body: "请查看验收证据".into(),
        parent_id: None,
        mentions: BTreeSet::from([viewer.id]),
        attachments: BTreeSet::from([upload.id]),
    });
    f.ok(Command::RemoveAttachment { id: upload.id });
    assert!(f.workspace.read_attachment(f.owner.id, upload.id).is_err());
    assert_eq!(
        f.workspace.read_attachment(f.owner.id, another.id).unwrap(),
        bytes
    );
    assert!(f.blob(bytes).exists());
    let mut inactive = viewer.clone();
    inactive.active = false;
    f.ok(Command::UpdateMember { member: inactive });
    assert!(f.workspace.snapshot(viewer.id).is_err());
    assert!(f.workspace.read_attachment(viewer.id, another.id).is_err());
    let before = value(f.state());
    assert_eq!(f.workspace.upload(upload, bytes).unwrap(), receipt);
    assert_eq!(value(f.state()), before);
}

#[test]
fn invalid_uploads_and_unbacked_attachment_commands_leave_no_business_changes() {
    let mut f = F::new();
    let doc = f.document("附件边界");
    let before = value(f.state());
    for index in 0..7 {
        let mut upload = f.upload_request(doc);
        let mut bytes = vec![b'x'];
        match index {
            0 => bytes.clear(),
            1 => bytes.resize(10 * 1024 * 1024 + 1, 0),
            2 => upload.name = "../escape".into(),
            3 => upload.id = Id::nil(),
            4 => upload.item_id = Id::new_v4(),
            5 => upload.mime.clear(),
            _ => upload.operation_id = Id::nil(),
        }
        assert!(f.workspace.upload(upload, &bytes).is_err());
        assert_eq!(value(f.state()), before);
    }
    let missing = Command::AddAttachment {
        id: Id::new_v4(),
        item_id: doc,
        name: "不存在.txt".into(),
        mime: "text/plain".into(),
        sha256: "a".repeat(64),
        size: 1,
    };
    let new = document("不应残留");
    let request = f.request(Command::Batch {
        commands: vec![Command::Create { item: new }, missing],
    });
    assert!(f.workspace.execute(request).is_err());
    assert_eq!(value(f.state()), before);
    let upload = f.upload_request(doc);
    f.workspace.upload(upload, b"real").unwrap();
    let hash = format!("{:x}", Sha256::digest(b"real"));
    let request = f.request(Command::AddAttachment {
        id: Id::new_v4(),
        item_id: doc,
        name: "错误大小.txt".into(),
        mime: "text/plain".into(),
        sha256: hash,
        size: 999,
    });
    assert!(f.workspace.execute(request).is_err());
    assert_eq!(f.state().attachments.len(), 1);
}

#[test]
fn sql_failure_rolls_back_projection_journal_and_can_retry_the_same_upload() {
    let mut f = F::new();
    let doc = f.document("事务文档");
    let before = value(f.state());
    let upload = f.upload_request(doc);
    let bytes = b"atomic content";
    let connection = Connection::open(f.root().join("data.sqlite")).unwrap();
    connection.execute_batch("CREATE TRIGGER refuse_journal BEFORE INSERT ON operations BEGIN SELECT RAISE(ABORT,'模拟持久化失败'); END;").unwrap();
    assert!(f.workspace.upload(upload.clone(), bytes).is_err());
    assert_eq!(value(f.state()), before);
    assert!(f.blob(bytes).exists());
    connection
        .execute_batch("DROP TRIGGER refuse_journal;")
        .unwrap();
    f.workspace.upload(upload.clone(), bytes).unwrap();
    assert_eq!(
        f.workspace.read_attachment(f.owner.id, upload.id).unwrap(),
        bytes
    );
    assert_eq!(f.state().revision, 2);
}

#[test]
fn missing_or_corrupted_blob_blocks_read_and_cold_open_without_forging_success() {
    let mut f = F::new();
    let doc = f.document("证据");
    let upload = f.upload_request(doc);
    let bytes = b"original evidence";
    f.workspace.upload(upload.clone(), bytes).unwrap();
    let blob = f.blob(bytes);
    fs::write(&blob, b"broken").unwrap();
    assert!(f.workspace.read_attachment(f.owner.id, upload.id).is_err());
    assert!(LocalWorkspace::open(f.root()).is_err());
    fs::write(&blob, bytes).unwrap();
    assert!(LocalWorkspace::open(f.root()).is_ok());
    fs::remove_file(blob).unwrap();
    assert!(f.workspace.read_attachment(f.owner.id, upload.id).is_err());
    assert!(f
        .workspace
        .backup(f.owner.id, f.directory.path().join("broken-backup"))
        .is_err());
    assert!(!f.directory.path().join("broken-backup").exists());
}

#[test]
fn attachment_directory_failure_is_atomic() {
    let mut f = F::new();
    let doc = f.document("落位失败");
    let before = value(f.state());
    let directory = f.root().join("attachments");
    fs::remove_dir(&directory).unwrap();
    fs::write(&directory, b"not a directory").unwrap();
    let upload = f.upload_request(doc);
    assert!(f.workspace.upload(upload, b"should not be saved").is_err());
    assert_eq!(value(f.state()), before);
}

#[test]
fn backup_uses_a_consistent_live_database_and_restores_history_tombstones_and_bytes() {
    let mut f = F::new();
    let doc = f.document("备份链路");
    let upload = f.upload_request(doc);
    let bytes = "可恢复附件".as_bytes();
    f.workspace.upload(upload.clone(), bytes).unwrap();
    f.ok(Command::AddComment {
        id: Id::new_v4(),
        item_id: doc,
        body: "备份前评论".into(),
        parent_id: None,
        mentions: BTreeSet::new(),
        attachments: BTreeSet::from([upload.id]),
    });
    f.ok(Command::RemoveAttachment { id: upload.id });
    let expected = value(f.state());
    assert!(f.root().join("data.sqlite-wal").exists());
    let source = f.backup();
    f.document("备份之后的新内容");
    let destination = f.directory.path().join("restored");
    let restored = LocalWorkspace::restore(&source, &destination).unwrap();
    assert_eq!(value(restored.snapshot(f.owner.id).unwrap()), expected);
    assert!(restored.read_attachment(f.owner.id, upload.id).is_err());
    let hash = format!("{:x}", Sha256::digest(bytes));
    assert_eq!(
        fs::read(destination.join("attachments").join(&hash[..2]).join(hash)).unwrap(),
        bytes
    );
    assert_eq!(f.state().items.len(), 2);
    drop(restored);
    assert_eq!(
        value(
            LocalWorkspace::open(destination)
                .unwrap()
                .snapshot(f.owner.id)
                .unwrap()
        ),
        expected
    );
}

#[test]
fn backups_and_creation_do_not_overwrite_existing_directories() {
    let f = F::new();
    let root = f.root();
    let marker = root.join("keep.txt");
    fs::write(&marker, b"keep").unwrap();
    assert!(LocalWorkspace::create(&root, Id::new_v4(), "覆盖".into(), f.owner.clone()).is_err());
    assert!(f.workspace.backup(f.owner.id, &root).is_err());
    let backup = f.backup();
    assert!(LocalWorkspace::restore(&backup, &root).is_err());
    assert_eq!(fs::read(marker).unwrap(), b"keep");
    assert!(LocalWorkspace::create(
        f.directory.path().join("no-parent/workspace"),
        Id::new_v4(),
        "不存在父目录".into(),
        f.owner.clone()
    )
    .is_err());
    assert!(LocalWorkspace::create(
        f.directory.path().join("invalid"),
        Id::nil(),
        "无效".into(),
        f.owner.clone()
    )
    .is_err());
    assert!(!f.directory.path().join("invalid").exists());
}

#[test]
fn only_active_owner_can_export_complete_backup() {
    let mut f = F::new();
    let viewer = member("viewer", Role::Viewer);
    f.ok(Command::AddMember {
        member: viewer.clone(),
    });
    for actor in [viewer.id, Id::new_v4()] {
        let destination = f.directory.path().join("denied");
        assert!(f.workspace.backup(actor, &destination).is_err());
        assert!(!destination.exists());
    }
}

#[test]
fn restore_rejects_tampering_missing_files_and_invalid_manifest_without_partial_destination() {
    for index in 0..10 {
        let mut f = F::new();
        let doc = f.document("备份校验");
        let upload = f.upload_request(doc);
        let bytes = b"backup evidence";
        f.workspace.upload(upload, bytes).unwrap();
        let source = f.backup();
        match index {
            0 => fs::write(source.join("data.sqlite"), b"damaged").unwrap(),
            1 => fs::remove_file(source.join("workspace.json")).unwrap(),
            2 => rewrite_manifest(&source, |m| m["schema"] = json!(999)),
            3 => rewrite_manifest(&source, |m| m["revision"] = json!(999)),
            4 => rewrite_manifest(&source, |m| m["workspace_id"] = json!(Id::new_v4())),
            5 => rewrite_manifest(&source, |m| {
                m["files"]
                    .as_object_mut()
                    .unwrap()
                    .insert("../outside".into(), json!("a".repeat(64)));
            }),
            6 => rewrite_manifest(&source, |m| {
                m["files"]
                    .as_object_mut()
                    .unwrap()
                    .insert("data.sqlite".into(), json!("invalid hash"));
            }),
            7 => rewrite_manifest(&source, |m| {
                m["files"].as_object_mut().unwrap().remove("workspace.json");
            }),
            8 => {
                let hash = format!("{:x}", Sha256::digest(bytes));
                fs::remove_file(source.join("attachments").join(&hash[..2]).join(hash)).unwrap();
            }
            _ => fs::write(source.join("backup.json"), b"invalid json").unwrap(),
        }
        let destination = f.directory.path().join("must-not-exist");
        assert!(
            LocalWorkspace::restore(&source, &destination).is_err(),
            "场景 {index}"
        );
        assert!(!destination.exists());
        assert_eq!(f.state().items.len(), 1);
    }
}

#[test]
fn database_format_and_journal_projection_integrity_are_checked_on_open() {
    for index in 0..7 {
        let mut f = F::new();
        let doc = f.document("完整性校验");
        let root = f.root();
        let db = Connection::open(root.join("data.sqlite")).unwrap();
        match index {
            0 => {
                db.pragma_update(None, "user_version", 999).unwrap();
            }
            1 => {
                db.execute("UPDATE workspace SET revision=999", []).unwrap();
            }
            2 => {
                db.execute(
                    "UPDATE objects SET category='unknown' WHERE id=?1",
                    [doc.to_string()],
                )
                .unwrap();
            }
            3 => {
                db.execute(
                    "UPDATE objects SET data=json_set(data,'$.title','无日志修改') WHERE id=?1",
                    [doc.to_string()],
                )
                .unwrap();
            }
            4 => {
                db.execute(
                    "UPDATE objects SET data=json_set(data,'$.id',?1) WHERE id=?2",
                    [Id::new_v4().to_string(), doc.to_string()],
                )
                .unwrap();
            }
            5 => {
                db.execute("UPDATE operations SET request='invalid json'", [])
                    .unwrap();
            }
            _ => {
                db.execute(
                    "UPDATE workspace SET genesis=json_set(genesis,'$.revision',4)",
                    [],
                )
                .unwrap();
            }
        }
        drop(db);
        assert!(LocalWorkspace::open(root).is_err(), "场景 {index}");
    }
    assert!(LocalWorkspace::open("/不存在的研流工作空间").is_err());
}

#[test]
fn workspace_manifest_mode_identity_and_size_are_validated() {
    for index in 0..6 {
        let f = F::new();
        let file = f.root().join("workspace.json");
        let mut info: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        match index {
            0 => info["mode"] = json!("git"),
            1 => info["schema"] = json!(2),
            2 => info["id"] = json!(Id::new_v4()),
            3 => info["name"] = json!("不同空间"),
            4 => {
                fs::write(&file, b"invalid json").unwrap();
                assert!(LocalWorkspace::open(f.root()).is_err());
                continue;
            }
            _ => info["name"] = json!("x".repeat(17000)),
        }
        fs::write(file, serde_json::to_vec(&info).unwrap()).unwrap();
        assert!(LocalWorkspace::open(f.root()).is_err());
    }
}

#[cfg(unix)]
#[test]
fn symlinked_database_blob_and_backup_directories_cannot_escape_the_workspace() {
    use std::os::unix::fs::symlink;
    let mut f = F::new();
    let doc = f.document("链接边界");
    let upload = f.upload_request(doc);
    let bytes = b"protected";
    f.workspace.upload(upload.clone(), bytes).unwrap();
    let blob = f.blob(bytes);
    let outside = f.directory.path().join("outside");
    fs::write(&outside, bytes).unwrap();
    fs::remove_file(&blob).unwrap();
    symlink(&outside, &blob).unwrap();
    assert!(f.workspace.read_attachment(f.owner.id, upload.id).is_err());
    assert!(LocalWorkspace::open(f.root()).is_err());
    fs::remove_file(&blob).unwrap();
    fs::write(&blob, bytes).unwrap();
    let backup = f.backup();
    let original = backup.join("attachments");
    let moved = f.directory.path().join("moved");
    fs::rename(&original, &moved).unwrap();
    symlink(&moved, &original).unwrap();
    assert!(LocalWorkspace::restore(&backup, f.directory.path().join("blocked")).is_err());
    let db = f.root().join("data.sqlite");
    drop(f.workspace);
    let moved_db = f.directory.path().join("database");
    fs::rename(&db, &moved_db).unwrap();
    symlink(moved_db, &db).unwrap();
    assert!(LocalWorkspace::open(f.directory.path().join("workspace")).is_err());
}

#[test]
fn archived_object_blocks_attachment_download_but_remains_recoverable() {
    let mut f = F::new();
    let doc = f.document("归档附件");
    let upload = f.upload_request(doc);
    f.workspace.upload(upload.clone(), b"evidence").unwrap();
    f.ok(Command::Archive {
        id: doc,
        version: 1,
        note: "归档内容".into(),
    });
    assert!(f.workspace.read_attachment(f.owner.id, upload.id).is_err());
    f.ok(Command::Restore {
        id: doc,
        version: 2,
    });
    assert_eq!(
        f.workspace.read_attachment(f.owner.id, upload.id).unwrap(),
        b"evidence"
    );
    let state = f.state();
    assert_eq!(state.versions.len(), 1);
    assert_eq!(state.history.len(), 4);
    assert_eq!(state.items[&doc].version, 3);
    assert_eq!(state.attachments[&upload.id].size, 8);
    assert!(state.operations.values().all(|o| o.receipt.revision > 0));
}

#[cfg(unix)]
#[test]
fn restoring_read_only_backup_files_produces_a_writable_workspace() {
    use std::os::unix::fs::PermissionsExt;
    let mut f = F::new();
    f.document("只读备份中的内容");
    let backup = f.backup();
    for name in ["data.sqlite", "workspace.json", "backup.json"] {
        fs::set_permissions(backup.join(name), fs::Permissions::from_mode(0o444)).unwrap();
    }
    let mut restored =
        LocalWorkspace::restore(&backup, f.directory.path().join("writable")).unwrap();
    restored
        .execute(f.request(Command::Create {
            item: document("恢复后可继续保存"),
        }))
        .unwrap();
    assert_eq!(restored.snapshot(f.owner.id).unwrap().items.len(), 2);
}
