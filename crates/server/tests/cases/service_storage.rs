use super::{F, NEXT_PASSWORD, PASSWORD};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use farflow_core::{hash_file, Id};
use farflow_server::Server;
use serde_json::{json, Value};
use sqlx_core::{query::query, query_scalar::query_scalar};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

async fn source() -> (F, String, Id, Id, Value) {
    let f = F::new().await;
    let token = f.admin().await;
    let space = Id::new_v4();
    assert_eq!(
        f.api(
            "POST",
            "/api/workspaces",
            Some(&token),
            json!({"id":space,"name":"恢复演练"})
        )
        .await
        .0,
        StatusCode::CREATED
    );
    let document = Id::new_v4();
    let operation = json!({"id":Id::new_v4(),"command":{"type":"create","item":{"id":document,"kind":"document","title":"恢复后核对","description":"正文\n保留制表\t与负零","attributes":{"score":-0.0}}}});
    assert_eq!(
        f.api(
            "POST",
            &format!("/api/workspaces/{space}/commands"),
            Some(&token),
            operation.clone()
        )
        .await
        .0,
        StatusCode::OK
    );
    let attachment = Id::new_v4();
    let response=f.send(Request::builder().method("POST")
        .uri(format!("/api/workspaces/{space}/attachments?operation_id={}&id={attachment}&item_id={document}&name=evidence.txt&mime=text%2Fplain",Id::new_v4()))
        .header("Authorization",format!("Bearer {token}"))
        .body(Body::from("real backup evidence")).unwrap()).await;
    assert_eq!(response.status(), StatusCode::OK);
    (f, token, space, attachment, operation)
}
fn rewrite(backup: &Path, relative: &str, bytes: &[u8]) {
    fs::write(backup.join(relative), bytes).unwrap();
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(backup.join("backup.json")).unwrap()).unwrap();
    manifest["files"][relative]["sha256"] = json!(hash_file(&backup.join(relative)).unwrap());
    manifest["files"][relative]["size"] = json!(bytes.len());
    fs::write(
        backup.join("backup.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
}
async fn backup(f: &F, token: &str, label: &str) -> PathBuf {
    let path = f.files.with_extension(label);
    f.server.backup(token, &path).await.unwrap();
    path
}

#[tokio::test]
async fn service_backup_restores_accounts_all_spaces_raw_payloads_and_retry_receipts() {
    let (f, token, space, attachment, operation) = source().await;
    let user = f.create(&token, "temporary", false).await;
    let temporary = f.login("temporary", PASSWORD).await;
    let ready = f.create(&token, "disabled", false).await;
    let first = f.login("disabled", PASSWORD).await;
    f.password(&first).await;
    assert_eq!(f.api("PATCH",&format!("/api/users/{}",ready["id"].as_str().unwrap()),Some(&token),json!({"version":2,"name":"停用成员","department":"质量部","active":false,"is_admin":false})).await.0,StatusCode::OK);
    let second = Id::new_v4();
    f.api(
        "POST",
        "/api/workspaces",
        Some(&token),
        json!({"id":second,"name":"另一空间"}),
    )
    .await;
    let before = f
        .api(
            "GET",
            &format!("/api/workspaces/{space}"),
            Some(&token),
            Value::Null,
        )
        .await
        .1;
    let path = backup(&f, &token, "backup").await;
    assert!(!fs::read_to_string(path.join("accounts.jsonl"))
        .unwrap()
        .contains(&token));
    assert!(!path.join("sessions.jsonl").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for file in [
            path.join("accounts.jsonl"),
            path.join("account-events.jsonl"),
            path.join("backup.json"),
        ] {
            assert_eq!(
                fs::metadata(file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
    let target = F::new().await;
    let result = Server::restore(&path, &target.url, &target.files)
        .await
        .unwrap();
    assert!(!result.already_completed);
    for old in [&token, &temporary] {
        assert_eq!(
            target
                .api("GET", "/api/auth/me", Some(old), Value::Null)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
    }
    let restored = target.login("owner", PASSWORD).await;
    assert_eq!(
        target
            .api(
                "GET",
                &format!("/api/workspaces/{space}"),
                Some(&restored),
                Value::Null
            )
            .await
            .1,
        before
    );
    assert_eq!(
        target
            .api("GET", "/api/workspaces", Some(&restored), Value::Null)
            .await
            .1
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let receipt = target
        .api(
            "POST",
            &format!("/api/workspaces/{space}/commands"),
            Some(&restored),
            operation.clone(),
        )
        .await;
    assert_eq!(receipt.0, StatusCode::OK);
    assert_eq!(receipt.1["revision"], 1);
    let response = target
        .send(
            Request::builder()
                .uri(format!("/api/workspaces/{space}/attachments/{attachment}"))
                .header("Authorization", format!("Bearer {restored}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(
        axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap(),
        "real backup evidence"
    );
    let forced = target.login("temporary", PASSWORD).await;
    assert_eq!(
        target
            .api("GET", "/api/auth/me", Some(&forced), Value::Null)
            .await
            .1["must_change_password"],
        true
    );
    assert_eq!(
        target
            .api(
                "POST",
                "/api/auth/login",
                None,
                json!({"account":"disabled","password":NEXT_PASSWORD})
            )
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        target
            .api(
                "GET",
                &format!("/api/users/{}/history", user["id"].as_str().unwrap()),
                Some(&restored),
                Value::Null
            )
            .await
            .1["events"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    target.create(&restored, "after_restore", false).await;
    assert!(
        Server::restore(&path, &target.url, &target.files)
            .await
            .unwrap()
            .already_completed
    );
    assert!(Server::restore(&path, &f.url, &f.files).await.is_err());
    assert_ne!(
        fs::read(f.files.join("storage.json")).unwrap(),
        fs::read(target.files.join("storage.json")).unwrap()
    );
}

#[tokio::test]
async fn interrupted_restore_rolls_back_database_blocks_bootstrap_and_resumes_same_backup() {
    let (f, token, _, _, _) = source().await;
    let path = backup(&f, &token, "resume-backup").await;
    let other = backup(&f, &token, "other-backup").await;
    let target = F::new().await;
    query("ALTER TABLE ff_workspaces ADD CONSTRAINT injected_restore_failure CHECK (name<>'恢复演练')").execute(&target.pool).await.unwrap();
    assert!(Server::restore(&path, &target.url, &target.files)
        .await
        .is_err());
    assert_eq!(
        query_scalar::<_, i64>("SELECT COUNT(*) FROM ff_accounts")
            .fetch_one(&target.pool)
            .await
            .unwrap(),
        0
    );
    assert!(!query_scalar::<_, bool>("SELECT completed FROM ff_restore")
        .fetch_one(&target.pool)
        .await
        .unwrap());
    assert!(target
        .server
        .bootstrap("other".into(), "不能覆盖".into(), PASSWORD.into())
        .await
        .is_err());
    assert!(Server::connect(&target.url, &target.files).await.is_err());
    assert!(Server::restore(&other, &target.url, &target.files)
        .await
        .is_err());
    query("ALTER TABLE ff_workspaces DROP CONSTRAINT injected_restore_failure")
        .execute(&target.pool)
        .await
        .unwrap();
    assert!(
        !Server::restore(&path, &target.url, &target.files)
            .await
            .unwrap()
            .already_completed
    );
    assert!(Server::connect(&target.url, &target.files).await.is_ok());
    assert!(!target.login("owner", PASSWORD).await.is_empty());
}

#[tokio::test]
async fn damaged_or_rewritten_packages_fail_before_touching_the_restore_database() {
    let (f, token, space, _, _) = source().await;
    let path = backup(&f, &token, "tamper-backup").await;
    let relative = format!("workspaces/{space}/operations.jsonl");
    let original = fs::read(path.join(&relative)).unwrap();
    fs::write(path.join(&relative), b"damaged").unwrap();
    let target = F::new().await;
    assert!(Server::restore(&path, &target.url, &target.files)
        .await
        .is_err());
    assert_eq!(
        query_scalar::<_, i64>("SELECT COUNT(*) FROM ff_restore")
            .fetch_one(&target.pool)
            .await
            .unwrap(),
        0
    );
    for corrupt in [b"{}\n".as_slice(), b"{\"revision\":1}", b""] {
        rewrite(&path, &relative, corrupt);
        assert!(Server::restore(&path, &target.url, &target.files)
            .await
            .is_err());
    }
    rewrite(&path, &relative, &original);
    let account_bytes = fs::read(path.join("accounts.jsonl")).unwrap();
    let mut account: Value = serde_json::from_slice(&account_bytes).unwrap();
    account["password_hash"]=json!("$argon2id$v=19$m=4294967295,t=999,p=1$c29tZXNhbHQ$MDEyMzQ1Njc4OTAxMjM0NTY3ODkwMTIzNDU2Nzg5MDE");
    rewrite(&path, "accounts.jsonl", format!("{account}\n").as_bytes());
    assert!(Server::restore(&path, &target.url, &target.files)
        .await
        .is_err());
    rewrite(&path, "accounts.jsonl", &account_bytes);
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(path.join("backup.json")).unwrap()).unwrap();
    manifest["files"]["../escape"] = json!({"sha256":"a".repeat(64),"size":0});
    fs::write(
        path.join("backup.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    assert!(Server::restore(&path, &target.url, &target.files)
        .await
        .is_err());
    assert_eq!(
        query_scalar::<_, i64>("SELECT COUNT(*) FROM ff_accounts")
            .fetch_one(&target.pool)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn backup_authorization_destinations_and_executable_commands_are_real() {
    let (f, token, _, _, _) = source().await;
    let user = f.create(&token, "ordinary_backup", false).await;
    let initial = f.login(user["account"].as_str().unwrap(), PASSWORD).await;
    let rejected = f.files.with_extension("rejected");
    for unauthorized in ["", "\n", &"a".repeat(64), &initial] {
        assert!(f.server.backup(unauthorized, &rejected).await.is_err());
        assert!(!rejected.exists());
    }
    f.password(&initial).await;
    let ordinary = f.login("ordinary_backup", NEXT_PASSWORD).await;
    assert!(f.server.backup(&ordinary, &rejected).await.is_err());
    assert!(f
        .server
        .backup(&token, f.files.join("inside"))
        .await
        .is_err());
    fs::create_dir(&rejected).unwrap();
    fs::write(rejected.join("keep"), "现有文件").unwrap();
    assert!(f.server.backup(&token, &rejected).await.is_err());
    assert_eq!(
        fs::read_to_string(rejected.join("keep")).unwrap(),
        "现有文件"
    );
    let path = f.files.with_extension("cli-backup");
    let output = Command::new(env!("CARGO_BIN_EXE_farflow-server"))
        .arg("backup")
        .arg(&path)
        .env("DATABASE_URL", &f.url)
        .env("FARFLOW_DATA_DIR", &f.files)
        .env("FARFLOW_BACKUP_TOKEN", &token)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains(&token));
    let target = F::new().await;
    for _ in 0..2 {
        let result = Command::new(env!("CARGO_BIN_EXE_farflow-server"))
            .arg("restore")
            .arg(&path)
            .env("DATABASE_URL", &target.url)
            .env("FARFLOW_DATA_DIR", &target.files)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    assert!(!target.login("owner", PASSWORD).await.is_empty());
}

#[tokio::test]
async fn online_backup_keeps_one_snapshot_while_business_and_attachments_continue() {
    let (f, token, space, _, operation) = source().await;
    let before = f
        .api(
            "GET",
            &format!("/api/workspaces/{space}"),
            Some(&token),
            Value::Null,
        )
        .await
        .1;
    let mut lock = f.pool.begin().await.unwrap();
    query("LOCK TABLE ff_account_events IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .unwrap();
    let destination = f.files.with_extension("concurrent-backup");
    let copy = destination.clone();
    let server = f.server.clone();
    let auth = token.clone();
    let running = tokio::spawn(async move { server.backup(&auth, copy).await });
    tokio::time::timeout(std::time::Duration::from_secs(5),async {
        loop {
            let waiting:bool=query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query LIKE '%FROM ff_account_events a%')").fetch_one(&f.pool).await.unwrap();
            if waiting { break; }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await.expect("备份应在已经取得快照后等待受控表锁");
    let document = operation["command"]["item"]["id"].as_str().unwrap();
    let changed=f.api("POST",&format!("/api/workspaces/{space}/commands"),Some(&token),json!({"id":Id::new_v4(),"command":{"type":"add_comment","id":Id::new_v4(),"item_id":document,"body":"快照之后新增评论","parent_id":null,"mentions":[],"attachments":[]}})).await;
    assert_eq!(changed.0, StatusCode::OK);
    let response=f.send(Request::builder().method("POST").uri(format!("/api/workspaces/{space}/attachments?operation_id={}&id={}&item_id={document}&name=later.txt&mime=text%2Fplain",Id::new_v4(),Id::new_v4()))
        .header("Authorization",format!("Bearer {token}")).body(Body::from("after snapshot")).unwrap()).await;
    assert_eq!(response.status(), StatusCode::OK);
    lock.commit().await.unwrap();
    running.await.unwrap().unwrap();
    let target = F::new().await;
    Server::restore(&destination, &target.url, &target.files)
        .await
        .unwrap();
    let restored = target.login("owner", PASSWORD).await;
    assert_eq!(
        target
            .api(
                "GET",
                &format!("/api/workspaces/{space}"),
                Some(&restored),
                Value::Null
            )
            .await
            .1,
        before
    );
    assert_ne!(
        f.api(
            "GET",
            &format!("/api/workspaces/{space}"),
            Some(&token),
            Value::Null
        )
        .await
        .1,
        before
    );
}

#[tokio::test]
async fn account_and_event_paging_and_zero_workspace_backups_restore_without_truncation() {
    let f = F::new().await;
    let token = f.admin().await;
    let owner: Id = query_scalar("SELECT id FROM ff_accounts")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    query("WITH added AS (INSERT INTO ff_accounts SELECT gen_random_uuid(),'bulk_'||n,'分页成员','研发部',password_hash,true,false,true,1 FROM generate_series(1,501) n CROSS JOIN (SELECT password_hash FROM ff_accounts WHERE id=$1) p RETURNING *) INSERT INTO ff_account_events(account_id,actor_id,action,at,data) SELECT id,$1,'create',1,json_build_object('before',NULL,'after',json_build_object('id',id,'account',account,'name',name,'department',department,'active',active,'is_admin',is_admin,'must_change_password',must_change_password,'version',version))::text FROM added ORDER BY account")
        .bind(owner).execute(&f.pool).await.unwrap();
    let highest: i64 = query_scalar("SELECT MAX(sequence) FROM ff_account_events")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    let path = backup(&f, &token, "paging-backup").await;
    assert_eq!(
        fs::read_to_string(path.join("accounts.jsonl"))
            .unwrap()
            .lines()
            .count(),
        502
    );
    assert_eq!(
        fs::read_to_string(path.join("account-events.jsonl"))
            .unwrap()
            .lines()
            .count(),
        502
    );
    let target = F::new().await;
    Server::restore(&path, &target.url, &target.files)
        .await
        .unwrap();
    let restored = target.login("owner", PASSWORD).await;
    assert_eq!(
        query_scalar::<_, i64>("SELECT COUNT(*) FROM ff_accounts")
            .fetch_one(&target.pool)
            .await
            .unwrap(),
        502
    );
    assert_eq!(
        target
            .api("GET", "/api/workspaces", Some(&restored), Value::Null)
            .await
            .1,
        json!([])
    );
    target.create(&restored, "after_paging", false).await;
    assert!(
        query_scalar::<_, i64>("SELECT MAX(sequence) FROM ff_account_events")
            .fetch_one(&target.pool)
            .await
            .unwrap()
            > highest
    );
}

pub(super) async fn empty_database(f: &F) -> String {
    let database = format!("ff_{}", Id::new_v4().simple());
    query(&format!("CREATE DATABASE {database}"))
        .execute(&f.pool)
        .await
        .unwrap();
    let start = f.url.find("/ff_").unwrap();
    let suffix = f.url.find('?').unwrap();
    format!("{}/{}{}", &f.url[..start], database, &f.url[suffix..])
}

#[tokio::test]
async fn database_and_files_cannot_be_swapped_or_copied_to_another_directory() {
    let f = F::new().await;
    let (a, b) = tokio::join!(
        Server::connect(&f.url, &f.files),
        Server::connect(&f.url, &f.files)
    );
    assert!(a.is_ok() && b.is_ok());
    let other = f.files.with_extension("other");
    assert!(Server::connect(&f.url, &other).await.is_err());
    fs::copy(f.files.join("storage.json"), other.join("storage.json")).unwrap();
    assert!(Server::connect(&f.url, &other).await.is_err());
    let other_database = empty_database(&f).await;
    assert!(Server::connect(&other_database, &f.files).await.is_err());
    assert!(Server::connect(&f.url, &f.files).await.is_ok());
    #[cfg(unix)]
    {
        let linked = f.files.with_extension("link");
        std::os::unix::fs::symlink(&f.files, &linked).unwrap();
        assert!(Server::connect(&f.url, linked).await.is_ok());
    }
}

#[tokio::test]
async fn pending_initialization_recovers_but_ready_storage_cannot_be_recreated() {
    let f = F::new().await;
    let initial = fs::read(f.files.join("storage.json")).unwrap();
    query("UPDATE ff_storage SET ready=false")
        .execute(&f.pool)
        .await
        .unwrap();
    fs::remove_file(f.files.join("storage.json")).unwrap();
    assert!(Server::connect(&f.url, &f.files).await.is_ok());
    assert_eq!(fs::read(f.files.join("storage.json")).unwrap(), initial);
    fs::remove_file(f.files.join("storage.json")).unwrap();
    assert!(Server::connect(&f.url, &f.files).await.is_err());
    assert!(!f.files.join("storage.json").exists());
    fs::write(f.files.join("storage.json"), &initial).unwrap();
    query("UPDATE ff_storage SET ready=false")
        .execute(&f.pool)
        .await
        .unwrap();
    let moved = f.files.with_extension("pending-replacement");
    assert!(Server::connect(&f.url, &moved).await.is_ok());
    assert_ne!(fs::read(moved.join("storage.json")).unwrap(), initial);
    assert!(Server::connect(&f.url, &f.files).await.is_err());
}

#[tokio::test]
async fn existing_unbound_data_needs_migration_and_runtime_marker_loss_stops_business() {
    let f = F::new().await;
    let token = f.admin().await;
    fs::remove_file(f.files.join("storage.json")).unwrap();
    let result = f
        .api(
            "POST",
            "/api/workspaces",
            Some(&token),
            json!({"id":Id::new_v4(),"name":"不能保存到缺失目录"}),
        )
        .await;
    assert_eq!(result.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        query_scalar::<_, i64>("SELECT COUNT(*) FROM ff_workspaces")
            .fetch_one(&f.pool)
            .await
            .unwrap(),
        0
    );
    query("DELETE FROM ff_storage")
        .execute(&f.pool)
        .await
        .unwrap();
    assert!(Server::connect(&f.url, &f.files).await.is_err());
    assert!(!f.files.join("storage.json").exists());
    assert_eq!(
        f.api("GET", "/api/auth/me", Some(&token), Value::Null)
            .await
            .0,
        StatusCode::OK
    );
}
