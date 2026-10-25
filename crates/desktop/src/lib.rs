use farflow_core::{
    Command, Error, GitIssue, GitPush, GitResolution, GitReview, GitTransport, GitWorkspace, Id,
    LocalWorkspace, Member, Mode, Receipt, Request, Result, Role, State, TransferStatus, Upload,
    WorkspaceInfo,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
};

mod transfer;
pub use transfer::{TransferAction, TransferResult, TransferStage, TransferStaged};

#[derive(Clone, Serialize)]
pub struct Directory {
    pub token: Id,
    pub path: PathBuf,
}

#[derive(Clone, Serialize)]
pub struct PackFile {
    pub token: Id,
    pub path: PathBuf,
}

#[derive(Clone)]
pub struct PackSource {
    path: PathBuf,
}

impl PackSource {
    fn path(&self) -> Result<&Path> {
        farflow_core::regular_file(&self.path)?;
        if std::fs::canonicalize(&self.path).map_err(storage)? != self.path {
            return Err(Error::Conflict("文件包路径已变化，请重新选择".into()));
        }
        Ok(&self.path)
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PackAction {
    Import,
    Export,
}

#[derive(Clone, Serialize)]
pub struct PackResult {
    pub action: PackAction,
    pub path: PathBuf,
    pub bytes: Option<u64>,
    pub issues: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub account: String,
    pub name: String,
    pub department: String,
}

#[derive(Serialize)]
pub struct OpenedWorkspace {
    pub handle: Id,
    pub workspace: WorkspaceInfo,
    pub path: PathBuf,
    pub identities: Vec<Member>,
}

pub struct GitDestination {
    directory: Id,
    parent: PathBuf,
    folder: String,
}

pub struct ClonedWorkspace {
    destination: GitDestination,
    workspace: WorkspaceInfo,
}

impl GitDestination {
    pub fn clone_remote(self, address: &str, control: &GitTransport) -> Result<ClonedWorkspace> {
        let path = self.new_path()?;
        let workspace = GitWorkspace::from_remote_with_control(address, &path, control)?
            .info()
            .clone();
        Ok(ClonedWorkspace {
            destination: self,
            workspace,
        })
    }

    pub fn clone_pack(self, source: PackSource) -> Result<ClonedWorkspace> {
        let workspace = GitWorkspace::from_pack(source.path()?, self.new_path()?)?
            .info()
            .clone();
        Ok(ClonedWorkspace {
            destination: self,
            workspace,
        })
    }

    fn new_path(&self) -> Result<PathBuf> {
        let path = self.path()?;
        if path.try_exists().map_err(storage)? || std::fs::symlink_metadata(&path).is_ok() {
            return Err(Error::Conflict(
                "目标目录已存在，请选择新的文件夹名称".into(),
            ));
        }
        Ok(path)
    }

    fn path(&self) -> Result<PathBuf> {
        farflow_core::regular_directory(&self.parent)?;
        if std::fs::canonicalize(&self.parent).map_err(storage)? != self.parent {
            return Err(Error::Conflict("保存目录已变化，请重新选择".into()));
        }
        Ok(self.parent.join(&self.folder))
    }
}

#[derive(Serialize)]
pub struct Snapshot {
    pub workspace: WorkspaceInfo,
    pub state: State,
    pub actor_id: Id,
    pub accounts: Vec<Member>,
    pub transfer: Option<TransferStatus>,
    pub issues: Vec<GitIssue>,
}

#[derive(Serialize)]
pub struct Saved {
    #[serde(flatten)]
    pub receipt: Receipt,
    pub warning: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachmentInput {
    pub operation_id: Id,
    pub id: Id,
    pub item_id: Id,
    pub name: String,
    pub mime: String,
}

#[derive(Clone)]
struct Binding {
    path: PathBuf,
    workspace: WorkspaceInfo,
    actor: Option<Id>,
}

pub struct GitSession {
    binding: Binding,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitRemoteAction {
    Pull,
    Push,
    Sync,
}

#[derive(Clone, Serialize)]
pub struct GitRemoteResult {
    pub push: Option<GitPush>,
    pub pending_local: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolutionInput {
    pub id: Id,
    pub token: String,
    pub targets: BTreeSet<Id>,
    pub command: Option<Command>,
    pub note: String,
    pub at: i64,
}

impl GitSession {
    pub fn export_pack(&self, path: &Path) -> Result<PackResult> {
        let actor = actor(&self.binding)?;
        let Backend::Git(workspace) = checked_backend(&self.binding)? else {
            return Err(Error::Validation("当前空间不是 Git 模式".into()));
        };
        workspace.export_pack(actor, path)?;
        Ok(PackResult {
            action: PackAction::Export,
            path: path.to_path_buf(),
            bytes: Some(std::fs::metadata(path).map_err(storage)?.len()),
            issues: None,
        })
    }

    pub fn import_pack(&self, source: PackSource) -> Result<PackResult> {
        let actor = actor(&self.binding)?;
        let Backend::Git(workspace) = checked_backend(&self.binding)? else {
            return Err(Error::Validation("当前空间不是 Git 模式".into()));
        };
        let path = source.path()?;
        let view = workspace.import_pack(actor, path)?;
        Ok(PackResult {
            action: PackAction::Import,
            path: path.to_path_buf(),
            bytes: None,
            issues: Some(view.issues.len()),
        })
    }

    pub fn remote(
        &self,
        action: GitRemoteAction,
        address: &str,
        control: &GitTransport,
    ) -> Result<GitRemoteResult> {
        let actor = actor(&self.binding)?;
        let Backend::Git(workspace) = checked_backend(&self.binding)? else {
            return Err(Error::Validation("当前空间不是 Git 模式".into()));
        };
        match action {
            GitRemoteAction::Pull => {
                workspace.pull_with_control(actor, address, control)?;
                Ok(GitRemoteResult {
                    push: None,
                    pending_local: None,
                })
            }
            GitRemoteAction::Push => Ok(GitRemoteResult {
                push: Some(workspace.push_with_control(actor, address, control)?),
                pending_local: None,
            }),
            GitRemoteAction::Sync => {
                let result = workspace.sync_with_control(actor, address, control)?;
                Ok(GitRemoteResult {
                    push: Some(result.push),
                    pending_local: Some(result.pending_local),
                })
            }
        }
    }
}

#[derive(Default)]
pub struct Desktop {
    directories: BTreeMap<Id, PathBuf>,
    packs: BTreeMap<Id, PackSource>,
    workspaces: BTreeMap<Id, Binding>,
}

enum Backend {
    Local(LocalWorkspace),
    Git(GitWorkspace),
}

impl Backend {
    fn open(path: &Path, mode: Mode) -> Result<Self> {
        match mode {
            Mode::Local => Ok(Self::Local(LocalWorkspace::open(path)?)),
            Mode::Git => Ok(Self::Git(GitWorkspace::open(path)?)),
            Mode::Service => Err(Error::Validation("服务工作空间应通过服务地址登录".into())),
        }
    }

    fn info(&self) -> &WorkspaceInfo {
        match self {
            Self::Local(workspace) => workspace.info(),
            Self::Git(workspace) => workspace.info(),
        }
    }

    fn identities(&self) -> Result<Vec<Member>> {
        match self {
            Self::Local(workspace) => workspace.identities(),
            Self::Git(workspace) => workspace.identities(),
        }
    }
}

impl Desktop {
    // 仅由原生目录对话框调用，网页拿到的是授权标识，不能任意传入文件路径。
    pub fn select_directory(&mut self, path: &Path) -> Result<Directory> {
        let path = std::fs::canonicalize(path).map_err(storage)?;
        farflow_core::regular_directory(&path)?;
        let token = Id::new_v4();
        self.directories.insert(token, path.clone());
        Ok(Directory { token, path })
    }

    pub fn select_pack(&mut self, path: &Path) -> Result<PackFile> {
        let path = std::fs::canonicalize(path).map_err(storage)?;
        farflow_core::regular_file(&path)?;
        let token = Id::new_v4();
        self.packs.insert(token, PackSource { path: path.clone() });
        Ok(PackFile { token, path })
    }

    pub fn pack_source(&self, token: Id) -> Result<PackSource> {
        self.packs
            .get(&token)
            .cloned()
            .ok_or_else(|| Error::Forbidden("文件包尚未经过系统选择授权".into()))
    }

    pub fn create(
        &mut self,
        directory: Id,
        folder: &str,
        mode: Mode,
        name: String,
        profile: Profile,
    ) -> Result<OpenedWorkspace> {
        validate_folder(folder)?;
        let parent = self.directory(directory)?;
        farflow_core::regular_directory(parent)?;
        let path = parent.join(folder);
        let owner = Member {
            id: Id::new_v4(),
            account: profile.account,
            name: profile.name,
            department: profile.department,
            roles: BTreeSet::from([Role::Owner]),
            active: true,
        };
        let id = Id::new_v4();
        match mode {
            Mode::Local => {
                LocalWorkspace::create(&path, id, name, owner)?;
            }
            Mode::Git => {
                GitWorkspace::create(&path, id, name, owner)?;
            }
            Mode::Service => {
                return Err(Error::Validation("服务工作空间应通过服务地址登录".into()))
            }
        }
        self.bind(&path, mode)
    }

    pub fn inspect(&mut self, directory: Id, mode: Mode) -> Result<OpenedWorkspace> {
        let path = self.directory(directory)?.to_path_buf();
        self.bind(&path, mode)
    }

    pub fn git_destination(&self, directory: Id, folder: &str) -> Result<GitDestination> {
        validate_folder(folder)?;
        let destination = GitDestination {
            directory,
            parent: self.directory(directory)?.to_path_buf(),
            folder: folder.into(),
        };
        destination.new_path()?;
        Ok(destination)
    }

    pub fn open_cloned(&mut self, cloned: &ClonedWorkspace) -> Result<OpenedWorkspace> {
        if self.directory(cloned.destination.directory)? != cloned.destination.parent {
            return Err(Error::Forbidden("克隆保存目录未授权".into()));
        }
        let path = cloned.destination.path()?;
        if GitWorkspace::open(&path)?.info() != &cloned.workspace {
            return Err(Error::Conflict(
                "克隆完成后的工作空间已被替换，请重新打开".into(),
            ));
        }
        self.bind(&path, Mode::Git)
    }

    pub fn select_identity(&mut self, handle: Id, actor: Id) -> Result<Snapshot> {
        let mut binding = self.binding(handle)?.clone();
        binding.actor = Some(actor);
        let snapshot = read_snapshot(&binding)?;
        self.workspaces.insert(handle, binding);
        Ok(snapshot)
    }

    pub fn snapshot(&self, handle: Id) -> Result<Snapshot> {
        read_snapshot(self.binding(handle)?)
    }

    pub fn git_session(&self, handle: Id) -> Result<GitSession> {
        let binding = self.binding(handle)?.clone();
        if binding.workspace.mode != Mode::Git {
            return Err(Error::Validation("当前空间不是 Git 模式".into()));
        }
        read_snapshot(&binding)?;
        Ok(GitSession { binding })
    }

    pub fn git_review(&self, handle: Id) -> Result<GitReview> {
        let (workspace, actor) = self.git_backend(handle)?;
        workspace.review(actor)
    }

    pub fn git_resolve(&self, handle: Id, input: ResolutionInput) -> Result<Saved> {
        let (workspace, actor_id) = self.git_backend(handle)?;
        let saved = workspace.resolve_reviewed(
            GitResolution {
                id: input.id,
                actor_id,
                at: input.at,
                targets: input.targets,
                command: input.command,
                note: input.note,
            },
            &input.token,
        )?;
        Ok(Saved {
            receipt: saved.receipt,
            warning: saved.warning,
        })
    }

    pub fn git_rebuild(&self, handle: Id) -> Result<Snapshot> {
        let (workspace, actor) = self.git_backend(handle)?;
        workspace.rebuild_index(actor)?;
        self.snapshot(handle)
    }

    fn git_backend(&self, handle: Id) -> Result<(GitWorkspace, Id)> {
        let binding = self.binding(handle)?;
        let actor = actor(binding)?;
        match checked_backend(binding)? {
            Backend::Git(workspace) => Ok((workspace, actor)),
            Backend::Local(_) => Err(Error::Validation("当前空间不是 Git 模式".into())),
        }
    }

    pub fn execute(
        &self,
        handle: Id,
        operation: Id,
        command: Command,
        expected_revision: u64,
        at: i64,
    ) -> Result<Saved> {
        let binding = self.binding(handle)?;
        let actor = actor(binding)?;
        let request = Request {
            id: operation,
            actor_id: actor,
            at,
            command,
        };
        match checked_backend(binding)? {
            Backend::Local(mut workspace) => Ok(Saved {
                receipt: workspace.execute_checked(request, expected_revision)?,
                warning: None,
            }),
            Backend::Git(workspace) => {
                let save = workspace.execute_checked(request, expected_revision)?;
                Ok(Saved {
                    receipt: save.receipt,
                    warning: save.warning,
                })
            }
        }
    }

    pub fn upload(
        &self,
        handle: Id,
        input: AttachmentInput,
        bytes: &[u8],
        at: i64,
    ) -> Result<Saved> {
        if bytes.len() > 10 * 1024 * 1024 {
            return Err(Error::Validation("单个附件不能超过 10 MiB".into()));
        }
        let binding = self.binding(handle)?;
        let upload = Upload {
            operation_id: input.operation_id,
            actor_id: actor(binding)?,
            at,
            id: input.id,
            item_id: input.item_id,
            name: input.name,
            mime: input.mime,
        };
        match checked_backend(binding)? {
            Backend::Local(mut workspace) => Ok(Saved {
                receipt: workspace.upload(upload, bytes)?,
                warning: None,
            }),
            Backend::Git(workspace) => {
                let save = workspace.upload(upload, bytes)?;
                Ok(Saved {
                    receipt: save.receipt,
                    warning: save.warning,
                })
            }
        }
    }

    pub fn attachment(&self, handle: Id, id: Id) -> Result<Vec<u8>> {
        let binding = self.binding(handle)?;
        let actor = actor(binding)?;
        match checked_backend(binding)? {
            Backend::Local(workspace) => workspace.read_attachment(actor, id),
            Backend::Git(workspace) => workspace.read_attachment(actor, id),
        }
    }

    pub fn close(&mut self, handle: Id) {
        self.workspaces.remove(&handle);
    }

    fn directory(&self, token: Id) -> Result<&Path> {
        self.directories
            .get(&token)
            .map(PathBuf::as_path)
            .ok_or_else(|| Error::Forbidden("保存目录尚未经过系统选择授权".into()))
    }

    fn binding(&self, handle: Id) -> Result<&Binding> {
        self.workspaces
            .get(&handle)
            .ok_or_else(|| Error::Forbidden("工作空间尚未打开或已经关闭".into()))
    }

    fn bind(&mut self, path: &Path, mode: Mode) -> Result<OpenedWorkspace> {
        let backend = Backend::open(path, mode)?;
        let workspace = backend.info().clone();
        let identities = backend.identities()?;
        let path = std::fs::canonicalize(path).map_err(storage)?;
        let handle = Id::new_v4();
        self.workspaces.insert(
            handle,
            Binding {
                path: path.clone(),
                workspace: workspace.clone(),
                actor: None,
            },
        );
        Ok(OpenedWorkspace {
            handle,
            workspace,
            path,
            identities,
        })
    }
}

fn checked_backend(binding: &Binding) -> Result<Backend> {
    let backend = Backend::open(&binding.path, binding.workspace.mode)?;
    if backend.info() != &binding.workspace {
        return Err(Error::Conflict(
            "目录中的工作空间已被替换，请重新打开".into(),
        ));
    }
    Ok(backend)
}

fn actor(binding: &Binding) -> Result<Id> {
    binding
        .actor
        .ok_or_else(|| Error::Forbidden("请先选择当前成员身份".into()))
}

fn read_snapshot(binding: &Binding) -> Result<Snapshot> {
    let actor_id = actor(binding)?;
    let (state, transfer, issues) = match checked_backend(binding)? {
        Backend::Local(workspace) => (
            workspace.snapshot(actor_id)?,
            workspace.transfer_status(actor_id)?,
            vec![],
        ),
        Backend::Git(workspace) => {
            let view = workspace.snapshot(actor_id)?;
            (
                view.state,
                workspace.transfer_status(actor_id)?,
                view.issues.into_values().collect(),
            )
        }
    };
    Ok(Snapshot {
        workspace: binding.workspace.clone(),
        state,
        actor_id,
        accounts: vec![],
        transfer,
        issues,
    })
}

fn validate_folder(folder: &str) -> Result<()> {
    let components: Vec<_> = Path::new(folder).components().collect();
    let stem = folder.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = ["CON", "PRN", "AUX", "NUL"].contains(&stem.as_str())
        || ["COM", "LPT"].iter().any(|prefix| {
            stem.strip_prefix(prefix).is_some_and(|suffix| {
                suffix.len() == 1 && ('1'..='9').contains(&suffix.chars().next().unwrap())
            })
        });
    if folder.is_empty()
        || folder.len() > 200
        || folder.trim() != folder
        || folder.ends_with('.')
        || components.len() != 1
        || !matches!(components[0], Component::Normal(_))
        || folder
            .chars()
            .any(|c| c.is_control() || "<>:\"/\\|?*".contains(c))
        || reserved
    {
        return Err(Error::Validation(
            "目录名称须为单个有效文件夹名，不能含路径分隔符或系统保留名称".into(),
        ));
    }
    Ok(())
}

fn storage(error: impl std::fmt::Display) -> Error {
    Error::Storage(error.to_string())
}
