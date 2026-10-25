use farflow_core::*;
use rusqlite::Connection;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::sync::{Arc, Barrier};

struct F {
    directory: tempfile::TempDir,
    workspace: GitWorkspace,
    owner: Member,
    document: Id,
}
fn member(account: &str, role: Role) -> Member {
    Member {
        id: Id::new_v4(),
        account: account.into(),
        name: account.into(),
        department: "研发".into(),
        roles: BTreeSet::from([role]),
        active: true,
    }
}
fn document(title: &str) -> NewItem {
    serde_json::from_value(
        json!({"id":Id::new_v4(),"kind":"document","title":title,"description":"原始正文"}),
    )
    .unwrap()
}
fn req(actor: Id, command: Command) -> Request {
    Request {
        id: Id::new_v4(),
        actor_id: actor,
        at: 100,
        command,
    }
}
fn value(v: &impl serde::Serialize) -> Value {
    serde_json::to_value(v).unwrap()
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn op_path(id: Id) -> String {
    let id = id.to_string();
    format!("operations/{}/{}.json", &id[..2], id)
}
fn git(root: &Path, args: &[&str]) -> String {
    let output = Process::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@localhost",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
impl F {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let owner = member("owner", Role::Owner);
        let workspace = GitWorkspace::create(
            directory.path().join("repo"),
            Id::new_v4(),
            "研流仓库".into(),
            owner.clone(),
        )
        .unwrap();
        let item = document("基础文档");
        let id = item.id;
        workspace
            .execute(req(owner.id, Command::Create { item }))
            .unwrap();
        Self {
            directory,
            workspace,
            owner,
            document: id,
        }
    }
    fn root(&self) -> PathBuf {
        self.directory.path().join("repo")
    }
    fn state(&self) -> GitView {
        self.workspace.snapshot(self.owner.id).unwrap()
    }
    fn upload(&self, id: Id) -> Upload {
        Upload {
            operation_id: Id::new_v4(),
            actor_id: self.owner.id,
            at: 101,
            id: Id::new_v4(),
            item_id: id,
            name: "证据.txt".into(),
            mime: "text/plain".into(),
        }
    }
    fn pack(&self, name: &str) -> PathBuf {
        let path = self.directory.path().join(name);
        self.workspace.export_pack(self.owner.id, &path).unwrap();
        path
    }
    fn fork(&self) -> GitWorkspace {
        let pack = self.pack("initial.farflowpack");
        GitWorkspace::from_pack(pack, self.directory.path().join("fork")).unwrap()
    }
}

#[test]
fn repository_saves_immutable_operations_and_reopens_without_an_application_service() {
    let f = F::new();
    assert_eq!(f.workspace.info().mode, Mode::Git);
    assert_eq!(f.workspace.info().name, "研流仓库");
    let before = value(&f.state());
    let root = f.root();
    drop(f.workspace);
    let reopened = GitWorkspace::open(&root).unwrap();
    assert_eq!(value(&reopened.snapshot(f.owner.id).unwrap()), before);
    assert!(root.join(".git/farflow/index.sqlite").exists());
    let count = fs::read_dir(root.join("operations"))
        .unwrap()
        .map(|d| fs::read_dir(d.unwrap().path()).unwrap().count())
        .sum::<usize>();
    assert_eq!(count, 1);
    assert!(git(&root, &["ls-files"]).is_empty());
    reopened.commit(f.owner.id, "保存初始业务").unwrap();
    let tracked = git(&root, &["ls-files"]);
    assert!(tracked.contains("workspace.json"));
    assert!(tracked.contains("operations/"));
    assert!(!tracked.contains("sqlite"));
    assert!(!tracked.contains("device.json"));
}

#[test]
fn oversized_operations_are_rejected_before_source_files_are_written() {
    let f = F::new();
    let before = value(&f.state());
    let commands = (0..50)
        .map(|_| {
            let mut item = document("超大批次");
            item.description = "x".repeat(50_000);
            Command::Create { item }
        })
        .collect();
    let request = req(f.owner.id, Command::Batch { commands });
    let operation = request.id;
    let error = f.workspace.execute(request).unwrap_err();
    assert!(error.to_string().contains("2 MiB"), "{error}");
    assert!(!f.root().join(op_path(operation)).exists());
    assert_eq!(value(&f.state()), before);
    let reopened = GitWorkspace::open(f.root()).unwrap();
    assert_eq!(value(&reopened.snapshot(f.owner.id).unwrap()), before);
    reopened
        .execute(req(
            f.owner.id,
            Command::Create {
                item: document("正常保存"),
            },
        ))
        .unwrap();
    assert_eq!(reopened.snapshot(f.owner.id).unwrap().state.items.len(), 2);
}

#[test]
fn cache_failure_reports_saved_source_and_retries_do_not_duplicate_the_operation() {
    let f = F::new();
    let db = Connection::open(f.root().join(".git/farflow/index.sqlite")).unwrap();
    db.execute_batch("CREATE TRIGGER reject_index BEFORE UPDATE ON projection BEGIN SELECT RAISE(ABORT,'模拟索引失败'); END;").unwrap();
    let request = req(
        f.owner.id,
        Command::Create {
            item: document("索引失败仍已保存"),
        },
    );
    let operation = request.id;
    let saved = f.workspace.execute(request.clone()).unwrap();
    assert!(!saved.indexed);
    assert!(saved.warning.as_ref().unwrap().contains("已保存在本机"));
    assert!(f.root().join(op_path(operation)).exists());
    assert_eq!(f.state().state.items.len(), 2);
    let retry = f.workspace.execute(request).unwrap();
    assert_eq!(retry.receipt, saved.receipt);
    assert_eq!(f.state().state.operations.len(), 2);
    drop(db);
    let restored = f.workspace.rebuild_index(f.owner.id).unwrap();
    assert_eq!(restored.state.items.len(), 2);
    let result: GitSave = serde_json::from_value(value(&saved)).unwrap();
    assert_eq!(result.receipt.operation_id, operation);
}

#[test]
fn deleting_or_corrupting_index_rebuilds_from_source_and_does_not_change_data() {
    let f = F::new();
    let before = value(&f.state());
    let index = f.root().join(".git/farflow/index.sqlite");
    fs::remove_file(&index).unwrap();
    assert_eq!(
        value(&f.workspace.rebuild_index(f.owner.id).unwrap()),
        before
    );
    fs::write(&index, b"not a database").unwrap();
    assert_eq!(value(&f.state()), before);
    assert_eq!(
        value(&f.workspace.rebuild_index(f.owner.id).unwrap()),
        before
    );
    let connection = Connection::open(&index).unwrap();
    connection
        .execute("UPDATE projection SET data='invalid json'", [])
        .unwrap();
    assert_eq!(value(&f.state()), before);
}

#[test]
fn uploaded_bytes_are_deduplicated_and_retained_for_history() {
    let f = F::new();
    let first = f.upload(f.document);
    let second = f.upload(f.document);
    let bytes = "真实附件".as_bytes();
    let receipt = f.workspace.upload(first.clone(), bytes).unwrap();
    assert!(receipt.indexed);
    assert_eq!(
        f.workspace.upload(first.clone(), bytes).unwrap().receipt,
        receipt.receipt
    );
    f.workspace.upload(second.clone(), bytes).unwrap();
    assert_eq!(
        f.workspace.read_attachment(f.owner.id, first.id).unwrap(),
        bytes
    );
    let hash = hash(bytes);
    assert_eq!(
        fs::read_dir(f.root().join("attachments").join(&hash[..2]))
            .unwrap()
            .count(),
        1
    );
    f.workspace
        .execute(req(f.owner.id, Command::RemoveAttachment { id: first.id }))
        .unwrap();
    assert!(f.workspace.read_attachment(f.owner.id, first.id).is_err());
    assert_eq!(
        f.workspace.read_attachment(f.owner.id, second.id).unwrap(),
        bytes
    );
    let pack = f.pack("attachments.farflowpack");
    let restored = GitWorkspace::from_pack(pack, f.directory.path().join("restored")).unwrap();
    assert_eq!(
        restored.read_attachment(f.owner.id, second.id).unwrap(),
        bytes
    );
    assert!(restored.snapshot(f.owner.id).unwrap().state.attachments[&first.id].deleted);
}

#[test]
fn two_real_repositories_exchange_independent_changes_and_attachments() {
    let f = F::new();
    let fork = f.fork();
    let left = document("本机需求");
    let left_id = left.id;
    let right = document("另一个副本需求");
    let right_id = right.id;
    f.workspace
        .execute(req(f.owner.id, Command::Create { item: left }))
        .unwrap();
    fork.execute(req(f.owner.id, Command::Create { item: right }))
        .unwrap();
    let upload = f.upload(f.document);
    fork.upload(upload.clone(), b"remote attachment").unwrap();
    let fork_root = f.directory.path().join("fork");
    let forward = f.workspace.merge_from(f.owner.id, &fork_root).unwrap();
    let backward = fork.merge_from(f.owner.id, f.root()).unwrap();
    assert_eq!(value(&forward), value(&backward));
    assert!(forward.issues.is_empty());
    assert!(forward.state.items.contains_key(&left_id));
    assert!(forward.state.items.contains_key(&right_id));
    assert_eq!(
        f.workspace.read_attachment(f.owner.id, upload.id).unwrap(),
        b"remote attachment"
    );
    let before = value(&forward);
    f.workspace.merge_from(f.owner.id, &fork_root).unwrap();
    assert_eq!(value(&f.state()), before);
    let a: Value =
        serde_json::from_slice(&fs::read(f.root().join(".git/farflow/device.json")).unwrap())
            .unwrap();
    let b: Value =
        serde_json::from_slice(&fs::read(fork_root.join(".git/farflow/device.json")).unwrap())
            .unwrap();
    assert_ne!(a, b);
}

#[test]
fn offline_bundle_import_preserves_business_data_and_both_git_ancestors() {
    let f = F::new();
    let fork = f.fork();
    f.workspace
        .execute(req(
            f.owner.id,
            Command::Create {
                item: document("主仓库修改"),
            },
        ))
        .unwrap();
    let original = f.workspace.commit(f.owner.id, "主仓库记录").unwrap();
    fork.execute(req(
        f.owner.id,
        Command::Create {
            item: document("副本修改"),
        },
    ))
    .unwrap();
    let incoming = fork.commit(f.owner.id, "副本记录").unwrap();
    let pack = f.directory.path().join("incoming.farflowpack");
    fork.export_pack(f.owner.id, &pack).unwrap();
    let merged = f.workspace.import_pack(f.owner.id, &pack).unwrap();
    assert_eq!(merged.state.items.len(), 3);
    let committed = f.workspace.commit(f.owner.id, "合并文件包").unwrap();
    assert_eq!(
        git(
            &f.root(),
            &["merge-base", "--is-ancestor", &original, &committed]
        ),
        ""
    );
    assert_eq!(
        git(
            &f.root(),
            &["merge-base", "--is-ancestor", &incoming, &committed]
        ),
        ""
    );
    let before = value(&f.state());
    f.workspace.import_pack(f.owner.id, &pack).unwrap();
    assert_eq!(value(&f.state()), before);
    assert_eq!(
        f.workspace.commit(f.owner.id, "重复包无需新提交").unwrap(),
        committed
    );
}

#[test]
fn conflicting_content_is_preserved_and_resolution_is_persisted_as_a_new_operation() {
    let f = F::new();
    let fork = f.fork();
    for (workspace, text) in [(&f.workspace, "本机正文"), (&fork, "对方正文")] {
        workspace
            .execute(req(
                f.owner.id,
                Command::Update {
                    id: f.document,
                    version: 1,
                    patch: Patch {
                        description: Some(text.into()),
                        ..Default::default()
                    },
                },
            ))
            .unwrap();
    }
    let view = f
        .workspace
        .merge_from(f.owner.id, f.directory.path().join("fork"))
        .unwrap();
    assert_eq!(view.issues.len(), 2);
    assert_eq!(view.state.items[&f.document].description, "原始正文");
    let decision = GitResolution {
        id: Id::new_v4(),
        actor_id: f.owner.id,
        at: 200,
        targets: view.issues.keys().copied().collect(),
        command: Some(Command::Update {
            id: f.document,
            version: 1,
            patch: Patch {
                description: Some("核定的合并正文".into()),
                ..Default::default()
            },
        }),
        note: "比较并保留最终内容".into(),
    };
    let id = decision.id;
    let saved = f.workspace.resolve(decision.clone()).unwrap();
    assert!(saved.indexed);
    assert_eq!(
        f.workspace.resolve(decision).unwrap().receipt,
        saved.receipt
    );
    assert!(f.root().join(op_path(id)).exists());
    let reopened = GitWorkspace::open(f.root()).unwrap();
    let after = reopened.snapshot(f.owner.id).unwrap();
    assert!(after.issues.is_empty());
    assert_eq!(after.state.items[&f.document].description, "核定的合并正文");
}

#[test]
fn conflicting_attachment_proposals_keep_both_blobs_in_the_bundle() {
    let f = F::new();
    let fork = f.fork();
    let first = f.upload(f.document);
    let mut second = f.upload(f.document);
    second.id = first.id;
    f.workspace.upload(first, b"local proposal").unwrap();
    fork.upload(second, b"remote proposal").unwrap();
    let view = f
        .workspace
        .merge_from(f.owner.id, f.directory.path().join("fork"))
        .unwrap();
    assert_eq!(view.issues.len(), 2);
    let pack = f.pack("conflicts.farflowpack");
    let restored = GitWorkspace::from_pack(pack, f.directory.path().join("conflict-copy")).unwrap();
    assert_eq!(restored.snapshot(f.owner.id).unwrap().issues.len(), 2);
    for bytes in [b"local proposal".as_slice(), b"remote proposal".as_slice()] {
        let hash = hash(bytes);
        assert_eq!(
            fs::read(
                f.directory
                    .path()
                    .join("conflict-copy/attachments")
                    .join(&hash[..2])
                    .join(hash)
            )
            .unwrap(),
            bytes
        );
    }
}

#[test]
fn stale_simultaneous_writes_from_two_handles_have_exactly_one_winner() {
    let f = F::new();
    let root = f.root();
    let actor = f.owner.id;
    let id = f.document;
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|index| {
            let root = root.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let workspace = GitWorkspace::open(root).unwrap();
                barrier.wait();
                workspace.execute(req(
                    actor,
                    Command::Update {
                        id,
                        version: 1,
                        patch: Patch {
                            title: Some(format!("并发标题 {index}")),
                            ..Default::default()
                        },
                    },
                ))
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|r| r.is_err()).count(), 1);
    assert_eq!(f.state().state.items[&id].version, 2);
    assert_eq!(f.state().state.operations.len(), 2);
}

#[test]
fn role_checks_apply_to_read_write_commit_and_export() {
    let f = F::new();
    let viewer = member("viewer", Role::Viewer);
    f.workspace
        .execute(req(
            f.owner.id,
            Command::AddMember {
                member: viewer.clone(),
            },
        ))
        .unwrap();
    assert!(f.workspace.snapshot(viewer.id).is_ok());
    assert!(f
        .workspace
        .execute(req(
            viewer.id,
            Command::Create {
                item: document("越权")
            }
        ))
        .is_err());
    assert!(f.workspace.commit(viewer.id, "不能提交").is_err());
    assert!(f
        .workspace
        .export_pack(viewer.id, f.directory.path().join("denied.pack"))
        .is_err());
    assert!(f.workspace.snapshot(Id::new_v4()).is_err());
    assert!(f.workspace.rebuild_index(Id::new_v4()).is_err());
    let mut inactive = viewer;
    inactive.active = false;
    f.workspace
        .execute(req(
            f.owner.id,
            Command::UpdateMember {
                member: inactive.clone(),
            },
        ))
        .unwrap();
    assert!(f.workspace.snapshot(inactive.id).is_err());
    for note in ["", "\0"] {
        assert!(f.workspace.commit(f.owner.id, note).is_err());
    }
}

#[test]
fn invalid_uploads_and_unbacked_metadata_do_not_leave_operations() {
    let f = F::new();
    let before = value(&f.state());
    for index in 0..5 {
        let mut upload = f.upload(f.document);
        let mut bytes = vec![b'x'];
        match index {
            0 => bytes.clear(),
            1 => upload.item_id = Id::new_v4(),
            2 => upload.name = "../file".into(),
            3 => bytes.resize(10 * 1024 * 1024 + 1, 0),
            _ => upload.actor_id = Id::new_v4(),
        };
        assert!(f.workspace.upload(upload, &bytes).is_err());
        assert_eq!(value(&f.state()), before);
    }
    assert!(f
        .workspace
        .execute(req(
            f.owner.id,
            Command::AddAttachment {
                id: Id::new_v4(),
                item_id: f.document,
                name: "missing.txt".into(),
                mime: "text/plain".into(),
                sha256: "a".repeat(64),
                size: 1
            }
        ))
        .is_err());
    assert_eq!(value(&f.state()), before);
}

#[test]
fn committed_operation_tampering_and_removal_are_detected() {
    for remove in [false, true] {
        let f = F::new();
        f.workspace.commit(f.owner.id, "固定记录").unwrap();
        let id = *f.state().state.operations.keys().next().unwrap();
        let file = f.root().join(op_path(id));
        if remove {
            fs::remove_file(file).unwrap();
        } else {
            let mut data: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
            data["change"]["request"]["command"]["item"]["title"] = json!("直接改写历史");
            fs::write(file, serde_json::to_vec_pretty(&data).unwrap()).unwrap();
        }
        assert!(f.workspace.snapshot(f.owner.id).is_err());
        assert!(GitWorkspace::open(f.root()).is_err());
    }
}

#[test]
fn malformed_source_and_different_workspace_imports_do_not_modify_the_target() {
    let f = F::new();
    let other = F::new();
    let before = value(&f.state());
    assert!(f.workspace.merge_from(f.owner.id, other.root()).is_err());
    assert_eq!(value(&f.state()), before);
    let fork = f.fork();
    fork.execute(req(
        f.owner.id,
        Command::Create {
            item: document("被破坏的源操作"),
        },
    ))
    .unwrap();
    let view = fork.snapshot(f.owner.id).unwrap();
    let root = f.directory.path().join("fork");
    let original = f.state().state.operations;
    let id = *view
        .state
        .operations
        .keys()
        .find(|id| !original.contains_key(id))
        .unwrap();
    let file = root.join(op_path(id));
    let mut data: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    data["parents"] = json!([Id::new_v4()]);
    fs::write(file, serde_json::to_vec(&data).unwrap()).unwrap();
    assert!(f.workspace.merge_from(f.owner.id, root).is_err());
    assert_eq!(value(&f.state()), before);
}

fn prepare_pending(target: &Path, source: &Path, skip_existing: bool) -> Vec<String> {
    let pending = target.join(".git/farflow/pending");
    fs::create_dir(&pending).unwrap();
    let mut files = BTreeMap::new();
    for prefix in fs::read_dir(source.join("operations")).unwrap() {
        let prefix = prefix.unwrap();
        for file in fs::read_dir(prefix.path()).unwrap() {
            let file = file.unwrap();
            let relative = format!(
                "operations/{}/{}",
                prefix.file_name().to_string_lossy(),
                file.file_name().to_string_lossy()
            );
            if skip_existing && target.join(&relative).exists() {
                continue;
            }
            let bytes = fs::read(file.path()).unwrap();
            let output = pending.join(&relative);
            fs::create_dir_all(output.parent().unwrap()).unwrap();
            fs::write(output, &bytes).unwrap();
            files.insert(relative, hash(&bytes));
        }
    }
    fs::write(
        pending.join("plan.json"),
        serde_json::to_vec(&json!({"schema":1,"files":files})).unwrap(),
    )
    .unwrap();
    files.into_keys().collect()
}

#[test]
fn pending_multi_file_import_recovers_even_when_a_child_arrived_before_its_parent() {
    let f = F::new();
    let fork = f.fork();
    for index in 0..2 {
        fork.execute(req(
            f.owner.id,
            Command::Create {
                item: document(&format!("待恢复记录 {index}")),
            },
        ))
        .unwrap();
    }
    let source = f.directory.path().join("fork");
    let files = prepare_pending(&f.root(), &source, true);
    assert_eq!(files.len(), 2);
    let selected = &files[1];
    let output = f.root().join(selected);
    fs::create_dir_all(output.parent().unwrap()).unwrap();
    fs::copy(source.join(selected), output).unwrap();
    let view = f.workspace.snapshot(f.owner.id).unwrap();
    assert_eq!(view.state.items.len(), 3);
    assert!(!f.root().join(".git/farflow/pending").exists());
}

#[test]
fn corrupt_recovery_plan_is_rejected_before_new_source_files_are_installed() {
    let f = F::new();
    let fork = f.fork();
    fork.execute(req(
        f.owner.id,
        Command::Create {
            item: document("不应写入"),
        },
    ))
    .unwrap();
    let files = prepare_pending(&f.root(), &f.directory.path().join("fork"), true);
    let pending = f.root().join(".git/farflow/pending");
    let file = pending.join(&files[0]);
    let mut data: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    data["schema"] = json!(999);
    let bytes = serde_json::to_vec(&data).unwrap();
    fs::write(&file, &bytes).unwrap();
    let plan = json!({"schema":1,"files":{files[0].clone():hash(&bytes)}});
    fs::write(
        pending.join("plan.json"),
        serde_json::to_vec(&plan).unwrap(),
    )
    .unwrap();
    assert!(f.workspace.snapshot(f.owner.id).is_err());
    assert!(!f.root().join(&files[0]).exists());
    fs::remove_dir_all(pending).unwrap();
    assert_eq!(f.state().state.items.len(), 1);
}

#[test]
fn new_copies_reject_historical_operation_changes_even_when_the_tip_is_valid() {
    for remove in [false, true] {
        let f = F::new();
        f.workspace.commit(f.owner.id, "固定原始操作").unwrap();
        let original = f.state();
        let operation = *original.state.operations.keys().next().unwrap();
        let path = f.root().join(op_path(operation));
        if remove {
            fs::remove_file(&path).unwrap();
        } else {
            let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            value["change"]["request"]["at"] = json!(999);
            fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
        }
        git(&f.root(), &["add", "--all"]);
        git(&f.root(), &["commit", "-m", "人为改写历史源文件"]);
        let pack = f.directory.path().join("rewritten.farflowpack");
        git(
            &f.root(),
            &["bundle", "create", pack.to_str().unwrap(), "HEAD"],
        );
        let destination = f.directory.path().join("copy");
        let error = GitWorkspace::from_pack(&pack, &destination).err().unwrap();
        assert!(error.to_string().contains("不可变业务文件"), "{error}");
        assert!(!destination.exists());
        let error = GitWorkspace::from_remote(f.root().to_str().unwrap(), &destination)
            .err()
            .unwrap();
        assert!(error.to_string().contains("不可变业务文件"), "{error}");
        assert!(!destination.exists());
    }
}

#[test]
fn malformed_bundle_and_existing_destinations_preserve_existing_data() {
    let f = F::new();
    let malformed = f.directory.path().join("broken.pack");
    fs::write(&malformed, b"not a git bundle").unwrap();
    let before = value(&f.state());
    assert!(f.workspace.import_pack(f.owner.id, &malformed).is_err());
    let destination = f.directory.path().join("not-created");
    assert!(GitWorkspace::from_pack(&malformed, &destination).is_err());
    assert!(!destination.exists());
    assert_eq!(value(&f.state()), before);
    let pack = f.pack("valid.pack");
    assert!(f.workspace.export_pack(f.owner.id, &pack).is_err());
    assert!(GitWorkspace::from_pack(&pack, f.root()).is_err());
    assert!(GitWorkspace::create(f.root(), Id::new_v4(), "覆盖".into(), f.owner.clone()).is_err());
    assert!(GitWorkspace::create(
        f.directory.path().join("invalid"),
        Id::nil(),
        "无效".into(),
        f.owner.clone()
    )
    .is_err());
}

#[cfg(unix)]
#[test]
fn commit_does_not_execute_repository_hooks_or_content_filters() {
    use std::os::unix::fs::PermissionsExt;
    let f = F::new();
    let marker = f.directory.path().join("must-not-run");
    let hook = f.root().join(".git/hooks/pre-commit");
    fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    git(
        &f.root(),
        &[
            "config",
            "filter.evil.clean",
            &format!("touch '{}'; cat", marker.display()),
        ],
    );
    fs::write(
        f.root().join(".git/info/attributes"),
        "*.json filter=evil\n",
    )
    .unwrap();
    f.workspace.commit(f.owner.id, "无需执行脚本").unwrap();
    assert!(!marker.exists());
    assert_eq!(f.state().state.items.len(), 1);
}

#[cfg(unix)]
#[test]
fn source_symlinks_and_unsupported_branches_are_rejected() {
    use std::os::unix::fs::symlink;
    let f = F::new();
    f.workspace.commit(f.owner.id, "初始记录").unwrap();
    let id = *f.state().state.operations.keys().next().unwrap();
    let file = f.root().join(op_path(id));
    let outside = f.directory.path().join("outside.json");
    fs::rename(&file, &outside).unwrap();
    symlink(&outside, &file).unwrap();
    assert!(f.workspace.snapshot(f.owner.id).is_err());
    fs::remove_file(file).unwrap();
    fs::rename(outside, f.root().join(op_path(id))).unwrap();
    git(&f.root(), &["checkout", "-b", "unsupported"]);
    assert!(f.workspace.snapshot(f.owner.id).is_err());
    assert!(GitWorkspace::open(f.root()).is_err());
}

#[test]
fn cloning_after_an_ownership_transfer_uses_current_members_without_reviving_the_old_owner() {
    let f = F::new();
    let next = member("next-owner", Role::Owner);
    f.workspace
        .execute(req(
            f.owner.id,
            Command::AddMember {
                member: next.clone(),
            },
        ))
        .unwrap();
    let mut old = f.owner.clone();
    old.active = false;
    f.workspace
        .execute(req(f.owner.id, Command::UpdateMember { member: old }))
        .unwrap();
    let pack = f.directory.path().join("ownership.pack");
    f.workspace.export_pack(next.id, &pack).unwrap();
    let copy = GitWorkspace::from_pack(&pack, f.directory.path().join("new-copy")).unwrap();
    assert!(copy.snapshot(f.owner.id).is_err());
    let view = copy.snapshot(next.id).unwrap();
    assert!(!view.state.members[&f.owner.id].active);
    assert!(view.state.members[&next.id].roles.contains(&Role::Owner));
    assert_eq!(view.state.operations.len(), 3);
}

#[test]
fn git_parent_failure_reports_the_already_imported_data_and_is_retryable() {
    let f = F::new();
    let fork = f.fork();
    fork.execute(req(
        f.owner.id,
        Command::Create {
            item: document("已导入但关联失败"),
        },
    ))
    .unwrap();
    let pack = f.directory.path().join("parent.pack");
    fork.export_pack(f.owner.id, &pack).unwrap();
    let parent_file = f.root().join(".git/farflow/parents.json");
    fs::create_dir(&parent_file).unwrap();
    let error = f.workspace.import_pack(f.owner.id, &pack).unwrap_err();
    assert!(error.to_string().contains("业务数据已导入"));
    assert_eq!(f.state().state.items.len(), 2);
    fs::remove_dir(parent_file).unwrap();
    f.workspace.import_pack(f.owner.id, &pack).unwrap();
    assert_eq!(f.state().state.items.len(), 2);
    f.workspace.commit(f.owner.id, "恢复提交关联").unwrap();
}

#[test]
fn chinese_and_space_containing_paths_roundtrip_through_a_bundle() {
    let directory = tempfile::tempdir().unwrap();
    let owner = member("owner", Role::Owner);
    let root = directory.path().join("中文 仓库 ");
    let workspace =
        GitWorkspace::create(&root, Id::new_v4(), "中文工作空间".into(), owner.clone()).unwrap();
    workspace
        .execute(req(
            owner.id,
            Command::Create {
                item: document("路径测试"),
            },
        ))
        .unwrap();
    let pack = directory.path().join("离线 数据.farflowpack");
    workspace.export_pack(owner.id, &pack).unwrap();
    let copy = GitWorkspace::from_pack(pack, directory.path().join("副本 目录")).unwrap();
    assert_eq!(copy.snapshot(owner.id).unwrap().state.items.len(), 1);
}

#[test]
fn packages_with_unexpected_tree_paths_are_rejected_without_touching_the_target() {
    let f = F::new();
    let fork = f.fork();
    let root = f.directory.path().join("fork");
    fs::write(root.join("unexpected.sh"), b"must not execute").unwrap();
    git(&root, &["add", "--", "unexpected.sh"]);
    git(&root, &["commit", "--quiet", "-m", "unexpected tree"]);
    let pack = f.directory.path().join("invalid-tree.pack");
    let pack_text = pack.to_str().unwrap();
    git(&root, &["bundle", "create", pack_text, "HEAD"]);
    let before = value(&f.state());
    assert!(f.workspace.import_pack(f.owner.id, pack).is_err());
    assert_eq!(value(&f.state()), before);
    drop(fork);
}

#[test]
fn malformed_file_paths_profiles_and_recovery_checksums_are_rejected() {
    for index in 0..7 {
        let f = F::new();
        let metadata = f.root().join(".git/farflow");
        match index {
            0 => fs::write(
                metadata.join("device.json"),
                serde_json::to_vec(&Id::nil()).unwrap(),
            )
            .unwrap(),
            1 => fs::write(f.root().join("extra.txt"), b"unexpected").unwrap(),
            2 => {
                let mut data: Value =
                    serde_json::from_slice(&fs::read(f.root().join("workspace.json")).unwrap())
                        .unwrap();
                data["info"]["schema"] = json!(99);
                fs::write(
                    f.root().join("workspace.json"),
                    serde_json::to_vec(&data).unwrap(),
                )
                .unwrap();
            }
            3 => {
                let pending = metadata.join("pending");
                fs::create_dir(&pending).unwrap();
                fs::write(
                    pending.join("plan.json"),
                    serde_json::to_vec(&json!({"schema":99,"files":{}})).unwrap(),
                )
                .unwrap();
            }
            4 => {
                let pending = metadata.join("pending");
                fs::create_dir(&pending).unwrap();
                fs::write(
                    pending.join("plan.json"),
                    serde_json::to_vec(&json!({"schema":1,"files":{"../outside":"a".repeat(64)}}))
                        .unwrap(),
                )
                .unwrap();
            }
            5 => {
                let id = *f.state().state.operations.keys().next().unwrap();
                let path = f.root().join(op_path(id));
                let moved = path.with_file_name(format!("{}.json", Id::new_v4()));
                fs::rename(path, moved).unwrap();
            }
            _ => {
                let pending = metadata.join("pending");
                let id = Id::new_v4();
                let relative = op_path(id);
                let output = pending.join(&relative);
                fs::create_dir_all(output.parent().unwrap()).unwrap();
                fs::write(output, b"bad checksum").unwrap();
                fs::write(
                    pending.join("plan.json"),
                    serde_json::to_vec(&json!({"schema":1,"files":{relative:"a".repeat(64)}}))
                        .unwrap(),
                )
                .unwrap();
            }
        }
        assert!(GitWorkspace::open(f.root()).is_err(), "场景 {index}");
    }
}

#[cfg(unix)]
#[test]
fn dangling_metadata_links_never_create_files_outside_the_repository() {
    use std::os::unix::fs::symlink;
    let f = F::new();
    let index = f.root().join(".git/farflow/index.sqlite");
    fs::remove_file(&index).unwrap();
    let outside = f.directory.path().join("must-not-create.sqlite");
    symlink(&outside, &index).unwrap();
    assert_eq!(f.state().state.items.len(), 1);
    assert!(!outside.exists());
    assert!(f.workspace.rebuild_index(f.owner.id).is_err());
    assert!(!outside.exists());
    let lock = f.root().join(".git/farflow/lock.sqlite");
    fs::remove_file(&lock).unwrap();
    symlink(&outside, &lock).unwrap();
    assert!(GitWorkspace::open(f.root()).is_err());
    assert!(!outside.exists());
}
