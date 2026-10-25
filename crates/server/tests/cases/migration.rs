use super::{F, NEXT_PASSWORD, PASSWORD};
use axum::{
    body::{to_bytes, Body},
    http::{Request as HttpRequest, StatusCode},
};
use farflow_core::{
    Command, GitWorkspace, Id, LocalWorkspace, Member, MigrationArchive, Mode, NewItem, Request,
    Role, State, TransferActivation, TransferPhase, TransferReceipt, Upload,
};
use farflow_server::Server;
use serde_json::{json, Value};
use sqlx_core::{query::query, query_scalar::query_scalar, row::Row};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
};

async fn migration_cli(
    f: &F,
    token: &str,
    arguments: &[std::ffi::OsString],
) -> std::process::Output {
    let mut process = std::process::Command::new(env!("CARGO_BIN_EXE_farflow-migrate"));
    process
        .args(arguments)
        .env_remove("DATABASE_URL")
        .env_remove("FARFLOW_DATA_DIR")
        .env_remove("FARFLOW_SESSION_TOKEN");
    if arguments.first().is_some_and(|value| value == "service") {
        process
            .env("DATABASE_URL", &f.url)
            .env("FARFLOW_DATA_DIR", &f.files)
            .env("FARFLOW_SESSION_TOKEN", token);
    }
    let output = tokio::task::spawn_blocking(move || process.output())
        .await
        .unwrap()
        .unwrap();
    for bytes in [&output.stdout, &output.stderr] {
        let text = String::from_utf8_lossy(bytes);
        assert!(!text.contains(token));
        assert!(!text.contains(&f.url));
        assert!(!text.contains("\"token\""));
    }
    output
}

async fn migration_cli_ok(f: &F, token: &str, arguments: &[std::ffi::OsString]) -> Value {
    let output = migration_cli(f, token, arguments).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn migration_control(
    base: &[std::ffi::OsString],
    operation: &str,
    tail: Vec<std::ffi::OsString>,
) -> Vec<std::ffi::OsString> {
    let mut arguments = base.to_vec();
    arguments[1] = operation.into();
    arguments.extend(tail);
    arguments
}

#[test]
fn executable_migration_configuration_errors_do_not_echo_credentials_or_choose_an_implicit_directory(
) {
    let temp = tempfile::tempdir().unwrap();
    let location = temp.path().join("不应创建的服务目录");
    let id = Id::new_v4().to_string();
    for supplied in 0..=3 {
        let mut process = std::process::Command::new(env!("CARGO_BIN_EXE_farflow-migrate"));
        process
            .args(["service", "status", &id])
            .env_remove("DATABASE_URL")
            .env_remove("FARFLOW_DATA_DIR")
            .env_remove("FARFLOW_SESSION_TOKEN");
        if supplied >= 1 {
            process.env("DATABASE_URL", "不应回显的无效数据库连接");
        }
        if supplied >= 2 {
            process.env("FARFLOW_SESSION_TOKEN", "不应回显的会话凭据");
        }
        if supplied >= 3 {
            process.env("FARFLOW_DATA_DIR", &location);
        }
        let output = process.output().unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(!error.contains("不应回显的无效数据库连接"));
        assert!(!error.contains("不应回显的会话凭据"));
        assert!(output.stdout.is_empty());
        assert!(!location.exists());
        assert!(error.contains(
            [
                "缺少DATABASE_URL",
                "缺少FARFLOW_SESSION_TOKEN",
                "缺少FARFLOW_DATA_DIR",
                "服务连接或目录核对失败"
            ][supplied]
        ));
    }
}

#[tokio::test]
async fn executable_migrations_preserve_ids_comments_bytes_and_actual_members_in_all_six_directions(
) {
    let f = F::new().await;
    let admin = f.admin().await;
    let admin_id = me(&f, &admin).await;
    let (viewer_account, _) = ready(&f, &admin, "cli_migration_viewer").await;
    let (developer_account, _) = ready(&f, &admin, "cli_migration_developer").await;
    let help = migration_cli(&f, &admin, &["--help".into()]).await;
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("先备份再冻结"));
    let bad = migration_cli(&f, &admin, &["local".into()]).await;
    assert!(!bad.status.success());
    let generated = migration_cli_ok(&f, &admin, &["new-id".into()]).await;
    assert!(!Id::parse_str(generated["id"].as_str().unwrap())
        .unwrap()
        .is_nil());
    for (from, to) in [
        ("local", "git"),
        ("local", "service"),
        ("git", "local"),
        ("git", "service"),
        ("service", "local"),
        ("service", "git"),
    ] {
        let src = source(None);
        let root = src._dir.path();
        let local_path = root.join("local");
        let developer = member(Id::new_v4(), "developer", Role::Developer);
        let comment = Id::new_v4();
        let mut local = LocalWorkspace::open(&local_path).unwrap();
        local
            .execute(Request {
                id: Id::new_v4(),
                actor_id: src.owner,
                at: 10,
                command: Command::AddMember {
                    member: developer.clone(),
                },
            })
            .unwrap();
        local
            .execute(Request {
                id: Id::new_v4(),
                actor_id: developer.id,
                at: 11,
                command: Command::AddComment {
                    id: comment,
                    item_id: src.item,
                    body: "开发成员的迁移评论，保留附件引用。".into(),
                    parent_id: None,
                    mentions: BTreeSet::from([src.owner]),
                    attachments: BTreeSet::from([src.attachment]),
                },
            })
            .unwrap();
        let ordinary = root.join("cli-ordinary");
        local.export_copy(src.owner, &ordinary).unwrap();
        let expected = local.snapshot(src.owner).unwrap();
        drop(local);
        let mappings = BTreeMap::from([
            (src.owner, admin_id),
            (src.viewer, viewer_account),
            (developer.id, developer_account),
        ]);
        let map_file = root.join("身份映射.json");
        fs::write(&map_file, serde_json::to_vec(&mappings).unwrap()).unwrap();
        let (workspace_id, source_path) = match from {
            "local" => (expected.id, Some(local_path.clone())),
            "git" => {
                let path = root.join("git-source");
                let (_, report) =
                    GitWorkspace::import_copy(&ordinary, &path, Id::new_v4()).unwrap();
                (report.target.unwrap().id, Some(path))
            }
            _ => {
                let id = Id::new_v4();
                f.server
                    .import_copy(&admin, &ordinary, id, mappings.clone())
                    .await
                    .unwrap();
                (id, None)
            }
        };
        let mut source_base: Vec<std::ffi::OsString> = vec![
            from.into(),
            "status".into(),
            if from == "service" {
                workspace_id.to_string().into()
            } else {
                source_path.as_ref().unwrap().as_os_str().into()
            },
        ];
        if from != "service" {
            source_base.push(src.owner.to_string().into());
            let identities = migration_cli_ok(
                &f,
                &admin,
                &[from.into(), "identities".into(), source_base[2].clone()],
            )
            .await;
            assert_eq!(identities["workspace"]["id"], workspace_id.to_string());
            assert_eq!(identities["members"].as_array().unwrap().len(), 3);
            assert!(identities["members"]
                .as_array()
                .unwrap()
                .iter()
                .any(|member| member["id"] == src.owner.to_string()));
        }
        assert!(migration_cli_ok(&f, &admin, &source_base).await["status"].is_null());
        let cancelled = Id::new_v4();
        migration_cli_ok(
            &f,
            &admin,
            &migration_control(
                &source_base,
                "prepare",
                vec![
                    cancelled.to_string().into(),
                    to.into(),
                    root.join("cancel-backup").into_os_string(),
                ],
            ),
        )
        .await;
        let cancelled_result = migration_cli_ok(
            &f,
            &admin,
            &migration_control(&source_base, "cancel", vec![cancelled.to_string().into()]),
        )
        .await;
        assert_eq!(cancelled_result["status"]["phase"], "cancelled");
        let transfer = Id::new_v4();
        let backup = root.join("源恢复备份");
        let frozen = migration_cli_ok(
            &f,
            &admin,
            &migration_control(
                &source_base,
                "prepare",
                vec![
                    transfer.to_string().into(),
                    to.into(),
                    backup.clone().into_os_string(),
                ],
            ),
        )
        .await;
        assert_eq!(frozen["status"]["phase"], "frozen");
        migration_cli_ok(
            &f,
            &admin,
            &migration_control(
                &source_base,
                "prepare",
                vec![
                    transfer.to_string().into(),
                    to.into(),
                    backup.clone().into_os_string(),
                ],
            ),
        )
        .await;
        if from == "git" && to == "local" {
            let restored = root.join("restored-origin");
            let result = migration_cli_ok(
                &f,
                &admin,
                &[
                    "git".into(),
                    "restore-origin".into(),
                    backup.into_os_string(),
                    restored.clone().into_os_string(),
                    src.owner.to_string().into(),
                    "--source-offline".into(),
                ],
            )
            .await;
            assert_eq!(result["status"]["phase"], "frozen");
            source_base[2] = restored.into_os_string();
        }
        let archive = root.join("正式交换包");
        if from == "git" {
            let before = migration_cli(
                &f,
                &admin,
                &migration_control(
                    &source_base,
                    "export",
                    vec![archive.clone().into_os_string()],
                ),
            )
            .await;
            assert!(!before.status.success());
            assert!(!archive.exists());
            let note = root.join("本人所有副本已停写.txt");
            let invalid_note = root.join("非UTF8声明.txt");
            fs::write(&invalid_note, [255]).unwrap();
            let invalid = migration_cli(
                &f,
                &admin,
                &migration_control(
                    &source_base,
                    "acknowledge",
                    vec![
                        transfer.to_string().into(),
                        invalid_note.into_os_string(),
                        "--all-copies-stopped".into(),
                    ],
                ),
            )
            .await;
            assert!(!invalid.status.success());
            assert!(String::from_utf8_lossy(&invalid.stderr).contains("UTF-8"));
            fs::write(&note, "本人全部副本已同步并停止写入，核对当前计划后确认。").unwrap();
            for actor in [src.owner, developer.id] {
                let mut base = source_base.clone();
                base[3] = actor.to_string().into();
                migration_cli_ok(
                    &f,
                    &admin,
                    &migration_control(
                        &base,
                        "acknowledge",
                        vec![
                            transfer.to_string().into(),
                            note.clone().into_os_string(),
                            "--all-copies-stopped".into(),
                        ],
                    ),
                )
                .await;
            }
            let events = migration_cli_ok(
                &f,
                &admin,
                &migration_control(&source_base, "events", vec![]),
            )
            .await;
            let active = events["plans"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["status"]["plan"]["id"] == transfer.to_string())
                .unwrap();
            assert_eq!(active["required_members"].as_array().unwrap().len(), 2);
            assert_eq!(active["confirmations"].as_object().unwrap().len(), 2);
        } else {
            assert!(!migration_cli_ok(
                &f,
                &admin,
                &migration_control(&source_base, "events", vec![])
            )
            .await["events"]
                .as_array()
                .unwrap()
                .is_empty());
        }
        let exported = migration_cli_ok(
            &f,
            &admin,
            &migration_control(
                &source_base,
                "export",
                vec![archive.clone().into_os_string()],
            ),
        )
        .await;
        assert_eq!(exported["report"]["source"]["id"], workspace_id.to_string());
        let inspected = migration_cli_ok(
            &f,
            &admin,
            &["inspect".into(), archive.clone().into_os_string()],
        )
        .await;
        assert_eq!(inspected["report"]["comment_count"], 1);
        assert_eq!(inspected["report"]["attachment_count"], 1);
        assert_eq!(
            inspected["report"]["identity_ids"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        let target = root.join("目标空间");
        let receipt_file = root.join("目标回执.json");
        let stage = if to == "service" {
            vec![
                to.into(),
                "stage".into(),
                archive.clone().into_os_string(),
                map_file.into_os_string(),
                receipt_file.clone().into_os_string(),
            ]
        } else {
            vec![
                to.into(),
                "stage".into(),
                archive.clone().into_os_string(),
                target.clone().into_os_string(),
                src.owner.to_string().into(),
                receipt_file.clone().into_os_string(),
            ]
        };
        let wrong_receipt = root.join("其他计划回执.json");
        fs::write(
            &wrong_receipt,
            serde_json::to_vec(&TransferReceipt {
                plan_checksum: "0".repeat(64),
            })
            .unwrap(),
        )
        .unwrap();
        let original_wrong_receipt = fs::read(&wrong_receipt).unwrap();
        let mut wrong_stage = stage.clone();
        *wrong_stage.last_mut().unwrap() = wrong_receipt.clone().into_os_string();
        assert!(!migration_cli(&f, &admin, &wrong_stage)
            .await
            .status
            .success());
        assert_eq!(fs::read(&wrong_receipt).unwrap(), original_wrong_receipt);
        if to != "service" {
            assert!(!target.exists());
        }
        let staged = migration_cli_ok(&f, &admin, &stage).await;
        assert_eq!(staged["report"]["target"]["id"], workspace_id.to_string());
        migration_cli_ok(&f, &admin, &stage).await;
        let mut target_base = vec![
            to.into(),
            "status".into(),
            if to == "service" {
                workspace_id.to_string().into()
            } else {
                target.clone().into_os_string()
            },
        ];
        if to != "service" {
            target_base.push(src.owner.to_string().into());
        }
        assert_eq!(
            migration_cli_ok(&f, &admin, &target_base).await["status"]["phase"],
            "staged"
        );
        let proof_file = root.join("源完成凭据.json");
        let complete = migration_control(
            &source_base,
            "complete",
            vec![
                receipt_file.clone().into_os_string(),
                proof_file.clone().into_os_string(),
            ],
        );
        let wrong_proof = root.join("不相符的完成凭据.json");
        let receipt: TransferReceipt =
            serde_json::from_slice(&fs::read(&receipt_file).unwrap()).unwrap();
        fs::write(
            &wrong_proof,
            serde_json::to_vec(&TransferActivation {
                transfer_id: transfer,
                plan_checksum: receipt.plan_checksum,
                token: "不应进入日志的其他凭据".into(),
            })
            .unwrap(),
        )
        .unwrap();
        let original_wrong_proof = fs::read(&wrong_proof).unwrap();
        let mut wrong_complete = complete.clone();
        *wrong_complete.last_mut().unwrap() = wrong_proof.clone().into_os_string();
        let rejected = migration_cli(&f, &admin, &wrong_complete).await;
        assert!(!rejected.status.success());
        assert!(!String::from_utf8_lossy(&rejected.stderr).contains("不应进入日志的其他凭据"));
        assert_eq!(fs::read(&wrong_proof).unwrap(), original_wrong_proof);
        assert_eq!(
            migration_cli_ok(&f, &admin, &source_base).await["status"]["phase"],
            "frozen"
        );
        let completed = migration_cli_ok(&f, &admin, &complete).await;
        assert_eq!(completed["status"]["phase"], "completed");
        let proof: TransferActivation =
            serde_json::from_slice(&fs::read(&proof_file).unwrap()).unwrap();
        assert!(!completed.to_string().contains(&proof.token));
        let original_proof = fs::read(&proof_file).unwrap();
        migration_cli_ok(&f, &admin, &complete).await;
        assert_eq!(fs::read(&proof_file).unwrap(), original_proof);
        assert!(!migration_cli(
            &f,
            &admin,
            &migration_control(&source_base, "cancel", vec![transfer.to_string().into()])
        )
        .await
        .status
        .success());
        let activated = migration_cli_ok(
            &f,
            &admin,
            &migration_control(
                &target_base,
                "activate",
                vec![proof_file.clone().into_os_string()],
            ),
        )
        .await;
        assert_eq!(activated["status"]["phase"], "active");
        migration_cli_ok(
            &f,
            &admin,
            &migration_control(
                &target_base,
                "activate",
                vec![proof_file.clone().into_os_string()],
            ),
        )
        .await;
        let bytes = b"mapped real evidence";
        let after = match to {
            "local" => {
                let workspace = LocalWorkspace::open(&target).unwrap();
                assert_eq!(
                    workspace
                        .read_attachment(src.owner, src.attachment)
                        .unwrap(),
                    bytes
                );
                serde_json::to_value(workspace.snapshot(src.owner).unwrap()).unwrap()
            }
            "git" => {
                let workspace = GitWorkspace::open(&target).unwrap();
                assert_eq!(
                    workspace
                        .read_attachment(src.owner, src.attachment)
                        .unwrap(),
                    bytes
                );
                serde_json::to_value(workspace.snapshot(src.owner).unwrap().state).unwrap()
            }
            _ => {
                assert_eq!(
                    attachment(&f, &admin, workspace_id, src.attachment).await,
                    bytes
                );
                snapshot(&f, &admin, workspace_id).await["state"].clone()
            }
        };
        assert_eq!(
            after["comments"][comment.to_string()],
            serde_json::to_value(&expected.comments[&comment]).unwrap()
        );
        assert_eq!(
            after["items"][src.item.to_string()],
            serde_json::to_value(&expected.items[&src.item]).unwrap()
        );
        assert_eq!(
            after["attachments"][src.attachment.to_string()],
            serde_json::to_value(&expected.attachments[&src.attachment]).unwrap()
        );
        assert_eq!(after["members"].as_object().unwrap().len(), 3);
        let next = Id::new_v4();
        let body = json!({"type":"add_comment","id":next,"item_id":src.item,"body":"迁移激活后继续协作","parent_id":null,"mentions":[],"attachments":[]});
        if to == "service" {
            assert_eq!(
                command(
                    &f,
                    &admin,
                    workspace_id,
                    json!({"id":Id::new_v4(),"expected_revision":after["revision"],"command":body})
                )
                .await
                .0,
                StatusCode::OK
            );
            assert!(
                snapshot(&f, &admin, workspace_id).await["state"]["comments"][next.to_string()]
                    .is_object()
            );
        } else {
            let request = Request {
                id: Id::new_v4(),
                actor_id: src.owner,
                at: activated["status"]["changed_at"].as_i64().unwrap() + 1,
                command: serde_json::from_value(body).unwrap(),
            };
            if to == "local" {
                LocalWorkspace::open(&target)
                    .unwrap()
                    .execute(request)
                    .unwrap();
            } else {
                GitWorkspace::open(&target)
                    .unwrap()
                    .execute(request)
                    .unwrap();
            }
        }
        migration_cli_ok(&f, &admin, &stage).await;
        let continued = match to {
            "local" => serde_json::to_value(
                LocalWorkspace::open(&target)
                    .unwrap()
                    .snapshot(src.owner)
                    .unwrap(),
            )
            .unwrap(),
            "git" => serde_json::to_value(
                GitWorkspace::open(&target)
                    .unwrap()
                    .snapshot(src.owner)
                    .unwrap()
                    .state,
            )
            .unwrap(),
            _ => snapshot(&f, &admin, workspace_id).await["state"].clone(),
        };
        assert_eq!(
            continued["comments"][next.to_string()]["body"],
            "迁移激活后继续协作"
        );
        let source_write = json!({"type":"add_comment","id":Id::new_v4(),"item_id":src.item,"body":"已完成的源不能继续写入","parent_id":null,"mentions":[],"attachments":[]});
        if from == "service" {
            assert_eq!(
                command(
                    &f,
                    &admin,
                    workspace_id,
                    json!({"id":Id::new_v4(),"command":source_write})
                )
                .await
                .0,
                StatusCode::CONFLICT
            );
        } else {
            let request = Request {
                id: Id::new_v4(),
                actor_id: src.owner,
                at: completed["status"]["changed_at"].as_i64().unwrap() + 1,
                command: serde_json::from_value(source_write).unwrap(),
            };
            let path = PathBuf::from(&source_base[2]);
            let result = if from == "local" {
                LocalWorkspace::open(&path).unwrap().execute(request)
            } else {
                GitWorkspace::open(&path)
                    .unwrap()
                    .execute(request)
                    .map(|saved| saved.receipt)
            };
            assert!(matches!(result, Err(farflow_core::Error::Conflict(_))));
        }
        assert_eq!(
            migration_cli_ok(&f, &admin, &source_base).await["status"]["phase"],
            "completed"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&proof_file).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(&receipt_file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}

struct Source {
    _dir: tempfile::TempDir,
    path: PathBuf,
    owner: Id,
    viewer: Id,
    item: Id,
    attachment: Id,
    request: Request,
    state: State,
}
fn member(id: Id, account: &str, role: Role) -> Member {
    Member {
        id,
        account: account.into(),
        name: format!("历史{account}"),
        department: "原团队".into(),
        roles: BTreeSet::from([role]),
        active: true,
    }
}
fn source(owner_id: Option<Id>) -> Source {
    let dir = tempfile::tempdir().unwrap();
    let owner = member(owner_id.unwrap_or_else(Id::new_v4), "owner", Role::Owner);
    let viewer = member(Id::new_v4(), "viewer", Role::Viewer);
    let mut local = LocalWorkspace::create(
        dir.path().join("local"),
        Id::new_v4(),
        "服务身份迁移".into(),
        owner.clone(),
    )
    .unwrap();
    local
        .execute(Request {
            id: Id::new_v4(),
            actor_id: owner.id,
            at: 1,
            command: Command::AddMember {
                member: viewer.clone(),
            },
        })
        .unwrap();
    let item:NewItem=serde_json::from_value(json!({"id":Id::new_v4(),"kind":"document","title":"原始业务","description":"原文\n制表\t与负零","attributes":{"score":-0.0}})).unwrap();
    let item_id = item.id;
    let request = Request {
        id: Id::new_v4(),
        actor_id: owner.id,
        at: 2,
        command: Command::Create { item },
    };
    local.execute(request.clone()).unwrap();
    let attachment = Id::new_v4();
    local
        .upload(
            Upload {
                operation_id: Id::new_v4(),
                actor_id: owner.id,
                at: 3,
                id: attachment,
                item_id,
                name: "evidence.txt".into(),
                mime: "text/plain".into(),
            },
            b"mapped real evidence",
        )
        .unwrap();
    let path = dir.path().join("package");
    local.export_copy(owner.id, &path).unwrap();
    Source {
        _dir: dir,
        path,
        owner: owner.id,
        viewer: viewer.id,
        item: item_id,
        attachment,
        request,
        state: local.snapshot(owner.id).unwrap(),
    }
}
async fn ready(f: &F, admin: &str, account: &str) -> (Id, String) {
    let user = f.create(admin, account, false).await;
    let first = f.login(account, PASSWORD).await;
    f.password(&first).await;
    (
        Id::parse_str(user["id"].as_str().unwrap()).unwrap(),
        f.login(account, NEXT_PASSWORD).await,
    )
}
async fn me(f: &F, token: &str) -> Id {
    Id::parse_str(
        f.api("GET", "/api/auth/me", Some(token), Value::Null)
            .await
            .1["id"]
            .as_str()
            .unwrap(),
    )
    .unwrap()
}
async fn snapshot(f: &F, token: &str, id: Id) -> Value {
    let (status, data) = f
        .api(
            "GET",
            &format!("/api/workspaces/{id}"),
            Some(token),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{data}");
    data
}
async fn command(f: &F, token: &str, id: Id, body: Value) -> (StatusCode, Value) {
    f.api(
        "POST",
        &format!("/api/workspaces/{id}/commands"),
        Some(token),
        body,
    )
    .await
}
async fn attachment(f: &F, token: &str, id: Id, file: Id) -> Vec<u8> {
    let response = f
        .send(
            HttpRequest::builder()
                .uri(format!("/api/workspaces/{id}/attachments/{file}"))
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    to_bytes(response.into_body(), 1024).await.unwrap().to_vec()
}

#[tokio::test]
async fn mapped_accounts_keep_historical_ids_and_continue_through_backup_and_export() {
    let f = F::new().await;
    let admin = f.admin().await;
    let admin_id = me(&f, &admin).await;
    let (other, other_token) = ready(&f, &admin, "mapped_viewer").await;
    let src = source(None);
    let id = Id::new_v4();
    let mappings = BTreeMap::from([(src.owner, admin_id), (src.viewer, other)]);
    let report = f
        .server
        .import_copy(&admin, &src.path, id, mappings.clone())
        .await
        .unwrap();
    assert_eq!(report.target.unwrap().mode, farflow_core::Mode::Service);
    let before = snapshot(&f, &admin, id).await;
    assert_eq!(before["actor_id"], json!(src.owner));
    let owner_profile = before["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == json!(src.owner))
        .unwrap();
    assert_eq!(owner_profile["account_id"], json!(admin_id));
    let mut copied: State = serde_json::from_value(before["state"].clone()).unwrap();
    copied.id = src.state.id;
    assert_eq!(
        serde_json::to_string(&copied).unwrap(),
        serde_json::to_string(&src.state).unwrap()
    );
    assert_eq!(
        attachment(&f, &other_token, id, src.attachment).await,
        b"mapped real evidence"
    );
    let old = json!({"id":src.request.id,"command":src.request.command});
    assert_eq!(command(&f, &admin, id, old.clone()).await.0, StatusCode::OK);
    assert_eq!(
        command(&f, &other_token, id, old).await.0,
        StatusCode::CONFLICT
    );
    let operation = Id::new_v4();
    let comment = json!({"id":operation,"command":{"type":"add_comment","id":Id::new_v4(),"item_id":src.item,"body":"真实服务账户继续协作","parent_id":null,"mentions":[src.viewer],"attachments":[]}});
    let saved = command(&f, &admin, id, comment.clone()).await;
    assert_eq!(saved.0, StatusCode::OK);
    assert_eq!(command(&f, &admin, id, comment).await.1, saved.1);
    let revision = snapshot(&f, &admin, id).await["state"]["revision"]
        .as_u64()
        .unwrap();
    let update = json!({"id":Id::new_v4(),"expected_revision":revision,"command":{"type":"update_member","member":{"id":src.viewer,"account":"untrusted","name":"不能使用","department":"不能使用","roles":["developer"],"active":true}}});
    assert_eq!(command(&f, &admin, id, update).await.0, StatusCode::OK);
    let updated = snapshot(&f, &admin, id).await;
    assert_eq!(
        updated["state"]["members"][src.viewer.to_string()]["id"],
        json!(src.viewer)
    );
    assert_eq!(
        updated["state"]["members"][src.viewer.to_string()]["account"],
        "viewer"
    );
    assert_eq!(
        snapshot(&f, &admin, id).await["state"]["history"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()["actor_id"],
        json!(src.owner)
    );
    f.server
        .import_copy(&admin, &src.path, id, mappings)
        .await
        .unwrap();
    let (new_id, _) = ready(&f, &admin, "viewer").await;
    let revision = snapshot(&f, &admin, id).await["state"]["revision"]
        .as_u64()
        .unwrap();
    let add = json!({"id":Id::new_v4(),"expected_revision":revision,"command":{"type":"add_member","member":{"id":new_id,"account":"ignored","name":"忽略","department":"","roles":["developer"],"active":true}}});
    assert_eq!(command(&f, &admin, id, add).await.0, StatusCode::OK);
    let after = snapshot(&f, &admin, id).await;
    assert_ne!(
        after["state"]["members"][new_id.to_string()]["account"],
        "viewer"
    );
    assert_eq!(
        after["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["id"] == json!(new_id))
            .unwrap()["account"],
        "viewer"
    );
    assert!(after["state"]["revision"].as_u64().unwrap() > src.state.revision);
    let exported = f.files.with_extension("mapped-export");
    f.server.export_copy(&admin, id, &exported).await.unwrap();
    let archive = MigrationArchive::open(&exported).unwrap();
    let history = archive.copied_history(Id::new_v4()).unwrap();
    assert_eq!(history.clients.len(), 3);
    assert_eq!(
        history.member_ids(),
        BTreeSet::from([src.owner, src.viewer, new_id])
    );
    let backup = f.files.with_extension("mapped-backup");
    f.server.backup(&admin, &backup).await.unwrap();
    let target = F::new().await;
    Server::restore(&backup, &target.url, &target.files)
        .await
        .unwrap();
    let restored = target.login("owner", PASSWORD).await;
    assert_eq!(snapshot(&target, &restored, id).await, after);
    assert_eq!(
        attachment(&target, &restored, id, src.attachment).await,
        b"mapped real evidence"
    );
    let again = target.files.with_extension("again-export");
    target
        .server
        .export_copy(&restored, id, &again)
        .await
        .unwrap();
    let local_path = target.files.with_extension("local-copy");
    let (mut local, _) = LocalWorkspace::import_copy(&again, &local_path, Id::new_v4()).unwrap();
    local
        .execute(Request {
            id: Id::new_v4(),
            actor_id: src.owner,
            at: 500,
            command: Command::AddComment {
                id: Id::new_v4(),
                item_id: src.item,
                body: "导出到本地继续".into(),
                parent_id: None,
                mentions: BTreeSet::new(),
                attachments: BTreeSet::new(),
            },
        })
        .unwrap();
    assert!(Server::connect(&f.url, &f.files).await.is_ok());
}

#[tokio::test]
async fn mapping_validation_and_uuid_collisions_never_grant_an_owners_role() {
    let f = F::new().await;
    let admin = f.admin().await;
    let admin_id = me(&f, &admin).await;
    let (actual_owner, owner_token) = ready(&f, &admin, "actual_owner").await;
    let (unbound, unbound_token) = ready(&f, &admin, "unbound_user").await;
    let src = source(Some(admin_id));
    let id = Id::new_v4();
    for mappings in [
        BTreeMap::new(),
        BTreeMap::from([(src.owner, admin_id), (src.viewer, admin_id)]),
        BTreeMap::from([(src.owner, admin_id), (src.viewer, Id::new_v4())]),
        BTreeMap::from([
            (src.owner, admin_id),
            (src.viewer, actual_owner),
            (Id::new_v4(), unbound),
        ]),
    ] {
        assert!(f
            .server
            .import_copy(&admin, &src.path, id, mappings)
            .await
            .is_err());
        assert_eq!(
            query_scalar::<_, i64>("SELECT COUNT(*) FROM ff_workspaces WHERE id=$1")
                .bind(id)
                .fetch_one(&f.pool)
                .await
                .unwrap(),
            0
        );
    }
    let mappings = BTreeMap::from([(src.owner, actual_owner), (src.viewer, admin_id)]);
    assert!(f
        .server
        .import_copy(&unbound_token, &src.path, id, mappings.clone())
        .await
        .is_err());
    assert!(f
        .server
        .import_copy("bad", &src.path, id, mappings.clone())
        .await
        .is_err());
    f.server
        .import_copy(&admin, &src.path, id, mappings.clone())
        .await
        .unwrap();
    assert_eq!(
        snapshot(&f, &admin, id).await["actor_id"],
        json!(src.viewer)
    );
    let body = json!({"id":Id::new_v4(),"command":{"type":"create","item":{"id":Id::new_v4(),"kind":"document","title":"不允许越权"}}});
    assert_eq!(
        command(&f, &admin, id, body.clone()).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(command(&f, &owner_token, id, body).await.0, StatusCode::OK);
    assert!(f
        .server
        .export_copy(&admin, id, f.files.with_extension("forbidden-export"))
        .await
        .is_err());
    assert_eq!(
        f.api(
            "GET",
            &format!("/api/workspaces/{id}"),
            Some(&unbound_token),
            Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let directory = f
        .api(
            "GET",
            &format!("/api/workspaces/{id}/people?text=actual_owner"),
            Some(&owner_token),
            Value::Null,
        )
        .await;
    assert_eq!(directory.1["accounts"][0]["member_id"], json!(src.owner));
    let revision = snapshot(&f, &owner_token, id).await["state"]["revision"]
        .as_u64()
        .unwrap();
    let duplicate = json!({"id":Id::new_v4(),"expected_revision":revision,"command":{"type":"add_member","member":{"id":actual_owner,"account":"unused","name":"重复认领","department":"","roles":["owner"],"active":true}}});
    assert_eq!(
        command(&f, &owner_token, id, duplicate).await.0,
        StatusCode::BAD_REQUEST
    );
    let mut changed = mappings;
    changed.insert(src.owner, unbound);
    assert!(f
        .server
        .import_copy(&admin, &src.path, id, changed)
        .await
        .is_err());
}

#[tokio::test]
async fn mapped_last_owner_disable_and_global_account_state_remain_enforced() {
    let f = F::new().await;
    let admin = f.admin().await;
    let admin_id = me(&f, &admin).await;
    let (owner, owner_token) = ready(&f, &admin, "space_owner").await;
    let (viewer, viewer_token) = ready(&f, &admin, "space_viewer").await;
    let src = source(None);
    let id = Id::new_v4();
    let mappings = BTreeMap::from([(src.owner, owner), (src.viewer, viewer)]);
    f.server
        .import_copy(&owner_token, &src.path, id, mappings)
        .await
        .unwrap();
    let disable = |version| json!({"version":version,"name":"当前成员","department":"研发","active":false,"is_admin":false});
    assert_eq!(
        f.api(
            "PATCH",
            &format!("/api/users/{owner}"),
            Some(&admin),
            disable(2)
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        f.api(
            "PATCH",
            &format!("/api/users/{viewer}"),
            Some(&admin),
            disable(2)
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        f.api(
            "GET",
            &format!("/api/workspaces/{id}"),
            Some(&viewer_token),
            Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let mention = json!({"id":Id::new_v4(),"command":{"type":"add_comment","id":Id::new_v4(),"item_id":src.item,"body":"不能提及已停用账户","parent_id":null,"mentions":[src.viewer],"attachments":[]}});
    assert!(f
        .server
        .import_copy(
            &admin,
            &src.path,
            Id::new_v4(),
            BTreeMap::from([(src.owner, viewer), (src.viewer, owner)])
        )
        .await
        .is_err());
    assert_eq!(
        command(&f, &owner_token, id, mention).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        snapshot(&f, &owner_token, id).await["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["id"] == json!(src.viewer))
            .unwrap()["active"],
        false
    );
    assert_eq!(
        f.api(
            "GET",
            &format!("/api/workspaces/{id}"),
            Some(&admin),
            Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_ne!(admin_id, owner);
    let exported = f.files.with_extension("disabled-export");
    assert!(f
        .server
        .export_copy(&owner_token, id, &exported)
        .await
        .is_err());
    let revision = snapshot(&f, &owner_token, id).await["state"]["revision"]
        .as_u64()
        .unwrap();
    let disable_member = json!({"id":Id::new_v4(),"expected_revision":revision,"command":{"type":"update_member","member":{"id":src.viewer,"account":"untrusted","name":"不采信","department":"","roles":["viewer"],"active":false}}});
    assert_eq!(
        command(&f, &owner_token, id, disable_member).await.0,
        StatusCode::OK
    );
    f.server
        .export_copy(&owner_token, id, &exported)
        .await
        .unwrap();
    let (local, _) = LocalWorkspace::import_copy(
        &exported,
        f.files.with_extension("disabled-local"),
        Id::new_v4(),
    )
    .unwrap();
    assert!(local.snapshot(src.viewer).is_err());
}

#[tokio::test]
async fn failed_commit_keeps_only_retryable_blobs_and_origin_corruption_is_detected() {
    let f = F::new().await;
    let admin = f.admin().await;
    let admin_id = me(&f, &admin).await;
    let (viewer, _) = ready(&f, &admin, "rollback_viewer").await;
    let src = source(None);
    let id = Id::new_v4();
    let mappings = BTreeMap::from([(src.owner, admin_id), (src.viewer, viewer)]);
    query("ALTER TABLE ff_workspace_origins ADD CONSTRAINT injected_failure FOREIGN KEY(workspace_id) REFERENCES ff_accounts(id) DEFERRABLE INITIALLY DEFERRED").execute(&f.pool).await.unwrap();
    assert!(f
        .server
        .import_copy(&admin, &src.path, id, mappings.clone())
        .await
        .is_err());
    assert_eq!(
        query_scalar::<_, i64>("SELECT COUNT(*) FROM ff_workspaces WHERE id=$1")
            .bind(id)
            .fetch_one(&f.pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM ff_workspace_identities WHERE workspace_id=$1"
        )
        .bind(id)
        .fetch_one(&f.pool)
        .await
        .unwrap(),
        0
    );
    assert!(f
        .files
        .join("workspaces")
        .join(id.to_string())
        .join("attachments")
        .exists());
    query("ALTER TABLE ff_workspace_origins DROP CONSTRAINT injected_failure")
        .execute(&f.pool)
        .await
        .unwrap();
    f.server
        .import_copy(&admin, &src.path, id, mappings)
        .await
        .unwrap();
    query("UPDATE ff_workspace_origin_operations SET receipt='{}' WHERE workspace_id=$1 AND id=$2")
        .bind(id)
        .bind(src.request.id)
        .execute(&f.pool)
        .await
        .unwrap();
    assert_eq!(
        command(
            &f,
            &admin,
            id,
            json!({"id":src.request.id,"command":src.request.command})
        )
        .await
        .0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert!(f
        .server
        .export_copy(&admin, id, f.files.with_extension("corrupt-export"))
        .await
        .is_err());
    let backup = f.files.with_extension("corrupt-backup");
    assert!(f.server.backup(&admin, &backup).await.is_err());
    assert!(!backup.exists());
    query("UPDATE ff_workspace_origins SET checksum='damaged' WHERE workspace_id=$1")
        .bind(id)
        .execute(&f.pool)
        .await
        .unwrap();
    assert!(f
        .server
        .export_copy(&admin, id, f.files.with_extension("corrupt-checksum"))
        .await
        .is_err());
    assert!(fs::read_dir(&src.path).unwrap().next().is_some());
}

#[tokio::test]
async fn unavailable_sources_invalid_tokens_and_missing_history_never_become_success() {
    let f = F::new().await;
    let admin = f.admin().await;
    let admin_id = me(&f, &admin).await;
    let (viewer, _) = ready(&f, &admin, "integrity_viewer").await;
    let src = source(None);
    let id = Id::new_v4();
    let mappings = BTreeMap::from([(src.owner, admin_id), (src.viewer, viewer)]);
    assert!(matches!(
        f.server
            .import_copy("invalid\nheader", &src.path, id, mappings.clone())
            .await,
        Err(farflow_core::Error::Forbidden(_))
    ));
    let destination = f.files.with_extension("must-not-export");
    assert!(matches!(
        f.server
            .export_copy("invalid\nheader", id, &destination)
            .await,
        Err(farflow_core::Error::Forbidden(_))
    ));
    assert!(!destination.exists());
    assert!(matches!(
        f.server
            .import_copy(
                &admin,
                f.files.with_extension("missing-source"),
                id,
                mappings.clone()
            )
            .await,
        Err(farflow_core::Error::Storage(_))
    ));
    f.server
        .import_copy(&admin, &src.path, id, mappings)
        .await
        .unwrap();
    let before = snapshot(&f, &admin, id).await;
    let forged = Id::new_v4();
    query("INSERT INTO ff_workspace_origin_operations VALUES ($1,$2,NULL,NULL,NULL)")
        .bind(id)
        .bind(forged)
        .execute(&f.pool)
        .await
        .unwrap();
    let body = json!({"id":forged,"command":src.request.command});
    assert_eq!(
        command(&f, &admin, id, body).await.0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    query("DELETE FROM ff_workspace_origin_operations WHERE workspace_id=$1 AND id=$2")
        .bind(id)
        .bind(forged)
        .execute(&f.pool)
        .await
        .unwrap();
    query("DELETE FROM ff_workspace_origins WHERE workspace_id=$1")
        .bind(id)
        .execute(&f.pool)
        .await
        .unwrap();
    assert_eq!(
        command(
            &f,
            &admin,
            id,
            json!({"id":src.request.id,"command":src.request.command})
        )
        .await
        .0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(snapshot(&f, &admin, id).await, before);
    let native = Id::new_v4();
    assert_eq!(
        f.api(
            "POST",
            "/api/workspaces",
            Some(&admin),
            json!({"id":native,"name":"缺损初始身份"})
        )
        .await
        .0,
        StatusCode::CREATED
    );
    let row = query("SELECT genesis FROM ff_workspaces WHERE id=$1")
        .bind(native)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    let original: String = row.get("genesis");
    let mut damaged: Value = serde_json::from_str(&original).unwrap();
    damaged["members"] = json!({});
    query("UPDATE ff_workspaces SET genesis=$2 WHERE id=$1")
        .bind(native)
        .bind(serde_json::to_string(&damaged).unwrap())
        .execute(&f.pool)
        .await
        .unwrap();
    assert!(matches!(
        f.server.export_copy(&admin, native, &destination).await,
        Err(farflow_core::Error::Validation(_))
    ));
    assert!(!destination.exists());
    query("UPDATE ff_workspaces SET genesis=$2 WHERE id=$1")
        .bind(native)
        .bind(original)
        .execute(&f.pool)
        .await
        .unwrap();
    f.server
        .export_copy(&admin, native, &destination)
        .await
        .unwrap();
}

#[tokio::test]
async fn executable_copy_commands_use_explicit_mapping_and_never_print_the_session() {
    let f = F::new().await;
    let admin = f.admin().await;
    let admin_id = me(&f, &admin).await;
    let (viewer, _) = ready(&f, &admin, "cli_viewer").await;
    let src = source(None);
    let id = Id::new_v4();
    let map = f.files.with_extension("mapping.json");
    fs::write(
        &map,
        serde_json::to_vec(&BTreeMap::from([
            (src.owner, admin_id),
            (src.viewer, viewer),
        ]))
        .unwrap(),
    )
    .unwrap();
    for _ in 0..2 {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_farflow-server"))
            .arg("import-copy")
            .arg(&src.path)
            .arg(&map)
            .arg(id.to_string())
            .env("DATABASE_URL", &f.url)
            .env("FARFLOW_DATA_DIR", &f.files)
            .env("FARFLOW_SESSION_TOKEN", &admin)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(!stdout.contains(&admin));
        assert!(!stdout.contains(&f.url));
    }
    let output_path = f.files.with_extension("cli-copy-export");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_farflow-server"))
        .arg("export-copy")
        .arg(id.to_string())
        .arg(&output_path)
        .env("DATABASE_URL", &f.url)
        .env("FARFLOW_DATA_DIR", &f.files)
        .env("FARFLOW_SESSION_TOKEN", &admin)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        MigrationArchive::open(&output_path)
            .unwrap()
            .report()
            .unwrap()
            .source
            .mode,
        farflow_core::Mode::Service
    );
}

#[tokio::test]
async fn abandoned_branch_actors_need_mappings_but_gain_no_live_membership() {
    let f = F::new().await;
    let admin = f.admin().await;
    let admin_id = me(&f, &admin).await;
    let (historic, historic_token) = ready(&f, &admin, "historical_actor").await;
    let dir = tempfile::tempdir().unwrap();
    let owner = member(Id::new_v4(), "source_owner", Role::Owner);
    let first = farflow_core::GitWorkspace::create(
        dir.path().join("first"),
        Id::new_v4(),
        "历史分支".into(),
        owner.clone(),
    )
    .unwrap();
    let pack = dir.path().join("base.farflowpack");
    first.export_pack(owner.id, &pack).unwrap();
    let peer_path = dir.path().join("peer");
    let peer = farflow_core::GitWorkspace::from_pack(&pack, &peer_path).unwrap();
    let historical = member(Id::new_v4(), "proposal", Role::Developer);
    let mut alternate = historical.clone();
    alternate.account = "alternative".into();
    alternate.roles = BTreeSet::from([Role::Viewer]);
    let abandoned_operation = Id::new_v4();
    first
        .execute(Request {
            id: abandoned_operation,
            actor_id: owner.id,
            at: 1,
            command: Command::AddMember {
                member: historical.clone(),
            },
        })
        .unwrap();
    peer.execute(Request {
        id: Id::new_v4(),
        actor_id: owner.id,
        at: 1,
        command: Command::AddMember { member: alternate },
    })
    .unwrap();
    let item: NewItem = serde_json::from_value(
        json!({"id":Id::new_v4(),"kind":"document","title":"放弃分支的内容"}),
    )
    .unwrap();
    first
        .execute(Request {
            id: Id::new_v4(),
            actor_id: historical.id,
            at: 2,
            command: Command::Create { item },
        })
        .unwrap();
    let conflict = first.merge_from(owner.id, &peer_path).unwrap();
    first
        .resolve(farflow_core::GitResolution {
            id: Id::new_v4(),
            actor_id: owner.id,
            at: 3,
            targets: conflict.issues.keys().copied().collect(),
            command: None,
            note: "放弃这批分支但保留原作者记录".into(),
        })
        .unwrap();
    let source_path = dir.path().join("package");
    let report = first.export_copy(owner.id, &source_path).unwrap();
    assert_eq!(report.members.len(), 1);
    assert_eq!(report.identity_ids.len(), 2);
    let id = Id::new_v4();
    assert!(f
        .server
        .import_copy(
            &admin,
            &source_path,
            id,
            BTreeMap::from([(owner.id, admin_id)])
        )
        .await
        .is_err());
    f.server
        .import_copy(
            &admin,
            &source_path,
            id,
            BTreeMap::from([(owner.id, admin_id), (historical.id, historic)]),
        )
        .await
        .unwrap();
    assert_eq!(
        command(
            &f,
            &admin,
            id,
            json!({"id":abandoned_operation,"command":Command::AddMember{member:historical.clone()}})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        f.api(
            "GET",
            &format!("/api/workspaces/{id}"),
            Some(&historic_token),
            Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let backup = f.files.with_extension("historical-backup");
    f.server.backup(&admin, &backup).await.unwrap();
    let exported = f.files.with_extension("historical-export");
    f.server.export_copy(&admin, id, &exported).await.unwrap();
    assert!(MigrationArchive::open(&exported)
        .unwrap()
        .report()
        .unwrap()
        .identity_ids
        .contains(&historical.id));
}

#[tokio::test]
async fn service_source_freezes_old_transactions_and_hands_the_same_history_to_git() {
    let f = F::new().await;
    let admin = f.admin().await;
    let admin_id = me(&f, &admin).await;
    let (viewer, viewer_token) = ready(&f, &admin, "formal_viewer").await;
    let src = source(None);
    let id = Id::new_v4();
    f.server
        .import_copy(
            &admin,
            &src.path,
            id,
            BTreeMap::from([(src.owner, admin_id), (src.viewer, viewer)]),
        )
        .await
        .unwrap();
    assert!(f
        .server
        .transfer_status(&admin, id)
        .await
        .unwrap()
        .is_none());
    assert!(f
        .server
        .transfer_events(&admin, id)
        .await
        .unwrap()
        .is_empty());
    let input = json!({"id":Id::new_v4(),"command":{"type":"add_comment","id":Id::new_v4(),"item_id":src.item,"body":"冻结前的真实服务评论","parent_id":null,"mentions":[],"attachments":[]}});
    assert_eq!(
        command(&f, &admin, id, input.clone()).await.0,
        StatusCode::OK
    );
    let before = snapshot(&f, &admin, id).await;
    let mut old = f.pool.begin().await.unwrap();
    query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *old)
        .await
        .unwrap();
    query("SELECT id FROM ff_workspaces WHERE id=$1")
        .bind(id)
        .fetch_one(&mut *old)
        .await
        .unwrap();
    let transfer = Id::new_v4();
    let backup = f.files.with_extension("before-formal");
    let frozen = f
        .server
        .prepare_transfer(&admin, id, transfer, Mode::Git, &backup)
        .await
        .unwrap();
    assert_eq!(frozen.phase, TransferPhase::Frozen);
    assert_eq!(frozen.changed_by, src.owner);
    assert!(backup.join("migration.json").exists());
    assert_eq!(
        f.server
            .prepare_transfer(
                &admin,
                id,
                transfer,
                Mode::Git,
                f.files.with_extension("unused-backup")
            )
            .await
            .unwrap(),
        frozen
    );
    assert!(!f.files.with_extension("unused-backup").exists());
    assert_eq!(
        snapshot(&f, &viewer_token, id).await["transfer"]["phase"],
        "frozen"
    );
    assert_eq!(command(&f, &admin, id, input).await.0, StatusCode::CONFLICT);
    assert_eq!(command(&f,&admin,id,json!({"id":Id::new_v4(),"command":{"type":"create","item":{"id":Id::new_v4(),"kind":"document","title":"不能继续写"}}})).await.0,StatusCode::CONFLICT);
    let error = query("INSERT INTO ff_workspace_operations VALUES($1,$2,999,'{}','{}','{}')")
        .bind(id)
        .bind(Id::new_v4())
        .execute(&mut *old)
        .await
        .unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("40001")
    );
    old.rollback().await.unwrap();
    for (table, column) in [
        ("ff_workspaces", "name"),
        ("ff_workspace_operations", "workspace_id"),
        ("ff_workspace_members", "workspace_id"),
        ("ff_workspace_identities", "workspace_id"),
    ] {
        let filter = if table == "ff_workspaces" {
            "id"
        } else {
            "workspace_id"
        };
        for statement in [
            format!("UPDATE {table} SET {column}={column} WHERE {filter}=$1"),
            format!("DELETE FROM {table} WHERE {filter}=$1"),
            format!("INSERT INTO {table} SELECT * FROM {table} WHERE {filter}=$1 LIMIT 1"),
        ] {
            let error = query(&statement)
                .bind(id)
                .execute(&f.pool)
                .await
                .unwrap_err();
            assert_eq!(
                error.as_database_error().unwrap().code().as_deref(),
                Some("55000")
            );
        }
        if table != "ff_workspaces" {
            let error = query(&format!(
                "UPDATE {table} SET workspace_id=$2 WHERE workspace_id=$1"
            ))
            .bind(id)
            .bind(Id::new_v4())
            .execute(&f.pool)
            .await
            .unwrap_err();
            assert_eq!(
                error.as_database_error().unwrap().code().as_deref(),
                Some("55000")
            );
        }
    }
    assert_eq!(snapshot(&f, &admin, id).await["state"], before["state"]);
    assert!(f
        .server
        .export_copy(&admin, id, f.files.with_extension("bypass-formal"))
        .await
        .is_err());
    let frozen_backup = f.files.with_extension("frozen-service-backup");
    f.server.backup(&admin, &frozen_backup).await.unwrap();
    let recovered = F::new().await;
    Server::restore(&frozen_backup, &recovered.url, &recovered.files)
        .await
        .unwrap();
    let recovered_token = recovered.login("owner", PASSWORD).await;
    assert_eq!(
        recovered
            .server
            .transfer_status(&recovered_token, id)
            .await
            .unwrap()
            .unwrap(),
        frozen
    );
    assert_eq!(command(&recovered,&recovered_token,id,json!({"id":Id::new_v4(),"command":{"type":"create","item":{"id":Id::new_v4(),"kind":"document","title":"冻结恢复仍不可写"}}})).await.0,StatusCode::CONFLICT);
    let package = f.files.with_extension("formal-package");
    f.server
        .export_transfer(&admin, id, &package)
        .await
        .unwrap();
    f.server
        .export_transfer(&admin, id, &package)
        .await
        .unwrap();
    let (git, report, receipt) = GitWorkspace::stage_transfer(
        &package,
        f.files.with_extension("formal-git"),
        src.owner,
        frozen.plan.prepared_at,
    )
    .unwrap();
    assert_eq!(report.target.unwrap().id, id);
    assert_eq!(
        serde_json::to_value(git.snapshot(src.owner).unwrap().state).unwrap(),
        before["state"]
    );
    assert_eq!(
        git.read_attachment(src.owner, src.attachment).unwrap(),
        b"mapped real evidence"
    );
    assert!(f
        .server
        .complete_transfer(&viewer_token, id, &receipt)
        .await
        .is_err());
    assert!(f
        .server
        .complete_transfer(
            &admin,
            id,
            &TransferReceipt {
                plan_checksum: "0".repeat(64)
            }
        )
        .await
        .is_err());
    let proof = f
        .server
        .complete_transfer(&admin, id, &receipt)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_string(
            &f.server
                .complete_transfer(&admin, id, &receipt)
                .await
                .unwrap()
        )
        .unwrap(),
        serde_json::to_string(&proof).unwrap()
    );
    assert!(f
        .server
        .cancel_transfer(&admin, id, transfer)
        .await
        .is_err());
    assert_eq!(f.server.transfer_events(&admin, id).await.unwrap().len(), 2);
    assert!(
        !serde_json::to_string(&f.server.transfer_events(&admin, id).await.unwrap())
            .unwrap()
            .contains(&proof.token)
    );
    git.activate_transfer(src.owner, frozen.plan.prepared_at + 1, &proof)
        .unwrap();
    git.execute(Request {
        id: Id::new_v4(),
        actor_id: src.owner,
        at: frozen.plan.prepared_at + 2,
        command: Command::AddComment {
            id: Id::new_v4(),
            item_id: src.item,
            body: "Git 接续".into(),
            parent_id: None,
            mentions: BTreeSet::new(),
            attachments: BTreeSet::new(),
        },
    })
    .unwrap();
    let backup = f.files.with_extension("completed-service-backup");
    f.server.backup(&admin, &backup).await.unwrap();
    let target = F::new().await;
    Server::restore(&backup, &target.url, &target.files)
        .await
        .unwrap();
    let token = target.login("owner", PASSWORD).await;
    assert_eq!(
        target
            .server
            .transfer_status(&token, id)
            .await
            .unwrap()
            .unwrap()
            .phase,
        TransferPhase::Completed
    );
    assert_eq!(
        serde_json::to_string(
            &target
                .server
                .complete_transfer(&token, id, &receipt)
                .await
                .unwrap()
        )
        .unwrap(),
        serde_json::to_string(&proof).unwrap()
    );
    assert_eq!(command(&target,&token,id,json!({"id":Id::new_v4(),"command":{"type":"create","item":{"id":Id::new_v4(),"kind":"document","title":"旧源仍只读"}}})).await.0,StatusCode::CONFLICT);
    assert!(Server::connect(&target.url, &target.files).await.is_ok());
}

#[tokio::test]
async fn local_source_stages_into_real_mapped_service_accounts_and_restores_active_control() {
    let f = F::new().await;
    let admin = f.admin().await;
    let admin_id = me(&f, &admin).await;
    let (owner, owner_token) = ready(&f, &admin, "formal_owner").await;
    let src = source(None);
    let mut local = LocalWorkspace::open(src._dir.path().join("local")).unwrap();
    let frozen = local
        .prepare_transfer(
            src.owner,
            Id::new_v4(),
            100,
            Mode::Service,
            src._dir.path().join("before-service"),
        )
        .unwrap();
    let package = src._dir.path().join("formal-service-package");
    local.export_transfer(src.owner, &package).unwrap();
    let mappings = BTreeMap::from([(src.owner, owner), (src.viewer, admin_id)]);
    assert!(f
        .server
        .stage_transfer(&admin, &package, mappings.clone())
        .await
        .is_err());
    assert!(f
        .server
        .import_copy(&owner_token, &package, Id::new_v4(), mappings.clone())
        .await
        .is_err());
    let (report, receipt) = f
        .server
        .stage_transfer(&owner_token, &package, mappings.clone())
        .await
        .unwrap();
    let id = src.state.id;
    assert_eq!(report.target.unwrap().id, id);
    assert_eq!(
        f.server
            .transfer_status(&owner_token, id)
            .await
            .unwrap()
            .unwrap()
            .phase,
        TransferPhase::Staged
    );
    assert!(f.server.transfer_status(&admin, id).await.is_err());
    assert_eq!(
        snapshot(&f, &owner_token, id).await["actor_id"],
        json!(src.owner)
    );
    assert_eq!(
        snapshot(&f, &admin, id).await["state"],
        serde_json::to_value(&src.state).unwrap()
    );
    let body = json!({"id":Id::new_v4(),"command":{"type":"add_comment","id":Id::new_v4(),"item_id":src.item,"body":"真实映射账户续写","parent_id":null,"mentions":[],"attachments":[]}});
    assert_eq!(
        command(&f, &owner_token, id, body.clone()).await.0,
        StatusCode::CONFLICT
    );
    let staged_backup = f.files.with_extension("staged-service-backup");
    f.server.backup(&admin, &staged_backup).await.unwrap();
    let recovered = F::new().await;
    Server::restore(&staged_backup, &recovered.url, &recovered.files)
        .await
        .unwrap();
    let recovered_token = recovered.login("formal_owner", NEXT_PASSWORD).await;
    assert_eq!(
        recovered
            .server
            .transfer_status(&recovered_token, id)
            .await
            .unwrap()
            .unwrap()
            .phase,
        TransferPhase::Staged
    );
    assert_eq!(
        command(&recovered, &recovered_token, id, body.clone())
            .await
            .0,
        StatusCode::CONFLICT
    );
    let wrong = TransferActivation {
        transfer_id: frozen.plan.id,
        plan_checksum: receipt.plan_checksum.clone(),
        token: "0".repeat(64),
    };
    assert!(f
        .server
        .activate_transfer(&owner_token, id, &wrong)
        .await
        .is_err());
    let proof = local.complete_transfer(src.owner, 101, &receipt).unwrap();
    assert!(f
        .server
        .activate_transfer(&admin, id, &proof)
        .await
        .is_err());
    let active = f
        .server
        .activate_transfer(&owner_token, id, &proof)
        .await
        .unwrap();
    assert_eq!(active.phase, TransferPhase::Active);
    assert_eq!(active.changed_by, src.owner);
    assert_eq!(
        f.server
            .activate_transfer(&owner_token, id, &proof)
            .await
            .unwrap(),
        active
    );
    assert_eq!(command(&f, &owner_token, id, body).await.0, StatusCode::OK);
    let after = snapshot(&f, &owner_token, id).await;
    assert_eq!(
        after["state"]["history"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()["actor_id"],
        json!(src.owner)
    );
    assert_eq!(
        f.server
            .stage_transfer(&owner_token, &package, mappings)
            .await
            .unwrap()
            .1,
        receipt
    );
    assert_eq!(snapshot(&f, &owner_token, id).await, after);
    let backup = f.files.with_extension("active-service-backup");
    f.server.backup(&admin, &backup).await.unwrap();
    let target = F::new().await;
    Server::restore(&backup, &target.url, &target.files)
        .await
        .unwrap();
    let token = target.login("formal_owner", NEXT_PASSWORD).await;
    assert_eq!(snapshot(&target, &token, id).await, after);
    assert_eq!(
        target
            .server
            .transfer_events(&token, id)
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        attachment(&target, &token, id, src.attachment).await,
        b"mapped real evidence"
    );
    let next = target
        .server
        .prepare_transfer(
            &token,
            id,
            Id::new_v4(),
            Mode::Git,
            target.files.with_extension("next-before"),
        )
        .await
        .unwrap();
    target
        .server
        .cancel_transfer(&token, id, next.plan.id)
        .await
        .unwrap();
    assert_eq!(
        target
            .server
            .transfer_events(&token, id)
            .await
            .unwrap()
            .len(),
        4
    );
    assert_eq!(command(&target,&token,id,json!({"id":Id::new_v4(),"command":{"type":"add_comment","id":Id::new_v4(),"item_id":src.item,"body":"取消后继续","parent_id":null,"mentions":[],"attachments":[]}})).await.0,StatusCode::OK);
}

#[tokio::test]
async fn native_service_transfer_cancellation_reconnects_and_defers_no_partial_freeze_or_completion(
) {
    let f = F::new().await;
    let admin = f.admin().await;
    let id = Id::new_v4();
    assert_eq!(
        f.api(
            "POST",
            "/api/workspaces",
            Some(&admin),
            json!({"id":id,"name":"原生源"})
        )
        .await
        .0,
        StatusCode::CREATED
    );
    let transfer = Id::new_v4();
    query("ALTER TABLE ff_workspace_transfer_events ADD CONSTRAINT injected_transfer_failure FOREIGN KEY(workspace_id) REFERENCES ff_accounts(id) DEFERRABLE INITIALLY DEFERRED").execute(&f.pool).await.unwrap();
    assert!(f
        .server
        .prepare_transfer(
            &admin,
            id,
            transfer,
            Mode::Git,
            f.files.with_extension("failed-before")
        )
        .await
        .is_err());
    assert!(f
        .server
        .transfer_status(&admin, id)
        .await
        .unwrap()
        .is_none());
    assert!(f
        .server
        .transfer_events(&admin, id)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(command(&f,&admin,id,json!({"id":Id::new_v4(),"command":{"type":"create","item":{"id":Id::new_v4(),"kind":"document","title":"失败后继续"}}})).await.0,StatusCode::OK);
    query("ALTER TABLE ff_workspace_transfer_events DROP CONSTRAINT injected_transfer_failure")
        .execute(&f.pool)
        .await
        .unwrap();
    let status = f
        .server
        .prepare_transfer(
            &admin,
            id,
            transfer,
            Mode::Git,
            f.files.with_extension("before-native"),
        )
        .await
        .unwrap();
    assert!(Server::connect(&f.url, &f.files).await.is_ok());
    let package = f.files.with_extension("native-package");
    f.server
        .export_transfer(&admin, id, &package)
        .await
        .unwrap();
    let (git, _, receipt) = GitWorkspace::stage_transfer(
        &package,
        f.files.with_extension("native-git"),
        status.plan.prepared_by,
        status.plan.prepared_at,
    )
    .unwrap();
    query("ALTER TABLE ff_workspace_transfer_events ADD CONSTRAINT stop_completion CHECK(data::jsonb->>'phase'<>'completed')").execute(&f.pool).await.unwrap();
    assert!(f
        .server
        .complete_transfer(&admin, id, &receipt)
        .await
        .is_err());
    assert_eq!(
        f.server
            .transfer_status(&admin, id)
            .await
            .unwrap()
            .unwrap()
            .phase,
        TransferPhase::Frozen
    );
    assert_eq!(f.server.transfer_events(&admin, id).await.unwrap().len(), 1);
    query("ALTER TABLE ff_workspace_transfer_events DROP CONSTRAINT stop_completion")
        .execute(&f.pool)
        .await
        .unwrap();
    let cancelled = f
        .server
        .cancel_transfer(&admin, id, transfer)
        .await
        .unwrap();
    assert_eq!(cancelled.phase, TransferPhase::Cancelled);
    assert_eq!(
        f.server
            .cancel_transfer(&admin, id, transfer)
            .await
            .unwrap(),
        cancelled
    );
    assert!(f
        .server
        .complete_transfer(&admin, id, &receipt)
        .await
        .is_err());
    assert_eq!(
        git.transfer_status(status.plan.prepared_by)
            .unwrap()
            .unwrap()
            .phase,
        TransferPhase::Staged
    );
    assert!(Server::connect(&f.url, &f.files).await.is_ok());
    assert!(f
        .server
        .prepare_transfer(
            &admin,
            id,
            transfer,
            Mode::Git,
            f.files.with_extension("reused-id")
        )
        .await
        .is_err());
    assert_eq!(command(&f,&admin,id,json!({"id":Id::new_v4(),"command":{"type":"create","item":{"id":Id::new_v4(),"kind":"document","title":"取消后恢复"}}})).await.0,StatusCode::OK);
    let next = f
        .server
        .prepare_transfer(
            &admin,
            id,
            Id::new_v4(),
            Mode::Git,
            f.files.with_extension("new-plan-before"),
        )
        .await
        .unwrap();
    f.server
        .cancel_transfer(&admin, id, next.plan.id)
        .await
        .unwrap();
    assert!(f
        .server
        .prepare_transfer(
            &admin,
            id,
            transfer,
            Mode::Git,
            f.files.with_extension("old-plan-reuse")
        )
        .await
        .is_err());
    let backup = f.files.with_extension("cancelled-backup");
    f.server.backup(&admin, &backup).await.unwrap();
    let target = F::new().await;
    Server::restore(&backup, &target.url, &target.files)
        .await
        .unwrap();
    let token = target.login("owner", PASSWORD).await;
    assert_eq!(
        target
            .server
            .transfer_status(&token, id)
            .await
            .unwrap()
            .unwrap()
            .phase,
        TransferPhase::Cancelled
    );
}

#[tokio::test]
async fn service_transfer_rejects_bad_sessions_barriers_control_and_actor_audit() {
    let f = F::new().await;
    let admin = f.admin().await;
    let id = Id::new_v4();
    f.api(
        "POST",
        "/api/workspaces",
        Some(&admin),
        json!({"id":id,"name":"完整性"}),
    )
    .await;
    assert!(f.server.transfer_status("bad\nheader", id).await.is_err());
    assert!(f
        .server
        .transfer_status("unknown-session", id)
        .await
        .is_err());
    assert!(f
        .server
        .transfer_status(&admin, Id::new_v4())
        .await
        .is_err());
    assert!(f
        .server
        .export_transfer(&admin, id, f.files.with_extension("not-frozen"))
        .await
        .is_err());
    assert!(f
        .server
        .cancel_transfer(&admin, id, Id::new_v4())
        .await
        .is_err());
    assert!(f
        .server
        .complete_transfer(
            &admin,
            id,
            &TransferReceipt {
                plan_checksum: "0".repeat(64)
            }
        )
        .await
        .is_err());
    assert!(f
        .server
        .activate_transfer(
            &admin,
            id,
            &TransferActivation {
                transfer_id: Id::new_v4(),
                plan_checksum: "0".repeat(64),
                token: "0".repeat(64)
            }
        )
        .await
        .is_err());
    assert!(f
        .server
        .prepare_transfer(
            &admin,
            id,
            Id::nil(),
            Mode::Git,
            f.files.with_extension("nil-id")
        )
        .await
        .is_err());
    assert!(f
        .server
        .prepare_transfer(
            &admin,
            id,
            Id::new_v4(),
            Mode::Service,
            f.files.with_extension("same-mode")
        )
        .await
        .is_err());
    let status = f
        .server
        .prepare_transfer(
            &admin,
            id,
            Id::new_v4(),
            Mode::Git,
            f.files.with_extension("before-corruption"),
        )
        .await
        .unwrap();
    assert!(f
        .server
        .cancel_transfer(&admin, id, Id::new_v4())
        .await
        .is_err());
    query("ALTER TABLE ff_workspace_members DISABLE TRIGGER ff_transfer_fence")
        .execute(&f.pool)
        .await
        .unwrap();
    assert!(f.server.transfer_status(&admin, id).await.is_err());
    assert!(Server::connect(&f.url, &f.files).await.is_err());
    query("ALTER TABLE ff_workspace_members ENABLE TRIGGER ff_transfer_fence")
        .execute(&f.pool)
        .await
        .unwrap();
    let old_hash: String =
        query_scalar("SELECT checksum FROM ff_workspace_transfer WHERE workspace_id=$1")
            .bind(id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    query("UPDATE ff_workspace_transfer SET checksum='damaged' WHERE workspace_id=$1")
        .bind(id)
        .execute(&f.pool)
        .await
        .unwrap();
    assert!(f.server.transfer_status(&admin, id).await.is_err());
    query("UPDATE ff_workspace_transfer SET checksum=$2 WHERE workspace_id=$1")
        .bind(id)
        .bind(old_hash)
        .execute(&f.pool)
        .await
        .unwrap();
    let (other, other_token) = ready(&f, &admin, "wrong_auditor").await;
    assert!(f.server.transfer_status(&other_token, id).await.is_err());
    f.create(&admin, "pending_transfer", false).await;
    let pending = f.login("pending_transfer", PASSWORD).await;
    assert!(f.server.transfer_status(&pending, id).await.is_err());
    query("UPDATE ff_workspace_transfer_events SET account_id=$2 WHERE workspace_id=$1")
        .bind(id)
        .bind(other)
        .execute(&f.pool)
        .await
        .unwrap();
    assert!(f.server.transfer_events(&admin, id).await.is_err());
    let backup = f.files.with_extension("bad-audit-backup");
    assert!(f.server.backup(&admin, &backup).await.is_err());
    assert!(!backup.exists());
    assert_eq!(status.phase, TransferPhase::Frozen);
    query("DELETE FROM ff_workspace_transfer WHERE workspace_id=$1")
        .bind(id)
        .execute(&f.pool)
        .await
        .unwrap();
    assert!(f.server.transfer_status(&admin, id).await.is_err());
    let error = query("UPDATE ff_workspaces SET revision=revision WHERE id=$1")
        .bind(id)
        .execute(&f.pool)
        .await
        .unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("55000")
    );
}

#[tokio::test]
async fn service_global_suspension_during_freeze_requires_a_new_explicit_member_cutpoint() {
    let f = F::new().await;
    let admin = f.admin().await;
    let admin_id = me(&f, &admin).await;
    let (viewer, _) = ready(&f, &admin, "suspended_transfer").await;
    let src = source(None);
    let id = Id::new_v4();
    f.server
        .import_copy(
            &admin,
            &src.path,
            id,
            BTreeMap::from([(src.owner, admin_id), (src.viewer, viewer)]),
        )
        .await
        .unwrap();
    let first = f
        .server
        .prepare_transfer(
            &admin,
            id,
            Id::new_v4(),
            Mode::Git,
            f.files.with_extension("before-suspension"),
        )
        .await
        .unwrap();
    let package = f.files.with_extension("before-suspension-package");
    f.server
        .export_transfer(&admin, id, &package)
        .await
        .unwrap();
    let (_, _, receipt) = GitWorkspace::stage_transfer(
        &package,
        f.files.with_extension("before-suspension-target"),
        src.owner,
        first.plan.prepared_at,
    )
    .unwrap();
    assert_eq!(f.api("PATCH",&format!("/api/users/{viewer}"),Some(&admin),json!({"version":2,"name":"停用成员","department":"研发","active":false,"is_admin":false})).await.0,StatusCode::OK);
    assert!(f
        .server
        .complete_transfer(&admin, id, &receipt)
        .await
        .is_err());
    assert!(f
        .server
        .export_transfer(&admin, id, f.files.with_extension("suspended-export"))
        .await
        .is_err());
    f.server
        .cancel_transfer(&admin, id, first.plan.id)
        .await
        .unwrap();
    let revision = snapshot(&f, &admin, id).await["state"]["revision"]
        .as_u64()
        .unwrap();
    assert_eq!(command(&f,&admin,id,json!({"id":Id::new_v4(),"expected_revision":revision,"command":{"type":"update_member","member":{"id":src.viewer,"account":"ignored","name":"不伪造资料","department":"","roles":["viewer"],"active":false}}})).await.0,StatusCode::OK);
    let next = f
        .server
        .prepare_transfer(
            &admin,
            id,
            Id::new_v4(),
            Mode::Git,
            f.files.with_extension("after-suspension-backup"),
        )
        .await
        .unwrap();
    let package = f.files.with_extension("after-suspension-package");
    f.server
        .export_transfer(&admin, id, &package)
        .await
        .unwrap();
    let (git, _, receipt) = GitWorkspace::stage_transfer(
        &package,
        f.files.with_extension("after-suspension-target"),
        src.owner,
        next.plan.prepared_at,
    )
    .unwrap();
    let proof = f
        .server
        .complete_transfer(&admin, id, &receipt)
        .await
        .unwrap();
    git.activate_transfer(src.owner, next.plan.prepared_at + 1, &proof)
        .unwrap();
    assert!(git.snapshot(src.viewer).is_err());
    assert_eq!(f.server.transfer_events(&admin, id).await.unwrap().len(), 4);
}

#[tokio::test]
async fn completed_read_only_source_does_not_trap_the_last_owner_account_forever() {
    let f = F::new().await;
    let admin = f.admin().await;
    let (owner, token) = ready(&f, &admin, "retired_source_owner").await;
    let id = Id::new_v4();
    assert_eq!(
        f.api(
            "POST",
            "/api/workspaces",
            Some(&token),
            json!({"id":id,"name":"交接后退役的源"})
        )
        .await
        .0,
        StatusCode::CREATED
    );
    let frozen = f
        .server
        .prepare_transfer(
            &token,
            id,
            Id::new_v4(),
            Mode::Git,
            f.files.with_extension("before-retirement"),
        )
        .await
        .unwrap();
    let disable =
        json!({"version":2,"name":"原所有者","department":"研发","active":false,"is_admin":false});
    assert_eq!(
        f.api(
            "PATCH",
            &format!("/api/users/{owner}"),
            Some(&admin),
            disable.clone()
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let package = f.files.with_extension("retirement-package");
    f.server
        .export_transfer(&token, id, &package)
        .await
        .unwrap();
    let (git, _, receipt) = GitWorkspace::stage_transfer(
        &package,
        f.files.with_extension("retirement-target"),
        owner,
        frozen.plan.prepared_at,
    )
    .unwrap();
    let proof = f
        .server
        .complete_transfer(&token, id, &receipt)
        .await
        .unwrap();
    git.activate_transfer(owner, frozen.plan.prepared_at + 1, &proof)
        .unwrap();
    assert_eq!(
        f.api(
            "PATCH",
            &format!("/api/users/{owner}"),
            Some(&admin),
            disable
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        f.api(
            "GET",
            &format!("/api/workspaces/{id}"),
            Some(&token),
            Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        f.api(
            "GET",
            &format!("/api/workspaces/{id}"),
            Some(&admin),
            Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let backup = f.files.with_extension("retired-source-backup");
    f.server.backup(&admin, &backup).await.unwrap();
    let target = F::new().await;
    Server::restore(&backup, &target.url, &target.files)
        .await
        .unwrap();
    let restored = target.login("owner", PASSWORD).await;
    let again = target.files.with_extension("retired-again-backup");
    target.server.backup(&restored, &again).await.unwrap();
    assert!(Server::connect(&target.url, &target.files).await.is_ok());
    let error = query("UPDATE ff_workspaces SET revision=revision WHERE id=$1")
        .bind(id)
        .execute(&target.pool)
        .await
        .unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("55000")
    );
}

#[tokio::test]
async fn service_to_local_handoff_preserves_real_client_origins_and_cancellation_stays_read_only() {
    let f = F::new().await;
    let admin = f.admin().await;
    let admin_id = me(&f, &admin).await;
    let (viewer, viewer_token) = ready(&f, &admin, "to_local_viewer").await;
    let src = source(None);
    let id = Id::new_v4();
    let bindings = BTreeMap::from([(src.owner, admin_id), (src.viewer, viewer)]);
    f.server
        .import_copy(&admin, &src.path, id, bindings.clone())
        .await
        .unwrap();
    let service_operation = Id::new_v4();
    let input = json!({"id":service_operation,"command":{"type":"add_comment","id":Id::new_v4(),"item_id":src.item,"body":"服务原始请求与真实账户","parent_id":null,"mentions":[src.viewer],"attachments":[src.attachment]}});
    assert_eq!(
        command(&f, &admin, id, input.clone()).await.0,
        StatusCode::OK
    );
    let before = snapshot(&f, &admin, id).await["state"].clone();
    let frozen = f
        .server
        .prepare_transfer(
            &admin,
            id,
            Id::new_v4(),
            Mode::Local,
            f.files.with_extension("before-local-transfer"),
        )
        .await
        .unwrap();
    assert_eq!(command(&f, &admin, id, input).await.0, StatusCode::CONFLICT);
    let package = f.files.with_extension("to-local-transfer");
    f.server
        .export_transfer(&admin, id, &package)
        .await
        .unwrap();
    let archive = MigrationArchive::open(&package).unwrap();
    let original = archive.transfer_history(Mode::Local).unwrap();
    assert!(original.clients.contains_key(&service_operation));
    assert!(!original.origins.is_empty());
    let destination = f.files.with_extension("local-transfer-target");
    assert!(LocalWorkspace::stage_transfer(
        &package,
        &destination,
        src.viewer,
        frozen.plan.prepared_at
    )
    .is_err());
    let (mut local, report, receipt) =
        LocalWorkspace::stage_transfer(&package, &destination, src.owner, frozen.plan.prepared_at)
            .unwrap();
    assert_eq!(report.target.unwrap().id, id);
    assert_eq!(
        serde_json::to_value(local.snapshot(src.owner).unwrap()).unwrap(),
        before
    );
    assert_eq!(
        local.read_attachment(src.viewer, src.attachment).unwrap(),
        b"mapped real evidence"
    );
    assert!(f
        .server
        .complete_transfer(&viewer_token, id, &receipt)
        .await
        .is_err());
    let proof = f
        .server
        .complete_transfer(&admin, id, &receipt)
        .await
        .unwrap();
    assert_eq!(
        snapshot(&f, &admin, id).await["transfer"]["phase"],
        "completed"
    );
    local
        .activate_transfer(src.owner, frozen.plan.prepared_at + 1, &proof)
        .unwrap();
    local.execute(src.request.clone()).unwrap();
    local
        .execute(Request {
            id: Id::new_v4(),
            actor_id: src.owner,
            at: frozen.plan.prepared_at + 2,
            command: Command::AddComment {
                id: Id::new_v4(),
                item_id: src.item,
                body: "本地离线继续协作".into(),
                parent_id: None,
                mentions: BTreeSet::from([src.viewer]),
                attachments: BTreeSet::from([src.attachment]),
            },
        })
        .unwrap();
    let after = local.snapshot(src.owner).unwrap();
    let (_, _, again) = LocalWorkspace::stage_transfer(
        &package,
        &destination,
        src.owner,
        frozen.plan.prepared_at + 3,
    )
    .unwrap();
    assert_eq!(again, receipt);
    assert_eq!(
        LocalWorkspace::open(&destination)
            .unwrap()
            .snapshot(src.owner)
            .unwrap()
            .revision,
        after.revision
    );
    let exported = f.files.with_extension("local-after-transfer-copy");
    local.export_copy(src.owner, &exported).unwrap();
    let roundtrip = MigrationArchive::open(&exported)
        .unwrap()
        .copied_history(Id::new_v4())
        .unwrap();
    assert_eq!(roundtrip.clients, original.clients);
    assert_eq!(roundtrip.origins.len(), original.origins.len() + 1);
    let backup = f.files.with_extension("active-local-transfer-backup");
    local.backup(src.owner, &backup).unwrap();
    let mut restored =
        LocalWorkspace::restore(&backup, f.files.with_extension("restored-local-transfer"))
            .unwrap();
    assert_eq!(
        serde_json::to_value(restored.snapshot(src.owner).unwrap()).unwrap(),
        serde_json::to_value(after).unwrap()
    );
    restored
        .execute(Request {
            id: Id::new_v4(),
            actor_id: src.owner,
            at: frozen.plan.prepared_at + 4,
            command: Command::AddComment {
                id: Id::new_v4(),
                item_id: src.item,
                body: "恢复后接续".into(),
                parent_id: None,
                mentions: BTreeSet::new(),
                attachments: BTreeSet::new(),
            },
        })
        .unwrap();

    let cancelled_id = Id::new_v4();
    f.server
        .import_copy(&admin, &src.path, cancelled_id, bindings)
        .await
        .unwrap();
    let cancelled = f
        .server
        .prepare_transfer(
            &admin,
            cancelled_id,
            Id::new_v4(),
            Mode::Local,
            f.files.with_extension("cancel-before-local"),
        )
        .await
        .unwrap();
    let cancelled_package = f.files.with_extension("cancelled-local-package");
    f.server
        .export_transfer(&admin, cancelled_id, &cancelled_package)
        .await
        .unwrap();
    let (mut abandoned, _, old_receipt) = LocalWorkspace::stage_transfer(
        &cancelled_package,
        f.files.with_extension("abandoned-local-target"),
        src.owner,
        cancelled.plan.prepared_at,
    )
    .unwrap();
    f.server
        .cancel_transfer(&admin, cancelled_id, cancelled.plan.id)
        .await
        .unwrap();
    assert!(f
        .server
        .complete_transfer(&admin, cancelled_id, &old_receipt)
        .await
        .is_err());
    assert_eq!(command(&f,&admin,cancelled_id,json!({"id":Id::new_v4(),"command":{"type":"add_comment","id":Id::new_v4(),"item_id":src.item,"body":"取消后源继续","parent_id":null,"mentions":[],"attachments":[]}})).await.0,StatusCode::OK);
    assert!(abandoned
        .activate_transfer(src.owner, cancelled.plan.prepared_at + 1, &proof)
        .is_err());
    assert_eq!(
        abandoned.transfer_status(src.owner).unwrap().unwrap().phase,
        TransferPhase::Staged
    );
    assert!(abandoned.execute(src.request).is_err());
}

#[tokio::test]
async fn git_source_hands_original_history_to_real_service_accounts_after_member_confirmation() {
    let f = F::new().await;
    let admin = f.admin().await;
    let admin_id = me(&f, &admin).await;
    let (viewer, viewer_token) = ready(&f, &admin, "git_handoff_viewer").await;
    let src = source(None);
    let (git, _) = GitWorkspace::import_copy(
        &src.path,
        f.files.with_extension("git-source"),
        Id::new_v4(),
    )
    .unwrap();
    let before = git.snapshot(src.owner).unwrap().state;
    let frozen = git
        .prepare_transfer(
            src.owner,
            Id::new_v4(),
            200,
            Mode::Service,
            f.files.with_extension("private-git-handoff-backup"),
        )
        .unwrap();
    assert!(git
        .export_transfer(src.owner, f.files.with_extension("unconfirmed-git-export"))
        .is_err());
    git.acknowledge_transfer(
        src.owner,
        201,
        frozen.plan.id,
        "本人全部副本已同步并停止业务写入",
    )
    .unwrap();
    let package = f.files.with_extension("git-service-handoff");
    git.export_transfer(src.owner, &package).unwrap();
    assert!(f
        .server
        .stage_transfer(
            &viewer_token,
            &package,
            BTreeMap::from([(src.owner, admin_id), (src.viewer, viewer)])
        )
        .await
        .is_err());
    let (report, receipt) = f
        .server
        .stage_transfer(
            &admin,
            &package,
            BTreeMap::from([(src.owner, admin_id), (src.viewer, viewer)]),
        )
        .await
        .unwrap();
    let id = report.target.unwrap().id;
    assert_eq!(id, before.id);
    assert_eq!(
        snapshot(&f, &admin, id).await["transfer"]["phase"],
        "staged"
    );
    assert_eq!(command(&f,&admin,id,json!({"id":Id::new_v4(),"command":{"type":"add_comment","id":Id::new_v4(),"item_id":src.item,"body":"提前业务不可写","parent_id":null,"mentions":[],"attachments":[]}})).await.0,StatusCode::CONFLICT);
    let proof = git.complete_transfer(src.owner, 202, &receipt).unwrap();
    assert!(f
        .server
        .activate_transfer(&viewer_token, id, &proof)
        .await
        .is_err());
    f.server
        .activate_transfer(&admin, id, &proof)
        .await
        .unwrap();
    let active = snapshot(&f, &admin, id).await;
    assert_eq!(active["state"], serde_json::to_value(&before).unwrap());
    assert_eq!(active["actor_id"], json!(src.owner));
    assert_eq!(
        attachment(&f, &viewer_token, id, src.attachment).await,
        b"mapped real evidence"
    );
    assert_eq!(
        command(
            &f,
            &admin,
            id,
            json!({"id":src.request.id,"command":src.request.command})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(command(&f,&admin,id,json!({"id":Id::new_v4(),"command":{"type":"add_comment","id":Id::new_v4(),"item_id":src.item,"body":"Git 交接后真实账户继续","parent_id":null,"mentions":[src.viewer],"attachments":[src.attachment]}})).await.0,StatusCode::OK);
    assert!(git.execute(src.request).is_err());
    assert_eq!(
        git.transfer_status(src.owner).unwrap().unwrap().phase,
        TransferPhase::Completed
    );
    let backup = f.files.with_extension("git-to-service-active-backup");
    f.server.backup(&admin, &backup).await.unwrap();
    let restored = F::new().await;
    Server::restore(&backup, &restored.url, &restored.files)
        .await
        .unwrap();
    let restored_token = restored.login("owner", PASSWORD).await;
    assert_eq!(
        snapshot(&restored, &restored_token, id).await["state"],
        snapshot(&f, &admin, id).await["state"]
    );
}
