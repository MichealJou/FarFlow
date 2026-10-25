use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use farflow_core::Id;
use farflow_server::Server;
use serde_json::{json, Value};
use sqlx_core::{query::query, query_scalar::query_scalar, row::Row};
use sqlx_postgres::{PgPool, PgPoolOptions};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, OnceLock, Weak},
};
use tower::ServiceExt;
#[path = "cases/migration.rs"]
mod migration;
#[path = "cases/service_storage.rs"]
mod service_storage;
#[path = "cases/workspaces.rs"]
mod workspaces;

const PASSWORD: &str = "FarFlow-初始密码-2026";
const NEXT_PASSWORD: &str = "FarFlow-本人新密码-2026";

struct Running(Child);
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Cluster {
    directory: tempfile::TempDir,
    url: String,
}
impl Drop for Cluster {
    fn drop(&mut self) {
        let _ = Command::new("pg_ctl")
            .arg("-D")
            .arg(self.directory.path().join("data"))
            .args(["-m", "immediate", "-w", "stop"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}
fn cluster() -> Arc<Cluster> {
    static CURRENT: OnceLock<Mutex<Weak<Cluster>>> = OnceLock::new();
    let mut current = CURRENT
        .get_or_init(|| Mutex::new(Weak::new()))
        .lock()
        .unwrap();
    if let Some(cluster) = current.upgrade() {
        return cluster;
    }
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let data = directory.path().join("data");
    let init = Command::new("initdb")
        .arg("-D")
        .arg(&data)
        .args([
            "-U",
            "farflow_test",
            "-A",
            "trust",
            "--encoding=UTF8",
            "--locale=C",
            "--no-instructions",
        ])
        .output()
        .expect("测试需要可用的 PostgreSQL initdb");
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    let started = Command::new("pg_ctl")
        .arg("-D")
        .arg(&data)
        .arg("-l")
        .arg(directory.path().join("postgres.log"))
        .arg("-o")
        .arg(format!(
            "-h '' -k {} -p 55432 -c fsync=off",
            directory.path().display()
        ))
        .args(["-w", "start"])
        .output()
        .unwrap();
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let url = format!(
        "postgresql://farflow_test@localhost/postgres?host={}&port=55432&sslmode=disable",
        directory.path().display()
    );
    let cluster = Arc::new(Cluster { directory, url });
    *current = Arc::downgrade(&cluster);
    cluster
}

struct F {
    _cluster: Arc<Cluster>,
    url: String,
    files: std::path::PathBuf,
    address: Option<String>,
    server: Server,
    pool: PgPool,
}
impl F {
    async fn new() -> Self {
        let cluster = tokio::task::spawn_blocking(cluster).await.unwrap();
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect(&cluster.url)
            .await
            .unwrap();
        let database = format!("ff_{}", Id::new_v4().simple());
        query(&format!("CREATE DATABASE {database}"))
            .execute(&admin)
            .await
            .unwrap();
        admin.close().await;
        let url = cluster.url.replace("/postgres?", &format!("/{database}?"));
        let files = cluster.directory.path().join(format!("{database}-files"));
        let server = Server::connect(&url, &files).await.unwrap();
        let pool = PgPoolOptions::new()
            .max_connections(3)
            .connect(&url)
            .await
            .unwrap();
        Self {
            _cluster: cluster,
            url,
            files,
            address: None,
            server,
            pool,
        }
    }
    async fn bootstrap(&self) {
        self.server
            .bootstrap("Owner".into(), "服务管理员".into(), PASSWORD.into())
            .await
            .unwrap();
    }
    async fn api(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Value,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header("Content-Type", "application/json");
        if let Some(token) = token {
            builder = builder.header("Authorization", format!("Bearer {token}"));
        }
        let response = self
            .send(
                builder
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await;
        assert_eq!(response.headers()["cache-control"], "no-store");
        let status = response.status();
        let data = to_bytes(response.into_body(), 2 * 1024 * 1024)
            .await
            .unwrap();
        let body = if data.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&data).unwrap()
        };
        (status, body)
    }
    async fn send(&self, mut request: Request<Body>) -> axum::response::Response {
        if let Some(address) = &self.address {
            *request.uri_mut() = format!("http://{address}{}", request.uri())
                .parse()
                .unwrap();
            let client =
                hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                    .build_http::<Body>();
            client.request(request).await.unwrap().map(Body::new)
        } else {
            self.server.router().oneshot(request).await.unwrap()
        }
    }
    async fn login(&self, account: &str, password: &str) -> String {
        let (status, body) = self
            .api(
                "POST",
                "/api/auth/login",
                None,
                json!({"account":account,"password":password}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["token"].as_str().unwrap().into()
    }
    async fn admin(&self) -> String {
        self.bootstrap().await;
        self.login("owner", PASSWORD).await
    }
    async fn create(&self, token: &str, account: &str, admin: bool) -> Value {
        let (status,body) = self.api("POST","/api/users",Some(token),json!({"account":account,"name":"研发成员","department":"研发部","password":PASSWORD,"is_admin":admin})).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body
    }
    async fn password(&self, token: &str) {
        let (status, body) = self
            .api(
                "POST",
                "/api/auth/password",
                Some(token),
                json!({"current_password":PASSWORD,"new_password":NEXT_PASSWORD}),
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    }
}

#[tokio::test]
async fn bootstrap_is_single_use_and_accounts_and_sessions_survive_reconnection() {
    let f = F::new().await;
    let (a, b) = tokio::join!(
        f.server
            .bootstrap("Owner".into(), "管理员".into(), PASSWORD.into()),
        f.server
            .bootstrap("Other".into(), "管理员".into(), PASSWORD.into())
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    let account = if a.is_ok() { "owner" } else { "other" };
    let token = f.login(account, PASSWORD).await;
    assert_eq!(
        f.api("GET", "/api/auth/me", Some(&token), Value::Null)
            .await
            .1["account"],
        account
    );
    let reopened = Server::connect(&f.url, &f.files).await.unwrap();
    let response = reopened
        .router()
        .oneshot(
            Request::builder()
                .uri("/api/auth/me")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let row = query("SELECT password_hash FROM ff_accounts")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    let hash: String = row.get("password_hash");
    assert!(hash.starts_with("$argon2id$v=19$"));
    assert!(!hash.contains(PASSWORD));
    let stored: String = query_scalar("SELECT token_hash FROM ff_sessions")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_ne!(stored, token);
    assert_eq!(stored.len(), 64);
}

#[tokio::test]
async fn passwords_are_changed_by_the_account_owner_and_every_old_session_is_revoked() {
    let f = F::new().await;
    let admin = f.admin().await;
    let user = f.create(&admin, "Dev", false).await;
    let token = f.login("DEV", PASSWORD).await;
    let other = f.login("dev", PASSWORD).await;
    assert_eq!(
        f.api("GET", "/api/auth/me", Some(&token), Value::Null)
            .await
            .1["must_change_password"],
        true
    );
    assert_eq!(
        f.api("GET", "/api/users", Some(&token), Value::Null)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    for body in [
        json!({"current_password":"错误原密码","new_password":NEXT_PASSWORD}),
        json!({"current_password":PASSWORD,"new_password":PASSWORD}),
        json!({"current_password":PASSWORD,"new_password":"short"}),
    ] {
        assert!(f
            .api("POST", "/api/auth/password", Some(&token), body)
            .await
            .0
            .is_client_error());
    }
    f.password(&token).await;
    for old in [&token, &other] {
        assert_eq!(
            f.api("GET", "/api/auth/me", Some(old), Value::Null).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        f.api(
            "POST",
            "/api/auth/login",
            None,
            json!({"account":"dev","password":PASSWORD})
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let new = f.login("dev", NEXT_PASSWORD).await;
    let profile = f
        .api("GET", "/api/auth/me", Some(&new), Value::Null)
        .await
        .1;
    assert_eq!(profile["id"], user["id"]);
    assert_eq!(profile["must_change_password"], false);
    assert_eq!(
        f.api("GET", "/api/users", Some(&new), Value::Null).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        f.api("POST", "/api/auth/logout", Some(&new), Value::Null)
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        f.api("GET", "/api/auth/me", Some(&new), Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn disabling_resetting_and_reenabling_users_does_not_restore_old_sessions() {
    let f = F::new().await;
    let admin = f.admin().await;
    let user = f.create(&admin, "tester", false).await;
    let token = f.login("tester", PASSWORD).await;
    let path = format!("/api/users/{}", user["id"].as_str().unwrap());
    let mut patch = json!({"version":user["version"],"name":"测试成员","department":"质量部","active":false,"is_admin":false});
    let disabled = f.api("PATCH", &path, Some(&admin), patch.clone()).await;
    assert_eq!(disabled.0, StatusCode::OK);
    assert_eq!(
        f.api("GET", "/api/auth/me", Some(&token), Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        f.api(
            "POST",
            "/api/auth/login",
            None,
            json!({"account":"tester","password":PASSWORD})
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        f.api("PATCH", &path, Some(&admin), patch.clone()).await.0,
        StatusCode::CONFLICT
    );
    patch["version"] = disabled.1["version"].clone();
    patch["active"] = json!(true);
    assert_eq!(
        f.api("PATCH", &path, Some(&admin), patch).await.0,
        StatusCode::OK
    );
    assert_eq!(
        f.api("GET", "/api/auth/me", Some(&token), Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let before_reset = f.login("tester", PASSWORD).await;
    let reset = f
        .api(
            "POST",
            &format!("{path}/password"),
            Some(&admin),
            json!({"password":NEXT_PASSWORD}),
        )
        .await;
    assert_eq!(reset.0, StatusCode::NO_CONTENT);
    assert_eq!(
        f.api("GET", "/api/auth/me", Some(&before_reset), Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let after_reset = f.login("tester", NEXT_PASSWORD).await;
    assert_eq!(
        f.api("GET", "/api/auth/me", Some(&after_reset), Value::Null)
            .await
            .1["must_change_password"],
        true
    );
    let listed = f.api("GET", "/api/users", Some(&admin), Value::Null).await;
    assert_eq!(listed.0, StatusCode::OK);
    assert_eq!(listed.1.as_array().unwrap().len(), 2);
    assert!(!listed.1.to_string().contains("password_hash"));
    assert!(!listed.1.to_string().contains("token"));
}

#[tokio::test]
async fn last_admin_is_protected_and_two_competing_demotions_have_only_one_winner() {
    let f = F::new().await;
    let first = f.admin().await;
    let owner = f
        .api("GET", "/api/auth/me", Some(&first), Value::Null)
        .await
        .1;
    let own_path = format!("/api/users/{}", owner["id"].as_str().unwrap());
    let demote =
        json!({"version":1,"name":"管理员","department":"管理部","active":true,"is_admin":false});
    assert_eq!(
        f.api("PATCH", &own_path, Some(&first), demote.clone())
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let second_user = f.create(&first, "second", true).await;
    let initial = f.login("second", PASSWORD).await;
    assert_eq!(
        f.api("GET", "/api/users", Some(&initial), Value::Null)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    f.password(&initial).await;
    let second = f.login("second", NEXT_PASSWORD).await;
    let second_path = format!("/api/users/{}", second_user["id"].as_str().unwrap());
    let (left,right)=tokio::join!(f.api("PATCH",&own_path,Some(&first),demote.clone()),f.api("PATCH",&second_path,Some(&second),json!({"version":2,"name":"管理员二","department":"管理部","active":true,"is_admin":false})));
    assert_eq!(
        usize::from(left.0 == StatusCode::OK) + usize::from(right.0 == StatusCode::OK),
        1
    );
    let count: i64 = query_scalar("SELECT COUNT(*) FROM ff_accounts WHERE active AND is_admin")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    let old = if left.0 == StatusCode::OK {
        &first
    } else {
        &second
    };
    assert_eq!(
        f.api("GET", "/api/auth/me", Some(old), Value::Null).await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn invalid_authentication_throttling_and_expiry_are_real_database_rules() {
    let f = F::new().await;
    let admin = f.admin().await;
    for header in [None, Some("bad"), Some(&"a".repeat(64))] {
        assert_eq!(
            f.api("GET", "/api/auth/me", header, Value::Null).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    for account in ["owner", "missing"] {
        for _ in 0..5 {
            let failed = f
                .api(
                    "POST",
                    "/api/auth/login",
                    None,
                    json!({"account":account,"password":"wrong-password-123"}),
                )
                .await;
            assert_eq!(failed.0, StatusCode::UNAUTHORIZED);
            assert_eq!(failed.1["message"], "账户或密码不正确");
        }
        assert_eq!(
            f.api(
                "POST",
                "/api/auth/login",
                None,
                json!({"account":account,"password":PASSWORD})
            )
            .await
            .0,
            StatusCode::TOO_MANY_REQUESTS
        );
    }
    query("UPDATE ff_login_attempts SET window_start=0,blocked_until=0")
        .execute(&f.pool)
        .await
        .unwrap();
    let token = f.login("owner", PASSWORD).await;
    query("UPDATE ff_sessions SET expires_at=0,created_at=-1")
        .execute(&f.pool)
        .await
        .unwrap();
    assert_eq!(
        f.api("GET", "/api/auth/me", Some(&admin), Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        f.api("POST", "/api/auth/logout", Some(&token), Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn input_validation_duplicate_accounts_and_missing_users_return_safe_errors() {
    let f = F::new().await;
    assert!(Server::connect("invalid-database-url", &f.files)
        .await
        .is_err());
    for (account, name, password) in [
        ("ab", "管理者", PASSWORD),
        ("admin", " ", PASSWORD),
        ("admin", "管理者", "short"),
    ] {
        assert!(f
            .server
            .bootstrap(account.into(), name.into(), password.into())
            .await
            .is_err());
    }
    let admin = f.admin().await;
    for body in [
        json!({"account":"ab","name":"成员","password":PASSWORD}),
        json!({"account":"valid","name":"成员","password":"short"}),
        json!({"account":"valid","name":"成员","department":"x".repeat(121),"password":PASSWORD}),
        json!({"account":"valid","name":"bad\nname","password":PASSWORD}),
        json!({"account":"owner","name":"重复管理员","password":PASSWORD}),
        json!({"account":"valid","name":"成员","password":PASSWORD,"actor_id":Id::new_v4()}),
    ] {
        assert!(f
            .api("POST", "/api/users", Some(&admin), body)
            .await
            .0
            .is_client_error());
    }
    assert_eq!(
        f.api(
            "POST",
            "/api/users",
            None,
            json!({"account":"valid","name":"成员","password":PASSWORD})
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let missing = format!("/api/users/{}", Id::new_v4());
    assert_eq!(
        f.api(
            "PATCH",
            &missing,
            Some(&admin),
            json!({"version":1,"name":"成员","department":"","active":true,"is_admin":false})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.api(
            "POST",
            &format!("{missing}/password"),
            Some(&admin),
            json!({"password":NEXT_PASSWORD})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.api("GET", "/missing", None, Value::Null).await.1["message"],
        "接口不存在"
    );
    assert_eq!(
        f.api("PATCH", "/api/users/invalid", Some(&admin), json!({}))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        f.api("GET", "/api/auth/login", None, Value::Null).await.0,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        f.api(
            "POST",
            "/api/auth/login",
            None,
            json!({"account":"owner","password":"x".repeat(17_000)})
        )
        .await
        .0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
}

#[tokio::test]
async fn ordinary_accounts_cannot_manage_users_or_spoof_an_administrator() {
    let f = F::new().await;
    let admin = f.admin().await;
    let user = f.create(&admin, "ordinary", false).await;
    let first = f.login("ordinary", PASSWORD).await;
    f.password(&first).await;
    let token = f.login("ordinary", NEXT_PASSWORD).await;
    let path = format!("/api/users/{}", user["id"].as_str().unwrap());
    for (method, path, body) in [
        ("GET", "/api/users".to_owned(), Value::Null),
        (
            "POST",
            "/api/users".to_owned(),
            json!({"account":"forged","name":"伪造管理员","password":PASSWORD,"is_admin":true}),
        ),
        (
            "PATCH",
            path.clone(),
            json!({"version":2,"name":"提权","department":"","active":true,"is_admin":true}),
        ),
        (
            "POST",
            format!("{path}/password"),
            json!({"password":PASSWORD}),
        ),
    ] {
        assert_eq!(
            f.api(method, &path, Some(&token), body.clone()).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            f.api(method, &path, None, body).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    let row = query("SELECT is_admin,password_hash FROM ff_accounts WHERE id=$1")
        .bind(Id::parse_str(user["id"].as_str().unwrap()).unwrap())
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert!(!row.get::<bool, _>("is_admin"));
    assert_eq!(
        f.api(
            "POST",
            "/api/auth/login",
            None,
            json!({"account":"ordinary","password":NEXT_PASSWORD,"actor_id":Id::new_v4()})
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
}

#[tokio::test]
async fn profile_updates_use_compare_and_swap_and_revocation_is_shared_between_servers() {
    let f = F::new().await;
    let admin = f.admin().await;
    let user = f.create(&admin, "profile", false).await;
    let token = f.login("profile", PASSWORD).await;
    let path = format!("/api/users/{}", user["id"].as_str().unwrap());
    let update =
        json!({"version":1,"name":"修改姓名","department":"质量部","active":true,"is_admin":false});
    let (first, second) = tokio::join!(
        f.api("PATCH", &path, Some(&admin), update.clone()),
        f.api("PATCH", &path, Some(&admin), update)
    );
    assert_eq!(
        usize::from(first.0 == StatusCode::OK) + usize::from(second.0 == StatusCode::OK),
        1
    );
    assert!(first.0 == StatusCode::CONFLICT || second.0 == StatusCode::CONFLICT);
    assert_eq!(
        f.api("GET", "/api/auth/me", Some(&token), Value::Null)
            .await
            .1["department"],
        "质量部"
    );
    let other = Server::connect(&f.url, &f.files).await.unwrap();
    let reset = f
        .api(
            "POST",
            &format!("{path}/password"),
            Some(&admin),
            json!({"password":NEXT_PASSWORD}),
        )
        .await;
    assert_eq!(reset.0, StatusCode::NO_CONTENT);
    let response = other
        .router()
        .oneshot(
            Request::builder()
                .uri("/api/auth/me")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    query("DROP TABLE ff_login_attempts")
        .execute(&f.pool)
        .await
        .unwrap();
    let failed = f
        .api(
            "POST",
            "/api/auth/login",
            None,
            json!({"account":"owner","password":PASSWORD}),
        )
        .await;
    assert_eq!(failed.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(!failed.1.to_string().contains("ff_login_attempts"));
}

#[tokio::test]
async fn account_history_is_private_paginated_secret_free_and_atomic_with_changes() {
    let f = F::new().await;
    let admin = f.admin().await;
    let user = f.create(&admin, "history", false).await;
    let initial = f.login("history", PASSWORD).await;
    f.password(&initial).await;
    let current = f.login("history", NEXT_PASSWORD).await;
    let path = format!("/api/users/{}", user["id"].as_str().unwrap());
    let history_path = format!("{path}/history");
    assert_eq!(
        f.api("GET", &history_path, None, Value::Null).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        f.api("GET", &history_path, Some(&current), Value::Null)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        f.api(
            "GET",
            &format!("{history_path}?after=-1"),
            Some(&admin),
            Value::Null
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        f.api(
            "GET",
            &format!("/api/users/{}/history", Id::new_v4()),
            Some(&admin),
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.api(
            "POST",
            &format!("{path}/password"),
            Some(&admin),
            json!({"password":PASSWORD})
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    for version in 3..104 {
        assert_eq!(f.api("PATCH",&path,Some(&admin),json!({"version":version,"name":format!("姓名{version}"),"department":"研发部","active":true,"is_admin":false})).await.0,StatusCode::OK);
    }
    let first = f.api("GET", &history_path, Some(&admin), Value::Null).await;
    assert_eq!(first.0, StatusCode::OK);
    let events = first.1["events"].as_array().unwrap();
    assert_eq!(events.len(), 100);
    assert_eq!(events[0]["action"], "create");
    assert_eq!(events[1]["action"], "change_password");
    assert_eq!(events[2]["action"], "reset_password");
    assert_eq!(events[1]["actor_id"], user["id"]);
    assert!(events[0]["at"].as_i64().unwrap() > 0);
    let cursor = first.1["next"].as_i64().unwrap();
    let second = f
        .api(
            "GET",
            &format!("{history_path}?after={cursor}"),
            Some(&admin),
            Value::Null,
        )
        .await
        .1;
    assert_eq!(second["events"].as_array().unwrap().len(), 4);
    assert!(second["next"].is_null());
    for forbidden in [PASSWORD, NEXT_PASSWORD, "password_hash", "token_hash"] {
        assert!(!first.1.to_string().contains(forbidden));
        assert!(!second.to_string().contains(forbidden));
    }
    query("DROP TABLE ff_account_events")
        .execute(&f.pool)
        .await
        .unwrap();
    assert_eq!(f.api("PATCH",&path,Some(&admin),json!({"version":104,"name":"不应保存","department":"研发部","active":true,"is_admin":false})).await.0,StatusCode::INTERNAL_SERVER_ERROR);
    let row = query("SELECT name,version FROM ff_accounts WHERE account='history'")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("name"), "姓名103");
    assert_eq!(row.get::<i64, _>("version"), 104);
}

#[tokio::test]
async fn executable_initializes_admin_and_serves_real_http_until_graceful_shutdown() {
    let f = F::new().await;
    let web = tempfile::tempdir().unwrap();
    std::fs::create_dir(web.path().join("assets")).unwrap();
    std::fs::write(
        web.path().join("index.html"),
        "<!doctype html><title>FarFlow 研流</title>",
    )
    .unwrap();
    std::fs::write(web.path().join("assets/app.js"), "console.log('FarFlow')").unwrap();
    let binary = env!("CARGO_BIN_EXE_farflow-server");
    let help = Command::new(binary)
        .arg("--help")
        .env_remove("DATABASE_URL")
        .output()
        .unwrap();
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("初始化首位管理员"));
    for args in [vec!["bad"], vec!["serve", "extra"], vec!["serve"]] {
        assert!(!Command::new(binary)
            .args(args)
            .env_remove("DATABASE_URL")
            .output()
            .unwrap()
            .status
            .success());
    }
    let initialized = Command::new(binary)
        .arg("init-admin")
        .env("DATABASE_URL", &f.url)
        .env("FARFLOW_DATA_DIR", &f.files)
        .env("FARFLOW_ADMIN_ACCOUNT", "owner")
        .env("FARFLOW_ADMIN_NAME", "初始管理员")
        .env("FARFLOW_ADMIN_PASSWORD", PASSWORD)
        .output()
        .unwrap();
    assert!(
        initialized.status.success(),
        "{}",
        String::from_utf8_lossy(&initialized.stderr)
    );
    let invalid = Command::new(binary)
        .arg("serve")
        .env("DATABASE_URL", &f.url)
        .env("FARFLOW_DATA_DIR", &f.files)
        .env("FARFLOW_LISTEN", "invalid")
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    let invalid_web = Command::new(binary)
        .arg("serve")
        .env("DATABASE_URL", &f.url)
        .env("FARFLOW_DATA_DIR", &f.files)
        .env("FARFLOW_LISTEN", "127.0.0.1:0")
        .env("FARFLOW_WEB_DIR", &f.files)
        .output()
        .unwrap();
    assert!(!invalid_web.status.success());
    assert!(invalid_web.stdout.is_empty());
    assert!(f.server.router_with_web(web.path()).is_ok());
    let mut process = Running(
        Command::new(binary)
            .arg("serve")
            .env("DATABASE_URL", &f.url)
            .env("FARFLOW_DATA_DIR", &f.files)
            .env("FARFLOW_LISTEN", "127.0.0.1:0")
            .env("FARFLOW_WEB_DIR", web.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut line = String::new();
    BufReader::new(process.0.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let address = line.trim().strip_prefix("FarFlow 服务已启动：").unwrap();
    let body = json!({"account":"owner","password":PASSWORD}).to_string();
    let mut connection = TcpStream::connect(address).unwrap();
    connection
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    write!(connection,"POST /api/auth/login HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",body.len(),body).unwrap();
    let mut response = String::new();
    connection.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"));
    let returned: Value = serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(returned["user"]["account"], "owner");
    assert_eq!(returned["token"].as_str().unwrap().len(), 64);
    for (path, status, expected) in [
        ("/", "200", "FarFlow 研流"),
        ("/assets/app.js", "200", "console.log('FarFlow')"),
        ("/api/missing", "404", "接口不存在"),
    ] {
        let mut connection = TcpStream::connect(address).unwrap();
        connection
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        write!(
            connection,
            "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        connection.read_to_string(&mut response).unwrap();
        assert!(
            response.starts_with(&format!("HTTP/1.1 {status}")),
            "{path}: {response}"
        );
        assert!(response.contains(expected), "{path}");
    }
    #[cfg(unix)]
    {
        assert!(Command::new("kill")
            .args(["-INT", &process.0.id().to_string()])
            .status()
            .unwrap()
            .success());
        assert!(process.0.wait().unwrap().success());
    }
}
