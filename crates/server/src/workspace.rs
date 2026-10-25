use super::{authenticate, database_time, unauthorized, ApiResult, Server, User, ACCOUNT_LOCK};
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use farflow_core::{
    read_attachment_bytes, sync_directory, write_attachment_bytes, Command, Error, Filter, Id,
    Member, Mode, Receipt, Request, Role, State as Domain, WorkspaceInfo,
};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx_core::{query::query, query_scalar::query_scalar, row::Row};
use sqlx_postgres::{PgConnection, PgTransaction};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path as FilePath, PathBuf},
};

pub(super) fn router() -> Router<Server> {
    Router::new()
        .route("/api/workspaces", get(list).post(create))
        .route("/api/workspaces/{id}", get(snapshot))
        .route("/api/workspaces/{id}/items", get(items))
        .route("/api/workspaces/{id}/people", get(people))
        .route(
            "/api/workspaces/{id}/commands",
            post(execute).layer(DefaultBodyLimit::max(2 * 1024 * 1024)),
        )
        .route(
            "/api/workspaces/{id}/attachments",
            post(upload).layer(DefaultBodyLimit::max(10 * 1024 * 1024)),
        )
        .route(
            "/api/workspaces/{id}/attachments/{attachment}",
            get(download),
        )
}

pub(super) async fn initialize_files(root: PathBuf) -> farflow_core::Result<PathBuf> {
    tokio::task::spawn_blocking(move || {
        let missing: Vec<_> = root
            .ancestors()
            .take_while(|path| !path.exists())
            .map(FilePath::to_owned)
            .collect();
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&root).map_err(disk_error)?;
        let root = fs::canonicalize(root).map_err(disk_error)?;
        directory(&root)?;
        for created in missing {
            let parent = created
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or(FilePath::new("."));
            sync_directory(parent).map_err(|_| Error::Storage("附件目录持久化失败".into()))?;
        }
        create_directory(&root.join("workspaces"))?;
        Ok(root)
    })
    .await
    .map_err(|_| Error::Storage("附件目录初始化任务失败".into()))?
}

fn directory(path: &FilePath) -> farflow_core::Result<()> {
    let meta = fs::symlink_metadata(path).map_err(disk_error)?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(Error::Storage("附件目录不是有效的独立目录".into()));
    }
    Ok(())
}
pub(super) fn create_directory(path: &FilePath) -> farflow_core::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(disk_error(error)),
    }
    directory(path)?;
    if let Some(parent) = path.parent() {
        sync_directory(parent).map_err(|_| Error::Storage("附件目录持久化失败".into()))?;
    }
    Ok(())
}
pub(super) fn files(server: &Server, id: Id, create: bool) -> farflow_core::Result<PathBuf> {
    let parent = server.data_dir.join("workspaces");
    directory(&parent)?;
    let root = parent.join(id.to_string());
    if create {
        create_directory(&root)?;
        create_directory(&root.join("attachments"))?;
    } else {
        directory(&root)?;
    }
    Ok(root)
}
pub(super) fn disk_error(_: std::io::Error) -> Error {
    Error::Storage("附件文件系统操作失败".into())
}
pub(super) fn encoded(value: &impl Serialize) -> farflow_core::Result<String> {
    serde_json::to_string(value).map_err(|_| Error::Storage("工作空间数据编码失败".into()))
}
pub(super) fn decoded<T: serde::de::DeserializeOwned>(value: &str) -> farflow_core::Result<T> {
    serde_json::from_str(value).map_err(|_| Error::Storage("工作空间数据格式异常".into()))
}
pub(super) fn checksum(value: &impl Serialize) -> farflow_core::Result<String> {
    Ok(format!("{:x}", Sha256::digest(encoded(value)?.as_bytes())))
}

pub(super) async fn session(
    server: &Server,
    headers: &HeaderMap,
) -> ApiResult<Option<(PgTransaction<'static>, User)>> {
    let mut tx = server.pool.begin().await?;
    // 工作空间操作共享身份锁，账户停用与所有者交接按同一顺序互斥，避免并发留下无人管理的空间。
    query("SELECT pg_advisory_xact_lock_shared($1)")
        .bind(ACCOUNT_LOCK)
        .execute(&mut *tx)
        .await?;
    let Some(user) = authenticate(&mut tx, headers).await? else {
        return Ok(None);
    };
    if user.must_change_password {
        return Err(Error::Forbidden("请先修改初始密码".into()).into());
    }
    let root = server.data_dir.clone();
    let id = server.storage_id;
    tokio::task::spawn_blocking(move || super::storage::verify(&root, id))
        .await
        .map_err(|_| Error::Storage("服务存储检查任务失败".into()))??;
    Ok(Some((tx, user)))
}
pub(super) fn access(state: &Domain, user: Id) -> farflow_core::Result<&Member> {
    state
        .members
        .get(&user)
        .filter(|member| member.active)
        .ok_or_else(|| Error::Forbidden("不是此工作空间的启用成员".into()))
}
pub(super) async fn load(
    connection: &mut PgConnection,
    id: Id,
    actor: Id,
    write: bool,
) -> ApiResult<Domain> {
    let sql = if write {
        "SELECT * FROM ff_workspaces WHERE id=$1 FOR UPDATE"
    } else {
        "SELECT * FROM ff_workspaces WHERE id=$1 FOR SHARE"
    };
    let row = query(sql)
        .bind(id)
        .fetch_optional(&mut *connection)
        .await?
        .ok_or_else(|| Error::NotFound("工作空间".into()))?;
    let state: Domain = decoded(&row.try_get::<String, _>("state")?)?;
    if state.schema != 1
        || state.id != id
        || state.name != row.get::<String, _>("name")
        || i64::try_from(state.revision).ok() != Some(row.get::<i64, _>("revision"))
        || checksum(&state)? != row.get::<String, _>("checksum")
    {
        return Err(Error::Storage("工作空间投影校验失败".into()).into());
    }
    access(&state, actor_for(connection, id, actor).await?)?;
    super::transfer::read(connection, id).await?;
    Ok(state)
}
fn info(state: &Domain) -> WorkspaceInfo {
    WorkspaceInfo {
        schema: state.schema,
        id: state.id,
        name: state.name.clone(),
        mode: Mode::Service,
    }
}
fn member(user: &User, roles: BTreeSet<Role>, active: bool) -> Member {
    Member {
        id: user.id,
        account: user.account.clone(),
        name: user.name.clone(),
        department: user.department.clone(),
        roles,
        active,
    }
}
fn profile(user: &User) -> Value {
    json!({"id":user.id,"account":user.account,"name":user.name,"department":user.department,"active":user.active})
}
pub(super) async fn accounts(
    connection: &mut PgConnection,
    workspace: Id,
    ids: &BTreeSet<Id>,
) -> ApiResult<BTreeMap<Id, User>> {
    let rows = query("SELECT a.*,ids.member_id AS domain_id FROM UNNEST($1::uuid[]) AS ids(member_id) LEFT JOIN ff_workspace_identities i ON i.workspace_id=$2 AND i.member_id=ids.member_id JOIN ff_accounts a ON a.id=COALESCE(i.account_id,ids.member_id)")
        .bind(ids.iter().copied().collect::<Vec<_>>())
        .bind(workspace)
        .fetch_all(connection)
        .await?;
    Ok(rows
        .iter()
        .map(|row| {
            let user = User::from_row(row);
            (row.get("domain_id"), user)
        })
        .collect())
}
pub(super) async fn sync_members(connection: &mut PgConnection, state: &Domain) -> ApiResult<()> {
    let members: Vec<_> = state.members.keys().copied().collect();
    query("INSERT INTO ff_workspace_identities SELECT $1,id,id FROM UNNEST($2::uuid[]) AS members(id) ON CONFLICT (workspace_id,member_id) DO NOTHING")
        .bind(state.id).bind(&members).execute(&mut *connection).await?;
    query("DELETE FROM ff_workspace_members WHERE workspace_id=$1")
        .bind(state.id)
        .execute(&mut *connection)
        .await?;
    let identities = bindings(connection, state.id).await?;
    let ids: Vec<_> = members
        .iter()
        .map(|id| {
            identities
                .get(id)
                .copied()
                .ok_or_else(|| Error::Storage("空间成员缺少账户绑定".into()))
        })
        .collect::<farflow_core::Result<_>>()?;
    let active: Vec<_> = state.members.values().map(|member| member.active).collect();
    let owner: Vec<_> = state
        .members
        .values()
        .map(|member| member.roles.contains(&Role::Owner))
        .collect();
    query("INSERT INTO ff_workspace_members SELECT $1,u,a,o FROM UNNEST($2::uuid[],$3::bool[],$4::bool[]) AS members(u,a,o)")
        .bind(state.id).bind(ids).bind(active).bind(owner).execute(connection).await?;
    Ok(())
}

pub(super) async fn actor_for(
    connection: &mut PgConnection,
    workspace: Id,
    account: Id,
) -> ApiResult<Id> {
    query_scalar(
        "SELECT member_id FROM ff_workspace_identities WHERE workspace_id=$1 AND account_id=$2",
    )
    .bind(workspace)
    .bind(account)
    .fetch_optional(connection)
    .await?
    .ok_or_else(|| Error::Forbidden("账户没有此空间的身份绑定".into()).into())
}
pub(super) async fn bindings(
    connection: &mut PgConnection,
    workspace: Id,
) -> ApiResult<BTreeMap<Id, Id>> {
    let rows=query("SELECT member_id,account_id FROM ff_workspace_identities WHERE workspace_id=$1 ORDER BY member_id")
        .bind(workspace).fetch_all(connection).await?;
    Ok(rows
        .iter()
        .map(|r| (r.get("member_id"), r.get("account_id")))
        .collect())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewWorkspace {
    id: Id,
    name: String,
}
async fn create(
    State(server): State<Server>,
    headers: HeaderMap,
    Json(input): Json<NewWorkspace>,
) -> ApiResult<Response> {
    let Some((mut tx, user)) = session(&server, &headers).await? else {
        return Ok(unauthorized());
    };
    let state = Domain::new(
        input.id,
        input.name.trim().into(),
        member(&user, BTreeSet::from([Role::Owner]), true),
    )?;
    let inserted =
        query("INSERT INTO ff_workspaces VALUES ($1,$2,$3,0,$4,$4,$5) ON CONFLICT DO NOTHING")
            .bind(state.id)
            .bind(&state.name)
            .bind(user.id)
            .bind(encoded(&state)?)
            .bind(checksum(&state)?)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    if inserted == 0 {
        let old = query("SELECT name,creator_id FROM ff_workspaces WHERE id=$1")
            .bind(input.id)
            .fetch_one(&mut *tx)
            .await?;
        if old.get::<Id, _>("creator_id") != user.id || old.get::<String, _>("name") != state.name {
            return Err(Error::Conflict("工作空间标识已被使用".into()).into());
        }
        let current = load(&mut tx, state.id, user.id, false).await?;
        tx.commit().await?;
        return Ok(Json(info(&current)).into_response());
    }
    sync_members(&mut tx, &state).await?;
    let cloned = server.clone();
    tokio::task::spawn_blocking(move || files(&cloned, input.id, true))
        .await
        .map_err(|_| Error::Storage("附件目录创建任务失败".into()))??;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(info(&state))).into_response())
}
async fn list(State(server): State<Server>, headers: HeaderMap) -> ApiResult<Response> {
    let Some((mut tx, user)) = session(&server, &headers).await? else {
        return Ok(unauthorized());
    };
    let rows=query("SELECT w.id,w.name FROM ff_workspaces w JOIN ff_workspace_members m ON m.workspace_id=w.id WHERE m.user_id=$1 AND m.active ORDER BY w.id")
        .bind(user.id).fetch_all(&mut *tx).await?;
    let result: Vec<_> = rows
        .iter()
        .map(|row| WorkspaceInfo {
            schema: 1,
            id: row.get("id"),
            name: row.get("name"),
            mode: Mode::Service,
        })
        .collect();
    tx.commit().await?;
    Ok(Json(result).into_response())
}
async fn snapshot(
    State(server): State<Server>,
    headers: HeaderMap,
    Path(id): Path<Id>,
) -> ApiResult<Response> {
    let Some((mut tx, user)) = session(&server, &headers).await? else {
        return Ok(unauthorized());
    };
    let state = load(&mut tx, id, user.id, false).await?;
    let users = accounts(&mut tx, id, &state.members.keys().copied().collect()).await?;
    let actor = actor_for(&mut tx, id, user.id).await?;
    let profiles: Vec<_> = users
        .iter()
        .map(|(id, user)| {
            let mut data = profile(user);
            data["account_id"] = json!(user.id);
            data["id"] = json!(id);
            data
        })
        .collect();
    let transfer = super::transfer::read(&mut tx, id).await?.map(|c| c.status);
    let result = json!({"workspace":info(&state),"state":state,"accounts":profiles,"actor_id":actor,"transfer":transfer});
    tx.commit().await?;
    Ok(Json(result).into_response())
}
async fn items(
    State(server): State<Server>,
    headers: HeaderMap,
    Path(id): Path<Id>,
    Query(filter): Query<Filter>,
) -> ApiResult<Response> {
    let Some((mut tx, user)) = session(&server, &headers).await? else {
        return Ok(unauthorized());
    };
    let state = load(&mut tx, id, user.id, false).await?;
    let result = json!({"items":state.search(&filter),"metrics":state.metrics(&filter),"revision":state.revision});
    tx.commit().await?;
    Ok(Json(result).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Directory {
    #[serde(default)]
    text: String,
    after: Option<Id>,
}
async fn people(
    State(server): State<Server>,
    headers: HeaderMap,
    Path(id): Path<Id>,
    Query(input): Query<Directory>,
) -> ApiResult<Response> {
    let Some((mut tx, user)) = session(&server, &headers).await? else {
        return Ok(unauthorized());
    };
    let state = load(&mut tx, id, user.id, false).await?;
    if !access(&state, actor_for(&mut tx, id, user.id).await?)?
        .roles
        .contains(&Role::Owner)
    {
        return Err(Error::Forbidden("只有工作空间所有者可以查询可添加账户".into()).into());
    }
    if input.text.chars().count() > 120 {
        return Err(Error::Validation("账户检索内容过长".into()).into());
    }
    let text = input
        .text
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let rows=query("SELECT a.*,i.member_id FROM ff_accounts a LEFT JOIN ff_workspace_identities i ON i.workspace_id=$3 AND i.account_id=a.id WHERE a.active AND a.id>$1 AND (a.account ILIKE '%' || $2 || '%' OR a.name ILIKE '%' || $2 || '%') ORDER BY a.id LIMIT 101")
        .bind(input.after.unwrap_or(Id::nil())).bind(text).bind(id).fetch_all(&mut *tx).await?;
    let next = if rows.len() > 100 {
        rows.get(99).map(|row| row.get::<Id, _>("id"))
    } else {
        None
    };
    let result = json!({"accounts":rows.iter().take(100).map(|row| {let mut data=profile(&User::from_row(row));data["member_id"]=json!(row.get::<Option<Id>,_>("member_id"));data}).collect::<Vec<_>>(),"next":next});
    tx.commit().await?;
    Ok(Json(result).into_response())
}

pub(super) use farflow_core::ClientOperation as ClientRequest;
async fn execute(
    State(server): State<Server>,
    headers: HeaderMap,
    Path(id): Path<Id>,
    Json(input): Json<ClientRequest>,
) -> ApiResult<Response> {
    let Some((mut tx, user)) = session(&server, &headers).await? else {
        return Ok(unauthorized());
    };
    let receipt = apply(&server, &mut tx, id, &user, input, None).await?;
    tx.commit().await?;
    Ok(Json(receipt).into_response())
}

pub(super) fn inspect(
    command: &Command,
    allow_attachment: bool,
) -> farflow_core::Result<(bool, BTreeSet<Id>, BTreeSet<Id>)> {
    let mut stack = vec![(command, 0)];
    let mut count = 0;
    let mut members = BTreeSet::new();
    let mut references = BTreeSet::new();
    while let Some((command, depth)) = stack.pop() {
        count += 1;
        if count > 1000 || depth > 8 {
            return Err(Error::Validation("单次命令数量或嵌套超过限制".into()));
        }
        match command {
            Command::Batch { commands } => {
                stack.extend(commands.iter().map(|command| (command, depth + 1)))
            }
            Command::AddMember { member } | Command::UpdateMember { member } => {
                members.insert(member.id);
            }
            Command::Create { item } => {
                if let Some(id) = item.assigned_to {
                    references.insert(id);
                }
            }
            Command::Update { patch, .. } if !patch.clear_assignee => {
                if let Some(id) = patch.assigned_to {
                    references.insert(id);
                }
            }
            Command::AddComment { mentions, .. } => references.extend(mentions),
            Command::AddAttachment { .. } if !allow_attachment => {
                return Err(Error::Validation("附件须通过真实上传接口保存".into()))
            }
            _ => {}
        }
    }
    Ok((!members.is_empty(), members, references))
}
async fn prepare(
    connection: &mut PgConnection,
    state: &Domain,
    actor: Id,
    command: &mut Command,
    allow_attachment: bool,
    expected: Option<u64>,
) -> ApiResult<BTreeMap<Id, User>> {
    let (membership, added, references) = inspect(command, allow_attachment)?;
    if membership && !access(state, actor)?.roles.contains(&Role::Owner) {
        return Err(Error::Forbidden("成员变更需要所有者权限".into()).into());
    }
    if membership && expected.is_none() {
        return Err(Error::Validation("成员变更需要当前工作空间版本".into()).into());
    }
    if references
        .iter()
        .any(|id| !state.members.contains_key(id) && !added.contains(id))
    {
        return Err(Error::Validation("指派或提及的账户必须属于此工作空间".into()).into());
    }
    let mut ids: BTreeSet<_> = state.members.keys().copied().collect();
    ids.extend(&added);
    let users = accounts(connection, state.id, &ids).await?;
    if references
        .iter()
        .any(|id| !users.get(id).is_some_and(|user| user.active))
    {
        return Err(Error::Validation("不能指派或提及已停用的服务账户".into()).into());
    }
    let mut names: BTreeMap<_, _> = state
        .members
        .values()
        .map(|m| (m.account.to_ascii_lowercase(), m.id))
        .collect();
    let mut stack = vec![command];
    while let Some(command) = stack.pop() {
        match command {
            Command::Batch { commands } => stack.extend(commands.iter_mut()),
            Command::AddMember { member: input } | Command::UpdateMember { member: input } => {
                let user = users
                    .get(&input.id)
                    .ok_or_else(|| Error::NotFound("服务账户".into()))?;
                let existing:Option<Id>=query_scalar("SELECT member_id FROM ff_workspace_identities WHERE workspace_id=$1 AND account_id=$2")
                    .bind(state.id).bind(user.id).fetch_optional(&mut *connection).await?;
                if existing.is_some_and(|id| id != input.id) {
                    return Err(Error::Validation(
                        "该账户已绑定历史身份，请使用已有成员标识".into(),
                    )
                    .into());
                }
                if input.active && !user.active {
                    return Err(
                        Error::Validation("已停用服务账户不能启用为工作空间成员".into()).into(),
                    );
                }
                let id = input.id;
                *input = member(user, input.roles.clone(), input.active);
                input.id = id;
                if let Some(previous) = state.members.get(&id) {
                    input.account = previous.account.clone();
                } else if names
                    .get(&input.account.to_ascii_lowercase())
                    .is_some_and(|other| *other != id)
                {
                    let base = format!("ff_{}", id.simple());
                    let mut candidate = base.clone();
                    let mut suffix = 0;
                    while names.get(&candidate).is_some_and(|other| *other != id) {
                        suffix += 1;
                        candidate = format!("{base}_{suffix}");
                    }
                    input.account = candidate;
                }
                names.insert(input.account.to_ascii_lowercase(), id);
            }
            _ => {}
        }
    }
    Ok(users)
}

async fn apply(
    server: &Server,
    connection: &mut PgConnection,
    id: Id,
    user: &User,
    input: ClientRequest,
    bytes: Option<Bytes>,
) -> ApiResult<Receipt> {
    let mut state = load(connection, id, user.id, true).await?;
    if let Some(control) = super::transfer::read(connection, id).await? {
        control.ensure_writable()?;
    }
    let actor = actor_for(connection, id, user.id).await?;
    let old=query("SELECT client,request,receipt FROM ff_workspace_operations WHERE workspace_id=$1 AND id=$2")
        .bind(id).bind(input.id).fetch_optional(&mut *connection).await?;
    if let Some(old) = old {
        let old_input: String = old.try_get("client")?;
        let request: Request = decoded(&old.try_get::<String, _>("request")?)?;
        let receipt: Receipt = decoded(&old.try_get::<String, _>("receipt")?)?;
        if old_input != encoded(&input)? || request.actor_id != actor {
            return Err(Error::Conflict("操作标识已被不同请求使用".into()).into());
        }
        if !state.operations.get(&input.id).is_some_and(|applied| {
            applied.receipt == receipt
                && checksum(&request).ok() == Some(applied.fingerprint.clone())
        }) {
            return Err(Error::Storage("工作空间回执与原始操作不一致".into()).into());
        }
        if let Some(bytes) = bytes {
            persist_attachment(server, id, &request.command, bytes).await?;
        }
        return Ok(receipt);
    }
    if let Some(receipt) = super::migration::retry_origin(connection, id, actor, &input).await? {
        return Ok(receipt);
    }
    if state.operations.contains_key(&input.id) {
        return Err(Error::Storage("工作空间缺少原始操作记录".into()).into());
    }
    if input
        .expected_revision
        .is_some_and(|version| version != state.revision)
    {
        return Err(Error::Conflict("工作空间已更新，请刷新后重试".into()).into());
    }
    let previous = state.members.clone();
    let mut effective = input.command.clone();
    let users = prepare(
        connection,
        &state,
        actor,
        &mut effective,
        bytes.is_some(),
        input.expected_revision,
    )
    .await?;
    let request = Request {
        id: input.id,
        actor_id: actor,
        at: database_time(connection).await?,
        command: effective,
    };
    let cloned = request.clone();
    let (next, receipt) = tokio::task::spawn_blocking(move || {
        let receipt = state.execute(cloned)?;
        Ok::<_, Error>((state, receipt))
    })
    .await
    .map_err(|_| Error::Storage("业务执行任务失败".into()))??;
    state = next;
    if !state.members.values().any(|member| {
        member.active
            && member.roles.contains(&Role::Owner)
            && users.get(&member.id).is_some_and(|user| user.active)
    }) {
        return Err(
            Error::Validation("至少保留一位账户及成员均启用的工作空间所有者".into()).into(),
        );
    }
    if let Some(bytes) = bytes {
        persist_attachment(server, id, &request.command, bytes).await?;
    }
    query("UPDATE ff_workspaces SET revision=$2,state=$3,checksum=$4 WHERE id=$1")
        .bind(id)
        .bind(i64::try_from(state.revision).map_err(|_| Error::Storage("业务版本超过限制".into()))?)
        .bind(encoded(&state)?)
        .bind(checksum(&state)?)
        .execute(&mut *connection)
        .await?;
    query("INSERT INTO ff_workspace_operations VALUES ($1,$2,$3,$4,$5,$6)")
        .bind(id)
        .bind(input.id)
        .bind(i64::try_from(state.revision).map_err(|_| Error::Storage("业务版本超过限制".into()))?)
        .bind(encoded(&input)?)
        .bind(encoded(&request)?)
        .bind(encoded(&receipt)?)
        .execute(&mut *connection)
        .await?;
    if previous != state.members {
        sync_members(connection, &state).await?;
    }
    Ok(receipt)
}

async fn persist_attachment(
    server: &Server,
    id: Id,
    command: &Command,
    bytes: Bytes,
) -> ApiResult<()> {
    let Command::AddAttachment { sha256, .. } = command else {
        return Err(Error::Validation("上传命令不正确".into()).into());
    };
    let hash = sha256.clone();
    let cloned = server.clone();
    tokio::task::spawn_blocking(move || {
        let root = files(&cloned, id, true)?;
        write_attachment_bytes(&root, &hash, &bytes)
            .map_err(|_| Error::Storage("附件内容保存失败".into()))
    })
    .await
    .map_err(|_| Error::Storage("附件保存任务失败".into()))??;
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Upload {
    operation_id: Id,
    id: Id,
    item_id: Id,
    name: String,
    mime: String,
}
async fn upload(
    State(server): State<Server>,
    headers: HeaderMap,
    Path(id): Path<Id>,
    Query(input): Query<Upload>,
    bytes: Bytes,
) -> ApiResult<Response> {
    let Some((mut tx, user)) = session(&server, &headers).await? else {
        return Ok(unauthorized());
    };
    let command = Command::AddAttachment {
        id: input.id,
        item_id: input.item_id,
        name: input.name,
        mime: input.mime,
        sha256: format!("{:x}", Sha256::digest(&bytes)),
        size: bytes.len() as u64,
    };
    let receipt = apply(
        &server,
        &mut tx,
        id,
        &user,
        ClientRequest {
            id: input.operation_id,
            expected_revision: None,
            command,
        },
        Some(bytes),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(receipt).into_response())
}
async fn download(
    State(server): State<Server>,
    headers: HeaderMap,
    Path((id, attachment)): Path<(Id, Id)>,
) -> ApiResult<Response> {
    let Some((mut tx, user)) = session(&server, &headers).await? else {
        return Ok(unauthorized());
    };
    let state = load(&mut tx, id, user.id, false).await?;
    let attachment = state
        .attachments
        .get(&attachment)
        .filter(|attachment| !attachment.deleted)
        .ok_or_else(|| Error::NotFound("附件".into()))?
        .clone();
    state.item(attachment.item_id)?;
    let cloned = server.clone();
    let metadata = attachment.clone();
    let bytes = tokio::task::spawn_blocking(move || {
        let root = files(&cloned, id, false)?;
        read_attachment_bytes(&root, &metadata)
            .map_err(|_| Error::Storage("附件缺失或内容校验失败".into()))
    })
    .await
    .map_err(|_| Error::Storage("附件读取任务失败".into()))??;
    tx.commit().await?;
    let disposition = format!(
        "attachment; filename*=UTF-8''{}",
        utf8_percent_encode(&attachment.name, NON_ALPHANUMERIC)
    );
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
            (header::CONTENT_DISPOSITION, disposition),
        ],
        bytes,
    )
        .into_response())
}

pub(super) async fn ensure_account_can_disable(
    connection: &mut PgConnection,
    id: Id,
) -> ApiResult<()> {
    let stranded:Vec<Id>=query_scalar("SELECT m.workspace_id FROM ff_workspace_members m WHERE m.user_id=$1 AND m.active AND m.owner AND NOT EXISTS(SELECT 1 FROM ff_workspace_members other JOIN ff_accounts a ON a.id=other.user_id WHERE other.workspace_id=m.workspace_id AND other.user_id<>$1 AND other.active AND other.owner AND a.active)")
        .bind(id).fetch_all(&mut *connection).await?;
    for workspace in stranded {
        let retired = super::transfer::read(connection, workspace)
            .await?
            .is_some_and(|control| {
                control.status.role == farflow_core::TransferRole::Source
                    && control.status.phase == farflow_core::TransferPhase::Completed
            });
        if retired {
            continue;
        }
        return Err(
            Error::Validation("账户仍是某工作空间最后一位有效所有者，请先完成交接".into()).into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_and_oversized_commands_are_bounded_before_account_lookups() {
        let base = Command::Restore {
            id: Id::new_v4(),
            version: 1,
        };
        let mut deep = base.clone();
        for _ in 0..10 {
            deep = Command::Batch {
                commands: vec![deep],
            };
        }
        assert!(inspect(&deep, false).is_err());
        assert!(inspect(
            &Command::Batch {
                commands: vec![base; 1001]
            },
            false
        )
        .is_err());
        let mut patch = farflow_core::Patch {
            assigned_to: Some(Id::new_v4()),
            ..Default::default()
        };
        let command = Command::Update {
            id: Id::new_v4(),
            version: 1,
            patch: patch.clone(),
        };
        assert_eq!(inspect(&command, false).unwrap().2.len(), 1);
        patch.clear_assignee = true;
        assert!(inspect(
            &Command::Update {
                id: Id::new_v4(),
                version: 1,
                patch
            },
            false
        )
        .unwrap()
        .2
        .is_empty());
    }

    #[tokio::test]
    async fn storage_initialization_rejects_files_and_linked_subdirectories() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("nested/data");
        let initialized = initialize_files(root.clone()).await.unwrap();
        assert_eq!(initialized, fs::canonicalize(root).unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&initialized).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        let invalid = directory.path().join("file");
        fs::write(&invalid, b"keep").unwrap();
        assert!(initialize_files(invalid.clone()).await.is_err());
        assert!(create_directory(&invalid).is_err());
        assert_eq!(fs::read(invalid).unwrap(), b"keep");
        #[cfg(unix)]
        {
            fs::remove_dir(initialized.join("workspaces")).unwrap();
            std::os::unix::fs::symlink(directory.path(), initialized.join("workspaces")).unwrap();
            assert!(initialize_files(initialized).await.is_err());
        }
    }
}
