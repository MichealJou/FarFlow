use crate::{
    sync_directory, Error, MigrationArchive, Result, TransferActivation, TransferReceipt,
    TransferStatus,
};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

fn invalid(message: &str) -> Error {
    Error::Validation(message.into())
}

pub fn read_transfer_document<T: DeserializeOwned>(path: &Path, limit: u64) -> Result<T> {
    serde_json::from_slice(&crate::read_file(path, limit)?)
        .map_err(|_| invalid("JSON文件格式无效，请核对字段和文件类型"))
}

fn prepare_transfer_output(
    path: &Path,
    protected: &Path,
) -> Result<(PathBuf, tempfile::NamedTempFile)> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = fs::canonicalize(parent).map_err(|_| invalid("输出父目录不存在或不可访问"))?;
    let name = path.file_name().ok_or_else(|| invalid("输出文件名无效"))?;
    let output = parent.join(name);
    if output.to_str().is_none() {
        return Err(invalid("输出路径必须是有效UTF-8文本"));
    }
    let protected = if protected.exists() {
        fs::canonicalize(protected).map_err(|_| invalid("业务目录不可访问"))?
    } else {
        let parent = protected
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::canonicalize(parent)
            .map_err(|_| invalid("业务父目录不可访问"))?
            .join(
                protected
                    .file_name()
                    .ok_or_else(|| invalid("业务目录名无效"))?,
            )
    };
    if output.starts_with(protected) {
        return Err(invalid("回执和完成凭据必须保存在业务目录之外"));
    }
    crate::regular_directory(&parent)?;
    if fs::symlink_metadata(&output).is_ok() {
        let _: Value = read_transfer_document(&output, 1024 * 1024)?;
    }
    let file = tempfile::NamedTempFile::new_in(&parent).map_err(|_| invalid("输出目录不可写"))?;
    Ok((output, file))
}

pub fn save_transfer_document(
    (path, mut temporary): (PathBuf, tempfile::NamedTempFile),
    value: &impl Serialize,
) -> Result<PathBuf> {
    let expected = serde_json::to_value(value).map_err(|_| invalid("迁移结果不能序列化"))?;
    let bytes = serde_json::to_vec_pretty(&expected).expect("JSON值可以编码为JSON字节");
    let parent = path.parent().expect("输出父目录已核对");
    crate::regular_directory(parent)?;
    if fs::canonicalize(parent).map_err(|_| invalid("输出父目录已变化"))? != parent {
        return Err(invalid("输出父目录已变化"));
    }
    if fs::symlink_metadata(&path).is_ok() {
        let existing: Value = read_transfer_document(&path, 1024 * 1024)?;
        if existing != expected {
            return Err(Error::Conflict(
                "输出文件已属于不同内容，请选新文件；已完成的迁移请用同一回执重取凭据".into(),
            ));
        }
        return Ok(path);
    }
    temporary
        .write_all(&bytes)
        .map_err(|_| invalid("迁移结果文件写入失败"))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|_| invalid("迁移结果文件持久化失败"))?;
    temporary
        .persist_noclobber(&path)
        .map_err(|_| invalid("迁移结果落位失败，未覆盖已有文件，请查询阶段并重试"))?;
    sync_directory(parent)?;
    Ok(path)
}

pub fn prepare_transfer_activation_output(
    path: &Path,
    protected: &Path,
    status: Option<TransferStatus>,
    receipt: &TransferReceipt,
) -> Result<(PathBuf, tempfile::NamedTempFile)> {
    let output = prepare_transfer_output(path, protected)?;
    if output.0.exists() {
        let previous: TransferActivation = read_transfer_document(&output.0, 1024 * 1024)?;
        let status = status.ok_or_else(|| invalid("现有凭据没有对应的源迁移计划"))?;
        // 已知不相符的输出文件必须在源永久只读之前拒绝。
        if status.plan.receipt()? != *receipt
            || previous.transfer_id != status.plan.id
            || previous.plan_checksum != receipt.plan_checksum
            || format!("{:x}", Sha256::digest(previous.token.as_bytes()))
                != status.plan.activation_hash
        {
            return Err(Error::Conflict(
                "现有凭据不属于当前计划，请选择新文件".into(),
            ));
        }
    }
    Ok(output)
}

pub fn prepare_transfer_receipt_output(
    path: &Path,
    protected: &Path,
    source: &Path,
) -> Result<(PathBuf, tempfile::NamedTempFile)> {
    let output = prepare_transfer_output(path, protected)?;
    if output.0.exists() {
        let previous: TransferReceipt = read_transfer_document(&output.0, 1024 * 1024)?;
        let archive = MigrationArchive::open(source)?;
        let plan = archive
            .transfer_plan()
            .ok_or_else(|| invalid("普通复制包不能用于正式迁移"))?;
        if previous != plan.receipt()? {
            return Err(Error::Conflict(
                "现有回执不属于当前计划，请选择新文件".into(),
            ));
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Id, Mode};
    use serde_json::json;
    use std::ffi::OsString;
    #[test]
    fn output_is_atomic_private_idempotent_and_never_replaces_another_document() {
        let temp = tempfile::tempdir().unwrap();
        let protected = temp.path().join("业务");
        fs::create_dir(&protected).unwrap();
        let output = temp.path().join("凭据.json");
        let value = json!({"transfer_id":Id::new_v4(),"token":"仅写文件的凭据"});
        let prepared = prepare_transfer_output(&output, &protected).unwrap();
        assert!(!output.exists());
        assert_eq!(
            save_transfer_document(prepared, &value).unwrap(),
            fs::canonicalize(&output).unwrap()
        );
        let original = fs::read(&output).unwrap();
        save_transfer_document(
            prepare_transfer_output(&output, &protected).unwrap(),
            &value,
        )
        .unwrap();
        assert!(save_transfer_document(
            prepare_transfer_output(&output, &protected).unwrap(),
            &json!({"other":true})
        )
        .is_err());
        assert_eq!(fs::read(&output).unwrap(), original);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&output).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let future = temp.path().join("尚未暂存的空间");
        let receipt = temp.path().join("回执.json");
        save_transfer_document(
            prepare_transfer_output(&receipt, &future).unwrap(),
            &json!({"plan_checksum":"公开校验值"}),
        )
        .unwrap();
        assert!(!future.exists());
        assert!(prepare_transfer_output(&protected.join("不应进入业务.json"), &protected).is_err());
        assert!(
            prepare_transfer_output(&temp.path().join("missing/output.json"), &protected).is_err()
        );
        assert!(prepare_transfer_output(Path::new("/"), &protected).is_err());
        let invalid = temp.path().join("坏文件");
        fs::write(&invalid, b"invalid").unwrap();
        assert!(prepare_transfer_output(&invalid, &protected).is_err());
        assert!(read_transfer_document::<TransferReceipt>(&output, 1024 * 1024).is_err());
        assert!(read_transfer_document::<Value>(&output, 1).is_err());
        let bad_parent = temp.path().join("缺失父级/业务");
        assert!(prepare_transfer_output(&receipt, &bad_parent).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            assert!(prepare_transfer_output(
                &temp.path().join(OsString::from_vec(vec![255])),
                &protected
            )
            .is_err());
            let alias = temp.path().join("别名");
            std::os::unix::fs::symlink(&protected, &alias).unwrap();
            assert!(prepare_transfer_output(&alias.join("凭据.json"), &protected).is_err());
            let file_alias = temp.path().join("文件链接");
            std::os::unix::fs::symlink(&output, &file_alias).unwrap();
            assert!(prepare_transfer_output(&file_alias, &protected).is_err());
        }
    }

    #[test]
    fn output_rechecks_replaced_parents_and_reports_a_file_that_appeared_late() {
        let temp = tempfile::tempdir().unwrap();
        let protected = temp.path().join("业务");
        fs::create_dir(&protected).unwrap();
        let parent = temp.path().join("输出");
        fs::create_dir(&parent).unwrap();
        let file = parent.join("结果.json");
        let prepared = prepare_transfer_output(&file, &protected).unwrap();
        fs::write(&file, b"{\"other\":true}").unwrap();
        assert!(save_transfer_document(prepared, &json!({"expected":true})).is_err());
        assert_eq!(fs::read(&file).unwrap(), b"{\"other\":true}");
        let prepared = prepare_transfer_output(&parent.join("新文件.json"), &protected).unwrap();
        fs::rename(&parent, temp.path().join("原输出")).unwrap();
        assert!(save_transfer_document(prepared, &json!({"expected":true})).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(temp.path().join("原输出"), &parent).unwrap();
            let prepared =
                prepare_transfer_output(&temp.path().join("其他文件.json"), &protected).unwrap();
            let alias = temp.path().join("输出链接");
            std::os::unix::fs::symlink(temp.path(), &alias).unwrap();
            let (path, temporary) = prepared;
            assert!(save_transfer_document(
                (alias.join(path.file_name().unwrap()), temporary),
                &json!({"expected":true})
            )
            .is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn real_file_failures_never_publish_a_partial_document_or_overwrite_the_destination() {
        use std::os::unix::{ffi::OsStringExt, fs::PermissionsExt};
        let temp = tempfile::tempdir().unwrap();
        let protected = temp.path().join("业务");
        fs::create_dir(&protected).unwrap();
        let denied = temp.path().join("只读输出");
        fs::create_dir(&denied).unwrap();
        fs::set_permissions(&denied, fs::Permissions::from_mode(0o500)).unwrap();
        let permission = prepare_transfer_output(&denied.join("凭据.json"), &protected);
        fs::set_permissions(&denied, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(permission.is_err());
        let file = temp.path().join("编码失败.json");
        let prepared = prepare_transfer_output(&file, &protected).unwrap();
        let invalid = PathBuf::from(OsString::from_vec(vec![255]));
        assert!(save_transfer_document(prepared, &invalid).is_err());
        assert!(!file.exists());
        let file = temp.path().join("写入失败.json");
        let (path, temporary) = prepare_transfer_output(&file, &protected).unwrap();
        let (writer, temporary_path) = temporary.into_parts();
        drop(writer);
        let read = fs::File::open(&temporary_path).unwrap();
        let readonly = tempfile::NamedTempFile::from_parts(read, temporary_path);
        assert!(save_transfer_document((path, readonly), &json!({"expected":true})).is_err());
        assert!(!file.exists());
        let file = temp.path().join("落位失败.json");
        let prepared = prepare_transfer_output(&file, &protected).unwrap();
        fs::remove_file(prepared.1.path()).unwrap();
        assert!(save_transfer_document(prepared, &json!({"expected":true})).is_err());
        assert!(!file.exists());
    }

    #[test]
    fn existing_receipts_and_proofs_are_checked_against_the_current_plan_before_state_changes() {
        use crate::{GitLog, Member, MigrationHistory, Role, TransferState};
        use std::collections::{BTreeMap, BTreeSet};
        let temp = tempfile::tempdir().unwrap();
        let protected = temp.path().join("业务");
        fs::create_dir(&protected).unwrap();
        let actor = Id::new_v4();
        let member = Member {
            id: actor,
            account: "owner".into(),
            name: "所有者".into(),
            department: "研发".into(),
            roles: BTreeSet::from([Role::Owner]),
            active: true,
        };
        let history = MigrationHistory {
            log: GitLog::new(Id::new_v4(), "计划核对".into(), member).unwrap(),
            origins: vec![],
            clients: BTreeMap::new(),
        };
        let mut control =
            TransferState::prepare(&history, Mode::Local, Mode::Git, Id::new_v4(), actor, 100)
                .unwrap();
        let plan = control.status.plan.clone();
        let status = control.status.clone();
        let receipt = plan.receipt().unwrap();
        let proof = control.complete(&receipt, actor, 100).unwrap();
        let file = temp.path().join("完成凭据.json");
        save_transfer_document(prepare_transfer_output(&file, &protected).unwrap(), &proof)
            .unwrap();
        assert!(prepare_transfer_activation_output(
            &file,
            &protected,
            Some(status.clone()),
            &receipt
        )
        .is_ok());
        assert!(prepare_transfer_activation_output(&file, &protected, None, &receipt).is_err());
        assert!(prepare_transfer_activation_output(
            &file,
            &protected,
            Some(status.clone()),
            &TransferReceipt {
                plan_checksum: "0".repeat(64)
            }
        )
        .is_err());
        for field in ["transfer_id", "plan_checksum", "token"] {
            let mut changed = serde_json::to_value(&proof).unwrap();
            changed[field] = json!(if field == "transfer_id" {
                Id::new_v4().to_string()
            } else {
                "0".repeat(64)
            });
            let bad = temp.path().join(format!("错误{field}.json"));
            fs::write(&bad, serde_json::to_vec(&changed).unwrap()).unwrap();
            assert!(prepare_transfer_activation_output(
                &bad,
                &protected,
                Some(status.clone()),
                &receipt
            )
            .is_err());
        }
        let archive = temp.path().join("正式包");
        history
            .export_transfer(&plan, actor, &protected, &archive)
            .unwrap();
        let file = temp.path().join("回执.json");
        save_transfer_document(
            prepare_transfer_output(&file, &protected).unwrap(),
            &receipt,
        )
        .unwrap();
        assert!(prepare_transfer_receipt_output(&file, &protected, &archive).is_ok());
        let wrong = temp.path().join("别的回执.json");
        save_transfer_document(
            prepare_transfer_output(&wrong, &protected).unwrap(),
            &TransferReceipt {
                plan_checksum: "0".repeat(64),
            },
        )
        .unwrap();
        assert!(prepare_transfer_receipt_output(&wrong, &protected, &archive).is_err());
        let ordinary = temp.path().join("普通包");
        history
            .export_copy(Mode::Local, actor, &protected, &ordinary)
            .unwrap();
        assert!(prepare_transfer_receipt_output(&file, &protected, &ordinary).is_err());
    }
}
