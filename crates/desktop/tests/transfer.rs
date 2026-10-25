use farflow_core::{Id, Mode, TransferPhase, TransferRole};
use farflow_desktop::{AttachmentInput, Desktop, Profile, TransferAction, TransferStage};
use serde_json::json;
use std::fs;

#[test]
fn changed_output_files_after_preflight_keep_committed_phases_and_allow_recovery() {
    use std::{
        io::{BufRead, BufReader, Write},
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    for mode in [Mode::Local, Mode::Git] {
        let temp = tempfile::tempdir().unwrap();
        let mut host = Desktop::default();
        let parent = host.select_directory(temp.path()).unwrap().token;
        let opened = host
            .create(
                parent,
                "source",
                mode,
                "输出竞争".into(),
                Profile {
                    account: "owner".into(),
                    name: "所有者".into(),
                    department: "研发".into(),
                },
            )
            .unwrap();
        let actor = opened.identities[0].id;
        host.select_identity(opened.handle, actor).unwrap();
        let id = Id::new_v4();
        let target = if mode == Mode::Local {
            Mode::Git
        } else {
            Mode::Local
        };
        host.transfer(
            opened.handle,
            TransferAction::Prepare {
                id,
                target,
                directory: parent,
                folder: "backup".into(),
            },
            10,
        )
        .unwrap();
        if mode == Mode::Git {
            host.transfer(
                opened.handle,
                TransferAction::Acknowledge {
                    id,
                    note: "本人全部副本已停写".into(),
                    all_copies_stopped: true,
                },
                11,
            )
            .unwrap();
        }
        host.transfer(
            opened.handle,
            TransferAction::Export {
                directory: parent,
                folder: "archive".into(),
            },
            12,
        )
        .unwrap();
        let source = host
            .select_directory(&temp.path().join("archive"))
            .unwrap()
            .token;
        let receipts = temp.path().join("receipts");
        fs::create_dir(&receipts).unwrap();
        let outputs = host.select_directory(&receipts).unwrap().token;
        let stage = move || TransferStage {
            source,
            directory: parent,
            folder: "target".into(),
            actor,
            receipt_directory: outputs,
            receipt_file: "receipt.json".into(),
        };
        host.stage_transfer(stage(), 13).unwrap();
        let original_receipt = fs::read(receipts.join("receipt.json")).unwrap();
        let receipt = host
            .select_pack(&receipts.join("receipt.json"))
            .unwrap()
            .token;
        let sqlite = if mode == Mode::Local {
            temp.path().join("source/data.sqlite")
        } else {
            temp.path().join("target/data.sqlite")
        };
        // 真实 SQLite 锁把阶段提交暂停在文件预检之后，才能核对并发落位失败的恢复语义。
        let mut writer = Command::new("python3")
            .args([
                "-c",
                r#"
import sqlite3,sys
db=sqlite3.connect(sys.argv[1],timeout=10)
if sys.argv[2]=='git':
    db.execute('PRAGMA journal_mode=DELETE')
    db.execute('BEGIN EXCLUSIVE')
else:
    db.execute('BEGIN IMMEDIATE')
print('ready',flush=True)
sys.stdin.readline()
db.rollback()
db.close()
"#,
            ])
            .arg(&sqlite)
            .arg(if mode == Mode::Local { "local" } else { "git" })
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut ready = String::new();
        BufReader::new(writer.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready.trim(), "ready");
        let handle = opened.handle;
        let worker = std::thread::spawn(move || {
            let result = if mode == Mode::Local {
                host.transfer(
                    handle,
                    TransferAction::Complete {
                        receipt,
                        directory: outputs,
                        file: "activation.json".into(),
                    },
                    14,
                )
                .map(|_| ())
            } else {
                host.stage_transfer(stage(), 14).map(|_| ())
            };
            (host, result)
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut prepared = false;
        while Instant::now() < deadline {
            prepared = fs::read_dir(&receipts).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".tmp")
            });
            if prepared {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let changed = receipts.join(if mode == Mode::Local {
            "activation.json"
        } else {
            "receipt.json"
        });
        if prepared {
            fs::write(&changed, b"{\"other\":true}").unwrap();
        }
        writer
            .stdin
            .take()
            .unwrap()
            .write_all(b"continue\n")
            .unwrap();
        assert!(writer.wait().unwrap().success());
        let (mut host, result) = worker.join().unwrap();
        assert!(prepared, "阶段操作未达到输出预检之后的真实等待边界");
        let error = result.err().unwrap().to_string();
        assert!(
            error.contains(if mode == Mode::Local {
                "源已确认完成"
            } else {
                "目标已暂存"
            }),
            "{error}"
        );
        assert_eq!(fs::read(&changed).unwrap(), b"{\"other\":true}");
        if mode == Mode::Local {
            assert_eq!(
                host.transfer_view(handle).unwrap().status.unwrap().phase,
                TransferPhase::Completed
            );
            host.transfer(
                handle,
                TransferAction::Complete {
                    receipt,
                    directory: outputs,
                    file: "activation-retry.json".into(),
                },
                15,
            )
            .unwrap();
            assert!(receipts.join("activation-retry.json").is_file());
        } else {
            fs::write(receipts.join("receipt.json"), original_receipt).unwrap();
            let retried = host
                .stage_transfer(
                    TransferStage {
                        source,
                        directory: parent,
                        folder: "target".into(),
                        actor,
                        receipt_directory: outputs,
                        receipt_file: "receipt.json".into(),
                    },
                    15,
                )
                .unwrap();
            let snapshot = host.select_identity(retried.opened.handle, actor).unwrap();
            assert_eq!(snapshot.transfer.unwrap().phase, TransferPhase::Staged);
        }
    }
}

#[test]
fn native_transfers_preserve_ids_comments_attachments_and_require_source_completion() {
    for mode in [Mode::Local, Mode::Git] {
        let temp = tempfile::tempdir().unwrap();
        let mut host = Desktop::default();
        let parent = host.select_directory(temp.path()).unwrap().token;
        let opened = host
            .create(
                parent,
                "source",
                mode,
                "桌面交接".into(),
                Profile {
                    account: "owner".into(),
                    name: "原所有者".into(),
                    department: "研发".into(),
                },
            )
            .unwrap();
        assert!(host.transfer_view(opened.handle).is_err());
        let actor = opened.identities[0].id;
        host.select_identity(opened.handle, actor).unwrap();
        let item = Id::new_v4();
        host.execute(opened.handle, Id::new_v4(), serde_json::from_value(json!({"type":"create","item":{"id":item,"kind":"document","title":"桌面正式交接"}})).unwrap(), 0, 1).unwrap();
        let attachment = Id::new_v4();
        host.upload(
            opened.handle,
            AttachmentInput {
                operation_id: Id::new_v4(),
                id: attachment,
                item_id: item,
                name: "说明.txt".into(),
                mime: "text/plain".into(),
            },
            b"original bytes",
            2,
        )
        .unwrap();
        host.execute(opened.handle,Id::new_v4(),serde_json::from_value(json!({"type":"add_comment","id":Id::new_v4(),"item_id":item,"body":"原始评论","mentions":[],"attachments":[attachment]})).unwrap(),2,3).unwrap();
        let original = serde_json::to_value(host.snapshot(opened.handle).unwrap().state).unwrap();
        let target = if mode == Mode::Local {
            Mode::Git
        } else {
            Mode::Local
        };
        assert!(host.transfer_view(opened.handle).unwrap().status.is_none());
        let cancelled = Id::new_v4();
        host.transfer(
            opened.handle,
            TransferAction::Prepare {
                id: cancelled,
                target,
                directory: parent,
                folder: "first-backup".into(),
            },
            10,
        )
        .unwrap();
        host.transfer(opened.handle, TransferAction::Cancel { id: cancelled }, 11)
            .unwrap();
        let id = Id::new_v4();
        let frozen = host
            .transfer(
                opened.handle,
                TransferAction::Prepare {
                    id,
                    target,
                    directory: parent,
                    folder: "backup".into(),
                },
                12,
            )
            .unwrap();
        assert_eq!(frozen.status.unwrap().phase, TransferPhase::Frozen);
        assert!(temp.path().join("backup").is_dir());
        if mode == Mode::Git {
            assert!(host
                .transfer(
                    opened.handle,
                    TransferAction::Export {
                        directory: parent,
                        folder: "archive".into()
                    },
                    13
                )
                .is_err());
            host.transfer(
                opened.handle,
                TransferAction::Acknowledge {
                    id,
                    note: "已同步本人全部副本并停止写入".into(),
                    all_copies_stopped: true,
                },
                13,
            )
            .unwrap();
        }
        let exported = host
            .transfer(
                opened.handle,
                TransferAction::Export {
                    directory: parent,
                    folder: "archive".into(),
                },
                14,
            )
            .unwrap();
        assert_eq!(exported.report.unwrap().comment_count, 1);
        let source = host
            .select_directory(&temp.path().join("archive"))
            .unwrap()
            .token;
        let report = host.inspect_transfer(source).unwrap();
        assert_eq!(report.source.id, opened.workspace.id);
        assert_eq!(report.attachment_count, 1);
        let stage = move || TransferStage {
            source,
            directory: parent,
            folder: "target".into(),
            actor,
            receipt_directory: parent,
            receipt_file: "receipt.json".into(),
        };
        let staged = host.stage_transfer(stage(), 15).unwrap();
        assert!(host.snapshot(staged.opened.handle).is_err());
        let snapshot = host.select_identity(staged.opened.handle, actor).unwrap();
        assert_eq!(snapshot.transfer.unwrap().phase, TransferPhase::Staged);
        assert_eq!(serde_json::to_value(snapshot.state).unwrap(), original);
        assert_eq!(
            host.attachment(staged.opened.handle, attachment).unwrap(),
            b"original bytes"
        );
        let bad_proof = temp.path().join("bad.json");
        fs::write(&bad_proof, b"{}").unwrap();
        let bad = host.select_pack(&bad_proof).unwrap().token;
        assert!(host
            .transfer(
                staged.opened.handle,
                TransferAction::Activate { activation: bad },
                16
            )
            .is_err());
        let receipt = host.select_pack(&staged.receipt_path).unwrap().token;
        let complete = || TransferAction::Complete {
            receipt,
            directory: parent,
            file: "activation.json".into(),
        };
        let result = host.transfer(opened.handle, complete(), 17).unwrap();
        assert_eq!(result.status.unwrap().phase, TransferPhase::Completed);
        assert!(!serde_json::to_string(&result.path)
            .unwrap()
            .contains("\"token\""));
        let proof = host.select_pack(&result.path.unwrap()).unwrap().token;
        host.transfer(opened.handle, complete(), 18).unwrap();
        let active = host
            .transfer(
                staged.opened.handle,
                TransferAction::Activate { activation: proof },
                19,
            )
            .unwrap();
        assert_eq!(active.status.unwrap().phase, TransferPhase::Active);
        assert!(host
            .transfer(opened.handle, TransferAction::Cancel { id }, 20)
            .is_err());
        host.execute(staged.opened.handle,Id::new_v4(),serde_json::from_value(json!({"type":"add_comment","id":Id::new_v4(),"item_id":item,"body":"目标继续保存","mentions":[],"attachments":[]})).unwrap(),3,21).unwrap();
        let again = host.stage_transfer(stage(), 22).unwrap();
        let data = host.select_identity(again.opened.handle, actor).unwrap();
        assert_eq!(data.state.comments.len(), 2);
        assert_eq!(data.transfer.unwrap().role, TransferRole::Target);
        assert_eq!(host.snapshot(opened.handle).unwrap().state.revision, 3);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(temp.path().join("activation.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
}

#[test]
fn native_transfer_permissions_paths_invalid_documents_and_directory_replacement_are_rechecked() {
    let temp = tempfile::tempdir().unwrap();
    let mut host = Desktop::default();
    let parent = host.select_directory(temp.path()).unwrap().token;
    let opened = host
        .create(
            parent,
            "local",
            Mode::Local,
            "拒绝边界".into(),
            Profile {
                account: "owner".into(),
                name: "所有者".into(),
                department: "".into(),
            },
        )
        .unwrap();
    let owner = opened.identities[0].id;
    host.select_identity(opened.handle, owner).unwrap();
    for (target, directory, folder) in [
        (Mode::Local, parent, "same"),
        (Mode::Git, Id::new_v4(), "not-granted"),
        (Mode::Git, parent, "../outside"),
    ] {
        assert!(host
            .transfer(
                opened.handle,
                TransferAction::Prepare {
                    id: Id::new_v4(),
                    target,
                    directory,
                    folder: folder.into()
                },
                1
            )
            .is_err());
    }
    assert!(host
        .transfer(
            opened.handle,
            TransferAction::Acknowledge {
                id: Id::new_v4(),
                note: "确认".into(),
                all_copies_stopped: false
            },
            1
        )
        .is_err());
    assert!(host
        .transfer(
            opened.handle,
            TransferAction::Acknowledge {
                id: Id::new_v4(),
                note: "确认".into(),
                all_copies_stopped: true
            },
            1
        )
        .is_err());
    assert!(host.inspect_transfer(parent).is_err());
    let ordinary = temp.path().join("ordinary");
    farflow_core::LocalWorkspace::open(temp.path().join("local"))
        .unwrap()
        .export_copy(owner, &ordinary)
        .unwrap();
    let ordinary = host.select_directory(&ordinary).unwrap().token;
    assert!(host.inspect_transfer(ordinary).is_err());
    let input = || TransferStage {
        source: ordinary,
        directory: parent,
        folder: "bad-target".into(),
        actor: owner,
        receipt_directory: parent,
        receipt_file: "receipt.json".into(),
    };
    assert!(host.stage_transfer(input(), 2).is_err());
    let receipt = temp.path().join("bad-receipt.json");
    fs::write(&receipt, "{}").unwrap();
    let file = host.select_pack(&receipt).unwrap().token;
    assert!(host
        .transfer(
            opened.handle,
            TransferAction::Complete {
                receipt: file,
                directory: parent,
                file: "proof.json".into()
            },
            2
        )
        .is_err());
    let id = Id::new_v4();
    host.transfer(
        opened.handle,
        TransferAction::Prepare {
            id,
            target: Mode::Service,
            directory: parent,
            folder: "service-backup".into(),
        },
        3,
    )
    .unwrap();
    host.transfer(
        opened.handle,
        TransferAction::Export {
            directory: parent,
            folder: "service-archive".into(),
        },
        4,
    )
    .unwrap();
    let source = host
        .select_directory(&temp.path().join("service-archive"))
        .unwrap()
        .token;
    assert_eq!(
        host.inspect_transfer(source)
            .unwrap()
            .transfer
            .unwrap()
            .target_mode,
        Mode::Service
    );
    let mut service = input();
    service.source = source;
    assert!(host
        .stage_transfer(service, 5)
        .err()
        .unwrap()
        .to_string()
        .contains("团队服务"));
    host.transfer(opened.handle, TransferAction::Cancel { id }, 6)
        .unwrap();
    let viewer = Id::new_v4();
    host.execute(opened.handle,Id::new_v4(),serde_json::from_value(json!({"type":"add_member","member":{"id":viewer,"account":"viewer","name":"查看者","department":"","roles":["viewer"],"active":true}})).unwrap(),0,7).unwrap();
    host.select_identity(opened.handle, viewer).unwrap();
    assert!(host.transfer_view(opened.handle).unwrap().events.is_empty());
    assert!(host
        .transfer(
            opened.handle,
            TransferAction::Prepare {
                id: Id::new_v4(),
                target: Mode::Git,
                directory: parent,
                folder: "viewer-backup".into()
            },
            8
        )
        .is_err());
    assert!(!temp.path().join("viewer-backup").exists());
    #[cfg(unix)]
    {
        let granted = temp.path().join("granted");
        fs::create_dir(&granted).unwrap();
        let directory = host.select_directory(&granted).unwrap().token;
        fs::rename(&granted, temp.path().join("kept")).unwrap();
        std::os::unix::fs::symlink(temp.path().join("kept"), &granted).unwrap();
        assert!(host
            .transfer(
                opened.handle,
                TransferAction::Export {
                    directory,
                    folder: "archive".into()
                },
                9
            )
            .is_err());
    }
    host.close(opened.handle);
    assert!(host.transfer_view(opened.handle).is_err());
    assert!(host
        .transfer(opened.handle, TransferAction::Cancel { id }, 10)
        .is_err());
    assert!(serde_json::from_value::<TransferAction>(json!({"type":"prepare","id":Id::new_v4(),"target":"git","directory":parent,"folder":"x","path":"/任意路径"})).is_err());
}
