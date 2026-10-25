use super::{database_error, workspace, Server, User};
use axum::http::{header, HeaderMap, HeaderValue};
use farflow_core::{
    ClientOperation, Error, GitChange, GitLog, Id, MigrationArchive, MigrationHistory,
    MigrationReport, Mode, Receipt, Request, Result, Role, State, TransferReceipt, TransferRole,
    TransferState,
};
use sqlx_core::{query::query, row::Row};
use sqlx_postgres::PgConnection;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
use workspace::{checksum, decoded, encoded};
const IMPORT_LOCK: i64 = 7_016_404_616;

#[derive(Clone, Copy)]
enum ImportMode {
    Copy(Id),
    Stage,
}

#[derive(Debug, PartialEq, Eq)]
struct OriginOperation {
    request: Option<String>,
    client: Option<String>,
    receipt: Option<String>,
}

impl Server {
    pub async fn import_copy(
        &self,
        token: &str,
        source: impl AsRef<Path>,
        new_id: Id,
        bindings: BTreeMap<Id, Id>,
    ) -> Result<MigrationReport> {
        let (report, _) = self
            .install_workspace(token, source, ImportMode::Copy(new_id), bindings)
            .await?;
        Ok(report)
    }

    pub async fn stage_transfer(
        &self,
        token: &str,
        source: impl AsRef<Path>,
        bindings: BTreeMap<Id, Id>,
    ) -> Result<(MigrationReport, TransferReceipt)> {
        let (report, receipt) = self
            .install_workspace(token, source, ImportMode::Stage, bindings)
            .await?;
        let Some(receipt) = receipt else {
            return Err(Error::Storage("正式迁移缺少目标回执".into()));
        };
        Ok((report, receipt))
    }

    async fn install_workspace(
        &self,
        token: &str,
        source: impl AsRef<Path>,
        mode: ImportMode,
        bindings: BTreeMap<Id, Id>,
    ) -> Result<(MigrationReport, Option<TransferReceipt>)> {
        let headers = headers(token)?;
        let Some((mut tx, user)) = workspace::session(self, &headers).await.map_err(|e| e.0)?
        else {
            return Err(Error::Forbidden("导入需要有效登录".into()));
        };
        let source = source.as_ref().to_owned();
        let (archive, history, source_root) = tokio::task::spawn_blocking(move || {
            let source_root = std::fs::canonicalize(&source)
                .map_err(|_| Error::Storage("迁移来源目录不可用".into()))?;
            let archive = MigrationArchive::open(source)?;
            let history = match mode {
                ImportMode::Copy(id) => archive.copied_history(id)?,
                ImportMode::Stage => archive.transfer_history(Mode::Service)?,
            };
            Ok::<_, Error>((archive, history, source_root))
        })
        .await
        .map_err(|_| Error::Storage("迁移预检任务失败".into()))??;
        let new_id = history.log.info().id;
        let plan = archive.transfer_plan().cloned();
        let target = self.data_dir.join("workspaces").join(new_id.to_string());
        if target.starts_with(&source_root) {
            return Err(Error::Validation("服务空间目录必须独立于来源交换包".into()));
        }
        let state = history.validate()?;
        if bindings.keys().copied().collect::<BTreeSet<_>>() != history.member_ids()
            || bindings.values().any(Id::is_nil)
            || bindings.values().copied().collect::<BTreeSet<_>>().len() != bindings.len()
        {
            return Err(Error::Validation(
                "所有历史身份必须逐一映射，且不能复用同一服务账户".into(),
            ));
        }
        let rows = query("SELECT * FROM ff_accounts WHERE id=ANY($1)")
            .bind(bindings.values().copied().collect::<Vec<_>>())
            .fetch_all(&mut *tx)
            .await
            .map_err(database_error)?;
        let accounts: BTreeMap<_, _> = rows
            .iter()
            .map(|row| {
                let user = User::from_row(row);
                (user.id, user)
            })
            .collect();
        if accounts.len() != bindings.len() {
            return Err(Error::Validation("身份映射包含不存在的服务账户".into()));
        }
        let owners: Vec<_> = state
            .members
            .values()
            .filter(|m| m.active && m.roles.contains(&Role::Owner))
            .filter_map(|m| bindings.get(&m.id).and_then(|id| accounts.get(id)))
            .filter(|u| u.active)
            .collect();
        if owners.is_empty() && matches!(mode, ImportMode::Copy(_)) {
            return Err(Error::Validation("导入后必须有启用的真实账户所有者".into()));
        }
        if !user.is_admin
            && matches!(mode, ImportMode::Copy(_))
            && !owners.iter().any(|owner| owner.id == user.id)
        {
            return Err(Error::Forbidden(
                "只有服务管理员或映射后的空间所有者可以导入".into(),
            ));
        }
        query("SELECT pg_advisory_xact_lock($1)")
            .bind(IMPORT_LOCK)
            .execute(&mut *tx)
            .await
            .map_err(database_error)?;
        let old = query("SELECT * FROM ff_workspaces WHERE id=$1 FOR UPDATE")
            .bind(new_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(database_error)?;
        if let Some(old) = old {
            let origin = read_origin(&mut tx, new_id).await?;
            let current = workspace::bindings(&mut tx, new_id)
                .await
                .map_err(|e| e.0)?;
            let projected: State = decoded(&old.get::<String, _>("state"))?;
            if (matches!(mode, ImportMode::Copy(_)) && old.get::<Id, _>("creator_id") != user.id)
                || origin.as_ref().map(checksum).transpose()? != Some(checksum(&history)?)
                || bindings.iter().any(|(m, a)| current.get(m) != Some(a))
            {
                return Err(Error::Conflict("空间标识已属于不同导入或身份映射".into()));
            }
            if checksum(&projected)? != old.get::<String, _>("checksum")
                || projected.id != new_id
                || projected.name != old.get::<String, _>("name")
                || projected.schema != 1
                || i64::try_from(projected.revision).ok() != Some(old.get::<i64, _>("revision"))
            {
                return Err(Error::Storage("已导入空间投影损坏".into()));
            }
            if !user.is_admin || matches!(mode, ImportMode::Stage) {
                let actor = workspace::actor_for(&mut tx, new_id, user.id)
                    .await
                    .map_err(|e| e.0)?;
                if !workspace::access(&projected, actor)?
                    .roles
                    .contains(&Role::Owner)
                {
                    return Err(Error::Forbidden("当前账户已不是空间所有者".into()));
                }
            }
            if let Some(plan) = &plan {
                let Some(control) = super::transfer::read(&mut tx, new_id).await? else {
                    return Err(Error::Conflict("已有空间不是该暂存迁移".into()));
                };
                if control.status.role != TransferRole::Target || control.status.plan != *plan {
                    return Err(Error::Conflict("已有空间属于不同迁移阶段或计划".into()));
                }
            }
            let current_history = load_history(&mut tx, &projected).await?;
            let root = workspace::files(self, new_id, false)?;
            tokio::task::spawn_blocking(move || {
                for attachment in current_history.attachments() {
                    farflow_core::read_attachment_bytes(&root, &attachment)?;
                }
                Ok::<_, Error>(())
            })
            .await
            .map_err(|_| Error::Storage("已导入附件核对任务失败".into()))??;
            return Ok((
                report(&archive, &history)?,
                plan.as_ref().map(|p| p.receipt()).transpose()?,
            ));
        }
        let control = if let Some(plan) = &plan {
            if !owners.iter().any(|owner| owner.id == user.id) {
                return Err(Error::Forbidden(
                    "正式暂存需要映射后的空间所有者，管理员身份不能替代".into(),
                ));
            }
            let Some(actor) = bindings
                .iter()
                .find_map(|(m, a)| (*a == user.id).then_some(*m))
            else {
                return Err(Error::Forbidden("当前账户没有历史所有者绑定".into()));
            };
            Some(TransferState::stage(
                plan.clone(),
                &history,
                Mode::Service,
                actor,
                super::database_time(&mut tx).await.map_err(|e| e.0)?,
            )?)
        } else {
            None
        };
        query("INSERT INTO ff_workspaces VALUES ($1,$2,$3,$4,$5,$5,$6)")
            .bind(new_id)
            .bind(&state.name)
            .bind(user.id)
            .bind(
                i64::try_from(state.revision)
                    .map_err(|_| Error::Validation("空间版本过大".into()))?,
            )
            .bind(encoded(&state)?)
            .bind(checksum(&state)?)
            .execute(&mut *tx)
            .await
            .map_err(database_error)?;
        for (member, account) in &bindings {
            query("INSERT INTO ff_workspace_identities VALUES ($1,$2,$3)")
                .bind(new_id)
                .bind(member)
                .bind(account)
                .execute(&mut *tx)
                .await
                .map_err(database_error)?;
        }
        store_origin(&mut tx, &history).await?;
        workspace::sync_members(&mut tx, &state)
            .await
            .map_err(|e| e.0)?;
        if let Some(control) = &control {
            super::transfer::write(&mut tx, control, user.id).await?;
        }
        let data = report(&archive, &history)?;
        let server = self.clone();
        tokio::task::spawn_blocking(move || {
            let root = workspace::files(&server, new_id, true)?;
            archive.write_blobs(root)
        })
        .await
        .map_err(|_| Error::Storage("迁移附件处理任务失败".into()))??;
        tx.commit().await.map_err(database_error)?;
        Ok((data, plan.as_ref().map(|p| p.receipt()).transpose()?))
    }

    pub async fn export_copy(
        &self,
        token: &str,
        id: Id,
        destination: impl AsRef<Path>,
    ) -> Result<MigrationReport> {
        let headers = headers(token)?;
        let Some((mut tx, user)) = workspace::session(self, &headers).await.map_err(|e| e.0)?
        else {
            return Err(Error::Forbidden("导出需要有效登录".into()));
        };
        let state = workspace::load(&mut tx, id, user.id, false)
            .await
            .map_err(|e| e.0)?;
        let actor = workspace::actor_for(&mut tx, id, user.id)
            .await
            .map_err(|e| e.0)?;
        if !workspace::access(&state, actor)?
            .roles
            .contains(&Role::Owner)
        {
            return Err(Error::Forbidden("只有空间所有者可以导出".into()));
        }
        if let Some(control) = super::transfer::read(&mut tx, id).await? {
            control.ensure_writable()?;
        }
        exportable_members(&mut tx, &state).await?;
        let history = load_history(&mut tx, &state).await?;
        let root = workspace::files(self, id, false)?;
        let destination = destination.as_ref().to_owned();
        let data = tokio::task::spawn_blocking(move || {
            history.export_copy(Mode::Service, actor, root, destination)
        })
        .await
        .map_err(|_| Error::Storage("导出处理任务失败".into()))??;
        tx.commit().await.map_err(database_error)?;
        Ok(data)
    }
}

pub(super) async fn exportable_members(connection: &mut PgConnection, state: &State) -> Result<()> {
    let users = workspace::accounts(
        connection,
        state.id,
        &state.members.keys().copied().collect(),
    )
    .await
    .map_err(|e| e.0)?;
    if state
        .members
        .values()
        .any(|member| member.active && !users.get(&member.id).is_some_and(|user| user.active))
    {
        return Err(Error::Validation("存在全局停用但空间仍启用的成员，请先明确停用其空间身份再导出，避免在无服务模式重新获得权限".into()));
    }
    Ok(())
}

fn headers(token: &str) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| Error::Forbidden("登录令牌格式无效".into()))?,
    );
    Ok(headers)
}
fn report(archive: &MigrationArchive, history: &MigrationHistory) -> Result<MigrationReport> {
    let mut report = archive.report()?;
    let mut info = history.log.info();
    info.mode = Mode::Service;
    report.target = Some(info);
    report.origins = history.origins.clone();
    Ok(report)
}

pub(super) async fn read_origin(
    connection: &mut PgConnection,
    id: Id,
) -> Result<Option<MigrationHistory>> {
    let row = query("SELECT data,checksum FROM ff_workspace_origins WHERE workspace_id=$1")
        .bind(id)
        .fetch_optional(connection)
        .await
        .map_err(database_error)?;
    let Some(row) = row else { return Ok(None) };
    let history: MigrationHistory = decoded(&row.get::<String, _>("data"))?;
    if checksum(&history)? != row.get::<String, _>("checksum") {
        return Err(Error::Storage("迁移来源校验失败".into()));
    }
    Ok(Some(history))
}
fn records(history: &MigrationHistory) -> Result<BTreeMap<Id, OriginOperation>> {
    let view = history.log.view()?;
    history
        .log
        .operations()
        .iter()
        .map(|(id, op)| {
            let request = match &op.change {
                GitChange::Apply { request } => Some(encoded(request)?),
                GitChange::Resolve { .. } => None,
            };
            Ok((
                *id,
                OriginOperation {
                    request,
                    client: history.clients.get(id).cloned(),
                    receipt: view.receipts.get(id).map(encoded).transpose()?,
                },
            ))
        })
        .collect()
}
pub(super) async fn store_origin(
    connection: &mut PgConnection,
    history: &MigrationHistory,
) -> Result<()> {
    let state = history.validate()?;
    query("INSERT INTO ff_workspace_origins VALUES ($1,$2,$3)")
        .bind(state.id)
        .bind(encoded(history)?)
        .bind(checksum(history)?)
        .execute(&mut *connection)
        .await
        .map_err(database_error)?;
    for (id, op) in records(history)? {
        query("INSERT INTO ff_workspace_origin_operations VALUES ($1,$2,$3,$4,$5)")
            .bind(state.id)
            .bind(id)
            .bind(op.request)
            .bind(op.client)
            .bind(op.receipt)
            .execute(&mut *connection)
            .await
            .map_err(database_error)?;
    }
    Ok(())
}
pub(super) async fn verify_origin_cache(
    connection: &mut PgConnection,
    id: Id,
    history: &MigrationHistory,
) -> Result<()> {
    let rows=query("SELECT id,request,client,receipt FROM ff_workspace_origin_operations WHERE workspace_id=$1 ORDER BY id").bind(id)
        .fetch_all(connection).await.map_err(database_error)?;
    let actual: BTreeMap<_, _> = rows
        .iter()
        .map(|r| {
            (
                r.get("id"),
                OriginOperation {
                    request: r.get("request"),
                    client: r.get("client"),
                    receipt: r.get("receipt"),
                },
            )
        })
        .collect();
    if actual != records(history)? {
        return Err(Error::Storage("迁移来源回执索引损坏".into()));
    }
    Ok(())
}
pub(super) async fn load_history(
    connection: &mut PgConnection,
    state: &State,
) -> Result<MigrationHistory> {
    let data: String =
        sqlx_core::query_scalar::query_scalar("SELECT genesis FROM ff_workspaces WHERE id=$1")
            .bind(state.id)
            .fetch_one(&mut *connection)
            .await
            .map_err(database_error)?;
    let genesis: State = decoded(&data)?;
    let mut history = if let Some(history) = read_origin(connection, state.id).await? {
        if encoded(&history.validate()?)? != encoded(&genesis)? {
            return Err(Error::Storage("来源与初始业务状态不一致".into()));
        }
        verify_origin_cache(connection, state.id, &history).await?;
        history
    } else {
        let owner = genesis
            .members
            .values()
            .next()
            .cloned()
            .ok_or_else(|| Error::Validation("缺少初始所有者".into()))?;
        let log = GitLog::new(state.id, state.name.clone(), owner)?;
        if encoded(&log.view()?.state)? != encoded(&genesis)? {
            return Err(Error::Storage("服务初始状态无效".into()));
        }
        MigrationHistory {
            log,
            origins: vec![],
            clients: BTreeMap::new(),
        }
    };
    let rows=query("SELECT id,revision,client,request,receipt FROM ff_workspace_operations WHERE workspace_id=$1 ORDER BY revision").bind(state.id)
        .fetch_all(&mut *connection).await.map_err(database_error)?;
    for row in rows {
        let request: Request = decoded(&row.get::<String, _>("request"))?;
        let client_text: String = row.get("client");
        let client: ClientOperation = decoded(&client_text)?;
        let receipt: Receipt = decoded(&row.get::<String, _>("receipt"))?;
        let revision = history.log.view()?.state.revision;
        client.verify_effective(&request)?;
        if request.id != row.get::<Id, _>("id")
            || history.log.operations().contains_key(&request.id)
            || client.expected_revision.is_some_and(|r| r != revision)
            || i64::try_from(receipt.revision).ok() != Some(row.get::<i64, _>("revision"))
            || history.log.execute(state.id, request.clone())? != receipt
        {
            return Err(Error::Storage("服务原始操作与回执不一致".into()));
        }
        history.clients.insert(request.id, client_text);
    }
    if encoded(&history.validate()?)? != encoded(state)? {
        return Err(Error::Storage("服务投影与完整操作历史不一致".into()));
    }
    let bindings = workspace::bindings(connection, state.id)
        .await
        .map_err(|e| e.0)?;
    if bindings.keys().copied().collect::<BTreeSet<_>>() != history.member_ids() {
        return Err(Error::Storage("服务历史身份映射不完整".into()));
    }
    Ok(history)
}

pub(super) async fn retry_origin(
    connection: &mut PgConnection,
    workspace: Id,
    actor: Id,
    input: &ClientOperation,
) -> Result<Option<Receipt>> {
    let row=query("SELECT request,client,receipt FROM ff_workspace_origin_operations WHERE workspace_id=$1 AND id=$2")
        .bind(workspace).bind(input.id).fetch_optional(&mut *connection).await.map_err(database_error)?;
    let Some(row) = row else { return Ok(None) };
    let history = read_origin(connection, workspace)
        .await?
        .ok_or_else(|| Error::Storage("缺少迁移来源".into()))?;
    let expected = records(&history)?
        .remove(&input.id)
        .ok_or_else(|| Error::Storage("来源操作不存在".into()))?;
    let actual = OriginOperation {
        request: row.get("request"),
        client: row.get("client"),
        receipt: row.get("receipt"),
    };
    if actual != expected {
        return Err(Error::Storage("迁移来源回执损坏".into()));
    }
    let Some(request) = actual.request else {
        return Err(Error::Conflict("迁移决策标识不能作为新操作使用".into()));
    };
    let request: Request = decoded(&request)?;
    if request.actor_id != actor
        || match actual.client {
            Some(client) => encoded(input)? != client,
            None => encoded(&input.command)? != encoded(&request.command)?,
        }
    {
        return Err(Error::Conflict("来源操作由不同请求或操作者使用".into()));
    }
    actual
        .receipt
        .map(|r| decoded(&r))
        .transpose()?
        .map(Some)
        .ok_or_else(|| Error::Conflict("来源提案未被采纳，不能重新执行".into()))
}
