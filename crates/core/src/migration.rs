use crate::{
    repository::attachments,
    store::{
        digest, new_destination, owner, promote, read_blob, read_json, regular_directory,
        regular_file, sync_directory, valid_hash, write_blob, write_json,
    },
    Error, GitLog, Id, Member, Mode, Result, State, WorkspaceInfo,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

const MAX_METADATA: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationOrigin {
    pub package_id: Id,
    pub source_workspace_id: Id,
    pub target_workspace_id: Id,
    pub source_mode: Mode,
    pub heads: BTreeSet<Id>,
    pub source_checksum: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct History {
    pub log: GitLog,
    pub origins: Vec<MigrationOrigin>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub clients: BTreeMap<Id, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationReport {
    pub package_id: Id,
    pub source: WorkspaceInfo,
    pub target: Option<WorkspaceInfo>,
    pub revision: u64,
    pub operation_count: usize,
    pub item_count: usize,
    pub comment_count: usize,
    pub attachment_count: usize,
    pub blob_count: usize,
    pub members: Vec<Member>,
    pub identity_ids: BTreeSet<Id>,
    pub origins: Vec<MigrationOrigin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer: Option<crate::TransferPlan>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Package {
    schema: u32,
    pub id: Id,
    pub source_mode: Mode,
    pub history: History,
    files: BTreeMap<String, String>,
    checksum: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    transfer: Option<crate::TransferPlan>,
}

pub struct MigrationArchive {
    pub(crate) directory: tempfile::TempDir,
    pub(crate) package: Package,
}

impl MigrationArchive {
    pub fn open(source: impl AsRef<Path>) -> Result<Self> {
        prepare(source.as_ref())
    }
    pub fn report(&self) -> Result<MigrationReport> {
        self.package.report(None)
    }
    pub fn copied_history(&self, id: Id) -> Result<History> {
        if self.package.transfer.is_some() {
            return invalid("正式迁移包不能绕过交接按复制导入");
        }
        self.package
            .history
            .copied(id, self.package.id, self.package.source_mode)
    }
    pub fn transfer_plan(&self) -> Option<&crate::TransferPlan> {
        self.package.transfer.as_ref()
    }
    pub fn transfer_history(&self, mode: Mode) -> Result<History> {
        let Some(plan) = self.transfer_plan() else {
            return invalid("普通复制包没有正式迁移计划");
        };
        plan.validate(&self.package.history)?;
        if plan.target_mode != mode || heads(&self.package.history.log) != plan.heads {
            return invalid("目标模式或迁移切点不匹配");
        }
        Ok(self.package.history.clone())
    }
    pub fn write_blobs(&self, destination: impl AsRef<Path>) -> Result<()> {
        copy_blobs(
            self.directory.path(),
            destination.as_ref(),
            &self.package.history,
        )
    }
}

impl History {
    pub(crate) fn prefix(&self, frontier: &BTreeSet<Id>) -> Result<Self> {
        let mut operations = BTreeMap::new();
        let mut pending: Vec<_> = frontier.iter().copied().collect();
        while let Some(id) = pending.pop() {
            if operations.contains_key(&id) {
                continue;
            }
            let Some(operation) = self.log.operations().get(&id) else {
                return invalid("迁移切点缺少原始操作");
            };
            pending.extend(operation.parents.iter().copied());
            operations.insert(id, operation.clone());
        }
        let clients = self
            .clients
            .iter()
            .filter(|(id, _)| operations.contains_key(id))
            .map(|(id, data)| (*id, data.clone()))
            .collect();
        Ok(Self {
            log: GitLog::from_parts(self.log.genesis().clone(), operations)?,
            origins: self.origins.clone(),
            clients,
        })
    }
    pub fn attachments(&self) -> Vec<crate::Attachment> {
        attachments(&self.log).into_values().collect()
    }
    pub fn validate(&self) -> Result<State> {
        self.validate_lineage()?;
        self.validate_clients()?;
        let view = self.log.view()?;
        if !view.issues.is_empty() {
            return Err(Error::Conflict(
                "工作空间仍有同步冲突，请处理后再迁移".into(),
            ));
        }
        Ok(view.state)
    }
    fn validate_clients(&self) -> Result<()> {
        for (id, client) in &self.clients {
            let client: crate::ClientOperation = serde_json::from_str(client).map_err(storage)?;
            let Some(crate::GitOperation {
                change: crate::GitChange::Apply { request },
                ..
            }) = self.log.operations().get(id)
            else {
                return invalid("客户端来源没有对应的业务操作");
            };
            if client.id != *id {
                return invalid("客户端来源标识不一致");
            }
            client.verify_effective(request)?;
        }
        Ok(())
    }

    fn validate_lineage(&self) -> Result<()> {
        if self.origins.len() > 32 {
            return invalid("迁移来源链超过支持的长度");
        }
        let mut previous = None;
        let mut roots = BTreeSet::new();
        for (index, origin) in self.origins.iter().enumerate() {
            if origin.package_id.is_nil()
                || origin.source_workspace_id.is_nil()
                || origin.target_workspace_id.is_nil()
                || origin.source_workspace_id == origin.target_workspace_id
                || !valid_hash(&origin.source_checksum)
                || previous.is_some_and(|id| id != origin.source_workspace_id)
                || !roots.insert(origin.source_workspace_id)
            {
                return invalid("迁移来源链标识或顺序无效");
            }
            let mut subset = BTreeMap::new();
            let mut stack: Vec<_> = origin.heads.iter().copied().collect();
            while let Some(id) = stack.pop() {
                if subset.contains_key(&id) {
                    continue;
                }
                let mut operation = self
                    .log
                    .operations()
                    .get(&id)
                    .cloned()
                    .ok_or_else(|| Error::Validation("迁移来源缺少原始操作".into()))?;
                stack.extend(operation.parents.iter().copied());
                operation.workspace_id = origin.source_workspace_id;
                subset.insert(id, operation);
            }
            let mut genesis = self.log.genesis().clone();
            genesis.id = origin.source_workspace_id;
            let subset_ids: BTreeSet<_> = subset.keys().copied().collect();
            let source = History {
                log: GitLog::from_parts(genesis, subset)?,
                origins: self.origins[..index].to_vec(),
                clients: self
                    .clients
                    .iter()
                    .filter(|(id, _)| subset_ids.contains(id))
                    .map(|(id, data)| (*id, data.clone()))
                    .collect(),
            };
            if !source.log.view()?.issues.is_empty()
                || heads(&source.log) != origin.heads
                || checksum(&source)? != origin.source_checksum
            {
                return invalid("迁移来源原始历史校验失败");
            }
            previous = Some(origin.target_workspace_id);
        }
        if previous.is_some_and(|id| id != self.log.info().id)
            || roots.contains(&self.log.info().id)
        {
            return invalid("迁移目标与来源链不一致");
        }
        Ok(())
    }

    pub fn copied(&self, id: Id, package_id: Id, source_mode: Mode) -> Result<Self> {
        self.validate()?;
        if id.is_nil()
            || id == self.log.info().id
            || self
                .origins
                .iter()
                .any(|o| o.source_workspace_id == id || o.target_workspace_id == id)
        {
            return invalid("复制需要全新的工作空间标识");
        }
        let origin = MigrationOrigin {
            package_id,
            source_workspace_id: self.log.info().id,
            target_workspace_id: id,
            source_mode,
            heads: heads(&self.log),
            source_checksum: checksum(self)?,
        };
        let mut genesis = self.log.genesis().clone();
        genesis.id = id;
        let operations = self
            .log
            .operations()
            .iter()
            .map(|(key, value)| {
                let mut operation = value.clone();
                operation.workspace_id = id;
                (*key, operation)
            })
            .collect();
        let mut origins = self.origins.clone();
        origins.push(origin);
        let copied = Self {
            log: GitLog::from_parts(genesis, operations)?,
            origins,
            clients: self.clients.clone(),
        };
        copied.validate()?;
        Ok(copied)
    }
}

impl History {
    pub fn export_transfer(
        &self,
        plan: &crate::TransferPlan,
        actor: Id,
        root: impl AsRef<Path>,
        destination: impl AsRef<Path>,
    ) -> Result<MigrationReport> {
        plan.validate(self)?;
        if heads(&self.log) != plan.heads {
            return invalid("源空间在冻结切点之后发生了变化");
        }
        export_with_plan(
            self.clone(),
            plan.source_mode,
            actor,
            root.as_ref(),
            destination.as_ref(),
            Some(plan.clone()),
        )
    }
    pub fn export_copy(
        &self,
        mode: Mode,
        actor: Id,
        root: impl AsRef<Path>,
        destination: impl AsRef<Path>,
    ) -> Result<MigrationReport> {
        export(
            self.clone(),
            mode,
            actor,
            root.as_ref(),
            destination.as_ref(),
        )
    }
    pub fn member_ids(&self) -> BTreeSet<Id> {
        let mut ids: BTreeSet<_> = self.log.genesis().members.keys().copied().collect();
        let mut commands = vec![];
        for operation in self.log.operations().values() {
            match &operation.change {
                crate::GitChange::Apply { request } => {
                    ids.insert(request.actor_id);
                    commands.push(&request.command);
                }
                crate::GitChange::Resolve { resolution } => {
                    ids.insert(resolution.actor_id);
                    if let Some(command) = &resolution.command {
                        commands.push(command);
                    }
                }
            }
        }
        while let Some(command) = commands.pop() {
            match command {
                crate::Command::Batch { commands: batch } => commands.extend(batch),
                crate::Command::AddMember { member } | crate::Command::UpdateMember { member } => {
                    ids.insert(member.id);
                }
                _ => {}
            }
        }
        ids
    }
}

pub(crate) fn validate_origins(
    log: &GitLog,
    origins: &[MigrationOrigin],
    clients: &BTreeMap<Id, String>,
) -> Result<()> {
    if origins.is_empty() && clients.is_empty() {
        return Ok(());
    }
    let history = History {
        log: log.clone(),
        origins: origins.to_vec(),
        clients: clients.clone(),
    };
    history.validate_lineage()?;
    history.validate_clients()
}

pub(crate) fn heads(log: &GitLog) -> BTreeSet<Id> {
    let mut out: BTreeSet<_> = log.operations().keys().copied().collect();
    for operation in log.operations().values() {
        for parent in &operation.parents {
            out.remove(parent);
        }
    }
    out
}
pub(crate) fn checksum(value: &impl Serialize) -> Result<String> {
    Ok(digest(&serde_json::to_vec(value).map_err(storage)?))
}

impl Package {
    fn payload_checksum(&self) -> Result<String> {
        if let Some(plan) = &self.transfer {
            return checksum(&(
                self.schema,
                self.id,
                self.source_mode,
                &self.history,
                &self.files,
                plan,
            ));
        }
        checksum(&(
            self.schema,
            self.id,
            self.source_mode,
            &self.history,
            &self.files,
        ))
    }
    pub fn report(&self, target: Option<WorkspaceInfo>) -> Result<MigrationReport> {
        let state = self.history.validate()?;
        let mut source = self.history.log.info();
        source.mode = self.source_mode;
        Ok(MigrationReport {
            package_id: self.id,
            source,
            target,
            revision: state.revision,
            operation_count: self.history.log.operations().len(),
            item_count: state.items.len(),
            comment_count: state.comments.len(),
            attachment_count: state.attachments.len(),
            blob_count: attachments(&self.history.log).len(),
            members: state.members.values().cloned().collect(),
            identity_ids: self.history.member_ids(),
            origins: self.history.origins.clone(),
            transfer: self.transfer.clone(),
        })
    }
}

pub fn inspect_migration(source: impl AsRef<Path>) -> Result<MigrationReport> {
    prepare(source.as_ref())?.package.report(None)
}

pub(crate) fn export(
    history: History,
    mode: Mode,
    actor: Id,
    root: &Path,
    destination: &Path,
) -> Result<MigrationReport> {
    export_with_plan(history, mode, actor, root, destination, None)
}

fn export_with_plan(
    history: History,
    mode: Mode,
    actor: Id,
    root: &Path,
    destination: &Path,
    transfer: Option<crate::TransferPlan>,
) -> Result<MigrationReport> {
    let state = history.validate()?;
    owner(&state, actor)?;
    check_destination(root, destination)?;
    if transfer.is_some() && fs::symlink_metadata(destination).is_ok() {
        let existing = prepare(destination)?;
        if existing.package.transfer == transfer
            && checksum(&existing.package.history)? == checksum(&history)?
        {
            return existing.report();
        }
        return Err(Error::Conflict("目标已属于不同迁移包".into()));
    }
    let stage = new_destination(destination)?;
    fs::create_dir(stage.path().join("attachments")).map_err(storage)?;
    let mut files = BTreeMap::new();
    for attachment in attachments(&history.log).values() {
        let bytes = read_blob(root, attachment)?;
        write_blob(stage.path(), &attachment.sha256, &bytes)?;
        files.insert(blob_path(&attachment.sha256), attachment.sha256.clone());
    }
    let mut package = Package {
        schema: if transfer.is_some() { 2 } else { 1 },
        id: transfer.as_ref().map_or_else(Id::new_v4, |plan| plan.id),
        source_mode: mode,
        history,
        files,
        checksum: String::new(),
        transfer,
    };
    package.checksum = package.payload_checksum()?;
    write_json(&stage.path().join("migration.json"), &package)?;
    if fs::metadata(stage.path().join("migration.json"))
        .map_err(storage)?
        .len()
        > MAX_METADATA
        || package.files.len() > 100_000
    {
        return invalid("迁移包元数据或附件数量超过限制");
    }
    sync_directory(stage.path())?;
    promote(stage.path(), destination)?;
    package.report(None)
}

pub(crate) fn prepare(source: &Path) -> Result<MigrationArchive> {
    regular_directory(source)?;
    let package: Package = read_json(&source.join("migration.json"), MAX_METADATA)?;
    if !matches!(
        (package.schema, package.transfer.is_some()),
        (1, false) | (2, true)
    ) || package.id.is_nil()
        || package.files.len() > 100_000
        || package
            .files
            .iter()
            .any(|(p, h)| !valid_hash(h) || *p != blob_path(h))
    {
        return invalid("迁移包格式或附件路径无效");
    }
    if package.payload_checksum()? != package.checksum {
        return invalid("迁移包元数据校验失败");
    }
    package.history.validate()?;
    if let Some(plan) = &package.transfer {
        plan.validate(&package.history)?;
        if plan.id != package.id
            || plan.source_mode != package.source_mode
            || heads(&package.history.log) != plan.heads
        {
            return invalid("正式迁移包与冻结计划不一致");
        }
    }
    let needed: BTreeMap<_, _> = attachments(&package.history.log)
        .values()
        .map(|a| (blob_path(&a.sha256), a.sha256.clone()))
        .collect();
    if needed != package.files {
        return invalid("迁移包附件清单不完整");
    }
    let mut actual = BTreeSet::new();
    let mut stack = vec![(source.to_owned(), 0)];
    let mut entries = 0usize;
    while let Some((directory, depth)) = stack.pop() {
        regular_directory(&directory)?;
        if depth > 2 {
            return invalid("迁移包目录层次无效");
        }
        for entry in fs::read_dir(directory).map_err(storage)? {
            let entry = entry.map_err(storage)?;
            entries += 1;
            if entries > 200_002 {
                return invalid("迁移包文件数量超过限制");
            }
            if entry.file_type().map_err(storage)?.is_dir() {
                stack.push((entry.path(), depth + 1));
            } else {
                regular_file(&entry.path())?;
                let relative = entry
                    .path()
                    .strip_prefix(source)
                    .map_err(storage)?
                    .to_str()
                    .ok_or_else(|| Error::Validation("迁移包路径编码无效".into()))?
                    .replace('\\', "/");
                actual.insert(relative);
            }
        }
    }
    let mut expected: BTreeSet<_> = needed.keys().cloned().collect();
    expected.insert("migration.json".into());
    if actual != expected {
        return invalid("迁移包包含缺失或额外文件");
    }
    let mut builder = tempfile::Builder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(fs::Permissions::from_mode(0o700));
    }
    let directory = builder.tempdir().map_err(storage)?;
    fs::create_dir(directory.path().join("attachments")).map_err(storage)?;
    for attachment in attachments(&package.history.log).values() {
        write_blob(
            directory.path(),
            &attachment.sha256,
            &read_blob(source, attachment)?,
        )?;
    }
    Ok(MigrationArchive { directory, package })
}

pub(crate) fn copy_blobs(source: &Path, target: &Path, history: &History) -> Result<()> {
    for attachment in attachments(&history.log).values() {
        write_blob(target, &attachment.sha256, &read_blob(source, attachment)?)?;
    }
    Ok(())
}
pub(crate) fn check_destination(source: &Path, destination: &Path) -> Result<()> {
    let root = fs::canonicalize(source).map_err(storage)?;
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = fs::canonicalize(parent).map_err(storage)?;
    if parent.starts_with(root) {
        return invalid("复制目标或交换包必须放在源目录之外");
    }
    Ok(())
}
fn blob_path(hash: &str) -> String {
    format!("attachments/{}/{hash}", &hash[..2])
}
fn invalid<T>(message: &str) -> Result<T> {
    Err(Error::Validation(message.into()))
}
fn storage(error: impl std::fmt::Display) -> Error {
    Error::Storage(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Command, NewItem, Request, Role};
    use serde_json::json;

    fn history() -> (History, Member) {
        let owner = Member {
            id: Id::new_v4(),
            account: "owner".into(),
            name: "所有者".into(),
            department: "研发部".into(),
            roles: BTreeSet::from([Role::Owner]),
            active: true,
        };
        (
            History {
                log: GitLog::new(Id::new_v4(), "来源校验".into(), owner.clone()).unwrap(),
                origins: vec![],
                clients: BTreeMap::new(),
            },
            owner,
        )
    }

    #[test]
    fn empty_histories_copy_and_origin_identity_frontier_and_checksum_are_validated() {
        let (mut source, owner) = history();
        let empty = source
            .copied(Id::new_v4(), Id::new_v4(), Mode::Local)
            .unwrap();
        assert!(empty.validate().unwrap().items.is_empty());
        assert!(source.copied(Id::new_v4(), Id::nil(), Mode::Local).is_err());
        let item: NewItem =
            serde_json::from_value(json!({"id":Id::new_v4(),"kind":"document","title":"原始操作"}))
                .unwrap();
        source
            .log
            .execute(
                Id::new_v4(),
                Request {
                    id: Id::new_v4(),
                    actor_id: owner.id,
                    at: 1,
                    command: Command::Create { item },
                },
            )
            .unwrap();
        let copied = source
            .copied(Id::new_v4(), Id::new_v4(), Mode::Git)
            .unwrap();
        for change in 0..7 {
            let mut invalid = copied.clone();
            match change {
                0 => invalid.origins[0].package_id = Id::nil(),
                1 => invalid.origins[0].source_checksum = "0".repeat(64),
                2 => invalid.origins[0]
                    .heads
                    .insert(Id::new_v4())
                    .then_some(())
                    .unwrap(),
                3 => {
                    invalid.origins[0].source_workspace_id = invalid.origins[0].target_workspace_id
                }
                4 => invalid.origins[0].target_workspace_id = Id::new_v4(),
                5 => invalid.origins[0].source_checksum = "bad".into(),
                _ => invalid.origins = vec![invalid.origins[0].clone(); 33],
            }
            assert!(invalid.validate().is_err());
        }
        let mut twice = copied
            .copied(Id::new_v4(), Id::new_v4(), Mode::Local)
            .unwrap();
        twice.origins[1].source_workspace_id = Id::new_v4();
        assert!(twice.validate().is_err());
        assert!(copied
            .copied(source.log.info().id, Id::new_v4(), Mode::Local)
            .is_err());
    }

    #[test]
    fn resigned_manifest_still_requires_all_history_blobs_and_a_bounded_plain_tree() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("attachments")).unwrap();
        let (mut source, owner) = history();
        let item: NewItem =
            serde_json::from_value(json!({"id":Id::new_v4(),"kind":"document","title":"有附件"}))
                .unwrap();
        let item_id = item.id;
        let device = Id::new_v4();
        source
            .log
            .execute(
                device,
                Request {
                    id: Id::new_v4(),
                    actor_id: owner.id,
                    at: 1,
                    command: Command::Create { item },
                },
            )
            .unwrap();
        let sha = digest(b"bytes");
        write_blob(dir.path(), &sha, b"bytes").unwrap();
        source
            .log
            .execute(
                device,
                Request {
                    id: Id::new_v4(),
                    actor_id: owner.id,
                    at: 2,
                    command: Command::AddAttachment {
                        id: Id::new_v4(),
                        item_id,
                        name: "证据".into(),
                        mime: "text/plain".into(),
                        sha256: sha,
                        size: 5,
                    },
                },
            )
            .unwrap();
        let output = tempfile::tempdir().unwrap();
        let path = output.path().join("package");
        export(source, Mode::Local, owner.id, dir.path(), &path).unwrap();
        let mut package: Package = read_json(&path.join("migration.json"), MAX_METADATA).unwrap();
        let original = fs::read(path.join("migration.json")).unwrap();
        package.files.clear();
        package.checksum = package.payload_checksum().unwrap();
        fs::write(
            path.join("migration.json"),
            serde_json::to_vec_pretty(&package).unwrap(),
        )
        .unwrap();
        assert!(prepare(&path).is_err());
        fs::write(path.join("migration.json"), original).unwrap();
        fs::create_dir_all(path.join("extra/a/b")).unwrap();
        assert!(prepare(&path).is_err());
    }
}
