use super::{database_error, database_time, migration, workspace, Server, User};
use axum::http::{header, HeaderMap, HeaderValue};
use farflow_core::{
    Error, Id, MigrationReport, Mode, Result, Role, State, TransferActivation, TransferPhase,
    TransferReceipt, TransferRole, TransferState, TransferStatus,
};
use serde::{Deserialize, Serialize};
use sqlx_core::{query::query, query_scalar::query_scalar, row::Row};
use sqlx_postgres::{PgConnection, PgTransaction};
use std::{collections::BTreeMap, path::Path};
use workspace::{checksum, decoded, encoded};

pub(super) const SCHEMA: &str = include_str!("../migrations/0006_transfer.sql");

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Event {
    pub account_id: Id,
    pub status: TransferStatus,
}

pub(super) async fn verify_barriers(connection: &mut PgConnection) -> Result<()> {
    let body:Option<String>=query_scalar("SELECT prosrc FROM pg_proc WHERE oid=to_regprocedure('ff_transfer_guard()') AND prorettype='trigger'::regtype AND provolatile='v' AND NOT prosecdef AND pronargs=0 AND prolang=(SELECT oid FROM pg_language WHERE lanname='plpgsql')")
        .fetch_optional(&mut *connection).await.map_err(database_error)?;
    let expected = SCHEMA
        .split("$farflow_guard$")
        .nth(1)
        .expect("内置迁移屏障定义完整");
    if body.as_deref() != Some(expected) {
        return Err(Error::Storage("服务迁移屏障函数缺失或损坏".into()));
    }
    let count:i64=query_scalar("SELECT COUNT(*) FROM pg_trigger WHERE tgrelid=ANY(ARRAY['ff_workspaces'::regclass,'ff_workspace_operations'::regclass,'ff_workspace_members'::regclass,'ff_workspace_identities'::regclass]) AND tgname='ff_transfer_fence' AND tgtype=31 AND tgenabled='O' AND NOT tgisinternal AND tgfoid=to_regprocedure('ff_transfer_guard()') AND tgqual IS NULL AND tgnargs=0 AND tgattr=''::int2vector")
        .fetch_one(&mut *connection).await.map_err(database_error)?;
    if count != 4 {
        return Err(Error::Storage("服务迁移写入屏障缺失或损坏".into()));
    }
    Ok(())
}

pub(super) async fn read(connection: &mut PgConnection, id: Id) -> Result<Option<TransferState>> {
    let row =
        query("SELECT data,checksum,writable FROM ff_workspace_transfer WHERE workspace_id=$1")
            .bind(id)
            .fetch_optional(&mut *connection)
            .await
            .map_err(database_error)?;
    let Some(row) = row else {
        let orphan: bool = query_scalar(
            "SELECT EXISTS(SELECT 1 FROM ff_workspace_transfer_events WHERE workspace_id=$1)",
        )
        .bind(id)
        .fetch_one(connection)
        .await
        .map_err(database_error)?;
        if orphan {
            return Err(Error::Storage("迁移历史存在但控制记录缺失".into()));
        }
        return Ok(None);
    };
    let control: TransferState = decoded(&row.get::<String, _>("data"))?;
    control.validate_header(id, Mode::Service)?;
    if checksum(&control)? != row.get::<String, _>("checksum")
        || control.writable() != row.get::<bool, _>("writable")
    {
        return Err(Error::Storage("服务迁移控制记录损坏".into()));
    }
    verify_barriers(connection).await?;
    let event=query("SELECT data,checksum FROM ff_workspace_transfer_events WHERE workspace_id=$1 ORDER BY sequence DESC LIMIT 1").bind(id).fetch_optional(connection).await.map_err(database_error)?;
    let Some(event) = event else {
        return Err(Error::Storage("迁移控制缺少阶段事件".into()));
    };
    let status: TransferStatus = decoded(&event.get::<String, _>("data"))?;
    if status != control.status || checksum(&status)? != event.get::<String, _>("checksum") {
        return Err(Error::Storage("迁移阶段与事件不一致".into()));
    }
    Ok(Some(control))
}

pub(super) async fn events(connection: &mut PgConnection, id: Id) -> Result<Vec<Event>> {
    let rows=query("SELECT account_id,data,checksum FROM ff_workspace_transfer_events WHERE workspace_id=$1 ORDER BY sequence").bind(id).fetch_all(connection).await.map_err(database_error)?;
    let mut result = vec![];
    for row in rows {
        let status: TransferStatus = decoded(&row.get::<String, _>("data"))?;
        if checksum(&status)? != row.get::<String, _>("checksum") {
            return Err(Error::Storage("迁移事件校验失败".into()));
        }
        result.push(Event {
            account_id: row.get("account_id"),
            status,
        });
    }
    Ok(result)
}

pub(super) fn verify_events(
    control: Option<&TransferState>,
    events: &[Event],
    history: &farflow_core::MigrationHistory,
    bindings: &BTreeMap<Id, Id>,
) -> Result<()> {
    if control.map(|c| &c.status) != events.last().map(|e| &e.status) {
        return Err(Error::Storage("迁移事件末尾与控制记录不符".into()));
    }
    if let Some(control) = control {
        control.validate(history, Mode::Service)?;
    }
    for event in events {
        event.status.validate(history, Mode::Service)?;
        if bindings.get(&event.status.changed_by) != Some(&event.account_id) {
            return Err(Error::Storage("迁移事件操作者绑定不符".into()));
        }
    }
    Ok(())
}

pub(super) async fn write(
    connection: &mut PgConnection,
    control: &TransferState,
    account: Id,
) -> Result<()> {
    let id = control.status.plan.workspace_id;
    let data = encoded(control)?;
    let old: Option<String> =
        query_scalar("SELECT data FROM ff_workspace_transfer WHERE workspace_id=$1")
            .bind(id)
            .fetch_optional(&mut *connection)
            .await
            .map_err(database_error)?;
    if old.as_deref() == Some(&data) {
        return Ok(());
    }
    query("INSERT INTO ff_workspace_transfer VALUES($1,$2,$3,$4) ON CONFLICT(workspace_id) DO UPDATE SET data=excluded.data,checksum=excluded.checksum,writable=excluded.writable")
        .bind(id).bind(data).bind(checksum(control)?).bind(control.writable()).execute(&mut *connection).await.map_err(database_error)?;
    let status = &control.status;
    query("INSERT INTO ff_workspace_transfer_events(workspace_id,account_id,data,checksum) VALUES($1,$2,$3,$4)")
        .bind(id).bind(account).bind(encoded(status)?).bind(checksum(status)?).execute(connection).await.map_err(database_error)?;
    Ok(())
}

pub(super) async fn restore(
    connection: &mut PgConnection,
    id: Id,
    control: Option<&TransferState>,
    events: &[Event],
) -> Result<()> {
    if let Some(control) = control {
        query("INSERT INTO ff_workspace_transfer VALUES($1,$2,$3,$4)")
            .bind(id)
            .bind(encoded(control)?)
            .bind(checksum(control)?)
            .bind(control.writable())
            .execute(&mut *connection)
            .await
            .map_err(database_error)?;
    }
    for event in events {
        query("INSERT INTO ff_workspace_transfer_events(workspace_id,account_id,data,checksum) VALUES($1,$2,$3,$4)").bind(id).bind(event.account_id).bind(encoded(&event.status)?).bind(checksum(&event.status)?).execute(&mut *connection).await.map_err(database_error)?;
    }
    Ok(())
}

async fn owned(
    server: &Server,
    token: &str,
    id: Id,
) -> Result<(PgTransaction<'static>, State, Id, User)> {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| Error::Forbidden("迁移需要有效登录".into()))?,
    );
    let Some((mut tx, user)) = workspace::session(server, &headers)
        .await
        .map_err(|e| e.0)?
    else {
        return Err(Error::Forbidden("迁移需要有效登录".into()));
    };
    let state = workspace::load(&mut tx, id, user.id, true)
        .await
        .map_err(|e| e.0)?;
    let actor = workspace::actor_for(&mut tx, id, user.id)
        .await
        .map_err(|e| e.0)?;
    if !workspace::access(&state, actor)?
        .roles
        .contains(&Role::Owner)
    {
        return Err(Error::Forbidden("只有空间所有者可以管理迁移".into()));
    }
    Ok((tx, state, actor, user))
}

async fn validate(connection: &mut PgConnection, state: &State) -> Result<Option<TransferState>> {
    let control = read(connection, state.id).await?;
    let events = events(connection, state.id).await?;
    if control.is_some() || !events.is_empty() {
        let history = migration::load_history(connection, state).await?;
        let bindings = workspace::bindings(connection, state.id)
            .await
            .map_err(|e| e.0)?;
        verify_events(control.as_ref(), &events, &history, &bindings)?;
    }
    Ok(control)
}

impl Server {
    pub async fn prepare_transfer(
        &self,
        token: &str,
        workspace: Id,
        id: Id,
        target: Mode,
        backup_destination: impl AsRef<Path>,
    ) -> Result<TransferStatus> {
        if id.is_nil() || target == Mode::Service {
            return Err(Error::Validation("迁移标识或目标模式无效".into()));
        }
        let (mut tx, state, _, _) = owned(self, token, workspace).await?;
        if let Some(control) = validate(&mut tx, &state).await? {
            if control.status.role == TransferRole::Source
                && control.status.phase == TransferPhase::Frozen
                && control.status.plan.id == id
                && control.status.plan.target_mode == target
            {
                return Ok(control.status);
            }
            control.ensure_writable()?;
            if control.status.plan.id == id {
                return Err(Error::Conflict("不能复用已结束的迁移标识".into()));
            }
        }
        if events(&mut tx, workspace)
            .await?
            .iter()
            .any(|event| event.status.plan.id == id)
        {
            return Err(Error::Conflict("不能复用历史迁移标识".into()));
        }
        tx.commit().await.map_err(database_error)?;
        self.export_copy(token, workspace, backup_destination)
            .await?;
        let (mut tx, state, actor, user) = owned(self, token, workspace).await?;
        if let Some(control) = validate(&mut tx, &state).await? {
            control.ensure_writable()?;
        }
        if events(&mut tx, workspace)
            .await?
            .iter()
            .any(|event| event.status.plan.id == id)
        {
            return Err(Error::Conflict("不能复用历史迁移标识".into()));
        }
        migration::exportable_members(&mut tx, &state).await?;
        let history = migration::load_history(&mut tx, &state).await?;
        let control = TransferState::prepare(
            &history,
            Mode::Service,
            target,
            id,
            actor,
            database_time(&mut tx).await.map_err(|e| e.0)?,
        )?;
        verify_barriers(&mut tx).await?;
        // 改变行版本，使冻结前的可重复读写事务也必须重试，不能凭旧快照绕过新屏障。
        query("UPDATE ff_workspaces SET revision=revision WHERE id=$1")
            .bind(workspace)
            .execute(&mut *tx)
            .await
            .map_err(database_error)?;
        write(&mut tx, &control, user.id).await?;
        validate(&mut tx, &state).await?;
        tx.commit().await.map_err(database_error)?;
        Ok(control.status)
    }

    pub async fn transfer_status(&self, token: &str, id: Id) -> Result<Option<TransferStatus>> {
        let (mut tx, state, _, _) = owned(self, token, id).await?;
        Ok(validate(&mut tx, &state).await?.map(|c| c.status))
    }

    pub async fn transfer_events(&self, token: &str, id: Id) -> Result<Vec<TransferStatus>> {
        let (mut tx, state, _, _) = owned(self, token, id).await?;
        validate(&mut tx, &state).await?;
        Ok(events(&mut tx, id)
            .await?
            .into_iter()
            .map(|e| e.status)
            .collect())
    }

    pub async fn export_transfer(
        &self,
        token: &str,
        id: Id,
        destination: impl AsRef<Path>,
    ) -> Result<MigrationReport> {
        let (mut tx, state, actor, _) = owned(self, token, id).await?;
        let Some(control) = validate(&mut tx, &state).await? else {
            return Err(Error::Validation("请先冻结源工作空间".into()));
        };
        if control.status.role != TransferRole::Source
            || !matches!(
                control.status.phase,
                TransferPhase::Frozen | TransferPhase::Completed
            )
        {
            return Err(Error::Validation("当前没有可导出的源迁移".into()));
        }
        if control.status.phase == TransferPhase::Frozen {
            migration::exportable_members(&mut tx, &state).await?;
        }
        let history = migration::load_history(&mut tx, &state).await?;
        let root = workspace::files(self, id, false)?;
        let destination = destination.as_ref().to_owned();
        let report = tokio::task::spawn_blocking(move || {
            history.export_transfer(&control.status.plan, actor, root, destination)
        })
        .await
        .map_err(|_| Error::Storage("正式迁移导出任务失败".into()))??;
        tx.commit().await.map_err(database_error)?;
        Ok(report)
    }

    pub async fn complete_transfer(
        &self,
        token: &str,
        id: Id,
        receipt: &TransferReceipt,
    ) -> Result<TransferActivation> {
        let (mut tx, state, actor, user) = owned(self, token, id).await?;
        let Some(mut control) = validate(&mut tx, &state).await? else {
            return Err(Error::Validation("源工作空间尚未冻结".into()));
        };
        if control.status.phase == TransferPhase::Frozen {
            migration::exportable_members(&mut tx, &state).await?;
        }
        let activation = control.complete(
            receipt,
            actor,
            database_time(&mut tx).await.map_err(|e| e.0)?,
        )?;
        write(&mut tx, &control, user.id).await?;
        tx.commit().await.map_err(database_error)?;
        Ok(activation)
    }

    pub async fn cancel_transfer(
        &self,
        token: &str,
        id: Id,
        transfer: Id,
    ) -> Result<TransferStatus> {
        let (mut tx, state, actor, user) = owned(self, token, id).await?;
        let Some(mut control) = validate(&mut tx, &state).await? else {
            return Err(Error::Validation("源工作空间尚未冻结".into()));
        };
        if control.status.plan.id != transfer {
            return Err(Error::Conflict("迁移标识不匹配".into()));
        }
        control.cancel(actor, database_time(&mut tx).await.map_err(|e| e.0)?)?;
        write(&mut tx, &control, user.id).await?;
        tx.commit().await.map_err(database_error)?;
        Ok(control.status)
    }

    pub async fn activate_transfer(
        &self,
        token: &str,
        id: Id,
        proof: &TransferActivation,
    ) -> Result<TransferStatus> {
        let (mut tx, state, actor, user) = owned(self, token, id).await?;
        let Some(mut control) = validate(&mut tx, &state).await? else {
            return Err(Error::Validation("目标没有暂存迁移".into()));
        };
        control.activate(proof, actor, database_time(&mut tx).await.map_err(|e| e.0)?)?;
        control.validate(
            &migration::load_history(&mut tx, &state).await?,
            Mode::Service,
        )?;
        write(&mut tx, &control, user.id).await?;
        tx.commit().await.map_err(database_error)?;
        Ok(control.status)
    }
}
