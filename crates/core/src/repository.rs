use crate::migration::{self, History};
use crate::store::{
    digest, hash_file, member, new_destination, owner, promote, read_blob, read_file, read_json,
    regular_directory, regular_file, sync_directory, valid_hash, write_blob, write_json,
};
use crate::transfer::TransferState;
use crate::{
    Attachment, Command, Error, GitChange, GitLog, GitOperation, GitResolution, GitView, Id,
    Member, Mode, Receipt, Request, Result, Role, State, Upload, WorkspaceInfo,
};
use crate::{MigrationOrigin, MigrationReport};
use crate::{TransferActivation, TransferPhase, TransferReceipt, TransferRole, TransferStatus};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command as Process, Output, Stdio};
use std::time::Duration;
const MANIFEST_LIMIT: u64 = 64 * 1024 * 1024;

mod handoff;
mod remote;
mod transport;
pub use handoff::GitTransferView;
pub use remote::{GitPush, GitPushFailure, GitSync};
pub use transport::GitTransport;
mod review;
pub use review::{GitDecisionReview, GitReview};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitSave {
    pub receipt: Receipt,
    pub indexed: bool,
    pub warning: Option<String>,
}

pub struct GitWorkspace {
    root: PathBuf,
    info: WorkspaceInfo,
    device: Id,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    info: WorkspaceInfo,
    genesis: State,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    origins: Vec<MigrationOrigin>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    clients: BTreeMap<Id, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    transfer: Option<TransferState>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportPlan {
    schema: u32,
    files: BTreeMap<String, String>,
}

struct Loaded {
    log: GitLog,
    files: BTreeMap<String, String>,
    origins: Vec<MigrationOrigin>,
    clients: BTreeMap<Id, String>,
    transfer: Option<TransferState>,
    handoffs: handoff::Handoffs,
}

impl Loaded {
    fn ensure_writable(&self) -> Result<()> {
        self.ensure_transport()?;
        self.handoffs.ensure_writable()
    }

    fn ensure_transport(&self) -> Result<()> {
        if let Some(control) = &self.transfer {
            control.ensure_writable()?;
        }
        Ok(())
    }
}

impl GitWorkspace {
    pub fn create(path: impl AsRef<Path>, id: Id, name: String, owner: Member) -> Result<Self> {
        let log = GitLog::new(id, name, owner)?;
        let destination = path.as_ref();
        let stage = new_destination(destination)?;
        initialize_history(
            stage.path(),
            &History {
                log,
                origins: vec![],
                clients: BTreeMap::new(),
            },
            None,
        )?;
        promote(stage.path(), destination)?;
        Self::open(destination)
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let root = fs::canonicalize(path).map_err(storage)?;
        regular_directory(&root.join(".git"))?;
        let top = git(&root, &args(&["rev-parse", "--show-toplevel"]), None, None)?;
        if fs::canonicalize(
            String::from_utf8(top)
                .map_err(storage)?
                .trim_end_matches(['\n', '\r']),
        )
        .map_err(storage)?
            != root
        {
            return invalid("必须打开独立的研流业务数据仓库");
        }
        let metadata = root.join(".git/farflow");
        if !metadata.exists() {
            fs::create_dir_all(&metadata).map_err(storage)?;
        }
        regular_directory(&metadata)?;
        let mut connection = lock_connection(&root)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        recover(&root)?;
        // Git 不保存空目录；普通 clone 仍应能打开空空间或尚无附件的空间。
        for name in ["operations", "attachments"] {
            let directory = root.join(name);
            match fs::create_dir(&directory) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(storage(error)),
            }
            regular_directory(&directory)?;
        }
        let loaded = load(&root, true)?;
        let device_file = metadata.join("device.json");
        let device = if device_file.exists() {
            read_json::<Id>(&device_file, 128)?
        } else {
            let id = Id::new_v4();
            atomic_json(&device_file, &id)?;
            id
        };
        if device.is_nil() {
            return invalid("本机设备身份无效");
        }
        let info = loaded.log.info();
        let view = loaded.log.view()?;
        let _ = save_index(&root, &loaded.files, &view);
        transaction.commit().map_err(storage)?;
        Ok(Self { root, info, device })
    }

    pub fn info(&self) -> &WorkspaceInfo {
        &self.info
    }

    // 仓库成员列表用于本机身份选择，不能把自动选中所有者当作登录。
    pub fn identities(&self) -> Result<Vec<Member>> {
        self.locked(|| {
            let loaded = load(&self.root, true)?;
            Ok(indexed_view(&self.root, &loaded)?
                .state
                .members
                .into_values()
                .collect())
        })
    }

    pub fn export_copy(&self, actor: Id, destination: impl AsRef<Path>) -> Result<MigrationReport> {
        self.locked(|| {
            let loaded = load(&self.root, true)?;
            loaded.ensure_writable()?;
            migration::export(
                History {
                    log: loaded.log,
                    origins: loaded.origins,
                    clients: loaded.clients,
                },
                Mode::Git,
                actor,
                &self.root,
                destination.as_ref(),
            )
        })
    }

    pub fn import_copy(
        source: impl AsRef<Path>,
        destination: impl AsRef<Path>,
        new_id: Id,
    ) -> Result<(Self, MigrationReport)> {
        migration::check_destination(source.as_ref(), destination.as_ref())?;
        let prepared = migration::prepare(source.as_ref())?;
        let history = prepared.copied_history(new_id)?;
        let stage = new_destination(destination.as_ref())?;
        let path = stage.path().to_owned();
        initialize_history(&path, &history, None)?;
        migration::copy_blobs(prepared.directory.path(), &path, &history)?;
        let verified = Self::open(&path)?;
        let mut report = prepared.package.report(Some(verified.info().clone()))?;
        report.origins = history.origins;
        drop(verified);
        sync_directory(&path)?;
        promote(&path, destination.as_ref())?;
        Ok((Self::open(destination)?, report))
    }

    pub fn snapshot(&self, actor: Id) -> Result<GitView> {
        self.locked(|| {
            let loaded = load(&self.root, true)?;
            let view = indexed_view(&self.root, &loaded)?;
            member(&view.state, actor)?;
            Ok(view)
        })
    }

    pub fn transfer_status(&self, actor: Id) -> Result<Option<TransferStatus>> {
        self.locked(|| {
            let loaded = load(&self.root, true)?;
            loaded
                .handoffs
                .authorize_read(&handoff::loaded_history(&loaded), actor)?;
            if let Some(status) = loaded.handoffs.current_status()? {
                return Ok(Some(status));
            }
            Ok(loaded.transfer.map(|state| state.status))
        })
    }

    pub fn stage_transfer(
        source: impl AsRef<Path>,
        destination: impl AsRef<Path>,
        actor: Id,
        at: i64,
    ) -> Result<(Self, MigrationReport, TransferReceipt)> {
        migration::check_destination(source.as_ref(), destination.as_ref())?;
        let prepared = migration::prepare(source.as_ref())?;
        let history = prepared.transfer_history(Mode::Git)?;
        owner(&history.validate()?, actor)?;
        let Some(plan) = prepared.transfer_plan().cloned() else {
            return invalid("缺少迁移计划");
        };
        if fs::symlink_metadata(destination.as_ref()).is_ok() {
            let workspace = Self::open(destination)?;
            let status = workspace.locked(|| {
                let loaded = load(&workspace.root, true)?;
                owner(&loaded.log.view()?.state, actor)?;
                Ok(loaded.transfer.map(|control| control.status))
            })?;
            if status
                .as_ref()
                .is_none_or(|s| s.role != TransferRole::Target || s.plan != plan)
            {
                return Err(Error::Conflict("目标目录已属于不同空间或迁移".into()));
            }
            let report = prepared.package.report(Some(workspace.info().clone()))?;
            return Ok((workspace, report, plan.receipt()?));
        }
        let control = TransferState::stage(plan.clone(), &history, Mode::Git, actor, at)?;
        let stage = new_destination(destination.as_ref())?;
        initialize_history(stage.path(), &history, Some(control))?;
        migration::copy_blobs(prepared.directory.path(), stage.path(), &history)?;
        let checked = Self::open(stage.path())?;
        let report = prepared.package.report(Some(checked.info().clone()))?;
        drop(checked);
        sync_directory(stage.path())?;
        promote(stage.path(), destination.as_ref())?;
        Ok((Self::open(destination)?, report, plan.receipt()?))
    }

    pub fn activate_transfer(
        &self,
        actor: Id,
        at: i64,
        activation: &TransferActivation,
    ) -> Result<TransferStatus> {
        self.locked(|| {
            let loaded = load(&self.root, true)?;
            if loaded.handoffs.has_records() {
                return Err(Error::Conflict(
                    "空间已发起后续 Git 源迁移，请按当前计划处理".into(),
                ));
            }
            let history = History {
                log: loaded.log,
                origins: loaded.origins,
                clients: loaded.clients,
            };
            owner(&history.validate()?, actor)?;
            let Some(mut control) = loaded.transfer else {
                return invalid("目标没有暂存迁移");
            };
            let before = control.status.clone();
            control.activate(activation, actor, at)?;
            control.validate(&history, Mode::Git)?;
            if before.phase == TransferPhase::Active {
                return Ok(control.status);
            }
            if head(&self.root)?.is_some() {
                return invalid("暂存目标已有外部提交，不能改写原始清单");
            }
            let manifest = Manifest {
                info: history.log.info(),
                genesis: history.log.genesis().clone(),
                origins: history.origins,
                clients: history.clients,
                transfer: Some(control.clone()),
            };
            let mut temporary = tempfile::NamedTempFile::new_in(&self.root).map_err(storage)?;
            temporary
                .write_all(&serde_json::to_vec_pretty(&manifest).map_err(storage)?)
                .map_err(storage)?;
            temporary.as_file().sync_all().map_err(storage)?;
            regular_file(&self.root.join("workspace.json"))?;
            temporary
                .persist(self.root.join("workspace.json"))
                .map_err(|error| storage(error.error))?;
            sync_directory(&self.root)?;
            Ok(control.status)
        })
    }

    pub fn execute(&self, request: Request) -> Result<GitSave> {
        self.save(request, None, None)
    }

    pub fn execute_checked(&self, request: Request, expected_revision: u64) -> Result<GitSave> {
        self.save(request, None, Some(expected_revision))
    }

    pub fn upload(&self, upload: Upload, bytes: &[u8]) -> Result<GitSave> {
        self.save(
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
        self.locked(|| {
            let loaded = load(&self.root, true)?;
            let view = indexed_view(&self.root, &loaded)?;
            member(&view.state, actor)?;
            let attachment = view
                .state
                .attachments
                .get(&id)
                .filter(|a| !a.deleted)
                .ok_or_else(|| Error::NotFound("附件".into()))?;
            view.state.item(attachment.item_id)?;
            read_blob(&self.root, attachment)
        })
    }

    pub fn resolve(&self, resolution: GitResolution) -> Result<GitSave> {
        self.save_resolution(resolution, None)
    }

    pub fn rebuild_index(&self, actor: Id) -> Result<GitView> {
        self.locked(|| {
            let loaded = load(&self.root, true)?;
            let view = loaded.log.view()?;
            member(&view.state, actor)?;
            let path = self.root.join(".git/farflow/index.sqlite");
            for path in [
                path.clone(),
                PathBuf::from(format!("{}-wal", path.display())),
                PathBuf::from(format!("{}-shm", path.display())),
            ] {
                if fs::symlink_metadata(&path).is_ok() {
                    regular_file(&path)?;
                    fs::remove_file(path).map_err(storage)?;
                }
            }
            save_index(&self.root, &loaded.files, &view)?;
            Ok(view)
        })
    }

    pub fn merge_from(&self, actor: Id, source: impl AsRef<Path>) -> Result<GitView> {
        let source = fs::canonicalize(source).map_err(storage)?;
        // 读取对方仓库不创建其本机身份或缓存。
        let incoming = load(&source, true)?;
        self.locked(|| self.merge_loaded(actor, &source, incoming))
    }

    pub fn commit(&self, actor: Id, note: &str) -> Result<String> {
        self.locked(|| {
            let loaded = load(&self.root, true)?;
            loaded.ensure_transport()?;
            let view = loaded.log.view()?;
            writable(&view.state, actor)?;
            if note.trim().is_empty() || note.len() > 8000 || note.contains('\0') {
                return invalid("提交说明需要 1 至 8000 字节");
            }
            commit_files(&self.root, &loaded.files, note)
        })
    }

    pub fn export_pack(&self, actor: Id, destination: impl AsRef<Path>) -> Result<()> {
        let destination = destination.as_ref();
        self.locked(|| {
            let loaded = load(&self.root, true)?;
            loaded.ensure_transport()?;
            owner(&loaded.log.view()?.state, actor)?;
            export_pack_files(&self.root, &loaded.files, destination)
        })
    }

    pub fn import_pack(&self, actor: Id, source: impl AsRef<Path>) -> Result<GitView> {
        let extracted = extract_pack(source.as_ref())?;
        self.import_extracted(actor, extracted.path())
    }

    fn import_extracted(&self, actor: Id, extracted: &Path) -> Result<GitView> {
        let incoming = load(&extracted.join("data"), false)?;
        self.locked(|| {
            let local = load(&self.root, true)?;
            local.ensure_transport()?;
            incoming.ensure_transport()?;
            member(&local.log.view()?.state, actor)?;
            if local.files.get("workspace.json") != incoming.files.get("workspace.json") {
                return invalid("工作空间来源清单不同，不能直接合并");
            }
            let mut verified = local.log;
            verified.merge(&incoming.log)?;
            // 继续使用已验证的临时对象库，避免再次读取可能变化的原始文件包。
            let parent = fetch_parent(&self.root, &extracted.join("objects.git"))?;
            let view = self.merge_loaded(actor, &extracted.join("data"), incoming)?;
            remember_parent(&self.root, &parent).map_err(|error| {
                Error::Storage(format!("业务数据已导入，Git 关联待恢复：{error}"))
            })?;
            Ok(view)
        })
    }

    pub fn from_pack(source: impl AsRef<Path>, destination: impl AsRef<Path>) -> Result<Self> {
        let extracted = extract_pack(source.as_ref())?;
        Self::from_extracted(extracted.path(), destination.as_ref())
    }

    fn from_extracted(extracted: &Path, destination: &Path) -> Result<Self> {
        let incoming = load(&extracted.join("data"), false)?;
        incoming.ensure_transport()?;
        let transfer = incoming.transfer;
        let history = History {
            log: incoming.log,
            origins: incoming.origins,
            clients: incoming.clients,
        };
        let staging = new_destination(destination)?;
        let prepared = staging.path().to_owned();
        initialize_history(&prepared, &history, transfer)?;
        migration::copy_blobs(&extracted.join("data"), &prepared, &history)?;
        handoff::copy_records(&extracted.join("data"), &prepared, &incoming.files)?;
        let workspace = Self::open(&prepared)?;
        // 创建副本不冒充初始所有者；该成员可能已在后续历史中停用。
        workspace.locked(|| {
            let parent = fetch_parent(&workspace.root, &extracted.join("objects.git"))?;
            remember_parent(&workspace.root, &parent)?;
            let loaded = load(&workspace.root, true)?;
            commit_files(&workspace.root, &loaded.files, "从研流文件包创建副本")
        })?;
        drop(workspace);
        promote(&prepared, destination)?;
        Self::open(destination)
    }

    fn save(
        &self,
        request: Request,
        bytes: Option<&[u8]>,
        expected_revision: Option<u64>,
    ) -> Result<GitSave> {
        self.locked(|| {
            let mut loaded = load(&self.root, true)?;
            loaded.ensure_writable()?;
            let current = loaded.log.view()?;
            member(&current.state, request.actor_id)?;
            if expected_revision.is_some_and(|revision| revision != current.state.revision)
                && !current.state.operations.contains_key(&request.id)
            {
                return Err(Error::Conflict("工作空间已更新，请刷新后重试".into()));
            }
            let receipt = loaded.log.execute(self.device, request.clone())?;
            if let Some(bytes) = bytes {
                let Command::AddAttachment { sha256, .. } = &request.command else {
                    return invalid("附件操作类型无效");
                };
                write_blob(&self.root, sha256, bytes)?;
            }
            validate_blobs(&self.root, &loaded.log)?;
            persist_operation(&self.root, &loaded.log.operations()[&request.id])?;
            finish_save(&self.root, receipt)
        })
    }

    fn locked<T>(&self, action: impl FnOnce() -> Result<T>) -> Result<T> {
        let mut connection = lock_connection(&self.root)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        recover(&self.root)?;
        let result = action();
        transaction.commit().map_err(storage)?;
        result
    }

    fn merge_loaded(&self, actor: Id, source: &Path, incoming: Loaded) -> Result<GitView> {
        let mut local = load(&self.root, true)?;
        local.ensure_transport()?;
        incoming.ensure_transport()?;
        if local.files.get("workspace.json") != incoming.files.get("workspace.json") {
            return invalid("工作空间来源清单不同，不能直接合并");
        }
        member(&local.log.view()?.state, actor)?;
        let view = local.log.merge(&incoming.log)?;
        let metadata = self.root.join(".git/farflow");
        let staging = tempfile::tempdir_in(&metadata).map_err(storage)?;
        let mut files = BTreeMap::new();
        for (relative, hash) in &incoming.files {
            if relative == "workspace.json" {
                continue;
            }
            if let Some(existing) = local.files.get(relative) {
                if existing != hash {
                    return Err(Error::Conflict("同一不可变文件内容不同".into()));
                }
                continue;
            }
            let output = staging.path().join(relative);
            fs::create_dir_all(
                output
                    .parent()
                    .ok_or_else(|| Error::Validation("导入路径无效".into()))?,
            )
            .map_err(storage)?;
            atomic_bytes(
                &output,
                &read_file(
                    &source.join(relative),
                    if relative.starts_with("attachments/") {
                        10 * 1024 * 1024
                    } else {
                        2 * 1024 * 1024
                    },
                )?,
            )?;
            if hash_file(&output)? != *hash {
                return invalid("导入文件在复制期间变化");
            }
            files.insert(relative.clone(), hash.clone());
        }
        if files.is_empty() {
            let _ = save_index(&self.root, &local.files, &view);
            return Ok(view);
        }
        write_json(
            &staging.path().join("plan.json"),
            &ImportPlan { schema: 1, files },
        )?;
        sync_directory(staging.path())?;
        promote(staging.path(), &metadata.join("pending"))?;
        recover(&self.root)?;
        let result = load(&self.root, true)?;
        let view = result.log.view()?;
        let _ = save_index(&self.root, &result.files, &view);
        Ok(view)
    }
}

fn export_pack_files(
    root: &Path,
    files: &BTreeMap<String, String>,
    destination: &Path,
) -> Result<()> {
    migration::check_destination(root, destination)?;
    if fs::symlink_metadata(destination).is_ok() {
        return Err(Error::Conflict("文件包目标已存在".into()));
    }
    commit_files(root, files, "FarFlow 文件包快照")?;
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let stage = new_destination(destination)?;
    let bundle = stage.path().join("workspace.farflowpack");
    git(
        root,
        &[
            "bundle".into(),
            "create".into(),
            bundle.as_os_str().to_owned(),
            "HEAD".into(),
        ],
        None,
        None,
    )?;
    let file = File::options()
        .read(true)
        .write(true)
        .open(&bundle)
        .map_err(storage)?;
    file.sync_all().map_err(storage)?;
    let temporary = tempfile::NamedTempFile::from_parts(
        file,
        tempfile::TempPath::try_from_path(bundle).map_err(storage)?,
    );
    temporary.persist_noclobber(destination).map_err(|error| {
        if error.error.kind() == std::io::ErrorKind::AlreadyExists {
            Error::Conflict("文件包目标已存在".into())
        } else {
            storage(error.error)
        }
    })?;
    sync_directory(parent)
}

fn initialize_history(
    root: &Path,
    history: &History,
    transfer: Option<TransferState>,
) -> Result<()> {
    write_json(
        &root.join("workspace.json"),
        &Manifest {
            info: history.log.info(),
            genesis: history.log.genesis().clone(),
            origins: history.origins.clone(),
            clients: history.clients.clone(),
            transfer,
        },
    )?;
    fs::create_dir(root.join("operations")).map_err(storage)?;
    fs::create_dir(root.join("attachments")).map_err(storage)?;
    for operation in history.log.operations().values() {
        persist_operation(root, operation)?;
    }
    git(
        root,
        &args(&[
            "init",
            "--quiet",
            "--initial-branch=main",
            "--object-format=sha1",
        ]),
        None,
        None,
    )?;
    sync_directory(root)
}

fn fetch_parent(root: &Path, objects: &Path) -> Result<String> {
    git(
        root,
        &[
            "fetch".into(),
            "--quiet".into(),
            "--no-tags".into(),
            "--".into(),
            objects.as_os_str().to_owned(),
            "refs/heads/main".into(),
        ],
        None,
        None,
    )?;
    Ok(
        String::from_utf8(git(root, &args(&["rev-parse", "FETCH_HEAD"]), None, None)?)
            .map_err(storage)?
            .trim()
            .to_owned(),
    )
}

fn lock_connection(root: &Path) -> Result<Connection> {
    regular_directory(&root.join(".git"))?;
    let metadata = root.join(".git/farflow");
    regular_directory(&metadata)?;
    let path = metadata.join("lock.sqlite");
    if fs::symlink_metadata(&path).is_ok() {
        regular_file(&path)?;
    }
    let connection = Connection::open(path).map_err(storage)?;
    connection
        .busy_timeout(Duration::from_secs(10))
        .map_err(storage)?;
    connection
        .execute_batch("CREATE TABLE IF NOT EXISTS workspace_lock (id INTEGER PRIMARY KEY);")
        .map_err(storage)?;
    Ok(connection)
}

fn load(root: &Path, check_git: bool) -> Result<Loaded> {
    regular_directory(root)?;
    if check_git {
        regular_directory(&root.join(".git"))?;
        if String::from_utf8(git(
            root,
            &args(&["symbolic-ref", "--quiet", "HEAD"]),
            None,
            None,
        )?)
        .map_err(storage)?
        .trim()
            != "refs/heads/main"
        {
            return invalid("研流数据仓库必须使用 main 分支");
        }
    }
    for entry in fs::read_dir(root).map_err(storage)? {
        let entry = entry.map_err(storage)?;
        let name = entry.file_name();
        if ![
            ".git",
            "workspace.json",
            "operations",
            "attachments",
            "transfers",
        ]
        .iter()
        .any(|v| name == *v)
        {
            return invalid("数据仓库含有格式之外的文件");
        }
    }
    let manifest: Manifest = canonical_json(&root.join("workspace.json"), MANIFEST_LIMIT)?;
    if manifest.info.mode != Mode::Git || manifest.info.schema != 1 {
        return invalid("工作空间不是受支持的 Git 格式");
    }
    let mut operations = BTreeMap::new();
    let mut files = BTreeMap::from([(
        "workspace.json".into(),
        hash_file(&root.join("workspace.json"))?,
    )]);
    let directory = root.join("operations");
    regular_directory(&directory)?;
    for prefix in fs::read_dir(&directory).map_err(storage)? {
        let prefix = prefix.map_err(storage)?;
        regular_directory(&prefix.path())?;
        for file in fs::read_dir(prefix.path()).map_err(storage)? {
            let file = file.map_err(storage)?;
            let operation: GitOperation = canonical_json(&file.path(), 2 * 1024 * 1024)?;
            let relative = operation_path(operation.id);
            if file.path() != root.join(&relative) {
                return invalid("操作文件路径与标识不一致");
            }
            if operations.insert(operation.id, operation).is_some() {
                return invalid("重复操作文件");
            }
            files.insert(relative, hash_file(&file.path())?);
        }
    }
    let log = GitLog::from_parts(manifest.genesis, operations)?;
    // 普通冲突可在仓库内处理；来源链本身仍须验证，不能借迁移接管另一空间。
    migration::validate_origins(&log, &manifest.origins, &manifest.clients)?;
    if let Some(control) = &manifest.transfer {
        control.validate(
            &History {
                log: log.clone(),
                origins: manifest.origins.clone(),
                clients: manifest.clients.clone(),
            },
            Mode::Git,
        )?;
    }
    if manifest.info != log.info() {
        return invalid("工作空间清单与初始数据不一致");
    }
    regular_directory(&root.join("attachments"))?;
    for attachment in attachments(&log).values() {
        read_blob(root, attachment)?;
        let relative = blob_path(&attachment.sha256);
        files.insert(relative.clone(), hash_file(&root.join(relative))?);
    }
    let handoffs = handoff::Handoffs::read(
        root,
        &History {
            log: log.clone(),
            origins: manifest.origins.clone(),
            clients: manifest.clients.clone(),
        },
        &mut files,
    )?;
    if check_git {
        verify_committed(root, &files)?;
    }
    Ok(Loaded {
        log,
        files,
        origins: manifest.origins,
        clients: manifest.clients,
        transfer: manifest.transfer,
        handoffs,
    })
}

pub(crate) fn attachments(log: &GitLog) -> BTreeMap<(String, u64), Attachment> {
    fn collect(
        command: &Command,
        actor: Id,
        at: i64,
        out: &mut BTreeMap<(String, u64), Attachment>,
    ) {
        match command {
            Command::AddAttachment {
                id,
                item_id,
                name,
                mime,
                sha256,
                size,
            } => {
                out.insert(
                    (sha256.clone(), *size),
                    Attachment {
                        id: *id,
                        item_id: *item_id,
                        name: name.clone(),
                        mime: mime.clone(),
                        sha256: sha256.clone(),
                        size: *size,
                        actor_id: actor,
                        at,
                        deleted: false,
                    },
                );
            }
            Command::Batch { commands } => {
                for command in commands {
                    collect(command, actor, at, out);
                }
            }
            _ => {}
        }
    }
    let mut out = BTreeMap::new();
    for operation in log.operations().values() {
        match &operation.change {
            GitChange::Apply { request } => {
                collect(&request.command, request.actor_id, request.at, &mut out)
            }
            GitChange::Resolve { resolution } => {
                if let Some(command) = &resolution.command {
                    collect(command, resolution.actor_id, resolution.at, &mut out);
                }
            }
        }
    }
    out
}

fn validate_blobs(root: &Path, log: &GitLog) -> Result<()> {
    for attachment in attachments(log).values() {
        read_blob(root, attachment)?;
    }
    Ok(())
}

fn indexed_view(root: &Path, loaded: &Loaded) -> Result<GitView> {
    let source = digest(&serde_json::to_vec(&loaded.files).map_err(storage)?);
    let cached = (|| -> Result<Option<GitView>> {
        let connection = index_connection(root)?;
        let row: Option<(String, String, String)> = connection
            .query_row(
                "SELECT source,checksum,data FROM projection WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(storage)?;
        if let Some((hash, checksum, data)) = row {
            if hash == source && digest(data.as_bytes()) == checksum {
                return Ok(Some(serde_json::from_str(&data).map_err(storage)?));
            }
        }
        Ok(None)
    })();
    if let Ok(Some(view)) = cached {
        return Ok(view);
    }
    let view = loaded.log.view()?;
    let _ = save_index(root, &loaded.files, &view);
    Ok(view)
}

fn index_connection(root: &Path) -> Result<Connection> {
    let path = root.join(".git/farflow/index.sqlite");
    if fs::symlink_metadata(&path).is_ok() {
        regular_file(&path)?;
    }
    let connection = Connection::open(path).map_err(storage)?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(storage)?;
    connection.execute_batch("CREATE TABLE IF NOT EXISTS projection (id INTEGER PRIMARY KEY CHECK(id=1),source TEXT NOT NULL,checksum TEXT NOT NULL,data TEXT NOT NULL);").map_err(storage)?;
    Ok(connection)
}

fn save_index(root: &Path, files: &BTreeMap<String, String>, view: &GitView) -> Result<()> {
    let connection = index_connection(root)?;
    let source = digest(&serde_json::to_vec(files).map_err(storage)?);
    let data = serde_json::to_string(view).map_err(storage)?;
    connection.execute("INSERT INTO projection VALUES (1,?1,?2,?3) ON CONFLICT(id) DO UPDATE SET source=excluded.source,checksum=excluded.checksum,data=excluded.data",params![source,digest(data.as_bytes()),data]).map_err(storage)?;
    Ok(())
}

fn finish_save(root: &Path, receipt: Receipt) -> Result<GitSave> {
    let loaded = load(root, true)?;
    let view = loaded.log.view()?;
    match save_index(root, &loaded.files, &view) {
        Ok(()) => Ok(GitSave {
            receipt,
            indexed: true,
            warning: None,
        }),
        Err(error) => Ok(GitSave {
            receipt,
            indexed: false,
            warning: Some(format!("操作已保存在本机，索引待恢复：{error}")),
        }),
    }
}

fn persist_operation(root: &Path, operation: &GitOperation) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(operation).map_err(storage)?;
    // 写入和重开采用同一上限，拒绝操作时不能留下无法重放的真源文件。
    if bytes.len() > 2 * 1024 * 1024 {
        return invalid("单次 Git 操作超过 2 MiB，请分批保存");
    }
    let relative = operation_path(operation.id);
    let path = root.join(relative);
    let parent = path
        .parent()
        .ok_or_else(|| Error::Validation("操作路径无效".into()))?;
    regular_directory(&root.join("operations"))?;
    if !parent.exists() {
        fs::create_dir(parent).map_err(storage)?;
        sync_directory(&root.join("operations"))?;
    }
    regular_directory(parent)?;
    if path.exists() {
        let old: GitOperation = canonical_json(&path, 2 * 1024 * 1024)?;
        if serde_json::to_value(old).map_err(storage)?
            != serde_json::to_value(operation).map_err(storage)?
        {
            return Err(Error::Conflict("同一操作文件内容不同".into()));
        }
        return Ok(());
    }
    atomic_bytes(&path, &bytes)
}

fn recover(root: &Path) -> Result<()> {
    let pending = root.join(".git/farflow/pending");
    if !pending.exists() {
        return Ok(());
    }
    regular_directory(&pending)?;
    let plan: ImportPlan = canonical_json(&pending.join("plan.json"), 32 * 1024 * 1024)?;
    if plan.schema != 1 {
        return invalid("待恢复导入格式不受支持");
    }
    // 在写入任何目标文件前核对整个恢复记录，避免损坏包留下半次成功。
    for (relative, hash) in &plan.files {
        if !safe_path(relative) || relative == "workspace.json" || !valid_hash(hash) {
            return invalid("导入恢复记录含非法路径");
        }
        validate_parent_chain(&pending, relative)?;
        if hash_file(&pending.join(relative))? != *hash {
            return invalid("待恢复导入文件损坏");
        }
        let target = root.join(relative);
        if target.exists() {
            validate_parent_chain(root, relative)?;
            if hash_file(&target)? != *hash {
                return Err(Error::Conflict("导入目标与恢复记录不同".into()));
            }
        }
    }
    validate_overlay(root, &pending, &plan)?;
    for (relative, _) in plan
        .files
        .iter()
        .filter(|(p, _)| p.starts_with("attachments/"))
        .chain(
            plan.files
                .iter()
                .filter(|(p, _)| p.starts_with("operations/")),
        )
        .chain(
            plan.files
                .iter()
                .filter(|(p, _)| p.starts_with("transfers/")),
        )
    {
        let output = root.join(relative);
        if output.exists() {
            continue;
        }
        let parent = output
            .parent()
            .ok_or_else(|| Error::Validation("导入路径无效".into()))?;
        if !parent.exists() {
            fs::create_dir_all(parent).map_err(storage)?;
        }
        validate_parent_chain(root, relative)?;
        atomic_bytes(
            &output,
            &read_file(
                &pending.join(relative),
                if relative.starts_with("attachments/") {
                    10 * 1024 * 1024
                } else {
                    2 * 1024 * 1024
                },
            )?,
        )?;
    }
    if root.join("transfers").exists() {
        sync_directory(&root.join("transfers"))?;
        sync_directory(root)?;
    }
    load(root, true)?;
    fs::remove_dir_all(&pending).map_err(storage)?;
    sync_directory(&root.join(".git/farflow"))
}

fn operation_path(id: Id) -> String {
    let id = id.to_string();
    format!("operations/{}/{}.json", &id[..2], id)
}

fn validate_overlay(root: &Path, pending: &Path, plan: &ImportPlan) -> Result<()> {
    let stage = tempfile::tempdir_in(root.join(".git/farflow")).map_err(storage)?;
    fs::copy(
        root.join("workspace.json"),
        stage.path().join("workspace.json"),
    )
    .map_err(storage)?;
    for category in ["operations", "attachments", "transfers"] {
        let directory = root.join(category);
        if category == "transfers" && !directory.exists() {
            continue;
        }
        regular_directory(&directory)?;
        fs::create_dir(stage.path().join(category)).map_err(storage)?;
        for prefix in fs::read_dir(directory).map_err(storage)? {
            let prefix = prefix.map_err(storage)?;
            regular_directory(&prefix.path())?;
            for file in fs::read_dir(prefix.path()).map_err(storage)? {
                let file = file.map_err(storage)?;
                regular_file(&file.path())?;
                let relative = Path::new(category)
                    .join(prefix.file_name())
                    .join(file.file_name());
                let relative = relative
                    .to_str()
                    .ok_or_else(|| Error::Validation("数据路径编码无效".into()))?;
                if !safe_path(relative) {
                    return invalid("数据目录含非法文件名");
                }
                let output = stage.path().join(relative);
                fs::create_dir_all(
                    output
                        .parent()
                        .ok_or_else(|| Error::Validation("恢复路径无效".into()))?,
                )
                .map_err(storage)?;
                fs::hard_link(file.path(), output).map_err(storage)?;
            }
        }
    }
    for relative in plan.files.keys() {
        let output = stage.path().join(relative);
        if !output.exists() {
            fs::create_dir_all(
                output
                    .parent()
                    .ok_or_else(|| Error::Validation("恢复路径无效".into()))?,
            )
            .map_err(storage)?;
            fs::hard_link(pending.join(relative), output).map_err(storage)?;
        }
    }
    let complete = load(stage.path(), false)?;
    if let Some(head) = head(root)? {
        let mut committed = BTreeMap::new();
        for (path, _) in tree(root, &head)? {
            if !complete.files.contains_key(&path) {
                return invalid("恢复数据缺少已提交记录");
            }
            committed.insert(path.clone(), hash_file(&root.join(path))?);
        }
        verify_committed(root, &committed)?;
    }
    Ok(())
}
fn blob_path(hash: &str) -> String {
    format!("attachments/{}/{}", &hash[..2], hash)
}
fn safe_path(path: &str) -> bool {
    if path == "workspace.json" {
        return true;
    }
    let parts: Vec<_> = path.split('/').collect();
    if parts.len() != 3 {
        return false;
    }
    match parts[0] {
        "transfers" => parts[2]
            .strip_suffix(".json")
            .and_then(|id| Id::parse_str(id).ok())
            .is_some_and(|id| handoff::record_path(id) == path),
        "operations" => parts[2]
            .strip_suffix(".json")
            .and_then(|id| Id::parse_str(id).ok())
            .is_some_and(|id| operation_path(id) == path),
        "attachments" => valid_hash(parts[2]) && parts[1] == &parts[2][..2],
        _ => false,
    }
}

fn validate_parent_chain(root: &Path, relative: &str) -> Result<()> {
    if !safe_path(relative) {
        return invalid("数据路径不受支持");
    }
    let mut path = root.to_path_buf();
    let parts: Vec<_> = relative.split('/').collect();
    for part in &parts[..parts.len() - 1] {
        path.push(part);
        regular_directory(&path)?;
    }
    Ok(())
}

fn canonical_json<T: serde::de::DeserializeOwned + Serialize>(
    path: &Path,
    limit: u64,
) -> Result<T> {
    let value: Value = serde_json::from_slice(&read_file(path, limit)?).map_err(storage)?;
    let decoded: T = serde_json::from_value(value.clone()).map_err(storage)?;
    if serde_json::to_value(&decoded).map_err(storage)? != value {
        return invalid("数据字段不完整或不符合规范格式");
    }
    Ok(decoded)
}

fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    atomic_bytes(path, &serde_json::to_vec_pretty(value).map_err(storage)?)
}
fn atomic_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Validation("文件路径无效".into()))?;
    regular_directory(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(storage)?;
    temporary.write_all(bytes).map_err(storage)?;
    temporary.as_file().sync_all().map_err(storage)?;
    temporary
        .persist_noclobber(path)
        .map_err(|error| storage(error.error))?;
    sync_directory(parent)
}

fn args(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}
fn raw_git(
    root: &Path,
    arguments: &[OsString],
    input: Option<&[u8]>,
    index: Option<&Path>,
) -> Result<Output> {
    raw_git_config(root, arguments, input, index, false)
}

fn raw_git_config(
    root: &Path,
    arguments: &[OsString],
    input: Option<&[u8]>,
    index: Option<&Path>,
    user_auth: bool,
) -> Result<Output> {
    let mut process = git_process(root, arguments, input.is_some(), index, user_auth);
    let mut child = process.spawn().map_err(storage)?;
    if let Some(input) = input {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Storage("无法打开 Git 输入".into()))?;
        std::thread::scope(|scope| {
            let writer = scope.spawn(move || stdin.write_all(input));
            let output = child.wait_with_output().map_err(storage)?;
            let written = writer
                .join()
                .map_err(|_| Error::Storage("Git 输入线程异常".into()))?;
            if output.status.success() {
                written.map_err(storage)?;
            }
            Ok(output)
        })
    } else {
        child.wait_with_output().map_err(storage)
    }
}

fn git_process(
    root: &Path,
    arguments: &[OsString],
    input: bool,
    index: Option<&Path>,
    user_auth: bool,
) -> Process {
    let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let mut process = Process::new("git");
    process.arg("-C").arg(root).args([
        "-c",
        &format!("core.hooksPath={null}"),
        "-c",
        "core.fsmonitor=false",
        "-c",
        "commit.gpgsign=false",
        "-c",
        "protocol.file.allow=always",
        "-c",
        "protocol.ext.allow=never",
        "-c",
        "user.name=FarFlow",
        "-c",
        "user.email=farflow@localhost",
    ]);
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CONFIG",
        "GIT_CONFIG_COUNT",
        "GIT_EXTERNAL_DIFF",
    ] {
        process.env_remove(key);
    }
    if !user_auth {
        process
            .env("GIT_CONFIG_GLOBAL", null)
            .env("GIT_CONFIG_NOSYSTEM", "1");
    }
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_TRACE") {
            process.env_remove(key);
        }
    }
    process.env_remove("GIT_CURL_VERBOSE");
    process
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0");
    if let Some(index) = index {
        process.env("GIT_INDEX_FILE", index);
    }
    process
        .args(arguments)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if input { Stdio::piped() } else { Stdio::null() });
    process
}

fn git(
    root: &Path,
    arguments: &[OsString],
    input: Option<&[u8]>,
    index: Option<&Path>,
) -> Result<Vec<u8>> {
    let output = raw_git(root, arguments, input, index)?;
    if !output.status.success() {
        return Err(Error::Storage(format!(
            "Git 操作失败：{}",
            String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(2000)
                .collect::<String>()
        )));
    }
    Ok(output.stdout)
}

fn tree(root: &Path, revision: &str) -> Result<BTreeMap<String, String>> {
    let bytes = git(
        root,
        &[
            "ls-tree".into(),
            "-rz".into(),
            "--full-tree".into(),
            revision.into(),
        ],
        None,
        None,
    )?;
    let mut files = BTreeMap::new();
    for record in bytes.split(|c| *c == 0).filter(|v| !v.is_empty()) {
        let record = std::str::from_utf8(record).map_err(storage)?;
        let (header, path) = record
            .split_once('\t')
            .ok_or_else(|| Error::Validation("Git 文件树格式无效".into()))?;
        let fields: Vec<_> = header.split(' ').collect();
        if fields.len() != 3
            || fields[0] != "100644"
            || fields[1] != "blob"
            || !object_id(fields[2])
            || !safe_path(path)
        {
            return invalid("Git 数据树含不支持的路径、文件类型或权限");
        }
        files.insert(path.into(), fields[2].into());
    }
    Ok(files)
}

fn head(root: &Path) -> Result<Option<String>> {
    let output = raw_git(root, &args(&["rev-parse", "--verify", "HEAD"]), None, None)?;
    if output.status.success() {
        let id = String::from_utf8(output.stdout)
            .map_err(storage)?
            .trim()
            .to_owned();
        if !object_id(&id) {
            return invalid("Git 提交标识无效");
        }
        Ok(Some(id))
    } else {
        Ok(None)
    }
}

fn working_objects(
    root: &Path,
    files: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    let input = files.keys().cloned().collect::<Vec<_>>().join("\n") + "\n";
    let output = git(
        root,
        &args(&["hash-object", "-w", "--no-filters", "--stdin-paths"]),
        Some(input.as_bytes()),
        None,
    )?;
    let text = String::from_utf8(output).map_err(storage)?;
    let objects: Vec<_> = text.lines().collect();
    if objects.len() != files.len() || objects.iter().any(|id| !object_id(id)) {
        return invalid("Git 内容对象数量不一致");
    }
    Ok(files
        .keys()
        .cloned()
        .zip(objects.into_iter().map(str::to_owned))
        .collect())
}

fn verify_committed(root: &Path, files: &BTreeMap<String, String>) -> Result<()> {
    if let Some(head) = head(root)? {
        let current = working_objects(root, files)?;
        for (path, id) in tree(root, &head)? {
            if current.get(&path) != Some(&id) {
                return Err(Error::Conflict(format!(
                    "已提交的不可变记录被改写或移除：{path}"
                )));
            }
        }
    }
    Ok(())
}

fn remember_parent(root: &Path, id: &str) -> Result<()> {
    if !object_id(id) {
        return invalid("导入提交标识无效");
    }
    if let Some(head) = head(root)? {
        let output = raw_git(
            root,
            &[
                "merge-base".into(),
                "--is-ancestor".into(),
                id.into(),
                head.into(),
            ],
            None,
            None,
        )?;
        if output.status.success() {
            return Ok(());
        }
        if output.status.code() != Some(1) {
            return invalid("无法核对导入提交的祖先关系");
        }
    }
    let path = root.join(".git/farflow/parents.json");
    let mut parents = if path.exists() {
        read_json::<Vec<String>>(&path, 1024 * 1024)?
    } else {
        Vec::new()
    };
    if !parents.iter().any(|p| p == id) {
        parents.push(id.into());
    }
    let parent = path
        .parent()
        .ok_or_else(|| Error::Validation("提交记录路径无效".into()))?;
    let mut file = tempfile::NamedTempFile::new_in(parent).map_err(storage)?;
    file.write_all(&serde_json::to_vec(&parents).map_err(storage)?)
        .map_err(storage)?;
    file.as_file().sync_all().map_err(storage)?;
    file.persist(&path).map_err(|e| storage(e.error))?;
    sync_directory(parent)
}

fn commit_files(root: &Path, files: &BTreeMap<String, String>, note: &str) -> Result<String> {
    let objects = working_objects(root, files)?;
    let stage = tempfile::tempdir_in(root.join(".git/farflow")).map_err(storage)?;
    let index = stage.path().join("index");
    git(root, &args(&["read-tree", "--empty"]), None, Some(&index))?;
    let mut input = Vec::new();
    for (path, id) in &objects {
        input.extend_from_slice(format!("100644 {id}\t{path}\0").as_bytes());
    }
    git(
        root,
        &args(&["update-index", "-z", "--index-info"]),
        Some(&input),
        Some(&index),
    )?;
    let tree = String::from_utf8(git(root, &args(&["write-tree"]), None, Some(&index))?)
        .map_err(storage)?
        .trim()
        .to_owned();
    let old = head(root)?;
    let mut parents = old.iter().cloned().collect::<Vec<_>>();
    let parent_file = root.join(".git/farflow/parents.json");
    if parent_file.exists() {
        for id in read_json::<Vec<String>>(&parent_file, 1024 * 1024)? {
            if !object_id(&id) {
                return invalid("导入提交记录无效");
            }
            if !parents.contains(&id) {
                parents.push(id);
            }
        }
    }
    if let Some(old) = &old {
        let old_tree = String::from_utf8(git(
            root,
            &["rev-parse".into(), format!("{old}^{{tree}}").into()],
            None,
            None,
        )?)
        .map_err(storage)?
        .trim()
        .to_owned();
        if old_tree == tree && parents.len() == 1 {
            if parent_file.exists() {
                fs::remove_file(&parent_file).map_err(storage)?;
            }
            return Ok(old.clone());
        }
    }
    let mut command = vec!["commit-tree".into(), tree.into()];
    for parent in &parents {
        command.push("-p".into());
        command.push(parent.into());
    }
    let commit = String::from_utf8(git(root, &command, Some(note.as_bytes()), None)?)
        .map_err(storage)?
        .trim()
        .to_owned();
    git(
        root,
        &[
            "update-ref".into(),
            "refs/heads/main".into(),
            commit.clone().into(),
            old.unwrap_or_else(|| "0".repeat(40)).into(),
        ],
        None,
        None,
    )?;
    git(
        root,
        &args(&["update-index", "-z", "--index-info"]),
        Some(&input),
        None,
    )?;
    if parent_file.exists() {
        fs::remove_file(parent_file).map_err(storage)?;
    }
    Ok(commit)
}

fn extract_pack(source: &Path) -> Result<tempfile::TempDir> {
    extract_bundle(source, "HEAD")
}

fn extract_bundle(source: &Path, reference: &str) -> Result<tempfile::TempDir> {
    let source = fs::canonicalize(source).map_err(storage)?;
    regular_file(&source)?;
    if fs::metadata(&source).map_err(storage)?.len() > 1024 * 1024 * 1024 {
        return invalid("文件包超过 1 GiB");
    }
    let mut builder = tempfile::Builder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(fs::Permissions::from_mode(0o700));
    }
    let stage = builder.tempdir().map_err(storage)?;
    let bare = stage.path().join("objects.git");
    fs::create_dir(&bare).map_err(storage)?;
    git(
        &bare,
        &args(&["init", "--bare", "--quiet", "--object-format=sha1"]),
        None,
        None,
    )?;
    git(
        &bare,
        &[
            "fetch".into(),
            "--quiet".into(),
            "--no-tags".into(),
            "--".into(),
            source.as_os_str().to_owned(),
            reference.into(),
        ],
        None,
        None,
    )?;
    extract_fetched(stage)
}

fn extract_fetched(stage: tempfile::TempDir) -> Result<tempfile::TempDir> {
    let bare = stage.path().join("objects.git");
    // 首次创建副本也需检查不可变真源的历史，不能只接受看似完整的最新文件树。
    let changed = git(
        &bare,
        &args(&[
            "log",
            "--format=",
            "--max-count=1",
            "--diff-filter=DM",
            "--name-only",
            "-m",
            "--no-renames",
            "FETCH_HEAD",
            "--",
            "workspace.json",
            "operations",
            "attachments",
            "transfers",
        ]),
        None,
        None,
    )?;
    if !changed.is_empty() {
        return invalid("Git 历史改写或删除过不可变业务文件，不能作为工作空间导入");
    }
    let files = tree(&bare, "FETCH_HEAD")?;
    let tip = String::from_utf8(git(&bare, &args(&["rev-parse", "FETCH_HEAD"]), None, None)?)
        .map_err(storage)?
        .trim()
        .to_owned();
    git(
        &bare,
        &["update-ref".into(), "refs/heads/main".into(), tip.into()],
        None,
        None,
    )?;
    if !files.contains_key("workspace.json") || files.len() > 100_002 {
        return invalid("文件包缺少工作空间清单或文件过多");
    }
    let data = stage.path().join("data");
    fs::create_dir(&data).map_err(storage)?;
    fs::create_dir(data.join("operations")).map_err(storage)?;
    fs::create_dir(data.join("attachments")).map_err(storage)?;
    let mut total = 0u64;
    for (path, id) in files {
        let size = String::from_utf8(git(
            &bare,
            &["cat-file".into(), "-s".into(), id.clone().into()],
            None,
            None,
        )?)
        .map_err(storage)?
        .trim()
        .parse::<u64>()
        .map_err(storage)?;
        let limit = if path.starts_with("attachments/") {
            10 * 1024 * 1024
        } else if path == "workspace.json" {
            MANIFEST_LIMIT
        } else {
            2 * 1024 * 1024
        };
        total += size;
        if size > limit || total > 1024 * 1024 * 1024 {
            return invalid("文件包展开大小超过限制");
        }
        let bytes = git(
            &bare,
            &["cat-file".into(), "blob".into(), id.into()],
            None,
            None,
        )?;
        let output = data.join(path);
        fs::create_dir_all(
            output
                .parent()
                .ok_or_else(|| Error::Validation("文件包路径无效".into()))?,
        )
        .map_err(storage)?;
        atomic_bytes(&output, &bytes)?;
    }
    load(&data, false)?;
    Ok(stage)
}

fn object_id(id: &str) -> bool {
    id.len() == 40
        && id
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}
fn writable(state: &State, actor: Id) -> Result<()> {
    let member = member(state, actor)?;
    if member.roles.iter().any(|r| {
        [
            Role::Owner,
            Role::Manager,
            Role::Product,
            Role::Developer,
            Role::Tester,
        ]
        .contains(r)
    }) {
        Ok(())
    } else {
        Err(Error::Forbidden("只读成员不能提交仓库".into()))
    }
}
fn storage(error: impl std::fmt::Display) -> Error {
    Error::Storage(error.to_string())
}
fn invalid<T>(message: &str) -> Result<T> {
    Err(Error::Validation(message.into()))
}
