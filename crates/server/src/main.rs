use farflow_core::Id;
use farflow_server::Server;
use std::{
    env,
    error::Error,
    ffi::OsString,
    net::SocketAddr,
    path::{Path, PathBuf},
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let arguments: Vec<_> = env::args_os().skip(1).collect();
    if arguments == ["--help"] {
        println!("FarFlow 服务：serve 启动；init-admin 初始化首位管理员。\nbackup <新目录> 导出完整备份，需要 FARFLOW_BACKUP_TOKEN 管理员会话。\nrestore <备份目录> 恢复到空数据库和独立数据目录；中断后用同一备份重试。\nimport-copy <交换目录> <身份映射JSON文件> <新空间ID> 复制到服务。\nexport-copy <空间ID> <新交换目录> 从服务导出；复制命令需要 FARFLOW_SESSION_TOKEN。\n需要 DATABASE_URL；监听地址可设 FARFLOW_LISTEN，默认 127.0.0.1:4640。\n附件目录可设 FARFLOW_DATA_DIR，默认 .runtime/service。\nFARFLOW_WEB_DIR 指向网页 dist/client 构建目录时，同一服务提供网页和 API。\n初始化时另设 FARFLOW_ADMIN_ACCOUNT、FARFLOW_ADMIN_NAME、FARFLOW_ADMIN_PASSWORD。");
        return Ok(());
    }
    let copy = copy_command(&arguments)?;
    let (mode, path) = if copy.is_some() {
        ("copy", None)
    } else {
        command(&arguments)?
    };
    let database = env::var("DATABASE_URL").map_err(|_| "缺少 DATABASE_URL")?;
    let data_dir = std::path::PathBuf::from(
        env::var_os("FARFLOW_DATA_DIR").unwrap_or(".runtime/service".into()),
    );
    if mode == "restore" {
        let result =
            Server::restore(path.expect("恢复命令已验证目录"), &database, &data_dir).await?;
        if result.already_completed {
            println!("该备份已完成恢复，未改动现有数据：{}", result.backup_id);
        } else {
            println!("服务恢复完成，请重新登录：{}", result.backup_id);
        }
        return Ok(());
    }
    let server = Server::connect(&database, &data_dir).await?;
    if let Some(copy) = copy {
        let token = env::var("FARFLOW_SESSION_TOKEN").map_err(|_| "缺少 FARFLOW_SESSION_TOKEN")?;
        let report = match copy {
            CopyAction::Import {
                source,
                mappings,
                id,
            } => {
                let mappings =
                    serde_json::from_slice(&farflow_core::read_file(&mappings, 16 * 1024 * 1024)?)
                        .map_err(|_| "身份映射文件格式无效")?;
                server.import_copy(&token, source, id, mappings).await?
            }
            CopyAction::Export { id, destination } => {
                server.export_copy(&token, id, destination).await?
            }
        };
        println!(
            "工作空间复制核对完成：\n{}",
            serde_json::to_string_pretty(&report)?
        );
        return Ok(());
    }
    if mode == "backup" {
        let token = env::var("FARFLOW_BACKUP_TOKEN").map_err(|_| "缺少 FARFLOW_BACKUP_TOKEN")?;
        let id = server
            .backup(&token, path.expect("备份命令已验证目录"))
            .await?;
        println!("服务备份完成：{id}");
        return Ok(());
    }
    if mode == "init-admin" {
        server
            .bootstrap(
                env::var("FARFLOW_ADMIN_ACCOUNT").map_err(|_| "缺少 FARFLOW_ADMIN_ACCOUNT")?,
                env::var("FARFLOW_ADMIN_NAME").map_err(|_| "缺少 FARFLOW_ADMIN_NAME")?,
                env::var("FARFLOW_ADMIN_PASSWORD").map_err(|_| "缺少 FARFLOW_ADMIN_PASSWORD")?,
            )
            .await?;
        println!("首位服务管理员已创建。");
        return Ok(());
    }
    let router = match env::var_os("FARFLOW_WEB_DIR") {
        Some(path) => server.router_with_web(Path::new(&path))?,
        None => server.router(),
    };
    let configured = env::var("FARFLOW_LISTEN");
    let address = listen_address(configured.as_deref().ok())?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    println!("FarFlow 服务已启动：{}", listener.local_addr()?);
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum CopyAction {
    Import {
        source: PathBuf,
        mappings: PathBuf,
        id: Id,
    },
    Export {
        id: Id,
        destination: PathBuf,
    },
}
fn copy_command(arguments: &[OsString]) -> Result<Option<CopyAction>, &'static str> {
    let mode = arguments.first().and_then(|v| v.to_str());
    let parse = |value: &OsString| {
        value
            .to_str()
            .and_then(|s| Id::parse_str(s).ok())
            .filter(|id| !id.is_nil())
            .ok_or("空间标识需为非空 UUID")
    };
    match mode {
        Some("import-copy")
            if arguments.len() == 4 && !arguments[1].is_empty() && !arguments[2].is_empty() =>
        {
            Ok(Some(CopyAction::Import {
                source: arguments[1].clone().into(),
                mappings: arguments[2].clone().into(),
                id: parse(&arguments[3])?,
            }))
        }
        Some("export-copy") if arguments.len() == 3 && !arguments[2].is_empty() => {
            Ok(Some(CopyAction::Export {
                id: parse(&arguments[1])?,
                destination: arguments[2].clone().into(),
            }))
        }
        Some("import-copy" | "export-copy") => Err("复制命令参数不足或多余，使用 --help 查看用法"),
        _ => Ok(None),
    }
}

fn command(arguments: &[OsString]) -> Result<(&str, Option<&Path>), &'static str> {
    let mode = arguments
        .first()
        .map(|a| a.to_str())
        .unwrap_or(Some("serve"))
        .ok_or("命令名称无效")?;
    match (mode, arguments.len()) {
        ("serve" | "init-admin", 0..=1) => Ok((mode, None)),
        ("backup" | "restore", 2) if !arguments[1].is_empty() => {
            Ok((mode, Some(Path::new(&arguments[1]))))
        }
        _ => Err("参数无效，使用 --help 查看用法"),
    }
}

fn listen_address(value: Option<&str>) -> Result<SocketAddr, &'static str> {
    value
        .unwrap_or("127.0.0.1:4640")
        .parse()
        .map_err(|_| "FARFLOW_LISTEN 地址无效")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn copy_commands_validate_ids_paths_and_argument_counts() {
        let id = Id::new_v4();
        assert_eq!(copy_command(&[]), Ok(None));
        assert_eq!(copy_command(&["serve".into()]), Ok(None));
        assert_eq!(
            copy_command(&[
                "import-copy".into(),
                "来源".into(),
                "映射.json".into(),
                id.to_string().into()
            ]),
            Ok(Some(CopyAction::Import {
                source: "来源".into(),
                mappings: "映射.json".into(),
                id
            }))
        );
        assert_eq!(
            copy_command(&["export-copy".into(), id.to_string().into(), "目标".into()]),
            Ok(Some(CopyAction::Export {
                id,
                destination: "目标".into()
            }))
        );
        for args in [
            vec!["import-copy"],
            vec!["export-copy"],
            vec!["export-copy", "bad", "目标"],
            vec![
                "export-copy",
                "00000000-0000-0000-0000-000000000000",
                "目标",
            ],
            vec!["import-copy", "", "映射", "bad"],
            vec!["export-copy", "bad", "目标", "多余"],
        ] {
            assert!(copy_command(&args.into_iter().map(Into::into).collect::<Vec<_>>()).is_err());
        }
    }

    #[test]
    fn commands_require_an_explicit_directory_for_backup_and_restore() {
        assert_eq!(command(&[]), Ok(("serve", None)));
        for mode in ["serve", "init-admin"] {
            assert_eq!(command(&[mode.into()]), Ok((mode, None)));
            assert!(command(&[mode.into(), "extra".into()]).is_err());
        }
        for mode in ["backup", "restore"] {
            assert!(command(&[mode.into()]).is_err());
            assert!(command(&[mode.into(), "".into()]).is_err());
            assert_eq!(
                command(&[mode.into(), "中文目录".into()]),
                Ok((mode, Some(Path::new("中文目录"))))
            );
        }
        assert!(command(&["unknown".into()]).is_err());
    }

    #[test]
    fn default_listener_stays_on_loopback_and_explicit_addresses_are_validated() {
        let default = listen_address(None).unwrap();
        assert!(default.ip().is_loopback());
        assert_eq!(default.port(), 4640);
        assert_eq!(listen_address(Some("127.0.0.1:0")).unwrap().port(), 0);
        assert!(listen_address(Some("[::1]:4640"))
            .unwrap()
            .ip()
            .is_loopback());
        for invalid in ["", "example.com:4640", "0.0.0.0:99999", "not-an-address"] {
            assert!(listen_address(Some(invalid)).is_err());
        }
    }
}
