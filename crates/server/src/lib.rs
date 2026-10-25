use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use axum::{
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, patch, post},
    Json, Router,
};
use farflow_core::{Error, Id};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx_core::{
    error::Error as DatabaseError, query::query, query_scalar::query_scalar, raw_sql::raw_sql,
    row::Row,
};
use sqlx_postgres::{PgConnection, PgPool, PgPoolOptions, PgRow};
use std::{sync::Arc, time::Duration};
use tokio::sync::Semaphore;
mod backup;
mod migration;
mod storage;
mod transfer;
mod web;
mod workspace;
pub use backup::RestoreResult;

const ACCOUNT_LOCK: i64 = 7_016_404_614;
const SCHEMA_LOCK: i64 = 7_016_404_615;
const SESSION_TTL: i64 = 8 * 60 * 60;

#[derive(Clone)]
pub struct Server {
    pool: PgPool,
    passwords: Arc<Semaphore>,
    dummy_hash: Arc<str>,
    data_dir: Arc<std::path::PathBuf>,
    storage_id: Id,
}

impl Server {
    pub async fn connect(
        database_url: &str,
        data_dir: impl AsRef<std::path::Path>,
    ) -> farflow_core::Result<Self> {
        Self::connect_inner(database_url, data_dir.as_ref(), false).await
    }

    async fn connect_inner(
        database_url: &str,
        data_dir: &std::path::Path,
        restoring: bool,
    ) -> farflow_core::Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(12)
            .acquire_timeout(Duration::from_secs(5))
            .connect(database_url)
            .await
            .map_err(|_| Error::Storage("无法连接服务数据库".into()))?;
        let mut transaction = pool.begin().await.map_err(database_error)?;
        query("SELECT pg_advisory_xact_lock($1)")
            .bind(SCHEMA_LOCK)
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?;
        raw_sql(include_str!("../migrations/0001_identity.sql"))
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?;
        raw_sql(include_str!("../migrations/0002_workspaces.sql"))
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?;
        raw_sql(include_str!("../migrations/0003_storage.sql"))
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?;
        raw_sql(include_str!("../migrations/0004_restore.sql"))
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?;
        raw_sql(include_str!("../migrations/0005_workspace_identity.sql"))
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?;
        let payload_columns:i64=query_scalar("SELECT COUNT(*) FROM information_schema.columns WHERE table_schema=current_schema() AND data_type='text' AND ((table_name='ff_workspaces' AND column_name IN ('genesis','state')) OR (table_name='ff_workspace_operations' AND column_name IN ('client','request','receipt')) OR (table_name='ff_workspace_origins' AND column_name IN ('data','checksum')) OR (table_name='ff_workspace_origin_operations' AND column_name IN ('request','client','receipt')))")
            .fetch_one(&mut *transaction).await.map_err(database_error)?;
        let has_transfer: bool =
            query_scalar("SELECT to_regclass('ff_workspace_transfer') IS NOT NULL")
                .fetch_one(&mut *transaction)
                .await
                .map_err(database_error)?;
        if has_transfer {
            let count: i64 = query_scalar("SELECT (SELECT COUNT(*) FROM ff_workspace_transfer)+(SELECT COUNT(*) FROM ff_workspace_transfer_events)")
                .fetch_one(&mut *transaction)
                .await
                .map_err(database_error)?;
            if count > 0 {
                transfer::verify_barriers(&mut transaction).await?;
            }
        }
        raw_sql(transfer::SCHEMA)
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?;
        transfer::verify_barriers(&mut transaction).await?;
        let transfer_columns:i64=query_scalar("SELECT COUNT(*) FROM information_schema.columns WHERE table_schema=current_schema() AND data_type='text' AND table_name IN ('ff_workspace_transfer','ff_workspace_transfer_events') AND column_name IN ('data','checksum')").fetch_one(&mut *transaction).await.map_err(database_error)?;
        if payload_columns != 10 || transfer_columns != 4 {
            return Err(Error::Validation(
                "服务工作空间存储格式不兼容，请先备份并升级".into(),
            ));
        }
        transaction.commit().await.map_err(database_error)?;
        let data_dir = workspace::initialize_files(data_dir.to_owned()).await?;
        let storage_id = storage::bind(&pool, data_dir.clone()).await?;
        if !restoring {
            backup::ensure_ready(&mut *pool.acquire().await.map_err(database_error)?).await?;
        }
        let passwords = Arc::new(Semaphore::new(4));
        let dummy_hash =
            password_hash(passwords.clone(), "FarFlow-unknown-account-dummy".into()).await?;
        Ok(Self {
            pool,
            passwords,
            dummy_hash: dummy_hash.into(),
            data_dir: Arc::new(data_dir),
            storage_id,
        })
    }

    pub async fn bootstrap(
        &self,
        account: String,
        name: String,
        password: String,
    ) -> farflow_core::Result<()> {
        let account = normalize_account(&account)?;
        validate_profile(&name, "")?;
        validate_password(&password)?;
        let hash = password_hash(self.passwords.clone(), password).await?;
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        query("SELECT pg_advisory_xact_lock($1)")
            .bind(ACCOUNT_LOCK)
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?;
        let count: i64 = query_scalar("SELECT COUNT(*) FROM ff_accounts")
            .fetch_one(&mut *transaction)
            .await
            .map_err(database_error)?;
        backup::ensure_ready(&mut transaction).await?;
        storage::verify(&self.data_dir, self.storage_id)?;
        if count != 0 {
            return Err(Error::Conflict(
                "服务已初始化，不能重复创建初始管理员".into(),
            ));
        }
        let row =
            query("INSERT INTO ff_accounts(id,account,name,department,password_hash,active,is_admin,must_change_password,version) VALUES ($1,$2,$3,'',$4,true,true,false,1) RETURNING *")
                .bind(Id::new_v4())
                .bind(account)
                .bind(name)
                .bind(hash)
                .fetch_one(&mut *transaction)
                .await
                .map_err(database_error)?;
        let user = User::from_row(&row);
        record(&mut transaction, user.id, user.id, "bootstrap", None, &user).await?;
        transaction.commit().await.map_err(database_error)?;
        Ok(())
    }

    pub fn router(&self) -> Router {
        Router::new()
            .route("/api/auth/login", post(login))
            .route("/api/auth/logout", post(logout))
            .route("/api/auth/me", get(me))
            .route("/api/auth/password", post(change_password))
            .route("/api/users", get(list_users).post(create_user))
            .route("/api/users/{id}", patch(update_user))
            .route("/api/users/{id}/password", post(reset_password))
            .route("/api/users/{id}/history", get(user_history))
            .layer(DefaultBodyLimit::max(16 * 1024))
            .merge(workspace::router())
            .layer(middleware::map_response(api_response))
            .with_state(self.clone())
    }

    pub fn router_with_web(
        &self,
        directory: impl AsRef<std::path::Path>,
    ) -> farflow_core::Result<Router> {
        web::router(self.router(), directory.as_ref(), &self.data_dir)
    }
}

async fn api_response(mut response: Response) -> Response {
    if (response.status().is_client_error() || response.status().is_server_error())
        && response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            != Some("application/json")
    {
        let message = match response.status() {
            StatusCode::NOT_FOUND => "接口不存在",
            StatusCode::METHOD_NOT_ALLOWED => "此接口不支持该请求方式",
            StatusCode::PAYLOAD_TOO_LARGE => "请求内容超过大小限制",
            StatusCode::UNSUPPORTED_MEDIA_TYPE => "请求需使用 JSON 格式",
            StatusCode::INTERNAL_SERVER_ERROR => "服务暂时不可用",
            _ => "请求字段或内容格式不正确",
        };
        let allow = response.headers().get(header::ALLOW).cloned();
        response = (response.status(), Json(json!({"message":message}))).into_response();
        if let Some(allow) = allow {
            response.headers_mut().insert(header::ALLOW, allow);
        }
    }
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        "no-store".parse().expect("固定响应头有效"),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        "nosniff".parse().expect("固定响应头有效"),
    );
    response
}

#[derive(Debug)]
struct ApiError(Error);
impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        Self(error)
    }
}
impl From<DatabaseError> for ApiError {
    fn from(error: DatabaseError) -> Self {
        Self(database_error(error))
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self.0 {
            Error::Validation(_) => StatusCode::BAD_REQUEST,
            Error::NotFound(_) => StatusCode::NOT_FOUND,
            Error::Conflict(_) => StatusCode::CONFLICT,
            Error::Forbidden(_) => StatusCode::FORBIDDEN,
            Error::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(json!({"message":self.0.to_string()}))).into_response()
    }
}
type ApiResult<T> = Result<T, ApiError>;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct User {
    id: Id,
    account: String,
    name: String,
    department: String,
    active: bool,
    is_admin: bool,
    must_change_password: bool,
    version: i64,
}
impl User {
    fn from_row(row: &PgRow) -> Self {
        Self {
            id: row.get("id"),
            account: row.get("account"),
            name: row.get("name"),
            department: row.get("department"),
            active: row.get("active"),
            is_admin: row.get("is_admin"),
            must_change_password: row.get("must_change_password"),
            version: row.get("version"),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Login {
    account: String,
    password: String,
}

async fn login(State(server): State<Server>, Json(input): Json<Login>) -> ApiResult<Response> {
    let account = normalize_account(&input.account)?;
    if input.password.len() > 128 || input.password.is_empty() {
        return Ok(invalid_login());
    }
    let mut tx = server.pool.begin().await?;
    let now = database_time(&mut tx).await?;
    query("DELETE FROM ff_login_attempts WHERE window_start < $1")
        .bind(now - 86400)
        .execute(&mut *tx)
        .await?;
    query("INSERT INTO ff_login_attempts VALUES ($1,0,$2,0) ON CONFLICT DO NOTHING")
        .bind(&account)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    let attempts = query("SELECT * FROM ff_login_attempts WHERE account=$1 FOR UPDATE")
        .bind(&account)
        .fetch_one(&mut *tx)
        .await?;
    if attempts.get::<i64, _>("blocked_until") > now {
        return Ok((
            StatusCode::TOO_MANY_REQUESTS,
            [(
                header::RETRY_AFTER,
                (attempts.get::<i64, _>("blocked_until") - now).to_string(),
            )],
            Json(json!({"message":"尝试过多，请稍后重试"})),
        )
            .into_response());
    }
    let row = query("SELECT * FROM ff_accounts WHERE account=$1")
        .bind(&account)
        .fetch_optional(&mut *tx)
        .await?;
    let hash = row.as_ref().map_or(server.dummy_hash.to_string(), |row| {
        row.get::<String, _>("password_hash")
    });
    let verified = password_matches(server.passwords.clone(), input.password, hash.clone()).await?;
    let current = query("SELECT * FROM ff_accounts WHERE account=$1 FOR SHARE")
        .bind(&account)
        .fetch_optional(&mut *tx)
        .await?;
    let valid = verified
        && current.as_ref().is_some_and(|row| {
            row.get::<bool, _>("active") && row.get::<String, _>("password_hash") == hash
        });
    if !valid {
        let window = attempts.get::<i64, _>("window_start");
        let failures = if now - window >= 600 {
            1
        } else {
            attempts.get::<i32, _>("failures") + 1
        };
        query("UPDATE ff_login_attempts SET failures=$2,window_start=$3,blocked_until=$4 WHERE account=$1")
            .bind(&account).bind(failures).bind(if now-window >= 600 {now} else {window})
            .bind(if failures>=5 {now+300} else {0}).execute(&mut *tx).await?;
        tx.commit().await?;
        return Ok(invalid_login());
    }
    let user = User::from_row(
        current
            .as_ref()
            .ok_or_else(|| Error::Forbidden("账户无效".into()))?,
    );
    let token = format!("{}{}", Id::new_v4().simple(), Id::new_v4().simple());
    let now = database_time(&mut tx).await?;
    query("DELETE FROM ff_sessions WHERE expires_at <= $1")
        .bind(now)
        .execute(&mut *tx)
        .await?;
    query("INSERT INTO ff_sessions VALUES ($1,$2,$3,$4)")
        .bind(token_hash(&token))
        .bind(user.id)
        .bind(now)
        .bind(now + SESSION_TTL)
        .execute(&mut *tx)
        .await?;
    query("DELETE FROM ff_login_attempts WHERE account=$1")
        .bind(account)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({"token":token,"expires_at":now+SESSION_TTL,"user":user})).into_response())
}

fn invalid_login() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        Json(json!({"message":"账户或密码不正确"})),
    )
        .into_response()
}

async fn me(State(server): State<Server>, headers: HeaderMap) -> ApiResult<Response> {
    let mut tx = server.pool.begin().await?;
    match authenticate(&mut tx, &headers).await? {
        Some(user) => Ok(Json(user).into_response()),
        None => Ok(unauthorized()),
    }
}

async fn logout(State(server): State<Server>, headers: HeaderMap) -> ApiResult<Response> {
    let mut tx = server.pool.begin().await?;
    if authenticate(&mut tx, &headers).await?.is_none() {
        return Ok(unauthorized());
    }
    query("DELETE FROM ff_sessions WHERE token_hash=$1")
        .bind(token_hash(bearer(&headers).unwrap_or("")))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChangePassword {
    current_password: String,
    new_password: String,
}
async fn change_password(
    State(server): State<Server>,
    headers: HeaderMap,
    Json(input): Json<ChangePassword>,
) -> ApiResult<Response> {
    validate_password(&input.new_password)?;
    if input.current_password.len() > 128 || input.current_password == input.new_password {
        return Err(Error::Validation("新旧密码不能相同，原密码不能超过 128 字节".into()).into());
    }
    let mut tx = server.pool.begin().await?;
    account_lock(&mut tx).await?;
    let Some(user) = authenticate(&mut tx, &headers).await? else {
        return Ok(unauthorized());
    };
    let old: String = query_scalar("SELECT password_hash FROM ff_accounts WHERE id=$1")
        .bind(user.id)
        .fetch_one(&mut *tx)
        .await?;
    if !password_matches(server.passwords.clone(), input.current_password, old).await? {
        return Err(Error::Validation("原密码不正确".into()).into());
    }
    let hash = password_hash(server.passwords.clone(), input.new_password).await?;
    let row=query("UPDATE ff_accounts SET password_hash=$2,must_change_password=false,version=version+1 WHERE id=$1 RETURNING *")
        .bind(user.id).bind(hash).fetch_one(&mut *tx).await?;
    record(
        &mut tx,
        user.id,
        user.id,
        "change_password",
        Some(&user),
        &User::from_row(&row),
    )
    .await?;
    revoke(&mut tx, user.id).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn list_users(State(server): State<Server>, headers: HeaderMap) -> ApiResult<Response> {
    let mut tx = server.pool.begin().await?;
    let Some(user) = authenticate(&mut tx, &headers).await? else {
        return Ok(unauthorized());
    };
    administrator(&user)?;
    let rows = query("SELECT * FROM ff_accounts ORDER BY account")
        .fetch_all(&mut *tx)
        .await?;
    Ok(Json(rows.iter().map(User::from_row).collect::<Vec<_>>()).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewUser {
    account: String,
    name: String,
    #[serde(default)]
    department: String,
    password: String,
    #[serde(default)]
    is_admin: bool,
}
async fn create_user(
    State(server): State<Server>,
    headers: HeaderMap,
    Json(input): Json<NewUser>,
) -> ApiResult<Response> {
    let account = normalize_account(&input.account)?;
    validate_profile(&input.name, &input.department)?;
    validate_password(&input.password)?;
    let mut tx = server.pool.begin().await?;
    account_lock(&mut tx).await?;
    let Some(admin) = authenticate(&mut tx, &headers).await? else {
        return Ok(unauthorized());
    };
    administrator(&admin)?;
    let hash = password_hash(server.passwords.clone(), input.password).await?;
    let row = query("INSERT INTO ff_accounts(id,account,name,department,password_hash,active,is_admin,must_change_password,version) VALUES ($1,$2,$3,$4,$5,true,$6,true,1) RETURNING *")
        .bind(Id::new_v4())
        .bind(account)
        .bind(input.name)
        .bind(input.department)
        .bind(hash)
        .bind(input.is_admin)
        .fetch_one(&mut *tx)
        .await?;
    let user = User::from_row(&row);
    record(&mut tx, user.id, admin.id, "create", None, &user).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(user)).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateUser {
    version: i64,
    name: String,
    department: String,
    active: bool,
    is_admin: bool,
}
async fn update_user(
    State(server): State<Server>,
    headers: HeaderMap,
    Path(id): Path<Id>,
    Json(input): Json<UpdateUser>,
) -> ApiResult<Response> {
    validate_profile(&input.name, &input.department)?;
    let mut tx = server.pool.begin().await?;
    account_lock(&mut tx).await?;
    let Some(admin) = authenticate(&mut tx, &headers).await? else {
        return Ok(unauthorized());
    };
    administrator(&admin)?;
    let old = query("SELECT * FROM ff_accounts WHERE id=$1 FOR UPDATE")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| Error::NotFound("服务账户".into()))?;
    let old = User::from_row(&old);
    if old.version != input.version {
        return Err(Error::Conflict("账户已更新，请刷新后重试".into()).into());
    }
    if old.active && old.is_admin && (!input.active || !input.is_admin) {
        let count: i64 = query_scalar("SELECT COUNT(*) FROM ff_accounts WHERE active AND is_admin")
            .fetch_one(&mut *tx)
            .await?;
        if count <= 1 {
            return Err(Error::Validation("必须保留至少一名启用的服务管理员".into()).into());
        }
    }
    if old.active && !input.active {
        workspace::ensure_account_can_disable(&mut tx, id).await?;
    }
    let row = query("UPDATE ff_accounts SET name=$2,department=$3,active=$4,is_admin=$5,version=version+1 WHERE id=$1 RETURNING *")
        .bind(id).bind(input.name).bind(input.department).bind(input.active).bind(input.is_admin).fetch_one(&mut *tx).await?;
    let user = User::from_row(&row);
    record(&mut tx, id, admin.id, "update", Some(&old), &user).await?;
    if !input.active || old.is_admin != input.is_admin {
        revoke(&mut tx, id).await?;
    }
    tx.commit().await?;
    Ok(Json(user).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResetPassword {
    password: String,
}
async fn reset_password(
    State(server): State<Server>,
    headers: HeaderMap,
    Path(id): Path<Id>,
    Json(input): Json<ResetPassword>,
) -> ApiResult<Response> {
    validate_password(&input.password)?;
    let mut tx = server.pool.begin().await?;
    account_lock(&mut tx).await?;
    let Some(admin) = authenticate(&mut tx, &headers).await? else {
        return Ok(unauthorized());
    };
    administrator(&admin)?;
    let old = query("SELECT * FROM ff_accounts WHERE id=$1 FOR UPDATE")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| Error::NotFound("服务账户".into()))?;
    let old = User::from_row(&old);
    let hash = password_hash(server.passwords.clone(), input.password).await?;
    let row = query("UPDATE ff_accounts SET password_hash=$2,must_change_password=true,version=version+1 WHERE id=$1 RETURNING *")
        .bind(id).bind(hash).fetch_one(&mut *tx).await?;
    record(
        &mut tx,
        id,
        admin.id,
        "reset_password",
        Some(&old),
        &User::from_row(&row),
    )
    .await?;
    revoke(&mut tx, id).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryQuery {
    #[serde(default)]
    after: i64,
}
async fn user_history(
    State(server): State<Server>,
    headers: HeaderMap,
    Path(id): Path<Id>,
    Query(input): Query<HistoryQuery>,
) -> ApiResult<Response> {
    if input.after < 0 {
        return Err(Error::Validation("历史游标无效".into()).into());
    }
    let mut tx = server.pool.begin().await?;
    let Some(admin) = authenticate(&mut tx, &headers).await? else {
        return Ok(unauthorized());
    };
    administrator(&admin)?;
    let exists: bool = query_scalar("SELECT EXISTS(SELECT 1 FROM ff_accounts WHERE id=$1)")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    if !exists {
        return Err(Error::NotFound("服务账户".into()).into());
    }
    let rows=query("SELECT sequence,actor_id,action,at,data FROM ff_account_events WHERE account_id=$1 AND sequence>$2 ORDER BY sequence LIMIT 101")
        .bind(id).bind(input.after).fetch_all(&mut *tx).await?;
    let more = rows.len() > 100;
    let events:Vec<_>=rows.iter().take(100).map(|row| {
        let data:serde_json::Value=serde_json::from_str(&row.get::<String,_>("data"))
            .map_err(|_| Error::Storage("账户操作记录格式异常".into()))?;
        Ok(json!({"sequence":row.get::<i64,_>("sequence"),"actor_id":row.get::<Id,_>("actor_id"),"action":row.get::<String,_>("action"),"at":row.get::<i64,_>("at"),"data":data}))
    }).collect::<farflow_core::Result<_>>()?;
    let next = if more {
        events.last().and_then(|event| event["sequence"].as_i64())
    } else {
        None
    };
    Ok(Json(json!({"events":events,"next":next})).into_response())
}

async fn record(
    connection: &mut PgConnection,
    target: Id,
    actor: Id,
    action: &str,
    before: Option<&User>,
    after: &User,
) -> farflow_core::Result<()> {
    // 审计与账号变更共用事务，记录公开资料快照，避免密码哈希或会话令牌进入历史。
    query("INSERT INTO ff_account_events(account_id,actor_id,action,at,data) VALUES ($1,$2,$3,FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint,$4)")
        .bind(target).bind(actor).bind(action).bind(json!({"before":before,"after":after}).to_string())
        .execute(connection).await.map_err(database_error)?;
    Ok(())
}

async fn authenticate(
    connection: &mut PgConnection,
    headers: &HeaderMap,
) -> ApiResult<Option<User>> {
    let Some(token) = bearer(headers) else {
        return Ok(None);
    };
    let now = database_time(connection).await?;
    let row = query("SELECT a.* FROM ff_sessions s JOIN ff_accounts a ON a.id=s.account_id WHERE s.token_hash=$1 AND s.expires_at>$2 AND a.active FOR SHARE OF a")
        .bind(token_hash(token)).bind(now).fetch_optional(connection).await?;
    Ok(row.as_ref().map(User::from_row))
}
fn bearer(headers: &HeaderMap) -> Option<&str> {
    let (scheme, token) = headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return None;
    }
    Some(token.trim_matches(' ')).filter(|token| {
        token.len() == 64
            && token
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}
fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        Json(json!({"message":"登录已失效，请重新登录"})),
    )
        .into_response()
}
fn administrator(user: &User) -> farflow_core::Result<()> {
    if user.must_change_password {
        return Err(Error::Forbidden("请先修改初始密码".into()));
    }
    if !user.is_admin {
        return Err(Error::Forbidden("需要服务管理员权限".into()));
    }
    Ok(())
}
async fn account_lock(connection: &mut PgConnection) -> ApiResult<()> {
    query("SELECT pg_advisory_xact_lock($1)")
        .bind(ACCOUNT_LOCK)
        .execute(connection)
        .await?;
    Ok(())
}
async fn revoke(connection: &mut PgConnection, id: Id) -> ApiResult<()> {
    query("DELETE FROM ff_sessions WHERE account_id=$1")
        .bind(id)
        .execute(connection)
        .await?;
    Ok(())
}
async fn database_time(connection: &mut PgConnection) -> ApiResult<i64> {
    Ok(
        query_scalar("SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint")
            .fetch_one(connection)
            .await?,
    )
}
fn token_hash(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}
fn normalize_account(value: &str) -> farflow_core::Result<String> {
    let value = value.trim().to_ascii_lowercase();
    if !(3..=120).contains(&value.len())
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._@-".contains(&byte))
    {
        return Err(Error::Validation(
            "账号需为 3 至 120 位字母、数字或 . _ @ -".into(),
        ));
    }
    Ok(value)
}
fn validate_profile(name: &str, department: &str) -> farflow_core::Result<()> {
    if name.trim().is_empty()
        || name.chars().count() > 120
        || department.chars().count() > 120
        || name.chars().chain(department.chars()).any(char::is_control)
    {
        return Err(Error::Validation(
            "姓名不能为空，姓名和部门各不超过 120 字，且不能含控制字符".into(),
        ));
    }
    Ok(())
}
fn validate_password(password: &str) -> farflow_core::Result<()> {
    if password.chars().count() < 12 || password.len() > 128 || password.trim().is_empty() {
        return Err(Error::Validation(
            "密码至少 12 个字符，不超过 128 字节，且不能全为空白".into(),
        ));
    }
    Ok(())
}
async fn password_hash(
    semaphore: Arc<Semaphore>,
    password: String,
) -> farflow_core::Result<String> {
    let permit = semaphore
        .acquire_owned()
        .await
        .map_err(|_| Error::Storage("密码处理已停止".into()))?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        Argon2::default()
            .hash_password(password.as_bytes())
            .map(|hash| hash.to_string())
            .map_err(|_| Error::Storage("密码处理失败".into()))
    })
    .await
    .map_err(|_| Error::Storage("密码处理任务失败".into()))?
}
async fn password_matches(
    semaphore: Arc<Semaphore>,
    password: String,
    hash: String,
) -> farflow_core::Result<bool> {
    let permit = semaphore
        .acquire_owned()
        .await
        .map_err(|_| Error::Storage("密码处理已停止".into()))?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let hash =
            PasswordHash::new(&hash).map_err(|_| Error::Storage("密码记录格式异常".into()))?;
        Ok(Argon2::default()
            .verify_password(password.as_bytes(), &hash)
            .is_ok())
    })
    .await
    .map_err(|_| Error::Storage("密码处理任务失败".into()))?
}
fn database_error(error: DatabaseError) -> Error {
    if error
        .as_database_error()
        .is_some_and(|error| error.is_unique_violation())
    {
        Error::Conflict("账号已存在".into())
    } else {
        Error::Storage("服务数据库操作失败".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn password_processing_uses_random_salts_and_reports_invalid_records_and_shutdown() {
        let semaphore = Arc::new(Semaphore::new(1));
        let password = "不少于十二个字符的中文安全口令".to_owned();
        let first = password_hash(semaphore.clone(), password.clone())
            .await
            .unwrap();
        let second = password_hash(semaphore.clone(), password.clone())
            .await
            .unwrap();
        assert_ne!(first, second);
        assert!(
            password_matches(semaphore.clone(), password.clone(), first.clone())
                .await
                .unwrap()
        );
        assert!(
            !password_matches(semaphore.clone(), "错误口令".into(), first)
                .await
                .unwrap()
        );
        assert!(
            password_matches(semaphore.clone(), password.clone(), "损坏的哈希".into())
                .await
                .is_err()
        );
        semaphore.close();
        assert!(password_hash(semaphore.clone(), password.clone())
            .await
            .is_err());
        assert!(password_matches(semaphore, password, second).await.is_err());
    }

    #[test]
    fn account_profile_password_and_token_boundaries_are_explicit() {
        assert_eq!(
            normalize_account("  Member@Example.com  ").unwrap(),
            "member@example.com"
        );
        for value in ["", "ab", "has space", "中文账号", &"a".repeat(121)] {
            assert!(normalize_account(value).is_err());
        }
        assert!(normalize_account(&"a".repeat(120)).is_ok());
        assert!(validate_profile(&"字".repeat(120), &"字".repeat(120)).is_ok());
        for (name, department) in [
            (" ".into(), "".into()),
            ("字".repeat(121), "".into()),
            ("姓名".into(), "字".repeat(121)),
            ("姓名".into(), "部门\0".into()),
        ] {
            assert!(validate_profile(&name, &department).is_err());
        }
        for password in [" ".repeat(12), "short".into(), "字".repeat(43)] {
            assert!(validate_password(&password).is_err());
        }
        assert!(validate_password(&"a".repeat(12)).is_ok());
        assert!(validate_password(&"a".repeat(128)).is_ok());
        for value in [
            "Basic aa".to_owned(),
            format!("Bearer {}", "A".repeat(64)),
            format!("Bearer {}", "a".repeat(63)),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(header::AUTHORIZATION, value.parse().unwrap());
            assert!(bearer(&headers).is_none());
        }
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            axum::http::HeaderValue::from_bytes(&[255]).unwrap(),
        );
        assert!(bearer(&headers).is_none());
        for scheme in ["Bearer", "bearer", "BEARER"] {
            headers.insert(
                header::AUTHORIZATION,
                format!("{scheme} {}", "a".repeat(64)).parse().unwrap(),
            );
            assert_eq!(bearer(&headers).unwrap(), "a".repeat(64));
        }
    }

    #[tokio::test]
    async fn api_errors_are_chinese_json_with_cache_and_content_protection() {
        for (error, status) in [
            (
                Error::Validation("字段无效".into()),
                StatusCode::BAD_REQUEST,
            ),
            (Error::NotFound("账户".into()), StatusCode::NOT_FOUND),
            (Error::Conflict("版本".into()), StatusCode::CONFLICT),
            (Error::Forbidden("角色".into()), StatusCode::FORBIDDEN),
            (
                Error::Storage("数据库不可用".into()),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ] {
            let response = api_response(ApiError(error).into_response()).await;
            assert_eq!(response.status(), status);
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            assert_eq!(
                response.headers()[header::X_CONTENT_TYPE_OPTIONS],
                "nosniff"
            );
        }
        for status in [
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            StatusCode::INTERNAL_SERVER_ERROR,
        ] {
            let response =
                api_response((status, "internal implementation detail").into_response()).await;
            let body = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            assert!(!String::from_utf8(body.to_vec())
                .unwrap()
                .contains("internal implementation detail"));
        }
    }
}
