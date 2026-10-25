use crate::migration::{self, History};
use crate::transfer::TransferState;
use crate::{
    Applied, Attachment, Command, Error, Id, Member, Mode, Receipt, Request, Result, Role, State,
};
use crate::{GitLog, MigrationReport};
use crate::{TransferActivation, TransferPhase, TransferReceipt, TransferRole, TransferStatus};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior, MAIN_DB};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

const SCHEMA: u32 = 1;
const MAX_ATTACHMENT: u64 = 10 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub schema: u32,
    pub id: Id,
    pub name: String,
    pub mode: Mode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Upload {
    pub operation_id: Id,
    pub actor_id: Id,
    pub at: i64,
    pub id: Id,
    pub item_id: Id,
    pub name: String,
    pub mime: String,
}

pub struct LocalWorkspace {
    root: PathBuf,
    connection: Connection,
    info: WorkspaceInfo,
}

#[derive(Serialize, Deserialize)]
struct BackupManifest {
    schema: u32,
    workspace_id: Id,
    revision: u64,
    files: BTreeMap<String, String>,
}

impl LocalWorkspace {
    pub fn create(path: impl AsRef<Path>, id: Id, name: String, owner: Member) -> Result<Self> {
        let state = State::new(id, name.clone(), owner)?;
        let destination = path.as_ref();
        let staging = new_destination(destination)?;
        let info = WorkspaceInfo {
            schema: SCHEMA,
            id,
            name,
            mode: Mode::Local,
        };
        write_json(&staging.path().join("workspace.json"), &info)?;
        fs::create_dir(staging.path().join("attachments")).map_err(storage)?;
        let mut connection = connect(&staging.path().join("data.sqlite"), true)?;
        connection
            .execute_batch(
                "CREATE TABLE workspace (singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                revision INTEGER NOT NULL, genesis TEXT NOT NULL);
             CREATE TABLE objects (id TEXT PRIMARY KEY, category TEXT NOT NULL, item_id TEXT,
                kind TEXT, status TEXT, product_id TEXT, project_id TEXT, assigned_to TEXT,
                account TEXT COLLATE NOCASE, deleted INTEGER NOT NULL, updated_at INTEGER,
                version INTEGER, data TEXT NOT NULL);
             CREATE UNIQUE INDEX member_account ON objects(account) WHERE category='member';
             CREATE INDEX item_filters ON objects(kind,status,deleted,updated_at);
             CREATE INDEX related_objects ON objects(item_id,category);
             CREATE TABLE content_versions (item_id TEXT NOT NULL, version INTEGER NOT NULL,
                data TEXT NOT NULL, PRIMARY KEY(item_id,version));
             CREATE TABLE history (sequence INTEGER PRIMARY KEY, operation_id TEXT NOT NULL,
                object_id TEXT NOT NULL, data TEXT NOT NULL);
             CREATE INDEX object_history ON history(object_id,sequence);
             CREATE TABLE operations (id TEXT PRIMARY KEY, revision INTEGER NOT NULL UNIQUE,
                request TEXT NOT NULL, fingerprint TEXT NOT NULL, receipt TEXT NOT NULL);
             PRAGMA user_version=1;",
            )
            .map_err(storage)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        transaction
            .execute("INSERT INTO workspace VALUES (1,0,?1)", [encode(&state)?])
            .map_err(storage)?;
        write_projection(&transaction, None, &state)?;
        transaction.commit().map_err(storage)?;
        drop(connection);
        sync_directory(staging.path())?;
        promote(staging.path(), destination)?;
        Self::open(destination)
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let root = fs::canonicalize(path).map_err(storage)?;
        regular_file(&root.join("workspace.json"))?;
        regular_directory(&root.join("attachments"))?;
        let info: WorkspaceInfo = read_json(&root.join("workspace.json"), 16 * 1024)?;
        if info.schema != SCHEMA || info.mode != Mode::Local {
            return invalid("此工作空间格式或存储模式不受本地适配器支持");
        }
        let connection = connect(&root.join("data.sqlite"), false)?;
        let transaction = connection.unchecked_transaction().map_err(storage)?;
        let state = load_state(&transaction)?;
        if info.id != state.id || info.name != state.name || state.schema != SCHEMA {
            return invalid("工作空间清单与数据库不一致");
        }
        verify_journal(&transaction, &state)?;
        validate_transfer(&transaction, &state)?;
        for attachment in required_attachments(&transaction, &state)?.values() {
            read_blob(&root, attachment)?;
        }
        transaction.commit().map_err(storage)?;
        Ok(Self {
            root,
            connection,
            info,
        })
    }

    pub fn info(&self) -> &WorkspaceInfo {
        &self.info
    }

    // 本机目录已由文件访问权限授权，身份选择不能依赖尚未选定的操作者。
    pub fn identities(&self) -> Result<Vec<Member>> {
        let transaction = self.connection.unchecked_transaction().map_err(storage)?;
        let state = load_state(&transaction)?;
        validate_transfer(&transaction, &state)?;
        transaction.commit().map_err(storage)?;
        Ok(state.members.into_values().collect())
    }

    pub fn export_copy(&self, actor: Id, destination: impl AsRef<Path>) -> Result<MigrationReport> {
        let transaction = self.connection.unchecked_transaction().map_err(storage)?;
        let state = load_state(&transaction)?;
        owner(&state, actor)?;
        ensure_local_writable(&transaction, &state)?;
        let history = portable_history(&transaction, &state)?;
        let report = migration::export(
            history,
            Mode::Local,
            actor,
            &self.root,
            destination.as_ref(),
        )?;
        transaction.commit().map_err(storage)?;
        Ok(report)
    }

    pub fn import_copy(
        source: impl AsRef<Path>,
        destination: impl AsRef<Path>,
        new_id: Id,
    ) -> Result<(Self, MigrationReport)> {
        migration::check_destination(source.as_ref(), destination.as_ref())?;
        let prepared = migration::prepare(source.as_ref())?;
        let history = prepared.copied_history(new_id)?;
        Self::install_history(&prepared, history, destination.as_ref(), None)
    }

    pub fn stage_transfer(
        source: impl AsRef<Path>,
        destination: impl AsRef<Path>,
        actor: Id,
        at: i64,
    ) -> Result<(Self, MigrationReport, TransferReceipt)> {
        migration::check_destination(source.as_ref(), destination.as_ref())?;
        let prepared = migration::prepare(source.as_ref())?;
        let history = prepared.transfer_history(Mode::Local)?;
        let Some(plan) = prepared.transfer_plan().cloned() else {
            return invalid("缺少迁移计划");
        };
        let control = TransferState::stage(plan.clone(), &history, Mode::Local, actor, at)?;
        if fs::symlink_metadata(destination.as_ref()).is_ok() {
            regular_directory(destination.as_ref())?;
            let workspace = Self::open(destination)?;
            let tx = workspace
                .connection
                .unchecked_transaction()
                .map_err(storage)?;
            let state = load_state(&tx)?;
            owner(&state, actor)?;
            validate_transfer(&tx, &state)?;
            if local_transfer(&tx)?.is_none_or(|existing| {
                existing.status.role != TransferRole::Target || existing.status.plan != plan
            }) {
                return Err(Error::Conflict("目标目录已属于不同空间或迁移".into()));
            }
            let report = prepared.package.report(Some(workspace.info().clone()))?;
            tx.commit().map_err(storage)?;
            return Ok((workspace, report, plan.receipt()?));
        }
        let (workspace, report) =
            Self::install_history(&prepared, history, destination.as_ref(), Some(control))?;
        Ok((workspace, report, plan.receipt()?))
    }

    fn install_history(
        prepared: &migration::MigrationArchive,
        history: History,
        destination: &Path,
        transfer: Option<TransferState>,
    ) -> Result<(Self, MigrationReport)> {
        let state = history.validate()?;
        let owner = history
            .log
            .genesis()
            .members
            .values()
            .next()
            .cloned()
            .ok_or_else(|| Error::Validation("缺少初始所有者".into()))?;
        let stage = new_destination(destination)?;
        let path = stage.path().join("workspace");
        let mut workspace = Self::create(&path, state.id, state.name.clone(), owner)?;
        migration::copy_blobs(prepared.directory.path(), &path, &history)?;
        let tx = workspace
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        tx.execute_batch("CREATE TABLE migration_origin(singleton INTEGER PRIMARY KEY CHECK(singleton=1),data TEXT NOT NULL);").map_err(storage)?;
        tx.execute_batch(
            "CREATE TABLE migration_receipts(id TEXT PRIMARY KEY,request TEXT,receipt TEXT);",
        )
        .map_err(storage)?;
        for (id, (request, receipt)) in origin_receipts(&history)? {
            tx.execute(
                "INSERT INTO migration_receipts VALUES (?1,?2,?3)",
                params![id, request, receipt],
            )
            .map_err(storage)?;
        }
        tx.execute(
            "INSERT INTO migration_origin VALUES (1,?1)",
            [encode(&history)?],
        )
        .map_err(storage)?;
        tx.execute(
            "UPDATE workspace SET genesis=?1 WHERE singleton=1",
            [encode(&state)?],
        )
        .map_err(storage)?;
        write_projection(&tx, None, &state)?;
        if let Some(control) = transfer {
            initialize_transfer(&tx)?;
            write_transfer(&tx, &control)?;
            validate_transfer(&tx, &state)?;
        } else {
            tx.pragma_update(None, "user_version", 2).map_err(storage)?;
        }
        tx.commit().map_err(storage)?;
        drop(workspace);
        let verified = Self::open(&path)?;
        let mut report = prepared.package.report(Some(verified.info().clone()))?;
        report.origins = history.origins;
        drop(verified);
        promote(&path, destination)?;
        Ok((Self::open(destination)?, report))
    }

    pub fn snapshot(&self, actor: Id) -> Result<State> {
        let transaction = self.connection.unchecked_transaction().map_err(storage)?;
        let state = load_state(&transaction)?;
        validate_transfer(&transaction, &state)?;
        member(&state, actor)?;
        transaction.commit().map_err(storage)?;
        Ok(state)
    }

    pub fn prepare_transfer(
        &mut self,
        actor: Id,
        id: Id,
        at: i64,
        target: Mode,
        backup_destination: impl AsRef<Path>,
    ) -> Result<TransferStatus> {
        if id.is_nil() || target == Mode::Local || at < 0 {
            return invalid("迁移标识、目标模式或时间无效");
        }
        let state = self.snapshot(actor)?;
        owner(&state, actor)?;
        if let Some(previous) = local_transfer(&self.connection)? {
            if previous.status.role == TransferRole::Source
                && previous.status.phase == TransferPhase::Frozen
                && previous.status.plan.id == id
                && previous.status.plan.target_mode == target
            {
                return Ok(previous.status);
            }
            previous.ensure_writable()?;
            if previous.status.plan.id == id {
                return invalid("不能复用已结束的迁移标识");
            }
        }
        if self
            .transfer_events(actor)?
            .iter()
            .any(|event| event.plan.id == id)
        {
            return invalid("不能复用历史迁移标识");
        }
        self.backup(actor, backup_destination)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let state = load_state(&tx)?;
        owner(&state, actor)?;
        ensure_local_writable(&tx, &state)?;
        if local_transfer(&tx)?.is_some()
            && read_transfer_events(&tx)?
                .iter()
                .any(|event| event.plan.id == id)
        {
            return invalid("不能复用历史迁移标识");
        }
        let history = portable_history(&tx, &state)?;
        let control = TransferState::prepare(&history, Mode::Local, target, id, actor, at)?;
        initialize_transfer(&tx)?;
        write_transfer(&tx, &control)?;
        validate_transfer(&tx, &state)?;
        tx.commit().map_err(storage)?;
        Ok(control.status)
    }

    pub fn transfer_status(&self, actor: Id) -> Result<Option<TransferStatus>> {
        self.snapshot(actor)?;
        Ok(local_transfer(&self.connection)?.map(|s| s.status))
    }

    pub fn transfer_events(&self, actor: Id) -> Result<Vec<TransferStatus>> {
        owner(&self.snapshot(actor)?, actor)?;
        if local_transfer(&self.connection)?.is_none() {
            return Ok(vec![]);
        }
        read_transfer_events(&self.connection)
    }

    pub fn export_transfer(
        &self,
        actor: Id,
        destination: impl AsRef<Path>,
    ) -> Result<MigrationReport> {
        let tx = self.connection.unchecked_transaction().map_err(storage)?;
        let state = load_state(&tx)?;
        owner(&state, actor)?;
        validate_transfer(&tx, &state)?;
        let Some(control) = local_transfer(&tx)? else {
            return invalid("请先冻结源工作空间");
        };
        let history = portable_history(&tx, &state)?;
        control.validate(&history, Mode::Local)?;
        if control.status.role != TransferRole::Source
            || !matches!(
                control.status.phase,
                TransferPhase::Frozen | TransferPhase::Completed
            )
        {
            return invalid("当前没有可导出的源迁移");
        }
        let report =
            history.export_transfer(&control.status.plan, actor, &self.root, destination)?;
        tx.commit().map_err(storage)?;
        Ok(report)
    }

    pub fn complete_transfer(
        &mut self,
        actor: Id,
        at: i64,
        receipt: &TransferReceipt,
    ) -> Result<TransferActivation> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let state = load_state(&tx)?;
        owner(&state, actor)?;
        validate_transfer(&tx, &state)?;
        let Some(mut control) = local_transfer(&tx)? else {
            return invalid("源工作空间尚未冻结");
        };
        control.validate(&portable_history(&tx, &state)?, Mode::Local)?;
        let activation = control.complete(receipt, actor, at)?;
        write_transfer(&tx, &control)?;
        tx.commit().map_err(storage)?;
        Ok(activation)
    }

    pub fn activate_transfer(
        &mut self,
        actor: Id,
        at: i64,
        activation: &TransferActivation,
    ) -> Result<TransferStatus> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let state = load_state(&tx)?;
        owner(&state, actor)?;
        validate_transfer(&tx, &state)?;
        let Some(mut control) = local_transfer(&tx)? else {
            return invalid("目标没有暂存迁移");
        };
        control.activate(activation, actor, at)?;
        control.validate(&portable_history(&tx, &state)?, Mode::Local)?;
        write_transfer(&tx, &control)?;
        validate_transfer(&tx, &state)?;
        tx.commit().map_err(storage)?;
        Ok(control.status)
    }

    pub fn cancel_transfer(&mut self, actor: Id, at: i64, id: Id) -> Result<TransferStatus> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let state = load_state(&tx)?;
        owner(&state, actor)?;
        validate_transfer(&tx, &state)?;
        let Some(mut control) = local_transfer(&tx)? else {
            return invalid("源工作空间尚未冻结");
        };
        control.validate(&portable_history(&tx, &state)?, Mode::Local)?;
        if control.status.plan.id != id {
            return invalid("迁移标识不匹配");
        }
        control.cancel(actor, at)?;
        write_transfer(&tx, &control)?;
        tx.commit().map_err(storage)?;
        Ok(control.status)
    }

    pub fn execute(&mut self, request: Request) -> Result<Receipt> {
        self.commit(request, None, None)
    }

    pub fn execute_checked(&mut self, request: Request, expected_revision: u64) -> Result<Receipt> {
        self.commit(request, None, Some(expected_revision))
    }

    pub fn upload(&mut self, upload: Upload, bytes: &[u8]) -> Result<Receipt> {
        // 哈希和大小取自真实文件，不能相信界面提交的元信息。
        self.commit(
            Request {
                id: upload.operation_id,
                actor_id: upload.actor_id,
                at: upload.at,
                command: Command::AddAttachment {
                    id: upload.id,
                    item_id: upload.item_id,
                    name: upload.name,
                    mime: upload.mime,
                    sha256: digest(bytes),
                    size: bytes.len() as u64,
                },
            },
            Some(bytes),
            None,
        )
    }

    pub fn read_attachment(&self, actor: Id, id: Id) -> Result<Vec<u8>> {
        let state = self.snapshot(actor)?;
        let attachment = state
            .attachments
            .get(&id)
            .filter(|a| !a.deleted)
            .ok_or_else(|| Error::NotFound("附件".into()))?;
        state.item(attachment.item_id)?;
        read_blob(&self.root, attachment)
    }

    pub fn backup(&self, actor: Id, destination: impl AsRef<Path>) -> Result<()> {
        let state = self.snapshot(actor)?;
        owner(&state, actor)?;
        let destination = destination.as_ref();
        let staging = new_destination(destination)?;
        let database = staging.path().join("data.sqlite");
        // 先取得一致数据库快照，再按该快照复制不可变附件，避免漏掉 WAL 中的操作。
        self.connection
            .backup(MAIN_DB, &database, None)
            .map_err(storage)?;
        let snapshot_connection =
            Connection::open_with_flags(&database, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(storage)?;
        let snapshot = load_state(&snapshot_connection)?;
        owner(&snapshot, actor)?;
        verify_journal(&snapshot_connection, &snapshot)?;
        validate_transfer(&snapshot_connection, &snapshot)?;
        let attachments = required_attachments(&snapshot_connection, &snapshot)?;
        drop(snapshot_connection);
        write_json(&staging.path().join("workspace.json"), &self.info)?;
        fs::create_dir(staging.path().join("attachments")).map_err(storage)?;
        for attachment in attachments.values() {
            let bytes = read_blob(&self.root, attachment)?;
            write_blob(staging.path(), &attachment.sha256, &bytes)?;
        }
        let mut files = BTreeMap::new();
        for relative in required_files(&attachments) {
            files.insert(relative.clone(), hash_file(&staging.path().join(relative))?);
        }
        write_json(
            &staging.path().join("backup.json"),
            &BackupManifest {
                schema: SCHEMA,
                workspace_id: snapshot.id,
                revision: snapshot.revision,
                files,
            },
        )?;
        sync_directory(staging.path())?;
        promote(staging.path(), destination)?;
        Ok(())
    }

    pub fn restore(source: impl AsRef<Path>, destination: impl AsRef<Path>) -> Result<Self> {
        let source = fs::canonicalize(source).map_err(storage)?;
        regular_file(&source.join("backup.json"))?;
        let manifest: BackupManifest = read_json(&source.join("backup.json"), 16 * 1024 * 1024)?;
        if manifest.schema != SCHEMA || manifest.files.len() > 100_002 {
            return invalid("备份格式或文件数量不受支持");
        }
        let destination = destination.as_ref();
        let staging = new_destination(destination)?;
        fs::create_dir(staging.path().join("attachments")).map_err(storage)?;
        for (relative, hash) in &manifest.files {
            if !safe_backup_path(relative) || !valid_hash(hash) {
                return invalid("备份包含非法路径或哈希");
            }
            let source_file = source.join(relative);
            if relative.starts_with("attachments/") {
                regular_directory(&source.join("attachments"))?;
            }
            if hash_file(&source_file)? != *hash {
                return invalid("备份文件校验失败");
            }
            let output = staging.path().join(relative);
            fs::create_dir_all(
                output
                    .parent()
                    .ok_or_else(|| Error::Validation("备份路径无效".into()))?,
            )
            .map_err(storage)?;
            // 恢复目标必须可写，不能继承只读备份文件的权限。
            let mut input = File::open(&source_file).map_err(storage)?;
            let mut target = File::options()
                .write(true)
                .create_new(true)
                .open(&output)
                .map_err(storage)?;
            std::io::copy(&mut input, &mut target).map_err(storage)?;
            target.sync_all().map_err(storage)?;
            if hash_file(&output)? != *hash {
                return invalid("备份文件在恢复期间发生变化");
            }
        }
        let restored = Self::open(staging.path())?;
        let state = load_state(&restored.connection)?;
        let attachments = required_attachments(&restored.connection, &state)?;
        if state.id != manifest.workspace_id
            || state.revision != manifest.revision
            || required_files(&attachments) != manifest.files.keys().cloned().collect()
        {
            return invalid("备份清单不完整或不属于所声明的工作空间版本");
        }
        drop(restored);
        sync_directory(staging.path())?;
        promote(staging.path(), destination)?;
        Self::open(destination)
    }

    fn commit(
        &mut self,
        request: Request,
        bytes: Option<&[u8]>,
        expected_revision: Option<u64>,
    ) -> Result<Receipt> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let before = load_state(&transaction)?;
        ensure_local_writable(&transaction, &before)?;
        if let Some((old, receipt)) = migration_receipt(&transaction, request.id)? {
            member(&before, request.actor_id)?;
            if old.as_deref() == Some(encode(&request)?.as_str()) {
                if let Some(receipt) = receipt {
                    return decode(&receipt);
                }
            }
            return Err(Error::Conflict(
                "操作标识已属于迁移历史；冲突提案及决策不能作为新操作重用".into(),
            ));
        }
        member(&before, request.actor_id)?;
        if expected_revision.is_some_and(|revision| revision != before.revision)
            && !before.operations.contains_key(&request.id)
        {
            return Err(Error::Conflict("工作空间已更新，请刷新后重试".into()));
        }
        let mut after = before.clone();
        let receipt = after.execute(request.clone())?;
        if after.revision == before.revision {
            transaction.commit().map_err(storage)?;
            return Ok(receipt);
        }
        if let Some(bytes) = bytes {
            let Command::AddAttachment { sha256, .. } = &request.command else {
                return invalid("附件操作类型无效");
            };
            write_blob(&self.root, sha256, bytes)?;
        }
        for (id, attachment) in &after.attachments {
            if !before.attachments.contains_key(id) {
                read_blob(&self.root, attachment)?;
            }
        }
        write_projection(&transaction, Some(&before), &after)?;
        let applied = &after.operations[&request.id];
        transaction
            .execute(
                "INSERT INTO operations VALUES (?1,?2,?3,?4,?5)",
                params![
                    request.id.to_string(),
                    after.revision,
                    encode(&request)?,
                    applied.fingerprint,
                    encode(&receipt)?
                ],
            )
            .map_err(storage)?;
        transaction.commit().map_err(storage)?;
        Ok(receipt)
    }
}

fn connect(path: &Path, create: bool) -> Result<Connection> {
    if !create {
        regular_file(path)?;
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | if create {
            OpenFlags::SQLITE_OPEN_CREATE
        } else {
            OpenFlags::empty()
        };
    let connection = Connection::open_with_flags(path, flags).map_err(storage)?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(storage)?;
    if !create {
        let schema: u32 = connection
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(storage)?;
        if ![SCHEMA, 2, 3].contains(&schema) {
            return invalid("数据库版本不受支持，请使用兼容版本或从备份恢复");
        }
        let integrity: String = connection
            .query_row("PRAGMA quick_check", [], |r| r.get(0))
            .map_err(storage)?;
        if integrity != "ok" {
            return invalid("数据库完整性检查失败");
        }
    }
    connection
        .execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")
        .map_err(storage)?;
    Ok(connection)
}

fn write_projection(connection: &Connection, before: Option<&State>, after: &State) -> Result<()> {
    let previous = before.map(objects).transpose()?.unwrap_or_default();
    let current = objects(after)?;
    let mut insert = connection.prepare_cached(
        "INSERT INTO objects VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
         ON CONFLICT(id) DO UPDATE SET category=excluded.category,item_id=excluded.item_id,
         kind=excluded.kind,status=excluded.status,product_id=excluded.product_id,
         project_id=excluded.project_id,assigned_to=excluded.assigned_to,account=excluded.account,
         deleted=excluded.deleted,updated_at=excluded.updated_at,version=excluded.version,data=excluded.data"
    ).map_err(storage)?;
    for (id, (category, value)) in current {
        if previous.get(&id) == Some(&(category, value.clone())) {
            continue;
        }
        insert
            .execute(params![
                id.to_string(),
                category,
                text(&value, "item_id"),
                text(&value, "kind"),
                text(&value, "status"),
                text(&value, "product_id"),
                text(&value, "project_id"),
                text(&value, "assigned_to"),
                text(&value, "account"),
                value["deleted"].as_bool().unwrap_or(false),
                value["updated_at"].as_i64(),
                value["version"].as_u64(),
                encode(&value)?
            ])
            .map_err(storage)?;
    }
    for version in after
        .versions
        .iter()
        .skip(before.map_or(0, |s| s.versions.len()))
    {
        connection
            .execute(
                "INSERT INTO content_versions VALUES (?1,?2,?3)",
                params![
                    version.item_id.to_string(),
                    version.version,
                    encode(version)?
                ],
            )
            .map_err(storage)?;
    }
    for (index, audit) in after
        .history
        .iter()
        .enumerate()
        .skip(before.map_or(0, |s| s.history.len()))
    {
        connection
            .execute(
                "INSERT INTO history VALUES (?1,?2,?3,?4)",
                params![
                    index + 1,
                    audit.operation_id.to_string(),
                    audit.object_id.to_string(),
                    encode(audit)?
                ],
            )
            .map_err(storage)?;
    }
    connection
        .execute(
            "UPDATE workspace SET revision=?1 WHERE singleton=1",
            [after.revision],
        )
        .map_err(storage)?;
    Ok(())
}

fn load_state(connection: &Connection) -> Result<State> {
    let (revision, genesis): (u64, String) = connection
        .query_row(
            "SELECT revision,genesis FROM workspace WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(storage)?;
    let mut state: State = decode(&genesis)?;
    state.revision = revision;
    state.members.clear();
    for (id, category, data) in rows(
        connection,
        "SELECT id,category,data FROM objects ORDER BY id",
    )? {
        let value: Value = decode(&data)?;
        if value["id"].as_str() != Some(id.as_str()) {
            return invalid("对象标识与数据库索引不一致");
        }
        let id = Id::parse_str(&id).map_err(storage)?;
        match category.as_str() {
            "member" => {
                state
                    .members
                    .insert(id, serde_json::from_value(value).map_err(storage)?);
            }
            "item" => {
                state
                    .items
                    .insert(id, serde_json::from_value(value).map_err(storage)?);
            }
            "comment" => {
                state
                    .comments
                    .insert(id, serde_json::from_value(value).map_err(storage)?);
            }
            "attachment" => {
                state
                    .attachments
                    .insert(id, serde_json::from_value(value).map_err(storage)?);
            }
            _ => return invalid("未知的业务数据类别"),
        }
    }
    state.versions = json_rows(
        connection,
        "SELECT data FROM content_versions ORDER BY rowid",
    )?;
    state.history = json_rows(connection, "SELECT data FROM history ORDER BY sequence")?;
    for (id, fingerprint, receipt) in rows(
        connection,
        "SELECT id,fingerprint,receipt FROM operations ORDER BY revision",
    )? {
        state.operations.insert(
            Id::parse_str(&id).map_err(storage)?,
            Applied {
                fingerprint,
                receipt: decode(&receipt)?,
            },
        );
    }
    Ok(state)
}

fn verify_journal(connection: &Connection, projected: &State) -> Result<()> {
    let genesis: String = connection
        .query_row("SELECT genesis FROM workspace WHERE singleton=1", [], |r| {
            r.get(0)
        })
        .map_err(storage)?;
    let mut state: State = decode(&genesis)?;
    let origin = origin_history(connection)?;
    let expected = if let Some(origin) = &origin {
        let expected = origin.validate()?;
        let mut rows = connection
            .prepare("SELECT id,request,receipt FROM migration_receipts")
            .map_err(storage)?;
        let actual = rows
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    (
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ),
                ))
            })
            .map_err(storage)?
            .collect::<std::result::Result<BTreeMap<_, _>, _>>()
            .map_err(storage)?;
        if actual != origin_receipts(origin)? {
            return invalid("迁移来源回执索引不一致");
        }
        expected
    } else {
        let first = state
            .members
            .values()
            .next()
            .cloned()
            .ok_or_else(|| Error::Validation("缺少初始所有者".into()))?;
        State::new(state.id, state.name.clone(), first)?
    };
    if encode(&expected)? != encode(&state)? {
        return invalid("初始工作空间数据无效");
    }
    let requests: Vec<Request> = json_rows(
        connection,
        "SELECT request FROM operations ORDER BY revision",
    )?;
    for request in requests {
        if origin
            .as_ref()
            .is_some_and(|h| h.log.operations().contains_key(&request.id))
        {
            return invalid("本地新操作重复使用了来源操作标识");
        }
        state.execute(request)?;
    }
    if encode(&state)? != encode(projected)? {
        return invalid("业务数据与操作日志不一致，需从有效备份恢复");
    }
    Ok(())
}

type OriginReceipts = BTreeMap<String, (Option<String>, Option<String>)>;
fn origin_receipts(history: &History) -> Result<OriginReceipts> {
    let view = history.log.view()?;
    history
        .log
        .operations()
        .iter()
        .map(|(id, op)| {
            let request = match &op.change {
                crate::GitChange::Apply { request } => Some(encode(request)?),
                crate::GitChange::Resolve { .. } => None,
            };
            Ok((
                id.to_string(),
                (request, view.receipts.get(id).map(encode).transpose()?),
            ))
        })
        .collect()
}
fn migration_receipt(
    connection: &Connection,
    id: Id,
) -> Result<Option<(Option<String>, Option<String>)>> {
    let schema: u32 = connection
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(storage)?;
    if schema < 2 {
        return Ok(None);
    }
    connection
        .query_row(
            "SELECT request,receipt FROM migration_receipts WHERE id=?1",
            [id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(storage)
}

fn origin_history(connection: &Connection) -> Result<Option<History>> {
    let schema: u32 = connection
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(storage)?;
    if schema < 2 {
        return Ok(None);
    }
    let data: Option<String> = connection
        .query_row(
            "SELECT data FROM migration_origin WHERE singleton=1",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(storage)?;
    match data {
        Some(data) => Ok(Some(decode(&data)?)),
        None if schema == 3 => {
            let count: i64 = connection
                .query_row("SELECT COUNT(*) FROM migration_receipts", [], |r| r.get(0))
                .map_err(storage)?;
            if count != 0 {
                return invalid("本地存在无来源的迁移回执");
            }
            Ok(None)
        }
        None => invalid("复制空间缺少原始来源"),
    }
}

fn local_transfer(connection: &Connection) -> Result<Option<TransferState>> {
    let schema: u32 = connection
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(storage)?;
    if schema != 3 {
        return Ok(None);
    }
    let (data, hash, writable): (String, String, bool) = connection
        .query_row(
            "SELECT data,checksum,writable FROM transfer_control WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .map_err(storage)?;
    let control: TransferState = decode(&data)?;
    if digest(data.as_bytes()) != hash || writable != control.writable() {
        return invalid("本地迁移控制记录损坏");
    }
    for table in [
        "workspace",
        "objects",
        "content_versions",
        "history",
        "operations",
    ] {
        for action in ["INSERT", "UPDATE", "DELETE"] {
            let name = format!("transfer_{table}_{action}");
            let sql: Option<String> = connection
                .query_row(
                    "SELECT sql FROM sqlite_schema WHERE type='trigger' AND name=?1",
                    [&name],
                    |r| r.get(0),
                )
                .optional()
                .map_err(storage)?;
            if sql.as_deref() != Some(transfer_trigger(table, action).as_str()) {
                return invalid("本地迁移写入屏障缺失或损坏");
            }
        }
    }
    Ok(Some(control))
}

fn validate_transfer(connection: &Connection, state: &State) -> Result<()> {
    if let Some(control) = local_transfer(connection)? {
        let history = portable_history(connection, state)?;
        control.validate(&history, Mode::Local)?;
        let events = read_transfer_events(connection)?;
        for event in &events {
            event.validate(&history, Mode::Local)?;
        }
        if events.last() != Some(&control.status) {
            return invalid("迁移事件与当前阶段不一致");
        }
    }
    Ok(())
}

fn read_transfer_events(connection: &Connection) -> Result<Vec<TransferStatus>> {
    let mut query = connection
        .prepare("SELECT data,checksum FROM transfer_events ORDER BY sequence")
        .map_err(storage)?;
    let rows = query
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(storage)?;
    let mut out = vec![];
    for row in rows {
        let (data, hash) = row.map_err(storage)?;
        if digest(data.as_bytes()) != hash {
            return invalid("迁移事件校验失败");
        }
        out.push(decode(&data)?);
    }
    Ok(out)
}

fn ensure_local_writable(connection: &Connection, state: &State) -> Result<()> {
    validate_transfer(connection, state)?;
    if let Some(control) = local_transfer(connection)? {
        control.ensure_writable()?;
    }
    Ok(())
}

fn initialize_transfer(connection: &Connection) -> Result<()> {
    connection.execute_batch("CREATE TABLE IF NOT EXISTS migration_origin(singleton INTEGER PRIMARY KEY CHECK(singleton=1),data TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS migration_receipts(id TEXT PRIMARY KEY,request TEXT,receipt TEXT);
        CREATE TABLE IF NOT EXISTS transfer_control(singleton INTEGER PRIMARY KEY CHECK(singleton=1),data TEXT NOT NULL,checksum TEXT NOT NULL,writable INTEGER NOT NULL CHECK(writable IN (0,1)));
        CREATE TABLE IF NOT EXISTS transfer_events(sequence INTEGER PRIMARY KEY,data TEXT NOT NULL,checksum TEXT NOT NULL);").map_err(storage)?;
    // 数据库触发器也约束冻结前已打开的旧客户端，不能只在新客户端入口检查。
    for table in [
        "workspace",
        "objects",
        "content_versions",
        "history",
        "operations",
    ] {
        for action in ["INSERT", "UPDATE", "DELETE"] {
            connection
                .execute_batch(&transfer_trigger(table, action).replacen(
                    "CREATE TRIGGER",
                    "CREATE TRIGGER IF NOT EXISTS",
                    1,
                ))
                .map_err(storage)?;
        }
    }
    connection
        .pragma_update(None, "user_version", 3)
        .map_err(storage)
}

fn transfer_trigger(table: &str, action: &str) -> String {
    format!("CREATE TRIGGER transfer_{table}_{action} BEFORE {action} ON {table} WHEN COALESCE((SELECT writable FROM transfer_control WHERE singleton=1),0)=0 BEGIN SELECT RAISE(ABORT,'工作空间正在迁移或已交接，当前只读'); END")
}

fn write_transfer(connection: &Connection, control: &TransferState) -> Result<()> {
    let data = encode(control)?;
    let old: Option<String> = connection
        .query_row(
            "SELECT data FROM transfer_control WHERE singleton=1",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(storage)?;
    if old.as_deref() == Some(&data) {
        return Ok(());
    }
    connection.execute("INSERT INTO transfer_control VALUES (1,?1,?2,?3) ON CONFLICT(singleton) DO UPDATE SET data=excluded.data,checksum=excluded.checksum,writable=excluded.writable",params![data,digest(data.as_bytes()),control.writable()]).map_err(storage)?;
    let event = encode(&control.status)?;
    connection
        .execute(
            "INSERT INTO transfer_events(data,checksum) VALUES (?1,?2)",
            params![event, digest(event.as_bytes())],
        )
        .map_err(storage)?;
    Ok(())
}

fn portable_history(connection: &Connection, projected: &State) -> Result<History> {
    verify_journal(connection, projected)?;
    let mut history = if let Some(origin) = origin_history(connection)? {
        origin
    } else {
        let data: String = connection
            .query_row("SELECT genesis FROM workspace WHERE singleton=1", [], |r| {
                r.get(0)
            })
            .map_err(storage)?;
        let state: State = decode(&data)?;
        let owner = state
            .members
            .values()
            .next()
            .cloned()
            .ok_or_else(|| Error::Validation("缺少初始所有者".into()))?;
        History {
            log: GitLog::new(state.id, state.name, owner)?,
            origins: vec![],
            clients: BTreeMap::new(),
        }
    };
    let requests: Vec<Request> = json_rows(
        connection,
        "SELECT request FROM operations ORDER BY revision",
    )?;
    for request in requests {
        history.log.execute(projected.id, request)?;
    }
    if encode(&history.validate()?)? != encode(projected)? {
        return invalid("迁移操作历史与本地投影不一致");
    }
    Ok(history)
}

fn required_attachments(
    connection: &Connection,
    state: &State,
) -> Result<BTreeMap<(String, u64), Attachment>> {
    let mut out = origin_history(connection)?
        .map(|h| crate::repository::attachments(&h.log))
        .unwrap_or_default();
    out.extend(
        state
            .attachments
            .values()
            .map(|a| ((a.sha256.clone(), a.size), a.clone())),
    );
    Ok(out)
}

fn objects(state: &State) -> Result<BTreeMap<Id, (&'static str, Value)>> {
    let mut objects = BTreeMap::new();
    for (id, member) in &state.members {
        objects.insert(
            *id,
            ("member", serde_json::to_value(member).map_err(storage)?),
        );
    }
    for (id, item) in &state.items {
        objects.insert(*id, ("item", serde_json::to_value(item).map_err(storage)?));
    }
    for (id, comment) in &state.comments {
        objects.insert(
            *id,
            ("comment", serde_json::to_value(comment).map_err(storage)?),
        );
    }
    for (id, attachment) in &state.attachments {
        objects.insert(
            *id,
            (
                "attachment",
                serde_json::to_value(attachment).map_err(storage)?,
            ),
        );
    }
    Ok(objects)
}

fn rows(connection: &Connection, query: &str) -> Result<Vec<(String, String, String)>> {
    connection
        .prepare(query)
        .map_err(storage)?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .map_err(storage)?
        .collect::<std::result::Result<_, _>>()
        .map_err(storage)
}

fn json_rows<T: serde::de::DeserializeOwned>(
    connection: &Connection,
    query: &str,
) -> Result<Vec<T>> {
    let mut statement = connection.prepare(query).map_err(storage)?;
    let rows = statement
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(storage)?;
    rows.map(|row| decode(&row.map_err(storage)?)).collect()
}

pub(crate) fn member(state: &State, actor: Id) -> Result<&Member> {
    state
        .members
        .get(&actor)
        .filter(|m| m.active)
        .ok_or_else(|| Error::Forbidden("账户未启用或不是工作空间成员".into()))
}

pub(crate) fn owner(state: &State, actor: Id) -> Result<()> {
    if !member(state, actor)?.roles.contains(&Role::Owner) {
        return Err(Error::Forbidden("只有所有者可以导出完整备份".into()));
    }
    Ok(())
}

pub fn write_blob(root: &Path, hash: &str, bytes: &[u8]) -> Result<()> {
    if !valid_hash(hash)
        || bytes.is_empty()
        || bytes.len() as u64 > MAX_ATTACHMENT
        || digest(bytes) != hash
    {
        return invalid("附件数据或内容哈希无效");
    }
    let directory = root.join("attachments").join(&hash[..2]);
    regular_directory(&root.join("attachments"))?;
    if !directory.exists() {
        fs::create_dir(&directory).map_err(storage)?;
        sync_directory(&root.join("attachments"))?;
    }
    regular_directory(&directory)?;
    let target = directory.join(hash);
    if target.exists() {
        let existing = read_file(&target, MAX_ATTACHMENT)?;
        if existing != bytes {
            return invalid("已有附件内容校验失败");
        }
        return Ok(());
    }
    let mut file = tempfile::NamedTempFile::new_in(&directory).map_err(storage)?;
    file.write_all(bytes).map_err(storage)?;
    file.as_file().sync_all().map_err(storage)?;
    match file.persist_noclobber(&target) {
        Ok(_) => {}
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            if read_file(&target, MAX_ATTACHMENT)? != bytes {
                return invalid("并发写入的附件内容不一致");
            }
        }
        Err(error) => return Err(storage(error.error)),
    }
    sync_directory(&directory)
}

pub fn read_blob(root: &Path, attachment: &Attachment) -> Result<Vec<u8>> {
    if !valid_hash(&attachment.sha256) {
        return invalid("附件内容哈希无效");
    }
    let directory = root.join("attachments").join(&attachment.sha256[..2]);
    regular_directory(&root.join("attachments"))?;
    regular_directory(&directory)?;
    let bytes = read_file(&directory.join(&attachment.sha256), MAX_ATTACHMENT)?;
    if bytes.len() as u64 != attachment.size || digest(&bytes) != attachment.sha256 {
        return invalid("附件内容损坏或与元信息不一致");
    }
    Ok(bytes)
}

fn required_files(attachments: &BTreeMap<(String, u64), Attachment>) -> BTreeSet<String> {
    let mut files = BTreeSet::from(["data.sqlite".into(), "workspace.json".into()]);
    for attachment in attachments.values() {
        files.insert(format!(
            "attachments/{}/{}",
            &attachment.sha256[..2],
            attachment.sha256
        ));
    }
    files
}

fn safe_backup_path(path: &str) -> bool {
    if ["data.sqlite", "workspace.json"].contains(&path) {
        return true;
    }
    let parts: Vec<_> = path.split('/').collect();
    parts.len() == 3
        && parts[0] == "attachments"
        && valid_hash(parts[2])
        && parts[1] == &parts[2][..2]
}

pub fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

pub(crate) fn new_destination(destination: &Path) -> Result<tempfile::TempDir> {
    if fs::symlink_metadata(destination).is_ok() {
        return Err(Error::Conflict("目标目录已存在，请选择新的目录".into()));
    }
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut builder = tempfile::Builder::new();
    builder.prefix(".farflow-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(fs::Permissions::from_mode(0o700));
    }
    builder.tempdir_in(parent).map_err(storage)
}

pub fn regular_file(path: &Path) -> Result<()> {
    if !fs::symlink_metadata(path)
        .map_err(storage)?
        .file_type()
        .is_file()
    {
        return invalid("需要普通文件，不能使用目录或符号链接");
    }
    Ok(())
}

pub(crate) fn promote(staging: &Path, destination: &Path) -> Result<()> {
    // 克隆准备期间其他进程仍可能创建目标目录，落位必须原子地拒绝替换。
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    rustix::fs::renameat_with(
        rustix::fs::CWD,
        staging,
        rustix::fs::CWD,
        destination,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(storage)?;
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let from: Vec<_> = staging.as_os_str().encode_wide().chain([0]).collect();
        let to: Vec<_> = destination.as_os_str().encode_wide().chain([0]).collect();
        if from[..from.len() - 1].contains(&0) || to[..to.len() - 1].contains(&0) {
            return invalid("目录路径包含无效字符");
        }
        // 两个缓冲区都以零结尾，且在系统调用返回前保持存活。
        if unsafe {
            windows_sys::Win32::Storage::FileSystem::MoveFileExW(from.as_ptr(), to.as_ptr(), 0)
        } == 0
        {
            return Err(storage(std::io::Error::last_os_error()));
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    return Err(Error::Storage("当前平台未提供目录原子落位能力".into()));

    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    {
        let parent = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        // 文件自身持久化之后，目录项也要落盘，才能覆盖断电后的恢复边界。
        sync_directory(parent)
    }
}

pub fn regular_directory(path: &Path) -> Result<()> {
    if !fs::symlink_metadata(path)
        .map_err(storage)?
        .file_type()
        .is_dir()
    {
        return invalid("需要普通目录，不能使用符号链接");
    }
    Ok(())
}

pub fn read_file(path: &Path, limit: u64) -> Result<Vec<u8>> {
    regular_file(path)?;
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(storage)?
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(storage)?;
    if bytes.len() as u64 > limit {
        return invalid("文件超过允许大小");
    }
    Ok(bytes)
}

pub(crate) fn read_json<T: serde::de::DeserializeOwned>(path: &Path, limit: u64) -> Result<T> {
    serde_json::from_slice(&read_file(path, limit)?).map_err(storage)
}

pub fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut options = File::options();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(storage)?;
    file.write_all(&serde_json::to_vec_pretty(value).map_err(storage)?)
        .map_err(storage)?;
    file.sync_all().map_err(storage)
}

pub fn hash_file(path: &Path) -> Result<String> {
    regular_file(path)?;
    // 同时检查父级，恢复包不能经由目录链接读取工作空间以外的文件。
    if let Some(parent) = path.parent() {
        regular_directory(parent)?;
    }
    let mut hash = Sha256::new();
    let mut file = File::open(path).map_err(storage)?;
    let mut buffer = [0; 65536];
    loop {
        let length = file.read(&mut buffer).map_err(storage)?;
        if length == 0 {
            break;
        }
        hash.update(&buffer[..length]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

pub fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)
        .map_err(storage)?
        .sync_all()
        .map_err(storage)?;
    Ok(())
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value[key].as_str()
}
fn encode(value: &impl Serialize) -> Result<String> {
    serde_json::to_string(value).map_err(storage)
}
fn decode<T: serde::de::DeserializeOwned>(value: &str) -> Result<T> {
    serde_json::from_str(value).map_err(storage)
}
pub(crate) fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn storage(error: impl std::fmt::Display) -> Error {
    Error::Storage(error.to_string())
}
fn invalid<T>(message: &str) -> Result<T> {
    Err(Error::Validation(message.into()))
}

#[cfg(test)]
mod directory_promotion_tests {
    use super::*;

    #[test]
    fn directory_promotion_never_replaces_a_target_created_after_preparation() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("prepared");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("data"), "待落位数据").unwrap();
        for target in ["empty", "not-empty", "file"] {
            let path = temp.path().join(target);
            if target == "file" {
                fs::write(&path, "保留文件").unwrap();
            } else {
                fs::create_dir(&path).unwrap();
                if target == "not-empty" {
                    fs::write(path.join("data"), "保留数据").unwrap();
                }
            }
            assert!(promote(&source, &path).is_err(), "目标 {target} 不得被覆盖");
            assert_eq!(
                fs::read_to_string(source.join("data")).unwrap(),
                "待落位数据"
            );
        }
        let target = temp.path().join("new-target");
        promote(&source, &target).unwrap();
        assert_eq!(
            fs::read_to_string(target.join("data")).unwrap(),
            "待落位数据"
        );
        assert!(!source.exists());
    }
}
