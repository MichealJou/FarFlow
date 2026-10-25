use super::*;
use farflow_core::{
    prepare_transfer_activation_output, prepare_transfer_receipt_output, read_transfer_document,
    save_transfer_document, GitTransferView, MigrationArchive, MigrationReport, TransferActivation,
    TransferReceipt,
};

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TransferAction {
    Prepare {
        id: Id,
        target: Mode,
        directory: Id,
        folder: String,
    },
    Export {
        directory: Id,
        folder: String,
    },
    Complete {
        receipt: Id,
        directory: Id,
        file: String,
    },
    Activate {
        activation: Id,
    },
    Cancel {
        id: Id,
    },
    Acknowledge {
        id: Id,
        note: String,
        all_copies_stopped: bool,
    },
}

#[derive(Serialize)]
pub struct TransferResult {
    pub status: Option<TransferStatus>,
    pub events: Vec<TransferStatus>,
    pub plans: Vec<GitTransferView>,
    pub report: Option<MigrationReport>,
    pub path: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferStage {
    pub source: Id,
    pub directory: Id,
    pub folder: String,
    pub actor: Id,
    pub receipt_directory: Id,
    pub receipt_file: String,
}

#[derive(Serialize)]
pub struct TransferStaged {
    pub opened: OpenedWorkspace,
    pub report: MigrationReport,
    pub receipt_path: PathBuf,
}

impl Desktop {
    pub fn transfer_view(&self, handle: Id) -> Result<TransferResult> {
        let binding = self.binding(handle)?;
        let snapshot = read_snapshot(binding)?;
        let actor = snapshot.actor_id;
        let (events, plans) = match checked_backend(binding)? {
            Backend::Local(workspace) => {
                let owner = snapshot.state.members[&actor].roles.contains(&Role::Owner);
                (
                    if owner {
                        workspace.transfer_events(actor)?
                    } else {
                        vec![]
                    },
                    vec![],
                )
            }
            Backend::Git(workspace) => (vec![], workspace.source_transfers(actor)?),
        };
        Ok(TransferResult {
            status: snapshot.transfer,
            events,
            plans,
            report: None,
            path: None,
        })
    }

    pub fn transfer(&self, handle: Id, action: TransferAction, at: i64) -> Result<TransferResult> {
        let binding = self.binding(handle)?;
        let actor = actor(binding)?;
        let mut backend = checked_backend(binding)?;
        let mut path = None;
        let mut report = None;
        match action {
            TransferAction::Prepare {
                id,
                target,
                directory,
                folder,
            } => {
                if target == binding.workspace.mode {
                    return Err(Error::Validation("正式迁移的源和目标模式必须不同".into()));
                }
                let backup = self.transfer_path(directory, &folder)?;
                match &mut backend {
                    Backend::Local(workspace) => {
                        workspace.prepare_transfer(actor, id, at, target, &backup)?;
                    }
                    Backend::Git(workspace) => {
                        workspace.prepare_transfer(actor, id, at, target, &backup)?;
                    }
                }
                path = Some(backup);
            }
            TransferAction::Export { directory, folder } => {
                let destination = self.transfer_path(directory, &folder)?;
                report = Some(match &backend {
                    Backend::Local(workspace) => workspace.export_transfer(actor, &destination)?,
                    Backend::Git(workspace) => workspace.export_transfer(actor, &destination)?,
                });
                path = Some(destination);
            }
            TransferAction::Complete {
                receipt,
                directory,
                file,
            } => {
                let source = self.pack_source(receipt)?;
                let receipt: TransferReceipt = read_transfer_document(source.path()?, 1024 * 1024)?;
                let destination = self.transfer_path(directory, &file)?;
                let status = match &backend {
                    Backend::Local(workspace) => workspace.transfer_status(actor)?,
                    Backend::Git(workspace) => workspace.transfer_status(actor)?,
                };
                let output = prepare_transfer_activation_output(
                    &destination,
                    &binding.path,
                    status,
                    &receipt,
                )?;
                let proof = match &mut backend {
                    Backend::Local(workspace) => {
                        workspace.complete_transfer(actor, at, &receipt)?
                    }
                    Backend::Git(workspace) => workspace.complete_transfer(actor, at, &receipt)?,
                };
                path = Some(save_transfer_document(output, &proof).map_err(|_| {
                    Error::Storage(
                        "源已确认完成，凭据保存失败；源保持只读，请用同一回执重试到新文件".into(),
                    )
                })?);
            }
            TransferAction::Activate { activation } => {
                let source = self.pack_source(activation)?;
                let proof: TransferActivation =
                    read_transfer_document(source.path()?, 1024 * 1024)?;
                match &mut backend {
                    Backend::Local(workspace) => {
                        workspace.activate_transfer(actor, at, &proof)?;
                    }
                    Backend::Git(workspace) => {
                        workspace.activate_transfer(actor, at, &proof)?;
                    }
                }
            }
            TransferAction::Cancel { id } => match &mut backend {
                Backend::Local(workspace) => {
                    workspace.cancel_transfer(actor, at, id)?;
                }
                Backend::Git(workspace) => {
                    workspace.cancel_transfer(actor, at, id)?;
                }
            },
            TransferAction::Acknowledge {
                id,
                note,
                all_copies_stopped,
            } => {
                if !all_copies_stopped {
                    return Err(Error::Validation(
                        "请明确确认本人全部 Git 副本已同步并停止写入".into(),
                    ));
                }
                let Backend::Git(workspace) = &backend else {
                    return Err(Error::Validation("只有 Git 源需要副本停写确认".into()));
                };
                workspace.acknowledge_transfer(actor, at, id, &note)?;
            }
        }
        let mut result = self.transfer_view(handle).map_err(|error| {
            Error::Storage(format!(
                "迁移动作已保存，重新读取阶段失败：{error}；请刷新核对，不要自动恢复旧源写入"
            ))
        })?;
        result.path = path;
        result.report = report;
        Ok(result)
    }

    pub fn inspect_transfer(&self, directory: Id) -> Result<MigrationReport> {
        let path = self.transfer_directory(directory)?;
        let archive = MigrationArchive::open(path)?;
        if archive.transfer_plan().is_none() {
            return Err(Error::Validation(
                "请选择正式迁移交换包目录，普通复制包不能用于交接".into(),
            ));
        }
        archive.report()
    }

    pub fn stage_transfer(&mut self, input: TransferStage, at: i64) -> Result<TransferStaged> {
        let source = self.transfer_directory(input.source)?.to_path_buf();
        let report = MigrationArchive::open(&source)?.report()?;
        let mode = report
            .transfer
            .as_ref()
            .ok_or_else(|| Error::Validation("普通复制包不能用于正式迁移".into()))?
            .target_mode;
        if mode == Mode::Service {
            return Err(Error::Validation(
                "此交换包的目标是团队服务，请在对应服务接收".into(),
            ));
        }
        let destination = self.transfer_path(input.directory, &input.folder)?;
        let receipt_path = self.transfer_path(input.receipt_directory, &input.receipt_file)?;
        let output = prepare_transfer_receipt_output(&receipt_path, &destination, &source)?;
        let (report, receipt) = match mode {
            Mode::Local => {
                let (_, report, receipt) =
                    LocalWorkspace::stage_transfer(source, &destination, input.actor, at)?;
                (report, receipt)
            }
            Mode::Git => {
                let (_, report, receipt) =
                    GitWorkspace::stage_transfer(source, &destination, input.actor, at)?;
                (report, receipt)
            }
            Mode::Service => unreachable!("服务目标已拒绝"),
        };
        let receipt_path = save_transfer_document(output, &receipt).map_err(|_| {
            Error::Storage(
                "目标已暂存，回执保存失败；目标仍只读，请用同一包和身份重试到新文件".into(),
            )
        })?;
        Ok(TransferStaged {
            opened: self.bind(&destination, mode)?,
            report,
            receipt_path,
        })
    }

    fn transfer_directory(&self, directory: Id) -> Result<&Path> {
        let path = self.directory(directory)?;
        farflow_core::regular_directory(path)?;
        if std::fs::canonicalize(path).map_err(storage)? != path {
            return Err(Error::Conflict("迁移目录已变化，请重新选择".into()));
        }
        Ok(path)
    }

    fn transfer_path(&self, directory: Id, name: &str) -> Result<PathBuf> {
        validate_folder(name)?;
        Ok(self.transfer_directory(directory)?.join(name))
    }
}
