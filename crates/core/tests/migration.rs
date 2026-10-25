use farflow_core::*;
use rusqlite::Connection;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, fs, path::Path};

fn member(account: &str, role: Role) -> Member {
    Member {
        id: Id::new_v4(),
        account: account.into(),
        name: account.into(),
        department: "研发部".into(),
        roles: BTreeSet::from([role]),
        active: true,
    }
}
fn request(actor: Id, command: Command) -> Request {
    Request {
        id: Id::new_v4(),
        actor_id: actor,
        at: 100,
        command,
    }
}
fn document() -> NewItem {
    serde_json::from_value(json!({"id":Id::new_v4(),"kind":"document","title":"迁移需求说明","description":"保留原文\n与制表\t内容","attributes":{"score":-0.0}})).unwrap()
}
fn upload(actor: Id, item_id: Id, id: Id) -> Upload {
    Upload {
        operation_id: Id::new_v4(),
        actor_id: actor,
        at: 101,
        id,
        item_id,
        name: "原始证据.txt".into(),
        mime: "text/plain".into(),
    }
}
fn compare(mut copied: State, source: &State) {
    assert_ne!(copied.id, source.id);
    copied.id = source.id;
    assert_eq!(
        serde_json::to_string(&copied).unwrap(),
        serde_json::to_string(source).unwrap()
    );
}
fn blob_path(root: &Path, bytes: &[u8]) -> std::path::PathBuf {
    let hash = format!("{:x}", Sha256::digest(bytes));
    root.join("attachments").join(&hash[..2]).join(hash)
}

#[test]
fn local_and_git_copy_roundtrip_keeps_every_id_receipt_tombstone_and_body() {
    let dir = tempfile::tempdir().unwrap();
    let owner = member("owner", Role::Owner);
    let local_path = dir.path().join("local");
    let mut local = LocalWorkspace::create(
        &local_path,
        Id::new_v4(),
        "三模式复制".into(),
        owner.clone(),
    )
    .unwrap();
    let item = document();
    let document = item.id;
    let original = request(owner.id, Command::Create { item });
    let receipt = local.execute(original.clone()).unwrap();
    let attachment = Id::new_v4();
    local
        .upload(upload(owner.id, document, attachment), b"real evidence")
        .unwrap();
    let comment = Id::new_v4();
    local
        .execute(request(
            owner.id,
            Command::AddComment {
                id: comment,
                item_id: document,
                body: "原始评论".into(),
                parent_id: None,
                mentions: BTreeSet::from([owner.id]),
                attachments: BTreeSet::from([attachment]),
            },
        ))
        .unwrap();
    local
        .execute(request(
            owner.id,
            Command::DeleteComment {
                id: comment,
                version: 1,
            },
        ))
        .unwrap();
    local
        .execute(request(
            owner.id,
            Command::RemoveAttachment { id: attachment },
        ))
        .unwrap();
    let before = local.snapshot(owner.id).unwrap();
    let package = dir.path().join("local-package");
    let report = local.export_copy(owner.id, &package).unwrap();
    assert_eq!(report.source.mode, Mode::Local);
    assert_eq!(report.item_count, 1);
    assert_eq!(report.comment_count, 1);
    assert_eq!(report.attachment_count, 1);
    assert_eq!(report.members, vec![owner.clone()]);
    assert!(report.target.is_none());
    assert_eq!(
        inspect_migration(&package).unwrap().package_id,
        report.package_id
    );
    let git_path = dir.path().join("git");
    let new_id = Id::new_v4();
    let (git, imported) = GitWorkspace::import_copy(&package, &git_path, new_id).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&git_path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    assert_eq!(imported.origins.len(), 1);
    assert_eq!(imported.target.unwrap().id, new_id);
    compare(git.snapshot(owner.id).unwrap().state, &before);
    assert_eq!(git.execute(original).unwrap().receipt, receipt);
    let next = request(
        owner.id,
        Command::AddComment {
            id: Id::new_v4(),
            item_id: document,
            body: "复制后继续评论".into(),
            parent_id: None,
            mentions: BTreeSet::new(),
            attachments: BTreeSet::new(),
        },
    );
    git.execute(next).unwrap();
    let git_state = git.snapshot(owner.id).unwrap().state;
    assert_eq!(local.snapshot(owner.id).unwrap().revision, before.revision);
    let second = dir.path().join("git-package");
    let second_report = git.export_copy(owner.id, &second).unwrap();
    assert_eq!(second_report.origins.len(), 1);
    assert_eq!(second_report.origins[0].source_workspace_id, before.id);
    assert_eq!(second_report.origins[0].target_workspace_id, new_id);
    assert_eq!(second_report.origins[0].package_id, report.package_id);
    let local2 = dir.path().join("local2");
    let (mut copied, _) = LocalWorkspace::import_copy(&second, &local2, Id::new_v4()).unwrap();
    compare(copied.snapshot(owner.id).unwrap(), &git_state);
    copied
        .execute(request(
            owner.id,
            Command::AddComment {
                id: Id::new_v4(),
                item_id: document,
                body: "本地再次协作".into(),
                parent_id: None,
                mentions: BTreeSet::new(),
                attachments: BTreeSet::new(),
            },
        ))
        .unwrap();
    let final_state = copied.snapshot(owner.id).unwrap();
    drop(copied);
    let reopened = LocalWorkspace::open(&local2).unwrap();
    assert_eq!(
        serde_json::to_string(&reopened.snapshot(owner.id).unwrap()).unwrap(),
        serde_json::to_string(&final_state).unwrap()
    );
    let third = dir.path().join("third-package");
    assert_eq!(
        reopened
            .export_copy(owner.id, &third)
            .unwrap()
            .origins
            .len(),
        2
    );
}

#[test]
fn resolved_git_proposals_and_both_blob_histories_survive_local_backup_and_git_bundle() {
    let dir = tempfile::tempdir().unwrap();
    let owner = member("owner", Role::Owner);
    let source = GitWorkspace::create(
        dir.path().join("source"),
        Id::new_v4(),
        "冲突历史".into(),
        owner.clone(),
    )
    .unwrap();
    let item = document();
    let id = item.id;
    source
        .execute(request(owner.id, Command::Create { item }))
        .unwrap();
    let bundle = dir.path().join("base.farflowpack");
    source.export_pack(owner.id, &bundle).unwrap();
    let peer_path = dir.path().join("peer");
    let peer = GitWorkspace::from_pack(&bundle, &peer_path).unwrap();
    let attachment = Id::new_v4();
    let left_upload = upload(owner.id, id, attachment);
    let retry_left = Request {
        id: left_upload.operation_id,
        actor_id: owner.id,
        at: left_upload.at,
        command: Command::AddAttachment {
            id: attachment,
            item_id: id,
            name: left_upload.name.clone(),
            mime: left_upload.mime.clone(),
            sha256: format!("{:x}", Sha256::digest(b"left proposal")),
            size: 13,
        },
    };
    source.upload(left_upload, b"left proposal").unwrap();
    peer.upload(upload(owner.id, id, attachment), b"right proposal")
        .unwrap();
    let conflict = source.merge_from(owner.id, &peer_path).unwrap();
    assert!(!conflict.issues.is_empty());
    let rejected = dir.path().join("rejected");
    assert!(source.export_copy(owner.id, &rejected).is_err());
    assert!(!rejected.exists());
    source
        .resolve(GitResolution {
            id: Id::new_v4(),
            actor_id: owner.id,
            at: 102,
            targets: conflict.issues.keys().copied().collect(),
            command: None,
            note: "两份建议都保留在历史，本次不采纳".into(),
        })
        .unwrap();
    let package = dir.path().join("package");
    let exported = source.export_copy(owner.id, &package).unwrap();
    assert_eq!(exported.attachment_count, 0);
    assert_eq!(exported.blob_count, 2);
    let local_path = dir.path().join("local");
    let (mut local, _) = LocalWorkspace::import_copy(&package, &local_path, Id::new_v4()).unwrap();
    assert!(local.execute(retry_left).is_err());
    assert!(local.snapshot(owner.id).unwrap().attachments.is_empty());
    for bytes in [b"left proposal".as_slice(), b"right proposal"] {
        assert_eq!(fs::read(blob_path(&local_path, bytes)).unwrap(), bytes);
    }
    let backup = dir.path().join("backup");
    local.backup(owner.id, &backup).unwrap();
    let restored = LocalWorkspace::restore(&backup, dir.path().join("restored")).unwrap();
    let second = dir.path().join("second-package");
    assert_eq!(
        restored.export_copy(owner.id, &second).unwrap().blob_count,
        2
    );
    let (git, _) =
        GitWorkspace::import_copy(&second, dir.path().join("git"), Id::new_v4()).unwrap();
    let expected = git.snapshot(owner.id).unwrap().state;
    let final_bundle = dir.path().join("final.farflowpack");
    git.export_pack(owner.id, &final_bundle).unwrap();
    let clone = GitWorkspace::from_pack(&final_bundle, dir.path().join("clone")).unwrap();
    assert_eq!(
        serde_json::to_string(&clone.snapshot(owner.id).unwrap().state).unwrap(),
        serde_json::to_string(&expected).unwrap()
    );
    fs::remove_file(blob_path(&local_path, b"left proposal")).unwrap();
    assert!(LocalWorkspace::open(&local_path).is_err());
}

#[test]
fn copy_permissions_new_id_and_existing_targets_preserve_the_source() {
    let dir = tempfile::tempdir().unwrap();
    let owner = member("owner", Role::Owner);
    let viewer = member("viewer", Role::Viewer);
    let mut local = LocalWorkspace::create(
        dir.path().join("source"),
        Id::new_v4(),
        "复制权限".into(),
        owner.clone(),
    )
    .unwrap();
    local
        .execute(request(
            owner.id,
            Command::AddMember {
                member: viewer.clone(),
            },
        ))
        .unwrap();
    let path = dir.path().join("package");
    assert!(local.export_copy(viewer.id, &path).is_err());
    assert!(!path.exists());
    local.export_copy(owner.id, &path).unwrap();
    assert!(local
        .export_copy(owner.id, dir.path().join("source/inner"))
        .is_err());
    assert!(LocalWorkspace::import_copy(&path, path.join("inner"), Id::new_v4()).is_err());
    assert!(GitWorkspace::import_copy(&path, path.join("inner"), Id::new_v4()).is_err());
    let source = local.snapshot(owner.id).unwrap();
    let target = dir.path().join("target");
    for id in [Id::nil(), source.id] {
        assert!(LocalWorkspace::import_copy(&path, &target, id).is_err());
        assert!(GitWorkspace::import_copy(&path, &target, id).is_err());
        assert!(!target.exists());
    }
    fs::create_dir(&target).unwrap();
    fs::write(target.join("keep"), "现有内容").unwrap();
    assert!(LocalWorkspace::import_copy(&path, &target, Id::new_v4()).is_err());
    assert!(GitWorkspace::import_copy(&path, &target, Id::new_v4()).is_err());
    assert_eq!(fs::read_to_string(target.join("keep")).unwrap(), "现有内容");
    assert!(local.export_copy(owner.id, &path).is_err());
    let git_path = dir.path().join("git");
    let (git, _) = GitWorkspace::import_copy(&path, &git_path, Id::new_v4()).unwrap();
    assert!(git.export_copy(owner.id, git_path.join("inner")).is_err());
    assert!(git
        .export_copy(viewer.id, dir.path().join("viewer-package"))
        .is_err());
    assert_eq!(
        serde_json::to_string(&source).unwrap(),
        serde_json::to_string(&local.snapshot(owner.id).unwrap()).unwrap()
    );
}

#[test]
fn malformed_packages_lineage_and_sqlite_origins_are_rejected_before_copy() {
    let dir = tempfile::tempdir().unwrap();
    let owner = member("owner", Role::Owner);
    let mut local = LocalWorkspace::create(
        dir.path().join("source"),
        Id::new_v4(),
        "来源核对".into(),
        owner.clone(),
    )
    .unwrap();
    let item = document();
    let id = item.id;
    local
        .execute(request(owner.id, Command::Create { item }))
        .unwrap();
    local
        .upload(upload(owner.id, id, Id::new_v4()), b"evidence")
        .unwrap();
    let path = dir.path().join("package");
    local.export_copy(owner.id, &path).unwrap();
    let metadata = fs::read(path.join("migration.json")).unwrap();
    let target = dir.path().join("target");
    fs::write(blob_path(&path, b"evidence"), "corrupt").unwrap();
    assert!(inspect_migration(&path).is_err());
    assert!(LocalWorkspace::import_copy(&path, &target, Id::new_v4()).is_err());
    assert!(!target.exists());
    fs::write(blob_path(&path, b"evidence"), b"evidence").unwrap();
    for change in 0..5 {
        let mut value: Value = serde_json::from_slice(&metadata).unwrap();
        match change {
            0 => value["schema"] = json!(99),
            1 => value["id"] = json!(Id::nil()),
            2 => value["files"]["../escape"] = json!("a".repeat(64)),
            3 => value["files"] = json!({}),
            _ => value["history"]["log"]["genesis"]["revision"] = json!(5),
        }
        fs::write(
            path.join("migration.json"),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
        assert!(inspect_migration(&path).is_err());
        assert!(GitWorkspace::import_copy(&path, &target, Id::new_v4()).is_err());
        assert!(!target.exists());
    }
    fs::write(path.join("migration.json"), metadata).unwrap();
    fs::write(path.join("extra"), "unexpected").unwrap();
    assert!(inspect_migration(&path).is_err());
    fs::remove_file(path.join("extra")).unwrap();
    #[cfg(unix)]
    {
        let bytes = fs::read(blob_path(&path, b"evidence")).unwrap();
        let real = dir.path().join("real");
        fs::write(&real, &bytes).unwrap();
        fs::remove_file(blob_path(&path, b"evidence")).unwrap();
        std::os::unix::fs::symlink(&real, blob_path(&path, b"evidence")).unwrap();
        assert!(inspect_migration(&path).is_err());
        fs::remove_file(blob_path(&path, b"evidence")).unwrap();
        fs::write(blob_path(&path, b"evidence"), bytes).unwrap();
    }
    let (copied, _) = LocalWorkspace::import_copy(&path, &target, Id::new_v4()).unwrap();
    drop(copied);
    let database = Connection::open(target.join("data.sqlite")).unwrap();
    let (id, old): (String, String) = database
        .query_row(
            "SELECT id,receipt FROM migration_receipts WHERE receipt IS NOT NULL LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    database
        .execute(
            "UPDATE migration_receipts SET receipt='{}' WHERE id=?1",
            [&id],
        )
        .unwrap();
    assert!(LocalWorkspace::open(&target).is_err());
    database
        .execute(
            "UPDATE migration_receipts SET receipt=?2 WHERE id=?1",
            rusqlite::params![id, old],
        )
        .unwrap();
    assert!(LocalWorkspace::open(&target).is_ok());
    database
        .execute("UPDATE migration_origin SET data='{}'", [])
        .unwrap();
    assert!(LocalWorkspace::open(&target).is_err());
}

#[test]
fn disabled_original_owners_stay_disabled_and_copy_origins_survive_remote_sync() {
    let dir = tempfile::tempdir().unwrap();
    let original = member("original", Role::Owner);
    let next = member("next", Role::Owner);
    let mut source = LocalWorkspace::create(
        dir.path().join("source"),
        Id::new_v4(),
        "交接后复制".into(),
        original.clone(),
    )
    .unwrap();
    source
        .execute(request(
            original.id,
            Command::AddMember {
                member: next.clone(),
            },
        ))
        .unwrap();
    let mut disabled = original.clone();
    disabled.active = false;
    source
        .execute(request(next.id, Command::UpdateMember { member: disabled }))
        .unwrap();
    let package = dir.path().join("package");
    assert!(source.export_copy(original.id, &package).is_err());
    source.export_copy(next.id, &package).unwrap();
    let (git, _) =
        GitWorkspace::import_copy(&package, dir.path().join("git"), Id::new_v4()).unwrap();
    assert!(git.snapshot(original.id).is_err());
    let expected = git.snapshot(next.id).unwrap().state;
    let remote = dir.path().join("remote.git");
    assert!(std::process::Command::new("git")
        .args(["init", "--bare", "--initial-branch=main"])
        .arg(&remote)
        .output()
        .unwrap()
        .status
        .success());
    assert!(
        git.sync(next.id, remote.to_str().unwrap())
            .unwrap()
            .push
            .pushed
    );
    let clone =
        GitWorkspace::from_remote(remote.to_str().unwrap(), dir.path().join("clone")).unwrap();
    assert_eq!(
        serde_json::to_string(&clone.snapshot(next.id).unwrap().state).unwrap(),
        serde_json::to_string(&expected).unwrap()
    );
    assert!(clone.snapshot(original.id).is_err());
    let again = dir.path().join("again");
    let report = clone.export_copy(next.id, &again).unwrap();
    assert_eq!(report.origins.len(), 1);
    assert!(
        !report
            .members
            .iter()
            .find(|m| m.id == original.id)
            .unwrap()
            .active
    );
}

#[test]
fn public_client_operations_compare_effective_profiles_but_never_hide_business_changes() {
    let owner = member("owner", Role::Owner);
    let item = document();
    let original = request(owner.id, Command::Create { item });
    let mut client = ClientOperation {
        id: original.id,
        expected_revision: None,
        command: original.command.clone(),
    };
    client.verify_effective(&original).unwrap();
    client.id = Id::new_v4();
    assert!(client.verify_effective(&original).is_err());
    client.id = original.id;
    if let Command::Create { item } = &mut client.command {
        item.attributes.insert("score".into(), json!(0.0));
    }
    assert!(client.verify_effective(&original).is_err());
    let real = member("real", Role::Developer);
    let operation = request(
        owner.id,
        Command::AddMember {
            member: real.clone(),
        },
    );
    let mut proposed = real;
    proposed.account = "untrusted".into();
    proposed.name = "不可信名称".into();
    let mut client = ClientOperation {
        id: operation.id,
        expected_revision: None,
        command: Command::AddMember { member: proposed },
    };
    assert!(client.verify_effective(&operation).is_err());
    client.expected_revision = Some(0);
    client.verify_effective(&operation).unwrap();
    if let Command::AddMember { member } = &mut client.command {
        member.roles.insert(Role::Owner);
    }
    assert!(client.verify_effective(&operation).is_err());
    let mut data = serde_json::to_value(&client).unwrap();
    data["actor_id"] = json!(owner.id);
    assert!(serde_json::from_value::<ClientOperation>(data).is_err());
}
