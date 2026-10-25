use super::{
    administrator, authenticate, database_error, database_time, normalize_account, storage,
    validate_profile, workspace, Server, ACCOUNT_LOCK, SCHEMA_LOCK,
};
use argon2::{Params, PasswordHash};
use axum::http::{header, HeaderMap, HeaderValue};
use farflow_core::{
    hash_file, read_attachment_bytes, read_file, regular_directory, sync_directory, valid_hash,
    write_attachment_bytes, write_json, Attachment, Error, GitLog, Id, MigrationHistory, Receipt,
    Request, Result, Role, State, TransferState,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx_core::{query::query, query_scalar::query_scalar, row::Row};
use sqlx_postgres::PgConnection;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{BufRead, BufReader, Read, Write},
    marker::PhantomData,
    path::Path,
};
use workspace::{checksum, create_directory, decoded, disk_error, encoded, ClientRequest};

const LINE_LIMIT: u64 = 16 * 1024 * 1024;
const STATE_LIMIT: u64 = 512 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileInfo {
    sha256: String,
    size: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: u32,
    id: Id,
    source_storage_id: Id,
    created_at: i64,
    files: BTreeMap<String, FileInfo>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Account {
    id: Id,
    account: String,
    name: String,
    department: String,
    password_hash: String,
    active: bool,
    is_admin: bool,
    must_change_password: bool,
    version: i64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AccountEvent {
    sequence: i64,
    account_id: Id,
    actor_id: Id,
    action: String,
    at: i64,
    data: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AccountHistory {
    before: Option<super::User>,
    after: super::User,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginAttempt {
    account: String,
    failures: i32,
    window_start: i64,
    blocked_until: i64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Workspace {
    id: Id,
    name: String,
    creator_id: Id,
    revision: i64,
    genesis: String,
    state: String,
    checksum: String,
    #[serde(default)]
    bindings: BTreeMap<Id, Id>,
    #[serde(default)]
    origin: Option<MigrationHistory>,
    #[serde(default)]
    transfer: Option<TransferState>,
    #[serde(default)]
    transfer_events: Vec<super::transfer::Event>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Operation {
    id: Id,
    revision: i64,
    client: String,
    request: String,
    receipt: String,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct RestoreResult {
    pub backup_id: Id,
    pub already_completed: bool,
}

pub(super) async fn ensure_ready(connection: &mut PgConnection) -> Result<()> {
    let pending: bool = query_scalar("SELECT EXISTS(SELECT 1 FROM ff_restore WHERE NOT completed)")
        .fetch_one(connection)
        .await
        .map_err(database_error)?;
    if pending {
        return Err(Error::Conflict(
            "服务恢复尚未完成，请重试同一备份的 restore 命令".into(),
        ));
    }
    Ok(())
}

impl Server {
    pub async fn backup(&self, token: &str, destination: impl AsRef<Path>) -> Result<Id> {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| Error::Forbidden("备份需要有效管理员会话".into()))?,
        );
        let mut tx = self.pool.begin().await.map_err(database_error)?;
        query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut *tx)
            .await
            .map_err(database_error)?;
        query("SELECT pg_advisory_xact_lock_shared($1)")
            .bind(ACCOUNT_LOCK)
            .execute(&mut *tx)
            .await
            .map_err(database_error)?;
        let user = authenticate(&mut tx, &headers)
            .await
            .map_err(|error| error.0)?
            .ok_or_else(|| Error::Forbidden("备份需要有效管理员会话".into()))?;
        administrator(&user)?;
        ensure_ready(&mut tx).await?;
        storage::verify(&self.data_dir, self.storage_id)?;
        let destination = destination.as_ref();
        let parent = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let parent = fs::canonicalize(parent).map_err(disk_error)?;
        if parent.starts_with(self.data_dir.as_ref()) {
            return invalid("备份请保存到服务数据目录之外");
        }
        let destination = parent.join(
            destination
                .file_name()
                .ok_or_else(|| Error::Validation("备份目录名称无效".into()))?,
        );
        // 先独占创建目录，清单最后落盘；中断产生的半包不会被当成有效备份。
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&destination).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                Error::Conflict("备份目标已存在，请选择新目录".into())
            } else {
                disk_error(error)
            }
        })?;
        let result = self.export(&mut tx, &destination).await;
        if result.is_err() {
            let _ = fs::remove_dir_all(&destination);
        }
        let id = result?;
        tx.commit().await.map_err(database_error)?;
        Ok(id)
    }

    async fn export(&self, connection: &mut PgConnection, output: &Path) -> Result<Id> {
        let created_at = database_time(connection).await.map_err(|e| e.0)?;
        export_rows(connection, "SELECT id::text AS cursor,row_to_json(a)::text AS data FROM ff_accounts a WHERE id::text>$1 ORDER BY id::text LIMIT 500", "", &output.join("accounts.jsonl")).await?;
        export_rows(connection, "SELECT sequence::text AS cursor,row_to_json(a)::text AS data FROM ff_account_events a WHERE sequence>$1::text::bigint ORDER BY sequence LIMIT 500", "0", &output.join("account-events.jsonl")).await?;
        export_rows(connection, "SELECT account AS cursor,row_to_json(a)::text AS data FROM ff_login_attempts a WHERE account>$1 ORDER BY account LIMIT 500", "", &output.join("login-attempts.jsonl")).await?;
        create_directory(&output.join("workspaces"))?;
        let mut after = Id::nil();
        loop {
            let rows = query("SELECT row_to_json(w)::text AS data FROM ff_workspaces w WHERE id>$1 ORDER BY id LIMIT 20")
                .bind(after).fetch_all(&mut *connection).await.map_err(database_error)?;
            if rows.is_empty() {
                break;
            }
            for row in rows {
                let mut data: Workspace = decoded(&row.get::<String, _>("data"))?;
                data.bindings = workspace::bindings(connection, data.id)
                    .await
                    .map_err(|e| e.0)?;
                data.origin = super::migration::read_origin(connection, data.id).await?;
                data.transfer = super::transfer::read(connection, data.id).await?;
                data.transfer_events = super::transfer::events(connection, data.id).await?;
                if let Some(history) = &data.origin {
                    super::migration::verify_origin_cache(connection, data.id, history).await?;
                }
                after = data.id;
                let target = output.join("workspaces").join(data.id.to_string());
                create_directory(&target)?;
                create_directory(&target.join("attachments"))?;
                write_json(&target.join("workspace.json"), &data)?;
                let mut file = private_file(&target.join("operations.jsonl"))?;
                let mut revision = 0i64;
                loop {
                    let operations = query("SELECT revision,row_to_json(o)::text AS data FROM (SELECT id,revision,client,request,receipt FROM ff_workspace_operations WHERE workspace_id=$1 AND revision>$2 ORDER BY revision LIMIT 500) o")
                        .bind(data.id).bind(revision).fetch_all(&mut *connection).await.map_err(database_error)?;
                    if operations.is_empty() {
                        break;
                    }
                    for operation in operations {
                        revision = operation.get("revision");
                        writeln!(file, "{}", operation.get::<String, _>("data"))
                            .map_err(disk_error)?;
                    }
                }
                file.sync_all().map_err(disk_error)?;
                let state: State = decoded(&data.state)?;
                let attachments = required_attachments(&data, &state);
                let source = if attachments.is_empty() {
                    None
                } else {
                    Some(workspace::files(self, data.id, false)?)
                };
                for attachment in &attachments {
                    let bytes = read_attachment_bytes(
                        source.as_ref().expect("附件非空时已有目录"),
                        attachment,
                    )?;
                    write_attachment_bytes(&target, &attachment.sha256, &bytes)?;
                }
                let actual = query("SELECT user_id,active,owner FROM ff_workspace_members WHERE workspace_id=$1 ORDER BY user_id")
                    .bind(data.id).fetch_all(&mut *connection).await.map_err(database_error)?;
                let indexed: Vec<_> = actual
                    .iter()
                    .map(|row| {
                        (
                            row.get::<Id, _>("user_id"),
                            row.get::<bool, _>("active"),
                            row.get::<bool, _>("owner"),
                        )
                    })
                    .collect();
                let mut expected: Vec<_> = state
                    .members
                    .values()
                    .map(|m| {
                        data.bindings
                            .get(&m.id)
                            .copied()
                            .map(|id| (id, m.active, m.roles.contains(&Role::Owner)))
                            .ok_or_else(|| Error::Storage("备份成员缺少账户绑定".into()))
                    })
                    .collect::<Result<_>>()?;
                expected.sort_unstable();
                if indexed != expected {
                    return invalid("工作空间成员索引与业务状态不一致");
                }
                sync_directory(&target)?;
            }
        }
        let manifest = Manifest {
            schema: 1,
            id: Id::new_v4(),
            source_storage_id: self.storage_id,
            created_at,
            files: describe_files(output)?,
        };
        validate_contents(output, &manifest)?;
        sync_directory(&output.join("workspaces"))?;
        write_json(&output.join("backup.json"), &manifest)?;
        sync_directory(output)?;
        sync_directory(output.parent().expect("备份已验证父目录"))?;
        Ok(manifest.id)
    }

    pub async fn restore(
        source: impl AsRef<Path>,
        database_url: &str,
        data_dir: impl AsRef<Path>,
    ) -> Result<RestoreResult> {
        let source = source.as_ref().to_owned();
        let prepared = tokio::task::spawn_blocking(move || prepare(&source))
            .await
            .map_err(|_| Error::Storage("备份预检任务失败".into()))??;
        let server = Self::connect_inner(database_url, data_dir.as_ref(), true).await?;
        let mut tx = server.pool.begin().await.map_err(database_error)?;
        restore_locks(&mut tx).await?;
        let old = query("SELECT fingerprint,completed FROM ff_restore WHERE singleton=1")
            .fetch_optional(&mut *tx)
            .await
            .map_err(database_error)?;
        if let Some(old) = &old {
            if old.get::<String, _>("fingerprint") != prepared.fingerprint {
                return invalid("目标已开始恢复另一份备份，不能混用");
            }
            if old.get::<bool, _>("completed") {
                return Ok(RestoreResult {
                    backup_id: prepared.manifest.id,
                    already_completed: true,
                });
            }
        }
        let occupied: bool = query_scalar("SELECT EXISTS(SELECT 1 FROM ff_accounts) OR EXISTS(SELECT 1 FROM ff_workspaces) OR EXISTS(SELECT 1 FROM ff_sessions) OR EXISTS(SELECT 1 FROM ff_account_events) OR EXISTS(SELECT 1 FROM ff_login_attempts) OR EXISTS(SELECT 1 FROM ff_workspace_operations) OR EXISTS(SELECT 1 FROM ff_workspace_members)")
            .fetch_one(&mut *tx).await.map_err(database_error)?;
        if occupied {
            return Err(Error::Conflict(
                "恢复只允许使用空数据库，不覆盖现有业务".into(),
            ));
        }
        if old.is_none() {
            if fs::read_dir(server.data_dir.join("workspaces"))
                .map_err(disk_error)?
                .next()
                .is_some()
            {
                return Err(Error::Conflict("恢复目标附件目录不是空目录".into()));
            }
            query("INSERT INTO ff_restore VALUES (1,$1,false)")
                .bind(&prepared.fingerprint)
                .execute(&mut *tx)
                .await
                .map_err(database_error)?;
        }
        // 独立提交恢复意图；中断后禁止初始化和正常启动，只允许同一备份继续。
        tx.commit().await.map_err(database_error)?;
        let mut tx = server.pool.begin().await.map_err(database_error)?;
        restore_locks(&mut tx).await?;
        let completed: bool = query_scalar("SELECT completed FROM ff_restore WHERE singleton=1")
            .fetch_one(&mut *tx)
            .await
            .map_err(database_error)?;
        if completed {
            return Ok(RestoreResult {
                backup_id: prepared.manifest.id,
                already_completed: true,
            });
        }
        install_files(&server, &prepared)?;
        for account in prepared.accounts.values() {
            query("INSERT INTO ff_accounts VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)")
                .bind(account.id)
                .bind(&account.account)
                .bind(&account.name)
                .bind(&account.department)
                .bind(&account.password_hash)
                .bind(account.active)
                .bind(account.is_admin)
                .bind(account.must_change_password)
                .bind(account.version)
                .execute(&mut *tx)
                .await
                .map_err(database_error)?;
        }
        for event in lines::<AccountEvent>(&prepared.root().join("account-events.jsonl"))? {
            let event = event?;
            query("INSERT INTO ff_account_events VALUES ($1,$2,$3,$4,$5,$6)")
                .bind(event.sequence)
                .bind(event.account_id)
                .bind(event.actor_id)
                .bind(event.action)
                .bind(event.at)
                .bind(event.data)
                .execute(&mut *tx)
                .await
                .map_err(database_error)?;
        }
        query("SELECT setval(pg_get_serial_sequence('ff_account_events','sequence'),COALESCE(MAX(sequence),1),MAX(sequence) IS NOT NULL) FROM ff_account_events")
            .execute(&mut *tx).await.map_err(database_error)?;
        for attempt in lines::<LoginAttempt>(&prepared.root().join("login-attempts.jsonl"))? {
            let attempt = attempt?;
            query("INSERT INTO ff_login_attempts VALUES ($1,$2,$3,$4)")
                .bind(attempt.account)
                .bind(attempt.failures)
                .bind(attempt.window_start)
                .bind(attempt.blocked_until)
                .execute(&mut *tx)
                .await
                .map_err(database_error)?;
        }
        for id in workspace_ids(&prepared.manifest)? {
            let root = prepared.root().join("workspaces").join(id.to_string());
            let data: Workspace =
                serde_json::from_slice(&read_file(&root.join("workspace.json"), STATE_LIMIT)?)
                    .map_err(codec)?;
            query("INSERT INTO ff_workspaces VALUES ($1,$2,$3,$4,$5,$6,$7)")
                .bind(data.id)
                .bind(&data.name)
                .bind(data.creator_id)
                .bind(data.revision)
                .bind(&data.genesis)
                .bind(&data.state)
                .bind(&data.checksum)
                .execute(&mut *tx)
                .await
                .map_err(database_error)?;
            for operation in lines::<Operation>(&root.join("operations.jsonl"))? {
                let op = operation?;
                query("INSERT INTO ff_workspace_operations VALUES ($1,$2,$3,$4,$5,$6)")
                    .bind(id)
                    .bind(op.id)
                    .bind(op.revision)
                    .bind(op.client)
                    .bind(op.request)
                    .bind(op.receipt)
                    .execute(&mut *tx)
                    .await
                    .map_err(database_error)?;
            }
            let state: State = decoded(&data.state)?;
            let mappings = if data.bindings.is_empty() {
                state.members.keys().map(|id| (*id, *id)).collect()
            } else {
                data.bindings
            };
            for (member, account) in mappings {
                query("INSERT INTO ff_workspace_identities VALUES ($1,$2,$3)")
                    .bind(id)
                    .bind(member)
                    .bind(account)
                    .execute(&mut *tx)
                    .await
                    .map_err(database_error)?;
            }
            if let Some(history) = &data.origin {
                super::migration::store_origin(&mut tx, history).await?;
            }
            workspace::sync_members(&mut tx, &state)
                .await
                .map_err(|error| error.0)?;
            super::transfer::restore(&mut tx, id, data.transfer.as_ref(), &data.transfer_events)
                .await?;
        }
        storage::verify(&server.data_dir, server.storage_id)?;
        query("UPDATE ff_restore SET completed=true WHERE singleton=1")
            .execute(&mut *tx)
            .await
            .map_err(database_error)?;
        tx.commit().await.map_err(database_error)?;
        Ok(RestoreResult {
            backup_id: prepared.manifest.id,
            already_completed: false,
        })
    }
}

async fn restore_locks(connection: &mut PgConnection) -> Result<()> {
    for lock in [SCHEMA_LOCK, ACCOUNT_LOCK] {
        query("SELECT pg_advisory_xact_lock($1)")
            .bind(lock)
            .execute(&mut *connection)
            .await
            .map_err(database_error)?;
    }
    Ok(())
}

async fn export_rows(
    connection: &mut PgConnection,
    sql: &str,
    initial: &str,
    path: &Path,
) -> Result<()> {
    let mut file = private_file(path)?;
    let mut cursor = initial.to_owned();
    loop {
        let rows = query(sql)
            .bind(&cursor)
            .fetch_all(&mut *connection)
            .await
            .map_err(database_error)?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            cursor = row.get("cursor");
            writeln!(file, "{}", row.get::<String, _>("data")).map_err(disk_error)?;
        }
    }
    file.sync_all().map_err(disk_error)
}

fn private_file(path: &Path) -> Result<File> {
    let mut options = File::options();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(disk_error)
}
struct JsonLines<T> {
    reader: BufReader<File>,
    ended: bool,
    kind: PhantomData<T>,
}
fn lines<T: DeserializeOwned>(path: &Path) -> Result<JsonLines<T>> {
    if !fs::symlink_metadata(path).map_err(disk_error)?.is_file() {
        return invalid("备份记录必须是普通文件");
    }
    Ok(JsonLines {
        reader: BufReader::new(File::open(path).map_err(disk_error)?),
        ended: false,
        kind: PhantomData,
    })
}
impl<T: DeserializeOwned> Iterator for JsonLines<T> {
    type Item = Result<T>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.ended {
            return None;
        }
        let mut bytes = Vec::new();
        let read = (&mut self.reader)
            .take(LINE_LIMIT + 1)
            .read_until(b'\n', &mut bytes);
        if matches!(read, Ok(0)) {
            self.ended = true;
            return None;
        }
        let result = read.map_err(disk_error).and_then(|_| {
            if bytes.len() as u64 > LINE_LIMIT || bytes.last() != Some(&b'\n') {
                return invalid("备份记录过大或未完整写入");
            }
            serde_json::from_slice(&bytes).map_err(codec)
        });
        if result.is_err() {
            self.ended = true;
        }
        Some(result)
    }
}

fn describe_files(root: &Path) -> Result<BTreeMap<String, FileInfo>> {
    let mut stack = vec![(root.to_owned(), 0)];
    let mut files = BTreeMap::new();
    let mut count = 0;
    while let Some((directory, depth)) = stack.pop() {
        regular_directory(&directory)?;
        if depth > 6 {
            return invalid("备份目录嵌套过深");
        }
        for entry in fs::read_dir(directory).map_err(disk_error)? {
            let entry = entry.map_err(disk_error)?;
            count += 1;
            if count > 200_000 {
                return invalid("备份文件数量超过限制");
            }
            let kind = entry.file_type().map_err(disk_error)?;
            if kind.is_dir() {
                stack.push((entry.path(), depth + 1));
            } else if kind.is_file() {
                let path = entry.path();
                let relative = path
                    .strip_prefix(root)
                    .expect("文件属于扫描目录")
                    .components()
                    .map(|c| {
                        c.as_os_str()
                            .to_str()
                            .ok_or_else(|| Error::Validation("备份路径编码无效".into()))
                    })
                    .collect::<Result<Vec<_>>>()?
                    .join("/");
                files.insert(
                    relative,
                    FileInfo {
                        size: entry.metadata().map_err(disk_error)?.len(),
                        sha256: hash_file(&path)?,
                    },
                );
            } else {
                return invalid("备份不能包含符号链接或特殊文件");
            }
        }
    }
    Ok(files)
}
fn safe_path(path: &str) -> bool {
    if [
        "accounts.jsonl",
        "account-events.jsonl",
        "login-attempts.jsonl",
    ]
    .contains(&path)
    {
        return true;
    }
    let parts: Vec<_> = path.split('/').collect();
    if parts.len() < 3
        || parts[0] != "workspaces"
        || Id::parse_str(parts[1])
            .ok()
            .is_none_or(|id| id.is_nil() || id.to_string() != parts[1])
    {
        return false;
    }
    (parts.len() == 3 && ["workspace.json", "operations.jsonl"].contains(&parts[2]))
        || (parts.len() == 5
            && parts[2] == "attachments"
            && valid_hash(parts[4])
            && parts[3] == &parts[4][..2])
}
fn workspace_ids(manifest: &Manifest) -> Result<BTreeSet<Id>> {
    manifest
        .files
        .keys()
        .filter(|p| p.starts_with("workspaces/"))
        .map(|p| {
            Id::parse_str(p.split('/').nth(1).unwrap_or(""))
                .map_err(|_| Error::Validation("备份工作空间路径无效".into()))
        })
        .collect()
}

fn validate_contents(root: &Path, manifest: &Manifest) -> Result<BTreeMap<Id, Account>> {
    if manifest.schema != 1
        || manifest.id.is_nil()
        || manifest.source_storage_id.is_nil()
        || manifest.created_at < 0
        || manifest.files.len() > 100_000
        || manifest.files.keys().any(|p| !safe_path(p))
    {
        return invalid("备份格式、路径或数量不受支持");
    }
    let mut accounts = BTreeMap::new();
    let mut names = BTreeSet::new();
    for account in lines::<Account>(&root.join("accounts.jsonl"))? {
        let account = account?;
        validate_account(&account)?;
        if !names.insert(account.account.clone()) || accounts.insert(account.id, account).is_some()
        {
            return invalid("备份账户标识或账号重复");
        }
    }
    if !accounts
        .values()
        .any(|a| a.active && a.is_admin && !a.must_change_password)
    {
        return invalid("备份没有可用的服务管理员");
    }
    let mut sequence = 0;
    for event in lines::<AccountEvent>(&root.join("account-events.jsonl"))? {
        let event = event?;
        if event.sequence <= sequence
            || event.at < 0
            || !accounts.contains_key(&event.account_id)
            || !accounts.contains_key(&event.actor_id)
            || ![
                "bootstrap",
                "create",
                "update",
                "change_password",
                "reset_password",
            ]
            .contains(&event.action.as_str())
        {
            return invalid("备份账户历史顺序或引用无效");
        }
        let data: AccountHistory = decoded(&event.data)?;
        validate_profile(&data.after.name, &data.after.department)?;
        if data.after.id != event.account_id
            || data.after.version < 1
            || normalize_account(&data.after.account)? != data.after.account
            || match data.before {
                Some(before) => {
                    before.version < 1
                        || before.id != event.account_id
                        || before.version.checked_add(1) != Some(data.after.version)
                }
                None => {
                    data.after.version != 1
                        || !["bootstrap", "create"].contains(&event.action.as_str())
                }
            }
        {
            return invalid("备份账户历史载荷无效");
        }
        sequence = event.sequence;
    }
    let mut attempts = BTreeSet::new();
    for attempt in lines::<LoginAttempt>(&root.join("login-attempts.jsonl"))? {
        let attempt = attempt?;
        if normalize_account(&attempt.account)? != attempt.account
            || attempt.failures < 0
            || attempt.window_start < 0
            || attempt.blocked_until < 0
            || !attempts.insert(attempt.account)
        {
            return invalid("备份登录限制记录无效");
        }
    }
    let mut required = BTreeSet::from([
        "accounts.jsonl".to_owned(),
        "account-events.jsonl".to_owned(),
        "login-attempts.jsonl".to_owned(),
    ]);
    for id in workspace_ids(manifest)? {
        let prefix = format!("workspaces/{id}");
        required.insert(format!("{prefix}/workspace.json"));
        required.insert(format!("{prefix}/operations.jsonl"));
        let attachments = verify_workspace(&root.join(&prefix), id, &accounts)?;
        for attachment in &attachments {
            read_attachment_bytes(&root.join(&prefix), attachment)?;
            required.insert(format!(
                "{prefix}/attachments/{}/{}",
                &attachment.sha256[..2],
                attachment.sha256
            ));
        }
    }
    if required != manifest.files.keys().cloned().collect() {
        return invalid("备份清单缺少所需文件或包含多余文件");
    }
    Ok(accounts)
}
fn validate_account(account: &Account) -> Result<()> {
    validate_profile(&account.name, &account.department)?;
    let hash = PasswordHash::new(&account.password_hash)
        .map_err(|_| Error::Validation("备份密码记录格式无效".into()))?;
    let params =
        Params::try_from(&hash).map_err(|_| Error::Validation("备份密码参数无效".into()))?;
    let default = Params::default();
    if account.id.is_nil()
        || account.version <= 0
        || normalize_account(&account.account)? != account.account
        || account.password_hash.len() > 512
        || hash.algorithm.as_str() != "argon2id"
        || hash.version != Some(19)
        || hash.salt.is_none()
        || hash.hash.as_ref().is_none_or(|h| h.len() != 32)
        || params.m_cost() != default.m_cost()
        || params.t_cost() != default.t_cost()
        || params.p_cost() != default.p_cost()
    {
        return invalid("备份账户或密码参数不受支持");
    }
    Ok(())
}
fn required_attachments(data: &Workspace, state: &State) -> Vec<Attachment> {
    let mut files: BTreeMap<_, _> = data
        .origin
        .as_ref()
        .map(|h| {
            h.attachments()
                .into_iter()
                .map(|a| ((a.sha256.clone(), a.size), a))
                .collect()
        })
        .unwrap_or_default();
    files.extend(
        state
            .attachments
            .values()
            .map(|a| ((a.sha256.clone(), a.size), a.clone())),
    );
    files.into_values().collect()
}
fn verify_workspace(
    root: &Path,
    id: Id,
    accounts: &BTreeMap<Id, Account>,
) -> Result<Vec<Attachment>> {
    let data: Workspace =
        serde_json::from_slice(&read_file(&root.join("workspace.json"), STATE_LIMIT)?)
            .map_err(codec)?;
    let mut state: State = decoded(&data.genesis)?;
    let expected = if let Some(history) = &data.origin {
        history.validate()?
    } else {
        let owner = state
            .members
            .values()
            .next()
            .cloned()
            .ok_or_else(|| Error::Validation("备份缺少初始所有者".into()))?;
        State::new(id, data.name.clone(), owner)?
    };
    if data.id != id
        || data.revision < 0
        || !accounts.contains_key(&data.creator_id)
        || (data.origin.is_none() && !state.members.contains_key(&data.creator_id))
        || encoded(&expected)? != encoded(&state)?
    {
        return invalid("备份初始工作空间无效");
    }
    let projected: State = decoded(&data.state)?;
    let bindings = if data.bindings.is_empty() && data.origin.is_none() {
        projected.members.keys().map(|id| (*id, *id)).collect()
    } else {
        data.bindings.clone()
    };
    let mut identities = data
        .origin
        .as_ref()
        .map(MigrationHistory::member_ids)
        .unwrap_or_else(|| state.members.keys().copied().collect());
    identities.extend(projected.members.keys());
    if bindings.keys().copied().collect::<BTreeSet<_>>() != identities
        || bindings.values().copied().collect::<BTreeSet<_>>().len() != bindings.len()
        || bindings.values().any(|id| !accounts.contains_key(id))
    {
        return invalid("备份身份映射不完整或重复");
    }
    let mut transfer_history = if data.transfer.is_some() || !data.transfer_events.is_empty() {
        Some(if let Some(history) = &data.origin {
            history.clone()
        } else {
            let owner = state
                .members
                .values()
                .next()
                .cloned()
                .ok_or_else(|| Error::Validation("迁移备份缺少初始所有者".into()))?;
            MigrationHistory {
                log: GitLog::new(id, data.name.clone(), owner)?,
                origins: vec![],
                clients: BTreeMap::new(),
            }
        })
    } else {
        None
    };
    for operation in lines::<Operation>(&root.join("operations.jsonl"))? {
        let op = operation?;
        let request: Request = decoded(&op.request)?;
        let receipt: Receipt = decoded(&op.receipt)?;
        let client: ClientRequest = decoded(&op.client)?;
        workspace::inspect(&client.command, true)?;
        client.verify_effective(&request)?;
        if op.id != request.id
            || !bindings.contains_key(&request.actor_id)
            || data
                .origin
                .as_ref()
                .is_some_and(|h| h.log.operations().contains_key(&request.id))
            || op.revision != i64::try_from(state.revision).unwrap_or(-1) + 1
            || client
                .expected_revision
                .is_some_and(|r| r != state.revision)
            || state.execute(request.clone())? != receipt
            || receipt.revision != op.revision as u64
        {
            return invalid("备份源操作、版本与回执不一致");
        }
        if let Some(history) = &mut transfer_history {
            if history.log.execute(id, request.clone())? != receipt {
                return invalid("迁移备份因果历史回执不符");
            }
            history.clients.insert(request.id, op.client);
        }
    }
    if let Some(history) = &transfer_history {
        if encoded(&history.validate()?)? != encoded(&state)? {
            return invalid("迁移备份完整历史与投影不符");
        }
        super::transfer::verify_events(
            data.transfer.as_ref(),
            &data.transfer_events,
            history,
            &bindings,
        )?;
    }
    let retired = data.transfer.as_ref().is_some_and(|control| {
        control.status.role == farflow_core::TransferRole::Source
            && control.status.phase == farflow_core::TransferPhase::Completed
    });
    if checksum(&state)? != data.checksum
        || state.revision != data.revision as u64
        || encoded(&state)? != encoded(&projected)?
        || (!retired
            && !state.members.values().any(|m| {
                m.active
                    && m.roles.contains(&Role::Owner)
                    && bindings
                        .get(&m.id)
                        .and_then(|a| accounts.get(a))
                        .is_some_and(|a| a.active)
            }))
    {
        return invalid("备份投影、日志或有效所有者不一致");
    }
    Ok(required_attachments(&data, &state))
}

struct Prepared {
    directory: tempfile::TempDir,
    manifest: Manifest,
    fingerprint: String,
    accounts: BTreeMap<Id, Account>,
}
impl Prepared {
    fn root(&self) -> &Path {
        self.directory.path()
    }
}
fn prepare(source: &Path) -> Result<Prepared> {
    regular_directory(source)?;
    let bytes = read_file(&source.join("backup.json"), 16 * 1024 * 1024)?;
    let manifest: Manifest = serde_json::from_slice(&bytes).map_err(codec)?;
    if manifest.schema != 1
        || manifest.files.len() > 100_000
        || manifest
            .files
            .iter()
            .any(|(path, info)| !safe_path(path) || !valid_hash(&info.sha256))
    {
        return invalid("备份清单格式无效");
    }
    let actual = describe_files(source)?;
    if actual
        .keys()
        .filter(|p| p.as_str() != "backup.json")
        .cloned()
        .collect::<BTreeSet<_>>()
        != manifest.files.keys().cloned().collect()
    {
        return invalid("备份实际文件与清单不一致");
    }
    let mut builder = tempfile::Builder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(fs::Permissions::from_mode(0o700));
    }
    let directory = builder.tempdir().map_err(disk_error)?;
    for (relative, info) in &manifest.files {
        let input = source.join(relative);
        if fs::metadata(&input).map_err(disk_error)?.len() != info.size {
            return invalid("备份文件大小不一致");
        }
        let output = directory.path().join(relative);
        let mut parents: Vec<_> = output
            .parent()
            .expect("备份文件有父目录")
            .ancestors()
            .take_while(|p| *p != directory.path())
            .collect();
        parents.reverse();
        for parent in parents {
            create_directory(parent)?;
        }
        let mut target = private_file(&output)?;
        let copied = std::io::copy(
            &mut File::open(input)
                .map_err(disk_error)?
                .take(info.size.saturating_add(1)),
            &mut target,
        )
        .map_err(disk_error)?;
        target.sync_all().map_err(disk_error)?;
        if copied != info.size || hash_file(&output)? != info.sha256 {
            return invalid("备份文件校验失败或在预检期间发生变化");
        }
    }
    // 零空间备份及没有附件的空间也保留有效目录。
    create_directory(&directory.path().join("workspaces"))?;
    for id in workspace_ids(&manifest)? {
        create_directory(
            &directory
                .path()
                .join("workspaces")
                .join(id.to_string())
                .join("attachments"),
        )?;
    }
    let accounts = validate_contents(directory.path(), &manifest)?;
    Ok(Prepared {
        directory,
        manifest,
        fingerprint: format!("{:x}", Sha256::digest(bytes)),
        accounts,
    })
}
fn install_files(server: &Server, prepared: &Prepared) -> Result<()> {
    for id in workspace_ids(&prepared.manifest)? {
        let source = prepared.root().join("workspaces").join(id.to_string());
        let data: Workspace =
            serde_json::from_slice(&read_file(&source.join("workspace.json"), STATE_LIMIT)?)
                .map_err(codec)?;
        let state: State = decoded(&data.state)?;
        let target = workspace::files(server, id, true)?;
        for attachment in &required_attachments(&data, &state) {
            write_attachment_bytes(
                &target,
                &attachment.sha256,
                &read_attachment_bytes(&source, attachment)?,
            )?;
        }
    }
    sync_directory(&server.data_dir.join("workspaces"))
}
fn codec(_: serde_json::Error) -> Error {
    Error::Validation("备份数据格式无效".into())
}
fn invalid<T>(message: &str) -> Result<T> {
    Err(Error::Validation(message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use farflow_core::Command;
    use serde_json::json;

    #[test]
    fn json_line_limits_require_complete_records_and_stop_after_the_first_error() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("records.jsonl");
        let value = "x".repeat(LINE_LIMIT as usize - 3);
        fs::write(
            &path,
            format!("{}\n", serde_json::to_string(&value).unwrap()),
        )
        .unwrap();
        assert_eq!(
            lines::<String>(&path)
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .len(),
            value.len()
        );
        fs::write(&path, vec![b'x'; LINE_LIMIT as usize + 1]).unwrap();
        let mut oversized = lines::<String>(&path).unwrap();
        assert!(oversized.next().unwrap().is_err());
        assert!(oversized.next().is_none());
        for invalid in ["{}", "\n", "invalid\n"] {
            fs::write(&path, invalid).unwrap();
            assert!(lines::<serde_json::Value>(&path)
                .unwrap()
                .next()
                .unwrap()
                .is_err());
        }
        fs::write(&path, b"{}\n[]\n").unwrap();
        assert_eq!(
            lines::<serde_json::Value>(&path)
                .unwrap()
                .collect::<Result<Vec<_>>>()
                .unwrap(),
            vec![json!({}), json!([])]
        );
        assert!(lines::<String>(directory.path()).is_err());
    }

    #[test]
    fn backup_paths_and_tree_entries_cannot_escape_through_components_or_links() {
        let id = Id::new_v4();
        let hash = "a".repeat(64);
        assert!(safe_path("accounts.jsonl"));
        assert!(safe_path(&format!("workspaces/{id}/workspace.json")));
        assert!(safe_path(&format!("workspaces/{id}/attachments/aa/{hash}")));
        for path in [
            "../escape".into(),
            "/accounts.jsonl".into(),
            "workspaces/x/workspace.json".into(),
            format!("workspaces/{}/workspace.json", Id::nil()),
            format!("workspaces/{id}/attachments/ab/{hash}"),
            format!("workspaces/{id}/../workspace.json"),
            format!("workspaces/{id}/unknown"),
        ] {
            assert!(!safe_path(&path));
        }
        let directory = tempfile::tempdir().unwrap();
        let mut deep = directory.path().to_owned();
        for _ in 0..7 {
            deep = deep.join("deep");
            fs::create_dir(&deep).unwrap();
        }
        assert!(describe_files(directory.path()).is_err());
        #[cfg(unix)]
        {
            let dir = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink("/missing", dir.path().join("linked")).unwrap();
            assert!(describe_files(dir.path()).is_err());
            assert!(prepare(dir.path()).is_err());
        }
    }

    #[test]
    fn membership_normalization_changes_only_historical_profile_fields() {
        let id = Id::new_v4();
        let base = farflow_core::Member {
            id,
            account: "client".into(),
            name: "客户端".into(),
            department: "客户端部门".into(),
            roles: BTreeSet::from([Role::Developer]),
            active: false,
        };
        let mut canonical = base.clone();
        canonical.account = "real".into();
        canonical.name = "服务记录".into();
        canonical.department = "服务部门".into();
        let client = Command::Batch {
            commands: vec![
                Command::AddMember {
                    member: base.clone(),
                },
                Command::UpdateMember { member: base },
            ],
        };
        let effective = Command::Batch {
            commands: vec![
                Command::AddMember {
                    member: canonical.clone(),
                },
                Command::UpdateMember { member: canonical },
            ],
        };
        let request = Request {
            id,
            actor_id: Id::new_v4(),
            at: 1,
            command: effective,
        };
        let mut operation = ClientRequest {
            id,
            expected_revision: Some(0),
            command: client,
        };
        operation.verify_effective(&request).unwrap();
        if let Command::Batch { commands } = &mut operation.command {
            if let Command::AddMember { member } = &mut commands[0] {
                member.active = true;
                member.roles.insert(Role::Owner);
            }
        }
        assert!(operation.verify_effective(&request).is_err());
    }
}
