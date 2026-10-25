use farflow_core::*;
use serde_json::json;
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command as Process,
};

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
fn document(actor: Id) -> Request {
    Request { id:Id::new_v4(), actor_id:actor, at:10, command:Command::Create { item:serde_json::from_value(json!({"id":Id::new_v4(),"kind":"document","title":"Git 交接原文","description":"保留\n与\t","attributes":{"score":-0.0}})).unwrap() } }
}
struct F {
    dir: tempfile::TempDir,
    source: GitWorkspace,
    owner: Member,
    developer: Member,
    viewer: Member,
    request: Request,
    item: Id,
    attachment: Id,
}
impl F {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let owner = member("owner", Role::Owner);
        let developer = member("developer", Role::Developer);
        let viewer = member("viewer", Role::Viewer);
        let source = GitWorkspace::create(
            dir.path().join("source"),
            Id::new_v4(),
            "Git 源交接".into(),
            owner.clone(),
        )
        .unwrap();
        for m in [&developer, &viewer] {
            source
                .execute(Request {
                    id: Id::new_v4(),
                    actor_id: owner.id,
                    at: 1,
                    command: Command::AddMember { member: m.clone() },
                })
                .unwrap();
        }
        let request = document(owner.id);
        let Command::Create { item } = &request.command else {
            unreachable!()
        };
        let item = item.id;
        source.execute(request.clone()).unwrap();
        let attachment = Id::new_v4();
        source
            .upload(
                Upload {
                    operation_id: Id::new_v4(),
                    actor_id: owner.id,
                    at: 11,
                    id: attachment,
                    item_id: item,
                    name: "Git证据.txt".into(),
                    mime: "text/plain".into(),
                },
                b"actual git handoff evidence",
            )
            .unwrap();
        Self {
            dir,
            source,
            owner,
            developer,
            viewer,
            request,
            item,
            attachment,
        }
    }
    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
    fn remote(&self) -> PathBuf {
        let path = self.path("remote.git");
        assert!(Process::new("git")
            .args(["init", "--bare", "--initial-branch=main"])
            .arg(&path)
            .output()
            .unwrap()
            .status
            .success());
        assert!(
            self.source
                .sync(self.owner.id, path.to_str().unwrap())
                .unwrap()
                .push
                .pushed
        );
        path
    }
    fn freeze(&self, mode: Mode) -> TransferStatus {
        self.source
            .prepare_transfer(
                self.owner.id,
                Id::new_v4(),
                200,
                mode,
                self.path("private-backup"),
            )
            .unwrap()
    }
    fn confirm_both(&self, id: Id) {
        self.source
            .acknowledge_transfer(self.owner.id, 201, id, "本人所有副本已同步并停写")
            .unwrap();
        self.source
            .acknowledge_transfer(self.developer.id, 202, id, "已核对所有本人副本与切点")
            .unwrap();
    }
}
fn transfer_files(root: &Path) -> Vec<PathBuf> {
    let mut files = vec![];
    for dir in fs::read_dir(root.join("transfers")).unwrap() {
        for file in fs::read_dir(dir.unwrap().path()).unwrap() {
            files.push(file.unwrap().path());
        }
    }
    files
}

#[test]
fn git_source_propagates_freeze_and_member_confirmations_before_local_handoff() {
    let f = F::new();
    let remote = f.remote();
    let peer = GitWorkspace::from_remote(remote.to_str().unwrap(), f.path("peer")).unwrap();
    let before = f.source.snapshot(f.owner.id).unwrap().state;
    assert!(f.source.source_transfers(f.owner.id).unwrap().is_empty());
    let status = f.freeze(Mode::Local);
    assert_eq!(status.phase, TransferPhase::Frozen);
    assert_eq!(
        f.source
            .prepare_transfer(
                f.owner.id,
                status.plan.id,
                201,
                Mode::Local,
                f.path("unused-backup")
            )
            .unwrap(),
        status
    );
    assert!(!f.path("unused-backup").exists());
    let recovery: serde_json::Value =
        serde_json::from_slice(&fs::read(f.path("private-backup/recovery.json")).unwrap()).unwrap();
    let token = recovery["control"]["token"].as_str().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(f.path("private-backup"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(f.path("private-backup/recovery.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    for path in transfer_files(&f.path("source")) {
        assert!(!fs::read_to_string(path).unwrap().contains(token));
    }
    assert!(f.source.execute(document(f.owner.id)).is_err());
    assert!(f
        .source
        .export_copy(f.owner.id, f.path("bypass-freeze"))
        .is_err());
    assert!(f
        .source
        .export_transfer(f.owner.id, f.path("too-early"))
        .is_err());
    assert!(f.source.source_transfers(Id::new_v4()).is_err());
    assert!(f
        .source
        .acknowledge_transfer(f.viewer.id, 201, status.plan.id, "只读成员")
        .is_err());
    f.source
        .acknowledge_transfer(f.owner.id, 201, status.plan.id, "已核对本人所有副本")
        .unwrap();
    assert!(
        f.source
            .sync(f.owner.id, remote.to_str().unwrap())
            .unwrap()
            .push
            .pushed
    );
    peer.pull(f.developer.id, remote.to_str().unwrap()).unwrap();
    assert_eq!(peer.transfer_status(f.viewer.id).unwrap().unwrap(), status);
    assert!(peer.execute(document(f.developer.id)).is_err());
    assert!(peer
        .cancel_transfer(f.owner.id, 202, status.plan.id)
        .is_err());
    let confirmed = peer
        .acknowledge_transfer(
            f.developer.id,
            202,
            status.plan.id,
            "已同步本人所有副本，停止写入",
        )
        .unwrap();
    assert_eq!(
        confirmed.required_members,
        BTreeSet::from([f.owner.id, f.developer.id])
    );
    assert_eq!(confirmed.confirmations.len(), 2);
    assert_eq!(
        peer.acknowledge_transfer(f.developer.id, 203, status.plan.id, "重复确认")
            .unwrap(),
        confirmed
    );
    assert!(
        peer.push(f.developer.id, remote.to_str().unwrap())
            .unwrap()
            .pushed
    );
    f.source.pull(f.owner.id, remote.to_str().unwrap()).unwrap();
    let package = f.path("formal-local");
    f.source.export_transfer(f.owner.id, &package).unwrap();
    let (mut local, report, receipt) =
        LocalWorkspace::stage_transfer(&package, f.path("local-target"), f.owner.id, 203).unwrap();
    assert_eq!(report.target.unwrap().id, before.id);
    assert_eq!(
        serde_json::to_string(&local.snapshot(f.owner.id).unwrap()).unwrap(),
        serde_json::to_string(&before).unwrap()
    );
    assert!(peer.complete_transfer(f.owner.id, 204, &receipt).is_err());
    assert!(f
        .source
        .complete_transfer(f.developer.id, 204, &receipt)
        .is_err());
    assert!(f
        .source
        .complete_transfer(
            f.owner.id,
            204,
            &TransferReceipt {
                plan_checksum: "0".repeat(64)
            }
        )
        .is_err());
    assert!(f
        .source
        .complete_transfer(f.owner.id, 201, &receipt)
        .is_err());
    assert_eq!(
        f.source.transfer_status(f.owner.id).unwrap().unwrap().phase,
        TransferPhase::Frozen
    );
    let proof = f
        .source
        .complete_transfer(f.owner.id, 204, &receipt)
        .unwrap();
    assert_eq!(proof.token, token);
    assert!(f
        .source
        .cancel_transfer(f.owner.id, 205, status.plan.id)
        .is_err());
    local.activate_transfer(f.owner.id, 205, &proof).unwrap();
    local.execute(f.request.clone()).unwrap();
    local
        .execute(Request {
            id: Id::new_v4(),
            actor_id: f.developer.id,
            at: 206,
            command: Command::AddComment {
                id: Id::new_v4(),
                item_id: f.item,
                body: "本地接续".into(),
                parent_id: None,
                mentions: BTreeSet::new(),
                attachments: BTreeSet::from([f.attachment]),
            },
        })
        .unwrap();
    assert_eq!(
        local.read_attachment(f.owner.id, f.attachment).unwrap(),
        b"actual git handoff evidence"
    );
    assert!(
        f.source
            .push(f.owner.id, remote.to_str().unwrap())
            .unwrap()
            .pushed
    );
    let cold =
        GitWorkspace::from_remote(remote.to_str().unwrap(), f.path("completed-clone")).unwrap();
    assert_eq!(
        cold.transfer_status(f.owner.id).unwrap().unwrap().phase,
        TransferPhase::Completed
    );
    assert!(cold.execute(document(f.owner.id)).is_err());
    assert_eq!(
        cold.complete_transfer(f.owner.id, 206, &receipt)
            .unwrap()
            .token,
        proof.token
    );
    assert_eq!(
        serde_json::to_string(&cold.snapshot(f.owner.id).unwrap().state).unwrap(),
        serde_json::to_string(&before).unwrap()
    );
    let pack = f.path("retired.pack");
    cold.export_pack(f.owner.id, &pack).unwrap();
    let packed = GitWorkspace::from_pack(&pack, f.path("retired-from-pack")).unwrap();
    assert_eq!(
        packed.source_transfers(f.viewer.id).unwrap()[0]
            .status
            .phase,
        TransferPhase::Completed
    );
    assert!(packed.execute(document(f.owner.id)).is_err());
}

#[test]
fn private_backup_restores_only_the_frozen_source_and_failed_completion_never_publishes_a_phase() {
    let f = F::new();
    let status = f.freeze(Mode::Local);
    assert!(GitWorkspace::restore_transfer(
        f.path("private-backup"),
        f.path("forbidden-restore"),
        f.developer.id
    )
    .is_err());
    assert!(!f.path("forbidden-restore").exists());
    let readonly = GitWorkspace::from_pack(
        f.path("private-backup/workspace.farflowpack"),
        f.path("public-clone"),
    )
    .unwrap();
    assert!(readonly.execute(document(f.owner.id)).is_err());
    f.confirm_both(status.plan.id);
    let secret = f.path(&format!(
        "source/.git/farflow/source-transfers/{}.json",
        status.plan.id
    ));
    let original = fs::read(&secret).unwrap();
    let mut damaged: serde_json::Value = serde_json::from_slice(&original).unwrap();
    damaged["token"] = json!("0".repeat(64));
    fs::write(&secret, serde_json::to_vec(&damaged).unwrap()).unwrap();
    assert!(f
        .source
        .complete_transfer(f.owner.id, 203, &status.plan.receipt().unwrap())
        .is_err());
    assert_eq!(
        f.source.transfer_status(f.owner.id).unwrap().unwrap().phase,
        TransferPhase::Frozen
    );
    fs::write(&secret, original).unwrap();
    fs::rename(f.path("source"), f.path("original-offline")).unwrap();
    assert!(f.source.execute(document(f.owner.id)).is_err());
    let restored_path = f.path("restored-source");
    let restored =
        GitWorkspace::restore_transfer(f.path("private-backup"), &restored_path, f.owner.id)
            .unwrap();
    assert_eq!(
        restored.transfer_status(f.owner.id).unwrap().unwrap(),
        status
    );
    assert!(restored.execute(document(f.owner.id)).is_err());
    for (actor, at, note) in [
        (f.owner.id, 199, "过早"),
        (f.owner.id, 201, ""),
        (f.owner.id, 201, "\0"),
    ] {
        assert!(restored
            .acknowledge_transfer(actor, at, status.plan.id, note)
            .is_err());
    }
    restored
        .acknowledge_transfer(f.owner.id, 201, status.plan.id, "本人副本已停写")
        .unwrap();
    restored
        .acknowledge_transfer(f.developer.id, 202, status.plan.id, "已停写")
        .unwrap();
    let again =
        GitWorkspace::restore_transfer(f.path("private-backup"), &restored_path, f.owner.id)
            .unwrap();
    assert_eq!(
        again.source_transfers(f.owner.id).unwrap()[0]
            .confirmations
            .len(),
        2
    );
    let package = f.path("recovered-local-package");
    restored.export_transfer(f.owner.id, &package).unwrap();
    let (mut target, _, receipt) =
        LocalWorkspace::stage_transfer(&package, f.path("recovered-local"), f.owner.id, 203)
            .unwrap();
    let proof = restored
        .complete_transfer(f.owner.id, 204, &receipt)
        .unwrap();
    target.activate_transfer(f.owner.id, 205, &proof).unwrap();
    target.execute(document(f.owner.id)).unwrap();
    assert!(
        GitWorkspace::restore_transfer(f.path("private-backup"), &restored_path, f.owner.id)
            .is_err()
    );
    assert_eq!(
        restored.transfer_status(f.owner.id).unwrap().unwrap().phase,
        TransferPhase::Completed
    );
    let recovery_path = f.path("private-backup/recovery.json");
    let mut recovery: serde_json::Value =
        serde_json::from_slice(&fs::read(&recovery_path).unwrap()).unwrap();
    recovery["schema"] = json!(99);
    fs::write(&recovery_path, serde_json::to_vec(&recovery).unwrap()).unwrap();
    assert!(GitWorkspace::restore_transfer(
        f.path("private-backup"),
        f.path("bad-recovery"),
        f.owner.id
    )
    .is_err());
    assert!(!f.path("bad-recovery").exists());
}

#[test]
fn late_offline_business_is_retained_and_requires_cancel_merge_and_a_new_cutpoint() {
    let f = F::new();
    let remote = f.remote();
    let peer = GitWorkspace::from_remote(remote.to_str().unwrap(), f.path("late-peer")).unwrap();
    let status = f.freeze(Mode::Local);
    f.source
        .acknowledge_transfer(f.owner.id, 201, status.plan.id, "已停写")
        .unwrap();
    assert!(
        f.source
            .push(f.owner.id, remote.to_str().unwrap())
            .unwrap()
            .pushed
    );
    let late = document(f.developer.id);
    let Command::Create { item } = &late.command else {
        unreachable!()
    };
    let late_id = item.id;
    peer.execute(late).unwrap();
    assert_eq!(
        peer.push(f.developer.id, remote.to_str().unwrap())
            .unwrap()
            .failure,
        Some(GitPushFailure::NonFastForward)
    );
    assert!(
        peer.sync(f.developer.id, remote.to_str().unwrap())
            .unwrap()
            .push
            .pushed
    );
    assert!(peer.source_transfers(f.developer.id).unwrap()[0].late_operations);
    assert!(peer
        .acknowledge_transfer(f.developer.id, 202, status.plan.id, "不能忽略迟到操作")
        .is_err());
    f.source.pull(f.owner.id, remote.to_str().unwrap()).unwrap();
    assert!(f
        .source
        .snapshot(f.owner.id)
        .unwrap()
        .state
        .items
        .contains_key(&late_id));
    assert!(f
        .source
        .export_transfer(f.owner.id, f.path("late-export"))
        .is_err());
    assert!(f
        .source
        .complete_transfer(f.owner.id, 203, &status.plan.receipt().unwrap())
        .is_err());
    let cancelled = f
        .source
        .cancel_transfer(f.owner.id, 203, status.plan.id)
        .unwrap();
    assert_eq!(
        f.source
            .cancel_transfer(f.owner.id, 204, status.plan.id)
            .unwrap(),
        cancelled
    );
    assert!(
        f.source
            .push(f.owner.id, remote.to_str().unwrap())
            .unwrap()
            .pushed
    );
    peer.pull(f.developer.id, remote.to_str().unwrap()).unwrap();
    peer.execute(document(f.developer.id)).unwrap();
    assert!(
        peer.push(f.developer.id, remote.to_str().unwrap())
            .unwrap()
            .pushed
    );
    f.source.pull(f.owner.id, remote.to_str().unwrap()).unwrap();
    assert!(f
        .source
        .prepare_transfer(
            f.owner.id,
            status.plan.id,
            205,
            Mode::Local,
            f.path("reuse-id")
        )
        .is_err());
    let confirmation_id = transfer_files(&f.path("source"))
        .iter()
        .find_map(|path| {
            let record: serde_json::Value =
                serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
            (record["change"]["type"] == "confirm")
                .then(|| Id::parse_str(record["id"].as_str().unwrap()).unwrap())
        })
        .unwrap();
    assert!(f
        .source
        .prepare_transfer(
            f.owner.id,
            confirmation_id,
            205,
            Mode::Local,
            f.path("reuse-confirmation-id")
        )
        .is_err());
    assert!(!f.path("reuse-confirmation-id").exists());
    let next = f
        .source
        .prepare_transfer(
            f.owner.id,
            Id::new_v4(),
            205,
            Mode::Local,
            f.path("next-backup"),
        )
        .unwrap();
    f.source
        .acknowledge_transfer(f.owner.id, 206, next.plan.id, "新切点已停写")
        .unwrap();
    f.source.push(f.owner.id, remote.to_str().unwrap()).unwrap();
    peer.pull(f.developer.id, remote.to_str().unwrap()).unwrap();
    peer.acknowledge_transfer(f.developer.id, 207, next.plan.id, "所有本人副本已合并停写")
        .unwrap();
    peer.push(f.developer.id, remote.to_str().unwrap()).unwrap();
    f.source.pull(f.owner.id, remote.to_str().unwrap()).unwrap();
    assert!(
        !f.source
            .source_transfers(f.owner.id)
            .unwrap()
            .iter()
            .find(|v| v.status.plan.id == next.plan.id)
            .unwrap()
            .late_operations
    );
    f.source
        .export_transfer(f.owner.id, f.path("after-late-package"))
        .unwrap();
    assert!(f
        .source
        .snapshot(f.owner.id)
        .unwrap()
        .state
        .items
        .contains_key(&late_id));
}

#[test]
fn parallel_freezes_remain_readable_but_cannot_complete_until_each_origin_cancels() {
    let f = F::new();
    let remote = f.remote();
    let peer =
        GitWorkspace::from_remote(remote.to_str().unwrap(), f.path("parallel-peer")).unwrap();
    let first = f.freeze(Mode::Local);
    let second = peer
        .prepare_transfer(
            f.owner.id,
            Id::new_v4(),
            200,
            Mode::Service,
            f.path("peer-backup"),
        )
        .unwrap();
    f.source
        .merge_from(f.owner.id, f.path("parallel-peer"))
        .unwrap();
    assert_eq!(f.source.source_transfers(f.owner.id).unwrap().len(), 2);
    assert!(f.source.transfer_status(f.owner.id).is_err());
    assert!(f.source.snapshot(f.viewer.id).is_ok());
    assert!(f.source.execute(document(f.owner.id)).is_err());
    assert!(f
        .source
        .acknowledge_transfer(f.owner.id, 201, first.plan.id, "存在并行计划")
        .is_err());
    assert!(f
        .source
        .export_transfer(f.owner.id, f.path("parallel-export"))
        .is_err());
    assert!(f
        .source
        .cancel_transfer(f.owner.id, 201, second.plan.id)
        .is_err());
    f.source
        .cancel_transfer(f.owner.id, 201, first.plan.id)
        .unwrap();
    peer.merge_from(f.owner.id, f.path("source")).unwrap();
    peer.cancel_transfer(f.owner.id, 202, second.plan.id)
        .unwrap();
    f.source
        .merge_from(f.owner.id, f.path("parallel-peer"))
        .unwrap();
    assert!(f
        .source
        .source_transfers(f.owner.id)
        .unwrap()
        .iter()
        .all(|v| v.status.phase == TransferPhase::Cancelled));
    f.source.execute(document(f.owner.id)).unwrap();
    let later = f
        .source
        .prepare_transfer(
            f.owner.id,
            Id::new_v4(),
            203,
            Mode::Local,
            f.path("parallel-new-backup"),
        )
        .unwrap();
    assert_eq!(
        f.source.transfer_status(f.owner.id).unwrap().unwrap(),
        later
    );
}

#[test]
fn invalid_git_prepare_paths_roles_and_tampered_public_records_never_open_writes() {
    let f = F::new();
    assert!(f
        .source
        .export_transfer(f.owner.id, f.path("not-frozen"))
        .is_err());
    assert!(f
        .source
        .complete_transfer(
            f.owner.id,
            200,
            &TransferReceipt {
                plan_checksum: "0".repeat(64)
            }
        )
        .is_err());
    assert!(f
        .source
        .cancel_transfer(f.owner.id, 200, Id::new_v4())
        .is_err());
    for (actor, id, at, mode) in [
        (f.owner.id, Id::nil(), 200, Mode::Local),
        (f.owner.id, Id::new_v4(), -1, Mode::Local),
        (f.owner.id, Id::new_v4(), 200, Mode::Git),
        (f.developer.id, Id::new_v4(), 200, Mode::Local),
    ] {
        assert!(f
            .source
            .prepare_transfer(actor, id, at, mode, f.path("invalid-backup"))
            .is_err());
        assert!(!f.path("invalid-backup").exists());
    }
    fs::write(f.path("occupied"), b"keep me").unwrap();
    let inside = f.path("source/inside-backup");
    assert!(f
        .source
        .prepare_transfer(f.owner.id, Id::new_v4(), 200, Mode::Local, &inside)
        .is_err());
    assert!(!inside.exists());
    let inside_pack = f.path("source/inside.farflowpack");
    assert!(f.source.export_pack(f.owner.id, &inside_pack).is_err());
    assert!(!inside_pack.exists());
    assert!(f.source.source_transfers(f.owner.id).unwrap().is_empty());
    #[cfg(unix)]
    {
        let alias = f.path("source-alias");
        std::os::unix::fs::symlink(f.path("source"), &alias).unwrap();
        assert!(f
            .source
            .prepare_transfer(
                f.owner.id,
                Id::new_v4(),
                200,
                Mode::Local,
                alias.join("inside-alias-backup")
            )
            .is_err());
        assert!(!f.path("source/inside-alias-backup").exists());
    }
    assert!(f
        .source
        .prepare_transfer(
            f.owner.id,
            Id::new_v4(),
            200,
            Mode::Local,
            f.path("occupied")
        )
        .is_err());
    assert_eq!(fs::read(f.path("occupied")).unwrap(), b"keep me");
    assert!(f.source.source_transfers(f.owner.id).unwrap().is_empty());
    let private_directory = f.path("source/.git/farflow/source-transfers");
    fs::write(&private_directory, b"not a directory").unwrap();
    assert!(f
        .source
        .prepare_transfer(
            f.owner.id,
            Id::new_v4(),
            200,
            Mode::Local,
            f.path("backup-before-private-failure")
        )
        .is_err());
    assert!(f
        .path("backup-before-private-failure/workspace.farflowpack")
        .exists());
    assert!(f.source.source_transfers(f.owner.id).unwrap().is_empty());
    fs::remove_file(private_directory).unwrap();
    f.source.execute(document(f.owner.id)).unwrap();
    let status = f.freeze(Mode::Local);
    let path = transfer_files(&f.path("source")).pop().unwrap();
    let original = fs::read(&path).unwrap();
    let base: serde_json::Value = serde_json::from_slice(&original).unwrap();
    let mut variants = vec![];
    let mut changed = base.clone();
    changed["device"] = json!(Id::nil());
    variants.push(changed);
    let mut changed = base.clone();
    changed["change"]["required"] = json!([f.owner.id]);
    variants.push(changed);
    let mut changed = base.clone();
    changed["change"]["previous"] = json!([status.plan.id]);
    variants.push(changed);
    let mut changed = base.clone();
    changed["change"]["previous"] = json!([Id::new_v4()]);
    variants.push(changed);
    for value in variants {
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(f.source.snapshot(f.owner.id).is_err());
        assert!(GitWorkspace::open(f.path("source")).is_err());
    }
    fs::write(&path, &original).unwrap();
    assert_eq!(
        f.source.transfer_status(f.owner.id).unwrap().unwrap(),
        status
    );
    let unknown = path.with_file_name("unexpected.json");
    fs::write(&unknown, &original).unwrap();
    assert!(f.source.snapshot(f.owner.id).is_err());
    fs::remove_file(unknown).unwrap();
    assert!(f.source.snapshot(f.owner.id).is_ok());
}

#[test]
fn completed_git_sources_keep_late_bytes_read_only_and_reexport_only_the_original_cut() {
    let f = F::new();
    let remote = f.remote();
    let offline =
        GitWorkspace::from_remote(remote.to_str().unwrap(), f.path("old-offline-copy")).unwrap();
    let before = f.source.snapshot(f.owner.id).unwrap().state;
    let frozen = f.freeze(Mode::Local);
    f.confirm_both(frozen.plan.id);
    let package = f.path("original-fixed-cut");
    f.source.export_transfer(f.owner.id, &package).unwrap();
    let (mut target, _, receipt) =
        LocalWorkspace::stage_transfer(&package, f.path("completed-local"), f.owner.id, 203)
            .unwrap();
    let proof = f
        .source
        .complete_transfer(f.owner.id, 203, &receipt)
        .unwrap();
    target.activate_transfer(f.owner.id, 204, &proof).unwrap();
    let late_file = Id::new_v4();
    offline
        .upload(
            Upload {
                operation_id: Id::new_v4(),
                actor_id: f.developer.id,
                at: 205,
                id: late_file,
                item_id: f.item,
                name: "迟到证据.txt".into(),
                mime: "text/plain".into(),
            },
            b"late evidence must survive",
        )
        .unwrap();
    f.source
        .merge_from(f.owner.id, f.path("old-offline-copy"))
        .unwrap();
    assert!(f.source.source_transfers(f.owner.id).unwrap()[0].late_operations);
    assert!(f.source.execute(document(f.owner.id)).is_err());
    assert!(f
        .source
        .cancel_transfer(f.owner.id, 206, frozen.plan.id)
        .is_err());
    assert_eq!(
        f.source
            .complete_transfer(f.owner.id, 206, &receipt)
            .unwrap()
            .token,
        proof.token
    );
    assert_eq!(
        f.source.read_attachment(f.owner.id, late_file).unwrap(),
        b"late evidence must survive"
    );
    assert!(!target
        .snapshot(f.owner.id)
        .unwrap()
        .attachments
        .contains_key(&late_file));
    let repeated = f.path("completed-reexport");
    f.source.export_transfer(f.owner.id, &repeated).unwrap();
    let original = MigrationArchive::open(&repeated)
        .unwrap()
        .transfer_history(Mode::Local)
        .unwrap()
        .validate()
        .unwrap();
    assert_eq!(
        serde_json::to_string(&original).unwrap(),
        serde_json::to_string(&before).unwrap()
    );
    let recovery = f.path("late-readonly-recovery.pack");
    f.source.export_pack(f.owner.id, &recovery).unwrap();
    let cold = GitWorkspace::from_pack(&recovery, f.path("late-readonly-clone")).unwrap();
    assert!(cold.source_transfers(f.owner.id).unwrap()[0].late_operations);
    assert_eq!(
        cold.read_attachment(f.owner.id, late_file).unwrap(),
        b"late evidence must survive"
    );
    assert!(cold.execute(document(f.owner.id)).is_err());
}

#[test]
fn late_ownership_changes_do_not_strand_frozen_control_or_restore_business_permissions() {
    let f = F::new();
    let remote = f.remote();
    let offline =
        GitWorkspace::from_remote(remote.to_str().unwrap(), f.path("ownership-offline")).unwrap();
    let frozen = f.freeze(Mode::Local);
    let mut next = f.developer.clone();
    next.roles.insert(Role::Owner);
    offline
        .execute(Request {
            id: Id::new_v4(),
            actor_id: f.owner.id,
            at: 201,
            command: Command::UpdateMember { member: next },
        })
        .unwrap();
    let mut original = f.owner.clone();
    original.active = false;
    offline
        .execute(Request {
            id: Id::new_v4(),
            actor_id: f.owner.id,
            at: 202,
            command: Command::UpdateMember { member: original },
        })
        .unwrap();
    f.source
        .merge_from(f.owner.id, f.path("ownership-offline"))
        .unwrap();
    assert!(f.source.snapshot(f.owner.id).is_err());
    assert!(f
        .source
        .cancel_transfer(f.developer.id, 203, frozen.plan.id)
        .is_err());
    assert!(f.source.source_transfers(f.owner.id).unwrap()[0].late_operations);
    assert_eq!(
        f.source.transfer_status(f.owner.id).unwrap().unwrap().phase,
        TransferPhase::Frozen
    );
    f.source
        .cancel_transfer(f.owner.id, 203, frozen.plan.id)
        .unwrap();
    assert!(f.source.snapshot(f.owner.id).is_err());
    assert!(f.source.execute(document(f.owner.id)).is_err());
    f.source.execute(document(f.developer.id)).unwrap();
    let next = f
        .source
        .prepare_transfer(
            f.developer.id,
            Id::new_v4(),
            204,
            Mode::Local,
            f.path("new-owner-backup"),
        )
        .unwrap();
    let views = f.source.source_transfers(f.developer.id).unwrap();
    assert_eq!(
        views
            .iter()
            .find(|v| v.status.plan.id == next.plan.id)
            .unwrap()
            .required_members,
        BTreeSet::from([f.developer.id])
    );
    assert!(f.source.source_transfers(f.owner.id).is_err());
}
