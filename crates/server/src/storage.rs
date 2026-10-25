use super::{database_error, Error, Id, SCHEMA_LOCK};
use farflow_core::sync_directory;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx_core::{query::query, query_scalar::query_scalar, row::Row};
use sqlx_postgres::PgPool;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Marker {
    schema: u32,
    id: Id,
    path_hash: String,
}

fn path_hash(root: &Path) -> String {
    format!("{:x}", Sha256::digest(root.as_os_str().as_encoded_bytes()))
}

pub(super) fn verify(root: &Path, id: Id) -> farflow_core::Result<()> {
    if !fs::symlink_metadata(root).map_err(disk)?.is_dir() {
        return Err(Error::Storage("服务数据目录已失效".into()));
    }
    let marker = read(root)?;
    if marker.schema != 1 || marker.id != id || marker.path_hash != path_hash(root) {
        return Err(Error::Validation("数据库与附件目录的存储身份不匹配".into()));
    }
    Ok(())
}
fn read(root: &Path) -> farflow_core::Result<Marker> {
    let file = root.join("storage.json");
    let meta = fs::symlink_metadata(&file).map_err(disk)?;
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > 4096 {
        return Err(Error::Validation("服务存储标记格式无效".into()));
    }
    serde_json::from_slice(&fs::read(file).map_err(disk)?)
        .map_err(|_| Error::Validation("服务存储标记损坏".into()))
}
fn marker(root: &Path, id: Id, may_create: bool) -> farflow_core::Result<()> {
    if fs::symlink_metadata(root.join("storage.json")).is_err() {
        if !may_create {
            return Err(Error::Validation(
                "服务存储标记缺失，需要从有效备份恢复".into(),
            ));
        }
        let data = serde_json::to_vec(&Marker {
            schema: 1,
            id,
            path_hash: path_hash(root),
        })
        .map_err(|_| Error::Storage("存储标记编码失败".into()))?;
        let mut temporary = tempfile::NamedTempFile::new_in(root).map_err(disk)?;
        temporary.write_all(&data).map_err(disk)?;
        temporary.as_file().sync_all().map_err(disk)?;
        match temporary.persist_noclobber(root.join("storage.json")) {
            Ok(_) => {}
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(disk(error.error)),
        }
        sync_directory(root)?;
    }
    verify(root, id)
}
fn disk(_: std::io::Error) -> Error {
    Error::Storage("服务存储标记读取或持久化失败".into())
}

pub(super) async fn bind(pool: &PgPool, root: PathBuf) -> farflow_core::Result<Id> {
    let digest = path_hash(&root);
    let mut tx = pool.begin().await.map_err(database_error)?;
    query("SELECT pg_advisory_xact_lock($1)")
        .bind(SCHEMA_LOCK)
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
    let old = query("SELECT id,path_hash,ready FROM ff_storage WHERE singleton=1")
        .fetch_optional(&mut *tx)
        .await
        .map_err(database_error)?;
    let entities: i64 = query_scalar(
        "SELECT (SELECT COUNT(*) FROM ff_accounts)+(SELECT COUNT(*) FROM ff_workspaces)",
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(database_error)?;
    let (id, ready) = if let Some(row) = old {
        let id: Id = row.get("id");
        let ready: bool = row.get("ready");
        if row.get::<String, _>("path_hash") != digest {
            if ready || entities > 0 {
                return Err(Error::Validation(
                    "附件目录与数据库绑定的路径不同，请使用原目录或执行备份恢复".into(),
                ));
            }
            let id = Id::new_v4();
            query("UPDATE ff_storage SET id=$1,path_hash=$2 WHERE singleton=1")
                .bind(id)
                .bind(&digest)
                .execute(&mut *tx)
                .await
                .map_err(database_error)?;
            (id, false)
        } else {
            (id, ready)
        }
    } else {
        if entities > 0 {
            return Err(Error::Validation(
                "旧服务尚未绑定存储目录，请先完成备份迁移".into(),
            ));
        }
        if fs::symlink_metadata(root.join("storage.json")).is_ok() {
            return Err(Error::Validation("此附件目录已经属于另一服务数据库".into()));
        }
        let id = Id::new_v4();
        query("INSERT INTO ff_storage VALUES (1,$1,$2,false)")
            .bind(id)
            .bind(&digest)
            .execute(&mut *tx)
            .await
            .map_err(database_error)?;
        (id, false)
    };
    // 先保存待初始化身份，文件落位后再标记就绪；中断重试仍使用同一身份。
    tx.commit().await.map_err(database_error)?;
    let copy = root.clone();
    tokio::task::spawn_blocking(move || marker(&copy, id, !ready))
        .await
        .map_err(|_| Error::Storage("存储标记处理任务失败".into()))??;
    let mut tx = pool.begin().await.map_err(database_error)?;
    query("SELECT pg_advisory_xact_lock($1)")
        .bind(SCHEMA_LOCK)
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
    let matches: bool = query_scalar(
        "SELECT EXISTS(SELECT 1 FROM ff_storage WHERE singleton=1 AND id=$1 AND path_hash=$2)",
    )
    .bind(id)
    .bind(digest)
    .fetch_one(&mut *tx)
    .await
    .map_err(database_error)?;
    if !matches {
        return Err(Error::Conflict("服务存储初始化已被另一个目录接管".into()));
    }
    query("UPDATE ff_storage SET ready=true WHERE singleton=1")
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
    tx.commit().await.map_err(database_error)?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn marker_is_atomic_private_and_rejects_wrong_identity_or_tampering() {
        let dir = tempfile::tempdir().unwrap();
        let id = Id::new_v4();
        marker(dir.path(), id, true).unwrap();
        verify(dir.path(), id).unwrap();
        marker(dir.path(), id, false).unwrap();
        assert!(verify(dir.path(), Id::new_v4()).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(dir.path().join("storage.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        fs::write(dir.path().join("storage.json"), b"broken").unwrap();
        assert!(verify(dir.path(), id).is_err());
        fs::remove_file(dir.path().join("storage.json")).unwrap();
        assert!(marker(dir.path(), id, false).is_err());
        fs::write(dir.path().join("storage.json"), vec![b'x'; 4097]).unwrap();
        assert!(verify(dir.path(), id).is_err());
    }
}
