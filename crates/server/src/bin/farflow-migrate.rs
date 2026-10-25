use farflow_core::{
    prepare_transfer_activation_output, prepare_transfer_receipt_output, read_transfer_document,
    save_transfer_document, Error, GitWorkspace, Id, LocalWorkspace, MigrationArchive, Mode,
    Result, TransferActivation, TransferReceipt,
};
use farflow_server::Server;
use serde_json::{json, Value};
use std::{
    env,
    ffi::OsString,
    path::{Path, PathBuf},
    process::ExitCode,
    time::{SystemTime, UNIX_EPOCH},
};

const HELP: &str = "FarFlow 正式迁移工具。各阶段可查询和重试，不自动取消或激活。\n\
new-id：生成一个迁移UUID。\n\
local|git identities <空间目录>：列出成员UUID、角色和启用状态，随后明确选择实际操作者。\n\
inspect <交换包目录>：核对完整历史、成员和附件。\n\
local|git <操作> <空间目录> <实际成员UUID> [参数]\n\
service <操作> <空间UUID> [参数]\n\
操作：\n\
  status：当前阶段。events：本地／服务阶段事件，Git 全部源计划与成员确认。\n\
  prepare <迁移UUID> <目标模式 local|git|service> <新备份目录>：先备份再冻结。\n\
  export <新交换包目录>：导出固定切点，Git 必须先收齐成员确认。\n\
  complete <回执JSON文件> <凭据JSON文件>：源永久只读，凭据只写本机文件。\n\
  activate <凭据JSON文件>：核对源完成凭据后开放目标写入。\n\
  cancel <迁移UUID>：源完成前取消。完成后不能取消。\n\
git acknowledge <空间目录> <实际成员UUID> <迁移UUID> <说明UTF-8文件> --all-copies-stopped\n\
  该标记表示本人全部副本已同步并停写，不能替其他成员确认。\n\
local|git stage <交换包目录> <目标空间目录> <实际所有者UUID> <回执JSON文件>\n\
service stage <交换包目录> <身份映射JSON文件> <回执JSON文件>\n\
git restore-origin <私有恢复目录> <目标空间目录> <实际所有者UUID> --source-offline\n\
  恢复前确认原发起副本已离线且未在另一处取消或完成。\n\
服务操作需要 DATABASE_URL、FARFLOW_DATA_DIR、FARFLOW_SESSION_TOKEN。\n\
身份映射为完整历史成员UUID到真实服务账户UUID的JSON对象。\n\
本地和Git无需数据库服务。输出回执／凭据须在业务目录外，禁止覆盖不同内容。\n\
凭据文件Unix权限0600；不要放入Git或公开日志。源完成后文件交付失败，可用同一回执重取。";

#[derive(Debug, PartialEq, Eq)]
enum Action {
    Status,
    Events,
    Prepare {
        id: Id,
        target: Mode,
        backup: PathBuf,
    },
    Export(PathBuf),
    Complete {
        receipt: PathBuf,
        activation: PathBuf,
    },
    Activate(PathBuf),
    Cancel(Id),
    Acknowledge {
        id: Id,
        note: PathBuf,
    },
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    NewId,
    Identities {
        mode: Mode,
        path: PathBuf,
    },
    Inspect(PathBuf),
    Local {
        path: PathBuf,
        actor: Id,
        action: Action,
    },
    Git {
        path: PathBuf,
        actor: Id,
        action: Action,
    },
    Service {
        workspace: Id,
        action: Action,
    },
    Stage {
        mode: Mode,
        source: PathBuf,
        destination: PathBuf,
        actor: Option<Id>,
        receipt: PathBuf,
    },
    Restore {
        source: PathBuf,
        destination: PathBuf,
        actor: Id,
    },
}

fn invalid(message: &str) -> Error {
    Error::Validation(message.into())
}

fn uuid(value: &OsString) -> Result<Id> {
    value
        .to_str()
        .and_then(|s| Id::parse_str(s).ok())
        .filter(|id| !id.is_nil())
        .ok_or_else(|| invalid("标识必须是非空UUID"))
}

fn path(value: &OsString) -> Result<PathBuf> {
    if value.is_empty() {
        return Err(invalid("目录或文件路径不能为空"));
    }
    Ok(value.into())
}

fn mode(value: &OsString) -> Result<Mode> {
    match value.to_str() {
        Some("local") => Ok(Mode::Local),
        Some("git") => Ok(Mode::Git),
        Some("service") => Ok(Mode::Service),
        _ => Err(invalid("模式必须是local、git或service")),
    }
}

fn parse(arguments: &[OsString]) -> Result<Command> {
    if arguments == ["new-id"] {
        return Ok(Command::NewId);
    }
    if let [name, source] = arguments {
        if name == "inspect" {
            return Ok(Command::Inspect(path(source)?));
        }
    }
    let [kind, operation, remaining @ ..] = arguments else {
        return Err(invalid("参数不足，使用--help查看用法"));
    };
    let selected = mode(kind)?;
    if operation == "identities" && selected != Mode::Service {
        if let [location] = remaining {
            return Ok(Command::Identities {
                mode: selected,
                path: path(location)?,
            });
        }
        return Err(invalid("成员查询只接受一个空间目录"));
    }
    if operation == "stage" {
        let (source, destination, actor, receipt) = match (selected, remaining) {
            (Mode::Service, [source, mappings, receipt]) => {
                (path(source)?, path(mappings)?, None, path(receipt)?)
            }
            (Mode::Local | Mode::Git, [source, destination, actor, receipt]) => (
                path(source)?,
                path(destination)?,
                Some(uuid(actor)?),
                path(receipt)?,
            ),
            _ => return Err(invalid("暂存参数不足或多余，使用--help查看用法")),
        };
        return Ok(Command::Stage {
            mode: selected,
            source,
            destination,
            actor,
            receipt,
        });
    }
    if selected == Mode::Git && operation == "restore-origin" {
        if let [source, destination, actor, confirmation] = remaining {
            if confirmation == "--source-offline" {
                return Ok(Command::Restore {
                    source: path(source)?,
                    destination: path(destination)?,
                    actor: uuid(actor)?,
                });
            }
        }
        return Err(invalid("恢复发起副本需要目录、成员和--source-offline声明"));
    }
    let (location, actor, values) = match (selected, remaining) {
        (Mode::Service, [location, values @ ..]) => (location, None, values),
        (Mode::Local | Mode::Git, [location, actor, values @ ..]) => {
            (location, Some(uuid(actor)?), values)
        }
        _ => return Err(invalid("缺少空间或实际成员标识")),
    };
    let action = match (operation.to_str(), values) {
        (Some("status"), []) => Action::Status,
        (Some("events"), []) => Action::Events,
        (Some("prepare"), [id, target, backup]) => {
            let target = mode(target)?;
            if target == selected {
                return Err(invalid("正式迁移的源和目标模式必须不同"));
            }
            Action::Prepare {
                id: uuid(id)?,
                target,
                backup: path(backup)?,
            }
        }
        (Some("export"), [destination]) => Action::Export(path(destination)?),
        (Some("complete"), [receipt, activation]) => Action::Complete {
            receipt: path(receipt)?,
            activation: path(activation)?,
        },
        (Some("activate"), [activation]) => Action::Activate(path(activation)?),
        (Some("cancel"), [id]) => Action::Cancel(uuid(id)?),
        (Some("acknowledge"), [id, note, confirmation])
            if selected == Mode::Git && confirmation == "--all-copies-stopped" =>
        {
            Action::Acknowledge {
                id: uuid(id)?,
                note: path(note)?,
            }
        }
        _ => return Err(invalid("操作或参数无效，使用--help查看用法")),
    };
    Ok(match selected {
        Mode::Local => Command::Local {
            path: path(location)?,
            actor: actor.expect("本地参数已核对"),
            action,
        },
        Mode::Git => Command::Git {
            path: path(location)?,
            actor: actor.expect("Git参数已核对"),
            action,
        },
        Mode::Service => Command::Service {
            workspace: uuid(location)?,
            action,
        },
    })
}

fn now() -> Result<i64> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| invalid("系统时间早于Unix纪元"))?
        .as_secs();
    i64::try_from(seconds).map_err(|_| invalid("系统时间超出范围"))
}

fn local(path: &Path, actor: Id, action: Action) -> Result<Value> {
    let mut workspace = LocalWorkspace::open(path)?;
    let at = now()?;
    Ok(match action {
        Action::Status => json!({"status":workspace.transfer_status(actor)?}),
        Action::Events => json!({"events":workspace.transfer_events(actor)?}),
        Action::Prepare { id, target, backup } => {
            json!({"status":workspace.prepare_transfer(actor,id,at,target,backup)?})
        }
        Action::Export(destination) => {
            json!({"report":workspace.export_transfer(actor,destination)?})
        }
        Action::Complete {
            receipt,
            activation,
        } => {
            let receipt: TransferReceipt = read_transfer_document(&receipt, 1024 * 1024)?;
            let output = prepare_transfer_activation_output(
                &activation,
                path,
                workspace.transfer_status(actor)?,
                &receipt,
            )?;
            let proof = workspace.complete_transfer(actor, at, &receipt)?;
            let file = save_transfer_document(output, &proof).map_err(|_| {
                Error::Storage(
                    "源已确认完成，凭据文件保存失败；源保持只读，请用同一回执重试到新文件".into(),
                )
            })?;
            json!({"status":workspace.transfer_status(actor)?,"activation_file":file})
        }
        Action::Activate(file) => {
            json!({"status":workspace.activate_transfer(actor,at,&read_transfer_document::<TransferActivation>(&file,1024*1024)?)?})
        }
        Action::Cancel(id) => json!({"status":workspace.cancel_transfer(actor,at,id)?}),
        Action::Acknowledge { .. } => return Err(invalid("只有Git源需要副本停写确认")),
    })
}

fn git(path: &Path, actor: Id, action: Action) -> Result<Value> {
    let workspace = GitWorkspace::open(path)?;
    let at = now()?;
    Ok(match action {
        Action::Status => json!({"status":workspace.transfer_status(actor)?}),
        Action::Events => json!({"plans":workspace.source_transfers(actor)?}),
        Action::Prepare { id, target, backup } => {
            json!({"status":workspace.prepare_transfer(actor,id,at,target,backup)?})
        }
        Action::Export(destination) => {
            json!({"report":workspace.export_transfer(actor,destination)?})
        }
        Action::Complete {
            receipt,
            activation,
        } => {
            let receipt: TransferReceipt = read_transfer_document(&receipt, 1024 * 1024)?;
            let output = prepare_transfer_activation_output(
                &activation,
                path,
                workspace.transfer_status(actor)?,
                &receipt,
            )?;
            let proof = workspace.complete_transfer(actor, at, &receipt)?;
            let file = save_transfer_document(output, &proof).map_err(|_| {
                Error::Storage(
                    "Git源已确认完成，凭据文件保存失败；请用同一回执重取，不要恢复旧源写入".into(),
                )
            })?;
            json!({"status":workspace.transfer_status(actor)?,"activation_file":file})
        }
        Action::Activate(file) => {
            json!({"status":workspace.activate_transfer(actor,at,&read_transfer_document::<TransferActivation>(&file,1024*1024)?)?})
        }
        Action::Cancel(id) => json!({"status":workspace.cancel_transfer(actor,at,id)?}),
        Action::Acknowledge { id, note } => {
            let bytes = farflow_core::read_file(&note, 32 * 1024)?;
            let note = String::from_utf8(bytes).map_err(|_| invalid("确认说明必须是UTF-8文本"))?;
            json!({"plan":workspace.acknowledge_transfer(actor,at,id,&note)?})
        }
    })
}

async fn service() -> Result<(Server, String, PathBuf)> {
    let database = env::var("DATABASE_URL").map_err(|_| invalid("缺少DATABASE_URL"))?;
    let token =
        env::var("FARFLOW_SESSION_TOKEN").map_err(|_| invalid("缺少FARFLOW_SESSION_TOKEN"))?;
    let data = PathBuf::from(
        env::var_os("FARFLOW_DATA_DIR")
            .ok_or_else(|| invalid("缺少FARFLOW_DATA_DIR，请明确实际服务目录"))?,
    );
    let server = Server::connect(&database, &data).await.map_err(|_| {
        Error::Storage("服务连接或目录核对失败，请检查数据库配置和实际数据目录".into())
    })?;
    Ok((server, token, data))
}

async fn service_action(workspace: Id, action: Action) -> Result<Value> {
    let (server, token, data) = service().await?;
    Ok(match action {
        Action::Status => json!({"status":server.transfer_status(&token,workspace).await?}),
        Action::Events => json!({"events":server.transfer_events(&token,workspace).await?}),
        Action::Prepare { id, target, backup } => {
            json!({"status":server.prepare_transfer(&token,workspace,id,target,backup).await?})
        }
        Action::Export(destination) => {
            json!({"report":server.export_transfer(&token,workspace,destination).await?})
        }
        Action::Complete {
            receipt,
            activation,
        } => {
            let receipt: TransferReceipt = read_transfer_document(&receipt, 1024 * 1024)?;
            let output = prepare_transfer_activation_output(
                &activation,
                &data,
                server.transfer_status(&token, workspace).await?,
                &receipt,
            )?;
            let proof = server
                .complete_transfer(&token, workspace, &receipt)
                .await?;
            let file = save_transfer_document(output, &proof).map_err(|_| {
                Error::Storage(
                    "服务源已确认完成，凭据文件保存失败；请用同一回执重取，源保持只读".into(),
                )
            })?;
            json!({"status":server.transfer_status(&token,workspace).await?,"activation_file":file})
        }
        Action::Activate(file) => {
            json!({"status":server.activate_transfer(&token,workspace,&read_transfer_document::<TransferActivation>(&file,1024*1024)?).await?})
        }
        Action::Cancel(id) => json!({"status":server.cancel_transfer(&token,workspace,id).await?}),
        Action::Acknowledge { .. } => return Err(invalid("服务源不接受Git副本确认")),
    })
}

async fn execute(command: Command) -> Result<Value> {
    match command {
        Command::NewId => Ok(json!({"id":Id::new_v4()})),
        Command::Identities { mode, path } => {
            if mode == Mode::Local {
                let workspace = LocalWorkspace::open(path)?;
                Ok(json!({"workspace":workspace.info(),"members":workspace.identities()?}))
            } else {
                let workspace = GitWorkspace::open(path)?;
                Ok(json!({"workspace":workspace.info(),"members":workspace.identities()?}))
            }
        }
        Command::Inspect(source) => Ok(json!({"report":MigrationArchive::open(source)?.report()?})),
        Command::Local {
            path,
            actor,
            action,
        } => local(&path, actor, action),
        Command::Git {
            path,
            actor,
            action,
        } => git(&path, actor, action),
        Command::Service { workspace, action } => service_action(workspace, action).await,
        Command::Restore {
            source,
            destination,
            actor,
        } => {
            let workspace = GitWorkspace::restore_transfer(source, destination, actor)?;
            Ok(
                json!({"status":workspace.transfer_status(actor)?,"plans":workspace.source_transfers(actor)?}),
            )
        }
        Command::Stage {
            mode,
            source,
            destination,
            actor,
            receipt,
        } => {
            let (report, proof, output) = match mode {
                Mode::Local => {
                    let output = prepare_transfer_receipt_output(&receipt, &destination, &source)?;
                    let (_, report, proof) = LocalWorkspace::stage_transfer(
                        source,
                        &destination,
                        actor.expect("本地所有者已核对"),
                        now()?,
                    )?;
                    (report, proof, output)
                }
                Mode::Git => {
                    let output = prepare_transfer_receipt_output(&receipt, &destination, &source)?;
                    let (_, report, proof) = GitWorkspace::stage_transfer(
                        source,
                        &destination,
                        actor.expect("Git所有者已核对"),
                        now()?,
                    )?;
                    (report, proof, output)
                }
                Mode::Service => {
                    let (server, token, data) = service().await?;
                    let mappings = read_transfer_document(&destination, 16 * 1024 * 1024)?;
                    let output = prepare_transfer_receipt_output(&receipt, &data, &source)?;
                    let (report, proof) = server.stage_transfer(&token, source, mappings).await?;
                    (report, proof, output)
                }
            };
            let file = save_transfer_document(output, &proof).map_err(|_| {
                Error::Storage(
                    "目标已暂存，回执文件保存失败；目标仍只读，请用同一包和映射重试到新文件".into(),
                )
            })?;
            Ok(json!({"report":report,"receipt_file":file}))
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let arguments: Vec<_> = env::args_os().skip(1).collect();
    if arguments == ["--help"] {
        println!("{HELP}");
        return ExitCode::SUCCESS;
    }
    match parse(&arguments) {
        Ok(command) => {
            match execute(command).await {
                Ok(result) => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&result).expect("结果为JSON值")
                    );
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("迁移操作未完成：{error}。请查询实际阶段，修复后重试；不要自动恢复旧源写入。");
                    ExitCode::FAILURE
                }
            }
        }
        Err(error) => {
            eprintln!("迁移参数无效：{error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(values: &[&str]) -> Vec<OsString> {
        values.iter().map(Into::into).collect()
    }

    #[test]
    fn commands_keep_modes_ids_paths_and_explicit_git_declarations() {
        let id = Id::new_v4().to_string();
        assert_eq!(parse(&arguments(&["new-id"])).unwrap(), Command::NewId);
        for (name, mode) in [("local", Mode::Local), ("git", Mode::Git)] {
            assert_eq!(
                parse(&arguments(&[name, "identities", "空间"])).unwrap(),
                Command::Identities {
                    mode,
                    path: "空间".into()
                }
            );
        }
        assert_eq!(
            parse(&arguments(&["inspect", "中文交换目录"])).unwrap(),
            Command::Inspect("中文交换目录".into())
        );
        for selected in ["local", "git", "service"] {
            let mut base = vec![
                selected,
                "status",
                if selected == "service" {
                    &id
                } else {
                    "空间目录"
                },
            ];
            if selected != "service" {
                base.push(&id);
            }
            let parsed = parse(&arguments(&base)).unwrap();
            match parsed {
                Command::Local { action, .. }
                | Command::Git { action, .. }
                | Command::Service { action, .. } => assert_eq!(action, Action::Status),
                _ => panic!("应解析空间操作"),
            }
            for (operation, tail, expected) in [
                ("events", vec![], Action::Events),
                (
                    "prepare",
                    vec![
                        id.as_str(),
                        if selected == "git" { "local" } else { "git" },
                        "新备份",
                    ],
                    Action::Prepare {
                        id: Id::parse_str(&id).unwrap(),
                        target: if selected == "git" {
                            Mode::Local
                        } else {
                            Mode::Git
                        },
                        backup: "新备份".into(),
                    },
                ),
                ("export", vec!["交换包"], Action::Export("交换包".into())),
                (
                    "complete",
                    vec!["回执.json", "凭据.json"],
                    Action::Complete {
                        receipt: "回执.json".into(),
                        activation: "凭据.json".into(),
                    },
                ),
                (
                    "activate",
                    vec!["凭据.json"],
                    Action::Activate("凭据.json".into()),
                ),
                (
                    "cancel",
                    vec![id.as_str()],
                    Action::Cancel(Id::parse_str(&id).unwrap()),
                ),
            ] {
                let mut values = base.clone();
                values[1] = operation;
                values.extend(tail);
                match parse(&arguments(&values)).unwrap() {
                    Command::Local { action, .. }
                    | Command::Git { action, .. }
                    | Command::Service { action, .. } => assert_eq!(action, expected),
                    _ => panic!("应解析空间操作"),
                }
            }
        }
        let actor = Id::parse_str(&id).unwrap();
        assert_eq!(
            parse(&arguments(&["local", "stage", "包", "新空间", &id, "回执"])).unwrap(),
            Command::Stage {
                mode: Mode::Local,
                source: "包".into(),
                destination: "新空间".into(),
                actor: Some(actor),
                receipt: "回执".into()
            }
        );
        assert_eq!(
            parse(&arguments(&["git", "stage", "包", "新空间", &id, "回执"])).unwrap(),
            Command::Stage {
                mode: Mode::Git,
                source: "包".into(),
                destination: "新空间".into(),
                actor: Some(actor),
                receipt: "回执".into()
            }
        );
        assert_eq!(
            parse(&arguments(&["service", "stage", "包", "映射", "回执"])).unwrap(),
            Command::Stage {
                mode: Mode::Service,
                source: "包".into(),
                destination: "映射".into(),
                actor: None,
                receipt: "回执".into()
            }
        );
        assert_eq!(
            parse(&arguments(&[
                "git",
                "restore-origin",
                "私有恢复",
                "新空间",
                &id,
                "--source-offline"
            ]))
            .unwrap(),
            Command::Restore {
                source: "私有恢复".into(),
                destination: "新空间".into(),
                actor
            }
        );
        assert_eq!(
            parse(&arguments(&[
                "git",
                "acknowledge",
                "空间",
                &id,
                &id,
                "声明.txt",
                "--all-copies-stopped"
            ]))
            .unwrap(),
            Command::Git {
                path: "空间".into(),
                actor,
                action: Action::Acknowledge {
                    id: actor,
                    note: "声明.txt".into()
                }
            }
        );
    }

    #[test]
    fn malformed_commands_never_default_to_an_owner_or_silently_drop_arguments() {
        let id = Id::new_v4().to_string();
        for values in [
            vec![],
            vec!["inspect"],
            vec!["inspect", ""],
            vec!["local", "identities"],
            vec!["git", "identities", "空间", "多余"],
            vec!["unknown", "status"],
            vec!["local", "status"],
            vec!["service", "status"],
            vec!["service", "status", "错误ID"],
            vec![
                "git",
                "status",
                "目录",
                "00000000-0000-0000-0000-000000000000",
            ],
            vec!["local", "status", "", &id],
            vec!["git", "status", "空间", &id, "多余"],
            vec!["local", "prepare", "空间", &id, &id, "local", "备份"],
            vec!["service", "prepare", &id, &id, "service", "备份"],
            vec!["git", "prepare", "空间", &id, &id, "unknown", "备份"],
            vec!["local", "stage", "包", "目录", &id],
            vec!["service", "stage", "包", "映射", "回执", "多余"],
            vec!["git", "acknowledge", "空间", &id, &id, "声明.txt"],
            vec![
                "local",
                "acknowledge",
                "空间",
                &id,
                &id,
                "声明.txt",
                "--all-copies-stopped",
            ],
            vec!["git", "restore-origin", "备份", "目录", &id],
            vec!["git", "restore-origin", "备份", "目录", &id, "错误标记"],
        ] {
            assert!(parse(&arguments(&values)).is_err(), "{values:?}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            assert!(mode(&OsString::from_vec(vec![255])).is_err());
            assert!(uuid(&OsString::from_vec(vec![255])).is_err());
            assert!(parse(&[
                "git".into(),
                OsString::from_vec(vec![255]),
                "目录".into(),
                id.into()
            ])
            .is_err());
        }
        assert!(now().unwrap() > 0);
    }
}
