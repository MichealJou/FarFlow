use farflow_core::*;
use rusqlite::Connection;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, fs, path::PathBuf};

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
fn request(actor: Id, command: Command) -> Request {
    Request {
        id: Id::new_v4(),
        actor_id: actor,
        at: 100,
        command,
    }
}
fn document(actor: Id) -> Request {
    let item:NewItem=serde_json::from_value(json!({"id":Id::new_v4(),"kind":"document","title":"迁移原始记录","description":"保留\n与\t内容","attributes":{"score":-0.0}})).unwrap();
    request(actor, Command::Create { item })
}
struct F {
    dir: tempfile::TempDir,
    source: LocalWorkspace,
    owner: Member,
    developer: Member,
    item: Id,
    attachment: Id,
}
impl F {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let owner = member("owner", Role::Owner);
        let developer = member("developer", Role::Developer);
        let mut source = LocalWorkspace::create(
            dir.path().join("source"),
            Id::new_v4(),
            "正式交接".into(),
            owner.clone(),
        )
        .unwrap();
        source
            .execute(request(
                owner.id,
                Command::AddMember {
                    member: developer.clone(),
                },
            ))
            .unwrap();
        let create = document(owner.id);
        let Command::Create { item } = &create.command else {
            unreachable!()
        };
        let item = item.id;
        source.execute(create).unwrap();
        let attachment = Id::new_v4();
        source
            .upload(
                Upload {
                    operation_id: Id::new_v4(),
                    actor_id: owner.id,
                    at: 101,
                    id: attachment,
                    item_id: item,
                    name: "证据.txt".into(),
                    mime: "text/plain".into(),
                },
                b"real transfer evidence",
            )
            .unwrap();
        Self {
            dir,
            source,
            owner,
            developer,
            item,
            attachment,
        }
    }
    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
    fn freeze(&mut self) -> TransferStatus {
        let backup = self.path("before-transfer");
        self.source
            .prepare_transfer(self.owner.id, Id::new_v4(), 200, Mode::Git, backup)
            .unwrap()
    }
    fn state(&self) -> State {
        self.source.snapshot(self.owner.id).unwrap()
    }
}

fn local_target_package(f: &F) -> (MigrationHistory, TransferState, PathBuf) {
    let ordinary = f.path("local-target-copy");
    f.source.export_copy(f.owner.id, &ordinary).unwrap();
    let history = MigrationArchive::open(&ordinary)
        .unwrap()
        .copied_history(Id::new_v4())
        .unwrap();
    let control = TransferState::prepare(
        &history,
        Mode::Service,
        Mode::Local,
        Id::new_v4(),
        f.owner.id,
        200,
    )
    .unwrap();
    let package = f.path("local-target-package");
    history
        .export_transfer(&control.status.plan, f.owner.id, f.path("source"), &package)
        .unwrap();
    (history, control, package)
}

#[test]
fn local_target_stages_same_ids_receipts_and_bytes_then_activates_and_restores() {
    let f = F::new();
    let (history, mut source, package) = local_target_package(&f);
    let before = history.validate().unwrap();
    let destination = f.path("local-target");
    let (mut target, report, receipt) =
        LocalWorkspace::stage_transfer(&package, &destination, f.owner.id, 201).unwrap();
    assert_eq!(report.target.unwrap().id, before.id);
    assert_eq!(report.origins.len(), 1);
    assert_eq!(target.info().mode, Mode::Local);
    assert_eq!(
        serde_json::to_string(&target.snapshot(f.owner.id).unwrap()).unwrap(),
        serde_json::to_string(&before).unwrap()
    );
    assert_eq!(
        target
            .read_attachment(f.developer.id, f.attachment)
            .unwrap(),
        b"real transfer evidence"
    );
    let mut second_connection = LocalWorkspace::open(&destination).unwrap();
    assert!(second_connection.execute(document(f.owner.id)).is_err());
    assert!(target.execute(document(f.owner.id)).is_err());
    assert!(target
        .export_copy(f.owner.id, f.path("bypass-local-stage"))
        .is_err());
    assert!(target.complete_transfer(f.owner.id, 202, &receipt).is_err());
    assert!(target
        .cancel_transfer(f.owner.id, 202, source.status.plan.id)
        .is_err());
    assert!(target
        .prepare_transfer(
            f.owner.id,
            Id::new_v4(),
            202,
            Mode::Git,
            f.path("no-stage-backup")
        )
        .is_err());
    assert!(!f.path("no-stage-backup").exists());
    assert!(target
        .upload(
            Upload {
                operation_id: Id::new_v4(),
                actor_id: f.owner.id,
                at: 202,
                id: Id::new_v4(),
                item_id: f.item,
                name: "不能提前上传.txt".into(),
                mime: "text/plain".into()
            },
            b"not yet"
        )
        .is_err());
    let (_, _, retry) =
        LocalWorkspace::stage_transfer(&package, &destination, f.owner.id, 202).unwrap();
    assert_eq!(receipt, retry);
    assert_eq!(target.transfer_events(f.owner.id).unwrap().len(), 1);
    let backup = f.path("staged-local-backup");
    target.backup(f.owner.id, &backup).unwrap();
    let mut restored = LocalWorkspace::restore(&backup, f.path("restored-local-stage")).unwrap();
    assert_eq!(
        restored.transfer_status(f.owner.id).unwrap(),
        target.transfer_status(f.owner.id).unwrap()
    );
    assert!(restored.execute(document(f.owner.id)).is_err());
    let proof = source.complete(&receipt, f.owner.id, 202).unwrap();
    for bad in [
        TransferActivation {
            transfer_id: Id::new_v4(),
            ..proof.clone()
        },
        TransferActivation {
            plan_checksum: "0".repeat(64),
            ..proof.clone()
        },
        TransferActivation {
            token: "0".repeat(64),
            ..proof.clone()
        },
    ] {
        assert!(target.activate_transfer(f.owner.id, 203, &bad).is_err());
    }
    assert!(target
        .activate_transfer(f.developer.id, 203, &proof)
        .is_err());
    assert!(target.activate_transfer(f.owner.id, 200, &proof).is_err());
    let active = target.activate_transfer(f.owner.id, 203, &proof).unwrap();
    assert_eq!(active.phase, TransferPhase::Active);
    assert_eq!(
        target.activate_transfer(f.owner.id, 204, &proof).unwrap(),
        active
    );
    assert_eq!(target.transfer_events(f.owner.id).unwrap().len(), 2);
    assert!(
        !serde_json::to_string(&target.transfer_events(f.owner.id).unwrap())
            .unwrap()
            .contains(&proof.token)
    );
    let old_request = history
        .log
        .operations()
        .values()
        .find_map(|op| match &op.change {
            GitChange::Apply { request } if matches!(request.command, Command::Create { .. }) => {
                Some(request.clone())
            }
            _ => None,
        })
        .unwrap();
    let old_receipt = history.log.view().unwrap().receipts[&old_request.id].clone();
    assert_eq!(target.execute(old_request).unwrap(), old_receipt);
    second_connection.execute(document(f.owner.id)).unwrap();
    let after = target.snapshot(f.owner.id).unwrap();
    let (again, _, _) =
        LocalWorkspace::stage_transfer(&package, &destination, f.owner.id, 204).unwrap();
    assert_eq!(again.snapshot(f.owner.id).unwrap().revision, after.revision);
    assert_eq!(again.transfer_events(f.owner.id).unwrap().len(), 2);
    let active_backup = f.path("active-local-backup");
    target.backup(f.owner.id, &active_backup).unwrap();
    let mut recovered =
        LocalWorkspace::restore(&active_backup, f.path("active-local-restored")).unwrap();
    assert_eq!(
        recovered.transfer_status(f.owner.id).unwrap().unwrap(),
        active
    );
    recovered.execute(document(f.owner.id)).unwrap();
    assert_eq!(
        recovered.read_attachment(f.owner.id, f.attachment).unwrap(),
        b"real transfer evidence"
    );
    restored.activate_transfer(f.owner.id, 203, &proof).unwrap();
    restored.execute(document(f.owner.id)).unwrap();
    let next = target
        .prepare_transfer(
            f.owner.id,
            Id::new_v4(),
            205,
            Mode::Git,
            f.path("local-next-before"),
        )
        .unwrap();
    assert!(target.activate_transfer(f.owner.id, 206, &proof).is_err());
    target
        .cancel_transfer(f.owner.id, 206, next.plan.id)
        .unwrap();
    assert_eq!(target.transfer_events(f.owner.id).unwrap().len(), 4);
    assert!(target
        .prepare_transfer(
            f.owner.id,
            source.status.plan.id,
            207,
            Mode::Git,
            f.path("reuse-local-target-plan")
        )
        .is_err());
    target.execute(document(f.owner.id)).unwrap();
}

#[test]
fn local_target_rejects_wrong_packages_actors_paths_and_preserves_existing_directories() {
    let mut f = F::new();
    let (_, mut control, package) = local_target_package(&f);
    let ordinary = f.path("local-target-copy");
    assert!(LocalWorkspace::stage_transfer(
        &ordinary,
        f.path("ordinary-is-not-formal"),
        f.owner.id,
        201
    )
    .is_err());
    for (actor, at) in [
        (f.developer.id, 201),
        (Id::new_v4(), 201),
        (f.owner.id, 199),
        (f.owner.id, -1),
    ] {
        assert!(LocalWorkspace::stage_transfer(
            &package,
            f.path("invalid-local-target"),
            actor,
            at
        )
        .is_err());
        assert!(!f.path("invalid-local-target").exists());
    }
    assert!(LocalWorkspace::stage_transfer(
        &package,
        package.join("inside-source"),
        f.owner.id,
        201
    )
    .is_err());
    let occupied = f.path("occupied-file");
    fs::write(&occupied, b"preserve me").unwrap();
    assert!(LocalWorkspace::stage_transfer(&package, &occupied, f.owner.id, 201).is_err());
    assert_eq!(fs::read(&occupied).unwrap(), b"preserve me");
    #[cfg(unix)]
    {
        let linked = f.path("linked-target");
        std::os::unix::fs::symlink(f.path("source"), &linked).unwrap();
        assert!(LocalWorkspace::stage_transfer(&package, &linked, f.owner.id, 201).is_err());
        assert_eq!(fs::read_link(&linked).unwrap(), f.path("source"));
    }
    let original = f.state();
    assert!(LocalWorkspace::stage_transfer(&package, f.path("source"), f.owner.id, 201).is_err());
    assert_eq!(
        serde_json::to_string(&original).unwrap(),
        serde_json::to_string(&f.state()).unwrap()
    );
    let fake_proof = TransferActivation {
        transfer_id: control.status.plan.id,
        plan_checksum: control.status.plan.receipt().unwrap().plan_checksum,
        token: "0".repeat(64),
    };
    assert!(f
        .source
        .activate_transfer(f.owner.id, 201, &fake_proof)
        .is_err());
    f.freeze();
    let git_package = f.path("git-target-package");
    f.source.export_transfer(f.owner.id, &git_package).unwrap();
    assert!(LocalWorkspace::stage_transfer(
        &git_package,
        f.path("wrong-mode-target"),
        f.owner.id,
        201
    )
    .is_err());
    let (mut target, _, receipt) =
        LocalWorkspace::stage_transfer(&package, f.path("cancelled-local-target"), f.owner.id, 201)
            .unwrap();
    control.cancel(f.owner.id, 202).unwrap();
    assert!(control.complete(&receipt, f.owner.id, 203).is_err());
    assert!(target
        .activate_transfer(f.owner.id, 203, &fake_proof)
        .is_err());
    assert!(target.execute(document(f.owner.id)).is_err());
    let bytes = fs::read_dir(package.join("attachments"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let blob = fs::read_dir(bytes).unwrap().next().unwrap().unwrap().path();
    fs::write(blob, b"damaged").unwrap();
    assert!(LocalWorkspace::stage_transfer(
        &package,
        f.path("corrupt-local-target"),
        f.owner.id,
        203
    )
    .is_err());
    assert!(!f.path("corrupt-local-target").exists());
}

#[test]
fn local_target_activation_rolls_back_event_failures_and_rejects_damaged_earlier_audit() {
    let f = F::new();
    let (_, mut control, package) = local_target_package(&f);
    let destination = f.path("atomic-local-target");
    let (mut target, _, receipt) =
        LocalWorkspace::stage_transfer(&package, &destination, f.owner.id, 201).unwrap();
    let proof = control.complete(&receipt, f.owner.id, 202).unwrap();
    let raw = Connection::open(destination.join("data.sqlite")).unwrap();
    assert!(raw.execute("UPDATE objects SET data=data", []).is_err());
    assert!(raw
        .execute(
            "INSERT INTO operations VALUES (?1,999,'{}','fingerprint','{}')",
            [Id::new_v4().to_string()]
        )
        .is_err());
    raw.execute_batch("CREATE TRIGGER injected_activation_failure BEFORE INSERT ON transfer_events BEGIN SELECT RAISE(ABORT,'injected failure'); END;").unwrap();
    assert!(target.activate_transfer(f.owner.id, 203, &proof).is_err());
    assert_eq!(
        target.transfer_status(f.owner.id).unwrap().unwrap().phase,
        TransferPhase::Staged
    );
    assert!(target.execute(document(f.owner.id)).is_err());
    assert_eq!(target.transfer_events(f.owner.id).unwrap().len(), 1);
    raw.execute_batch("DROP TRIGGER injected_activation_failure")
        .unwrap();
    target.activate_transfer(f.owner.id, 203, &proof).unwrap();
    let data: String = raw
        .query_row(
            "SELECT data FROM transfer_events WHERE sequence=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let mut damaged: Value = serde_json::from_str(&data).unwrap();
    damaged["changed_by"] = json!(f.developer.id);
    let damaged = serde_json::to_string(&damaged).unwrap();
    raw.execute(
        "UPDATE transfer_events SET data=?1,checksum=?2 WHERE sequence=1",
        rusqlite::params![damaged, format!("{:x}", Sha256::digest(damaged.as_bytes()))],
    )
    .unwrap();
    assert!(target.snapshot(f.owner.id).is_err());
    assert!(LocalWorkspace::open(&destination).is_err());
    raw.execute(
        "UPDATE transfer_events SET data=?1,checksum=?2 WHERE sequence=1",
        rusqlite::params![data, format!("{:x}", Sha256::digest(data.as_bytes()))],
    )
    .unwrap();
    assert!(target.snapshot(f.owner.id).is_ok());
    let mut next_owner = f.developer.clone();
    next_owner.roles.insert(Role::Owner);
    target
        .execute(request(
            f.owner.id,
            Command::UpdateMember { member: next_owner },
        ))
        .unwrap();
    let mut old_owner = f.owner.clone();
    old_owner.active = false;
    target
        .execute(request(
            f.developer.id,
            Command::UpdateMember { member: old_owner },
        ))
        .unwrap();
    assert!(LocalWorkspace::stage_transfer(&package, &destination, f.owner.id, 204).is_err());
    assert_eq!(
        target
            .activate_transfer(f.developer.id, 204, &proof)
            .unwrap()
            .changed_by,
        f.owner.id
    );
    assert!(LocalWorkspace::open(&destination).is_ok());
}

#[test]
fn local_to_git_handoff_preserves_ids_bytes_and_blocks_both_sides_until_completion() {
    let mut f = F::new();
    let before = f.state();
    let mut other = LocalWorkspace::open(f.path("source")).unwrap();
    assert!(f.source.transfer_status(f.owner.id).unwrap().is_none());
    assert!(f.source.transfer_events(f.owner.id).unwrap().is_empty());
    let status = f.freeze();
    assert_eq!(status.phase, TransferPhase::Frozen);
    assert!(f.path("before-transfer/backup.json").exists());
    assert_eq!(
        f.source
            .prepare_transfer(
                f.owner.id,
                status.plan.id,
                201,
                Mode::Git,
                f.path("unused-backup")
            )
            .unwrap(),
        status
    );
    assert!(!f.path("unused-backup").exists());
    assert_eq!(
        serde_json::to_string(&f.state()).unwrap(),
        serde_json::to_string(&before).unwrap()
    );
    assert!(f.source.execute(document(f.owner.id)).is_err());
    assert!(other.execute(document(f.owner.id)).is_err());
    assert!(f
        .source
        .export_copy(f.owner.id, f.path("bypass-copy"))
        .is_err());
    let package = f.path("transfer");
    assert!(f
        .source
        .export_transfer(f.owner.id, f.path("source/inside-source"))
        .is_err());
    assert_eq!(
        f.source.transfer_status(f.owner.id).unwrap().unwrap(),
        status
    );
    let first = f.source.export_transfer(f.owner.id, &package).unwrap();
    assert_eq!(
        f.source
            .export_transfer(f.owner.id, &package)
            .unwrap()
            .package_id,
        first.package_id
    );
    assert_eq!(first.transfer.as_ref(), Some(&status.plan));
    let archive = MigrationArchive::open(&package).unwrap();
    assert_eq!(archive.transfer_plan(), Some(&status.plan));
    assert!(archive.transfer_history(Mode::Local).is_err());
    assert!(archive.copied_history(Id::new_v4()).is_err());
    assert!(
        LocalWorkspace::import_copy(&package, f.path("forbidden-local"), Id::new_v4()).is_err()
    );
    assert!(GitWorkspace::import_copy(&package, f.path("forbidden-git"), Id::new_v4()).is_err());
    let target_path = f.path("git-target");
    assert!(GitWorkspace::stage_transfer(
        &package,
        f.path("unauthorized-stage"),
        f.developer.id,
        201
    )
    .is_err());
    assert!(!f.path("unauthorized-stage").exists());
    assert!(GitWorkspace::stage_transfer(&package, f.path("past-stage"), f.owner.id, 199).is_err());
    assert!(!f.path("past-stage").exists());
    let (target, report, receipt) =
        GitWorkspace::stage_transfer(&package, &target_path, f.owner.id, 201).unwrap();
    assert_eq!(report.target.unwrap().id, before.id);
    assert_eq!(target.info().id, before.id);
    assert_eq!(
        serde_json::to_string(&target.snapshot(f.owner.id).unwrap().state).unwrap(),
        serde_json::to_string(&before).unwrap()
    );
    assert_eq!(
        target.read_attachment(f.owner.id, f.attachment).unwrap(),
        b"real transfer evidence"
    );
    assert_eq!(
        target.transfer_status(f.owner.id).unwrap().unwrap().phase,
        TransferPhase::Staged
    );
    let (_, _, again) =
        GitWorkspace::stage_transfer(&package, &target_path, f.owner.id, 201).unwrap();
    assert_eq!(receipt, again);
    assert!(target.execute(document(f.owner.id)).is_err());
    assert!(target.commit(f.owner.id, "不能提前提交").is_err());
    assert!(target
        .export_pack(f.owner.id, f.path("forbidden.pack"))
        .is_err());
    assert!(target
        .export_copy(f.owner.id, f.path("forbidden-copy"))
        .is_err());
    assert!(target
        .resolve(GitResolution {
            id: Id::new_v4(),
            actor_id: f.owner.id,
            at: 202,
            targets: BTreeSet::new(),
            command: None,
            note: "不能提前解决".into()
        })
        .is_err());
    let remote = f.path("remote.git");
    assert!(std::process::Command::new("git")
        .args(["init", "--bare", "--initial-branch=main"])
        .arg(&remote)
        .output()
        .unwrap()
        .status
        .success());
    assert!(target.push(f.owner.id, remote.to_str().unwrap()).is_err());
    let wrong = TransferActivation {
        transfer_id: status.plan.id,
        plan_checksum: receipt.plan_checksum.clone(),
        token: "0".repeat(64),
    };
    assert!(target.activate_transfer(f.owner.id, 201, &wrong).is_err());
    assert!(f
        .source
        .complete_transfer(f.developer.id, 201, &receipt)
        .is_err());
    assert!(f
        .source
        .complete_transfer(
            f.owner.id,
            201,
            &TransferReceipt {
                plan_checksum: "0".repeat(64)
            }
        )
        .is_err());
    let activation = f
        .source
        .complete_transfer(f.owner.id, 202, &receipt)
        .unwrap();
    assert_eq!(
        f.source.transfer_status(f.owner.id).unwrap().unwrap().phase,
        TransferPhase::Completed
    );
    assert!(f
        .source
        .cancel_transfer(f.owner.id, 203, status.plan.id)
        .is_err());
    assert!(f.source.execute(document(f.owner.id)).is_err());
    assert_eq!(
        serde_json::to_string(
            &f.source
                .complete_transfer(f.owner.id, 203, &receipt)
                .unwrap()
        )
        .unwrap(),
        serde_json::to_string(&activation).unwrap()
    );
    assert_eq!(f.source.transfer_events(f.owner.id).unwrap().len(), 2);
    assert!(
        !serde_json::to_string(&f.source.transfer_events(f.owner.id).unwrap())
            .unwrap()
            .contains(&activation.token)
    );
    assert!(!fs::read_to_string(package.join("migration.json"))
        .unwrap()
        .contains(&activation.token));
    assert!(target
        .activate_transfer(f.developer.id, 204, &activation)
        .is_err());
    let active = target
        .activate_transfer(f.owner.id, 204, &activation)
        .unwrap();
    assert_eq!(active.phase, TransferPhase::Active);
    assert_eq!(
        target
            .activate_transfer(f.owner.id, 205, &activation)
            .unwrap(),
        active
    );
    target
        .execute(request(
            f.developer.id,
            Command::AddComment {
                id: Id::new_v4(),
                item_id: f.item,
                body: "目标继续开发".into(),
                parent_id: None,
                mentions: BTreeSet::new(),
                attachments: BTreeSet::from([f.attachment]),
            },
        ))
        .unwrap();
    let after = target.snapshot(f.owner.id).unwrap().state;
    target.commit(f.owner.id, "交接后继续协作").unwrap();
    let cold = GitWorkspace::open(&target_path).unwrap();
    assert_eq!(cold.transfer_status(f.owner.id).unwrap().unwrap(), active);
    let (_, _, _) = GitWorkspace::stage_transfer(&package, &target_path, f.owner.id, 205).unwrap();
    assert_eq!(
        cold.snapshot(f.owner.id).unwrap().state.revision,
        after.revision
    );
    let pack = f.path("active.pack");
    cold.export_pack(f.owner.id, &pack).unwrap();
    let copy = GitWorkspace::from_pack(&pack, f.path("clone")).unwrap();
    assert_eq!(copy.transfer_status(f.owner.id).unwrap().unwrap(), active);
    assert_eq!(
        copy.read_attachment(f.owner.id, f.attachment).unwrap(),
        b"real transfer evidence"
    );
    assert!(
        copy.sync(f.owner.id, remote.to_str().unwrap())
            .unwrap()
            .push
            .pushed
    );
    let fetched = GitWorkspace::from_remote(remote.to_str().unwrap(), f.path("fetched")).unwrap();
    assert_eq!(
        fetched.transfer_status(f.owner.id).unwrap().unwrap(),
        active
    );
    let backup = f.path("frozen-backup");
    f.source.backup(f.owner.id, &backup).unwrap();
    let mut restored = LocalWorkspace::restore(&backup, f.path("restored-source")).unwrap();
    assert!(restored.execute(document(f.owner.id)).is_err());
    assert_eq!(
        serde_json::to_string(
            &restored
                .complete_transfer(f.owner.id, 206, &receipt)
                .unwrap()
        )
        .unwrap(),
        serde_json::to_string(&activation).unwrap()
    );
}

#[test]
fn cancellation_restores_only_the_source_and_old_targets_never_gain_a_completion_token() {
    let mut f = F::new();
    let first = f.freeze();
    let package = f.path("cancelled-package");
    f.source.export_transfer(f.owner.id, &package).unwrap();
    let (staged, _, old_receipt) =
        GitWorkspace::stage_transfer(&package, f.path("old-target"), f.owner.id, 201).unwrap();
    assert!(f
        .source
        .cancel_transfer(f.owner.id, 201, Id::new_v4())
        .is_err());
    assert!(f
        .source
        .cancel_transfer(f.developer.id, 201, first.plan.id)
        .is_err());
    let cancelled = f
        .source
        .cancel_transfer(f.owner.id, 201, first.plan.id)
        .unwrap();
    assert_eq!(cancelled.phase, TransferPhase::Cancelled);
    assert_eq!(
        f.source
            .cancel_transfer(f.owner.id, 202, first.plan.id)
            .unwrap(),
        cancelled
    );
    assert!(f
        .source
        .complete_transfer(f.owner.id, 202, &old_receipt)
        .is_err());
    assert!(f
        .source
        .export_transfer(f.owner.id, f.path("cancelled-export"))
        .is_err());
    f.source.execute(document(f.owner.id)).unwrap();
    assert!(staged.execute(document(f.owner.id)).is_err());
    assert!(f
        .source
        .prepare_transfer(
            f.owner.id,
            first.plan.id,
            203,
            Mode::Git,
            f.path("reuse-id")
        )
        .is_err());
    let backup = f.path("second-backup");
    let second = f
        .source
        .prepare_transfer(f.owner.id, Id::new_v4(), 204, Mode::Git, backup)
        .unwrap();
    assert!(f
        .source
        .complete_transfer(f.owner.id, 205, &old_receipt)
        .is_err());
    let backup = f.path("pending-backup");
    f.source.backup(f.owner.id, &backup).unwrap();
    let mut resumed = LocalWorkspace::restore(&backup, f.path("resume")).unwrap();
    assert_eq!(
        resumed.transfer_status(f.owner.id).unwrap().unwrap(),
        second
    );
    let package = f.path("second-package");
    resumed.export_transfer(f.owner.id, &package).unwrap();
    let (target, _, receipt) =
        GitWorkspace::stage_transfer(&package, f.path("second-target"), f.owner.id, 205).unwrap();
    let proof = resumed
        .complete_transfer(f.owner.id, 206, &receipt)
        .unwrap();
    assert!(staged.activate_transfer(f.owner.id, 207, &proof).is_err());
    target.activate_transfer(f.owner.id, 207, &proof).unwrap();
    target.execute(document(f.owner.id)).unwrap();
    assert_eq!(resumed.transfer_events(f.owner.id).unwrap().len(), 4);
}

#[test]
fn sqlite_fence_also_blocks_previously_opened_sql_connections_and_rolls_back_failed_preparation() {
    let mut f = F::new();
    let raw = Connection::open(f.path("source/data.sqlite")).unwrap();
    raw.execute_batch("CREATE TABLE transfer_events(sequence INTEGER PRIMARY KEY,data TEXT NOT NULL,checksum TEXT NOT NULL); CREATE TRIGGER injected_transfer_failure BEFORE INSERT ON transfer_events BEGIN SELECT RAISE(ABORT,'injected failure'); END;").unwrap();
    let failed = f.path("failed-backup");
    assert!(f
        .source
        .prepare_transfer(f.owner.id, Id::new_v4(), 200, Mode::Git, failed)
        .is_err());
    assert_eq!(
        raw.pragma_query_value::<u32, _>(None, "user_version", |r| r.get(0))
            .unwrap(),
        1
    );
    assert!(f.source.transfer_status(f.owner.id).unwrap().is_none());
    f.source.execute(document(f.owner.id)).unwrap();
    raw.execute_batch("DROP TRIGGER injected_transfer_failure")
        .unwrap();
    raw.execute_batch(
        "CREATE TRIGGER transfer_workspace_INSERT BEFORE INSERT ON workspace BEGIN SELECT 1; END;",
    )
    .unwrap();
    assert!(f
        .source
        .prepare_transfer(
            f.owner.id,
            Id::new_v4(),
            200,
            Mode::Git,
            f.path("bad-barrier-backup")
        )
        .is_err());
    assert_eq!(
        raw.pragma_query_value::<u32, _>(None, "user_version", |r| r.get(0))
            .unwrap(),
        1
    );
    assert!(f.source.transfer_status(f.owner.id).unwrap().is_none());
    raw.execute_batch("DROP TRIGGER transfer_workspace_INSERT")
        .unwrap();
    let status = f.freeze();
    for table in [
        "workspace",
        "objects",
        "content_versions",
        "history",
        "operations",
    ] {
        assert!(
            raw.execute(&format!("UPDATE {table} SET rowid=rowid"), [])
                .is_err(),
            "{table}"
        );
        assert!(
            raw.execute(&format!("DELETE FROM {table}"), []).is_err(),
            "{table}"
        );
        assert!(
            raw.execute(
                &format!("INSERT INTO {table} SELECT * FROM {table} LIMIT 1"),
                []
            )
            .is_err(),
            "{table}"
        );
    }
    assert_eq!(
        raw.pragma_query_value::<u32, _>(None, "user_version", |r| r.get(0))
            .unwrap(),
        3
    );
    f.source
        .cancel_transfer(f.owner.id, 201, status.plan.id)
        .unwrap();
    raw.execute("UPDATE workspace SET revision=revision", [])
        .unwrap();
    assert!(LocalWorkspace::open(f.path("source")).is_ok());
    let next = f
        .source
        .prepare_transfer(
            f.owner.id,
            Id::new_v4(),
            202,
            Mode::Git,
            f.path("next-before"),
        )
        .unwrap();
    f.source
        .cancel_transfer(f.owner.id, 203, next.plan.id)
        .unwrap();
    assert!(f
        .source
        .prepare_transfer(
            f.owner.id,
            status.plan.id,
            204,
            Mode::Git,
            f.path("old-id-again")
        )
        .is_err());
    assert!(!f.path("old-id-again").exists());
}

#[test]
fn invalid_roles_inputs_existing_destinations_and_corrupt_control_are_rejected() {
    let mut f = F::new();
    assert!(f
        .source
        .export_transfer(f.owner.id, f.path("no-plan"))
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
        (f.owner.id, Id::nil(), 200, Mode::Git),
        (f.owner.id, Id::new_v4(), 200, Mode::Local),
        (f.owner.id, Id::new_v4(), -1, Mode::Git),
        (f.developer.id, Id::new_v4(), 200, Mode::Git),
    ] {
        assert!(f
            .source
            .prepare_transfer(actor, id, at, mode, f.path("invalid-backup"))
            .is_err());
        assert!(!f.path("invalid-backup").exists());
    }
    let ordinary = f.path("copy");
    f.source.export_copy(f.owner.id, &ordinary).unwrap();
    assert!(MigrationArchive::open(&ordinary)
        .unwrap()
        .transfer_history(Mode::Git)
        .is_err());
    assert!(
        GitWorkspace::stage_transfer(&ordinary, f.path("not-transfer"), f.owner.id, 201).is_err()
    );
    let native = GitWorkspace::create(
        f.path("native"),
        Id::new_v4(),
        "原有空间".into(),
        f.owner.clone(),
    )
    .unwrap();
    assert!(native.transfer_status(f.owner.id).unwrap().is_none());
    assert!(native
        .activate_transfer(
            f.owner.id,
            200,
            &TransferActivation {
                transfer_id: Id::new_v4(),
                plan_checksum: "0".repeat(64),
                token: "0".repeat(64)
            }
        )
        .is_err());
    let status = f.freeze();
    assert!(f.source.transfer_events(f.developer.id).is_err());
    assert!(f
        .source
        .prepare_transfer(
            f.owner.id,
            Id::new_v4(),
            201,
            Mode::Git,
            f.path("other-backup")
        )
        .is_err());
    let package = f.path("package");
    f.source.export_transfer(f.owner.id, &package).unwrap();
    assert!(GitWorkspace::stage_transfer(&package, f.path("native"), f.owner.id, 201).is_err());
    assert!(f.source.export_transfer(f.owner.id, &ordinary).is_err());
    let raw = Connection::open(f.path("source/data.sqlite")).unwrap();
    let (data, hash): (String, String) = raw
        .query_row("SELECT data,checksum FROM transfer_control", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    raw.execute("UPDATE transfer_control SET checksum='damaged'", [])
        .unwrap();
    assert!(f.source.snapshot(f.owner.id).is_err());
    assert!(LocalWorkspace::open(f.path("source")).is_err());
    raw.execute("UPDATE transfer_control SET checksum=?1", [&hash])
        .unwrap();
    raw.execute("UPDATE transfer_control SET writable=1", [])
        .unwrap();
    assert!(f.source.snapshot(f.owner.id).is_err());
    raw.execute("UPDATE transfer_control SET writable=0", [])
        .unwrap();
    let mut damaged: Value = serde_json::from_str(&data).unwrap();
    damaged["token"] = json!("0".repeat(64));
    let text = serde_json::to_string(&damaged).unwrap();
    raw.execute(
        "UPDATE transfer_control SET data=?1,checksum=?2",
        rusqlite::params![text, format!("{:x}", Sha256::digest(text.as_bytes()))],
    )
    .unwrap();
    assert!(f.source.snapshot(f.owner.id).is_err());
    raw.execute(
        "UPDATE transfer_control SET data=?1,checksum=?2",
        rusqlite::params![data, hash],
    )
    .unwrap();
    assert_eq!(
        f.source.transfer_status(f.owner.id).unwrap().unwrap(),
        status
    );
    let trigger: String = raw
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE name='transfer_workspace_UPDATE'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    raw.execute_batch("DROP TRIGGER transfer_workspace_UPDATE")
        .unwrap();
    assert!(f.source.snapshot(f.owner.id).is_err());
    raw.execute_batch(&trigger).unwrap();
    assert!(f.source.snapshot(f.owner.id).is_ok());
    raw.execute("UPDATE transfer_events SET checksum='damaged'", [])
        .unwrap();
    assert!(f.source.transfer_events(f.owner.id).is_err());
    assert!(f
        .source
        .backup(f.owner.id, f.path("corrupt-backup"))
        .is_err());
    assert!(!f.path("corrupt-backup").exists());
}
