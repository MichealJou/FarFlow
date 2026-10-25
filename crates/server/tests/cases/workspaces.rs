use super::{Running, F, NEXT_PASSWORD, PASSWORD};
use axum::{
    body::{to_bytes, Body},
    http::{HeaderMap, Request as HttpRequest, StatusCode},
};
use farflow_core::{
    Command, Id, Kind, Link, Member, NewItem, Patch, Relation, Request, ReviewDecision, Role, State,
};
use farflow_server::Server;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde_json::{json, Value};
use sqlx_core::{query::query, query_scalar::query_scalar, row::Row};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufRead, BufReader},
    process::{Command as Process, Stdio},
};

fn item(kind: Kind, title: &str, product: Option<Id>, project: Option<Id>) -> NewItem {
    serde_json::from_value(json!({"id":Id::new_v4(),"kind":kind,"title":title,"description":"业务详细说明","acceptance":"验收标准明确","product_id":product,"project_id":project})).unwrap()
}
fn path(id: Id) -> String {
    format!("/api/workspaces/{id}")
}
impl F {
    async fn workspace(&self, token: &str) -> Id {
        let id = Id::new_v4();
        let result = self
            .api(
                "POST",
                "/api/workspaces",
                Some(token),
                json!({"id":id,"name":"研流服务工作空间"}),
            )
            .await;
        assert_eq!(result.0, StatusCode::CREATED, "{}", result.1);
        assert_eq!(result.1["mode"], "service");
        id
    }
    async fn state(&self, token: &str, id: Id) -> State {
        let result = self.api("GET", &path(id), Some(token), Value::Null).await;
        assert_eq!(result.0, StatusCode::OK, "{}", result.1);
        serde_json::from_value(result.1["state"].clone()).unwrap()
    }
    async fn command(
        &self,
        token: &str,
        workspace: Id,
        command: Command,
        revision: Option<u64>,
    ) -> Value {
        let result = self
            .api(
                "POST",
                &format!("{}/commands", path(workspace)),
                Some(token),
                json!({"id":Id::new_v4(),"expected_revision":revision,"command":command}),
            )
            .await;
        assert_eq!(result.0, StatusCode::OK, "{}", result.1);
        result.1
    }
    async fn add(&self, token: &str, workspace: Id, user: &Value, roles: &[Role]) {
        let revision = self.state(token, workspace).await.revision;
        let member = Member {
            id: Id::parse_str(user["id"].as_str().unwrap()).unwrap(),
            account: "untrusted-profile".into(),
            name: "不能采用客户端姓名".into(),
            department: "不能采用客户端部门".into(),
            roles: roles.iter().copied().collect(),
            active: true,
        };
        self.command(
            token,
            workspace,
            Command::AddMember { member },
            Some(revision),
        )
        .await;
    }
    async fn ready(&self, admin: &str, account: &str, administrator: bool) -> (Value, String) {
        let user = self.create(admin, account, administrator).await;
        let first = self.login(account, PASSWORD).await;
        self.password(&first).await;
        let token = self.login(account, NEXT_PASSWORD).await;
        (user, token)
    }
    async fn insert(&self, token: &str, workspace: Id, item: NewItem) -> Id {
        let id = item.id;
        self.command(token, workspace, Command::Create { item }, None)
            .await;
        id
    }
    async fn transition(&self, token: &str, workspace: Id, id: Id, target: &str) {
        let version = self.state(token, workspace).await.items[&id].version;
        self.command(
            token,
            workspace,
            Command::Transition {
                id,
                version,
                target: target.into(),
                note: "实际执行与交接说明".into(),
            },
            None,
        )
        .await;
    }
    async fn approve(&self, token: &str, workspace: Id, id: Id) {
        self.transition(token, workspace, id, "reviewing").await;
        let version = self.state(token, workspace).await.items[&id].version;
        self.command(
            token,
            workspace,
            Command::Review {
                id,
                version,
                decision: ReviewDecision::Approve,
                note: "评审确认标准明确".into(),
            },
            None,
        )
        .await;
    }
    async fn raw(
        &self,
        method: &str,
        url: &str,
        token: Option<&str>,
        bytes: Vec<u8>,
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut request = HttpRequest::builder()
            .method(method)
            .uri(url)
            .header("Content-Type", "application/octet-stream");
        if let Some(token) = token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        let response = self.send(request.body(Body::from(bytes)).unwrap()).await;
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), 11 * 1024 * 1024)
            .await
            .unwrap()
            .to_vec();
        (status, headers, bytes)
    }
}

fn start_service(f: &mut F) -> Running {
    let mut running = Running(
        Process::new(env!("CARGO_BIN_EXE_farflow-server"))
            .arg("serve")
            .env("DATABASE_URL", &f.url)
            .env("FARFLOW_DATA_DIR", &f.files)
            .env("FARFLOW_LISTEN", "127.0.0.1:0")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut line = String::new();
    BufReader::new(running.0.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    f.address = Some(
        line.trim()
            .strip_prefix("FarFlow 服务已启动：")
            .unwrap()
            .into(),
    );
    running
}

fn stop_service(running: &mut Running) {
    #[cfg(unix)]
    {
        assert!(Process::new("kill")
            .args(["-INT", &running.0.id().to_string()])
            .status()
            .unwrap()
            .success());
        assert!(running.0.wait().unwrap().success());
    }
    #[cfg(not(unix))]
    {
        running.0.kill().unwrap();
        running.0.wait().unwrap();
    }
}
fn upload_url(workspace: Id, operation: Id, id: Id, object: Id, name: &str) -> String {
    format!("{}/attachments?operation_id={operation}&id={id}&item_id={object}&name={}&mime=text%2Fplain",path(workspace),utf8_percent_encode(name,NON_ALPHANUMERIC))
}

#[tokio::test]
async fn workspace_creation_is_idempotent_and_system_admin_does_not_bypass_membership() {
    let f = F::new().await;
    let admin = f.admin().await;
    let (_, owner) = f.ready(&admin, "workspace_owner", false).await;
    let workspace = f.workspace(&owner).await;
    let repeated = f
        .api(
            "POST",
            "/api/workspaces",
            Some(&owner),
            json!({"id":workspace,"name":"研流服务工作空间"}),
        )
        .await;
    assert_eq!(repeated.0, StatusCode::OK);
    assert_eq!(
        f.api(
            "POST",
            "/api/workspaces",
            Some(&owner),
            json!({"id":workspace,"name":"更换原始名称"})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        f.api("GET", "/api/workspaces", Some(&owner), Value::Null)
            .await
            .1
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(f
        .api("GET", "/api/workspaces", Some(&admin), Value::Null)
        .await
        .1
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(
        f.api("GET", &path(workspace), Some(&admin), Value::Null)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        f.api("GET", &path(workspace), None, Value::Null).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        f.api("GET", &path(Id::new_v4()), Some(&owner), Value::Null)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.api(
            "POST",
            "/api/workspaces",
            Some(&owner),
            json!({"id":Id::nil(),"name":"空间"})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let initial = f.create(&admin, "initial", false).await;
    let token = f.login("initial", PASSWORD).await;
    assert_eq!(
        f.api(
            "POST",
            "/api/workspaces",
            Some(&token),
            json!({"id":Id::new_v4(),"name":"未改密不能创建"})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert!(initial.get("password_hash").is_none());
}

#[tokio::test]
async fn real_accounts_supply_member_profiles_and_membership_requires_version_and_owner() {
    let f = F::new().await;
    let admin = f.admin().await;
    let workspace = f.workspace(&admin).await;
    let (dev, token) = f.ready(&admin, "developer", false).await;
    let id = Id::parse_str(dev["id"].as_str().unwrap()).unwrap();
    f.add(&admin, workspace, &dev, &[Role::Developer]).await;
    let state = f.state(&admin, workspace).await;
    assert_eq!(state.members[&id].account, "developer");
    assert_eq!(state.members[&id].name, "研发成员");
    let route = format!("{}/commands", path(workspace));
    let input = json!({"id":Id::new_v4(),"command":Command::UpdateMember{member:state.members[&id].clone()}});
    assert_eq!(
        f.api("POST", &route, Some(&admin), input.clone()).await.0,
        StatusCode::BAD_REQUEST
    );
    let mut current = input.clone();
    current["expected_revision"] = json!(state.revision);
    assert_eq!(
        f.api("POST", &route, Some(&token), current.clone()).await.0,
        StatusCode::FORBIDDEN
    );
    current["expected_revision"] = json!(0);
    assert_eq!(
        f.api("POST", &route, Some(&admin), current).await.0,
        StatusCode::CONFLICT
    );
    let mut disabled = state.members[&id].clone();
    disabled.active = false;
    f.command(
        &admin,
        workspace,
        Command::UpdateMember { member: disabled },
        Some(state.revision),
    )
    .await;
    assert_eq!(
        f.api("GET", &path(workspace), Some(&token), Value::Null)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert!(f
        .api("GET", "/api/workspaces", Some(&token), Value::Null)
        .await
        .1
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(
        f.api(
            "GET",
            &format!("{}/people", path(workspace)),
            Some(&token),
            Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let directory = f
        .api(
            "GET",
            &format!("{}/people?text=developer", path(workspace)),
            Some(&admin),
            Value::Null,
        )
        .await
        .1;
    assert_eq!(directory["accounts"].as_array().unwrap().len(), 1);
    assert!(!directory.to_string().contains("password"));
    assert!(!directory.to_string().contains("is_admin"));
}

#[tokio::test]
async fn commands_use_server_identity_persistent_idempotency_and_compare_and_swap() {
    let mut f = F::new().await;
    let admin = f.admin().await;
    let workspace = f.workspace(&admin).await;
    let document = item(Kind::Document, "幂等文档", None, None);
    let id = document.id;
    let request = json!({"id":Id::new_v4(),"command":Command::Create{item:document}});
    let route = format!("{}/commands", path(workspace));
    let first = f.api("POST", &route, Some(&admin), request.clone()).await;
    assert_eq!(first.0, StatusCode::OK);
    let state = f.state(&admin, workspace).await;
    assert!(state.items[&id].created_at > 0);
    assert_eq!(
        state.items[&id].created_by,
        state
            .members
            .values()
            .find(|member| member.account == "owner")
            .unwrap()
            .id
    );
    f.server = Server::connect(&f.url, &f.files).await.unwrap();
    assert_eq!(
        f.api("POST", &route, Some(&admin), request.clone()).await,
        first
    );
    let mut altered = request.clone();
    altered["command"]["item"]["title"] = json!("不能覆盖原始请求");
    assert_eq!(
        f.api("POST", &route, Some(&admin), altered).await.0,
        StatusCode::CONFLICT
    );
    for key in ["actor_id", "at"] {
        let mut forged = request.clone();
        forged[key] = json!(0);
        assert_eq!(
            f.api("POST", &route, Some(&admin), forged).await.0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    let (a,b)=tokio::join!(f.api("POST",&route,Some(&admin),json!({"id":Id::new_v4(),"command":Command::Update{id,version:1,patch:Patch{title:Some("标题甲".into()),..Default::default()}}})),f.api("POST",&route,Some(&admin),json!({"id":Id::new_v4(),"command":Command::Update{id,version:1,patch:Patch{title:Some("标题乙".into()),..Default::default()}}})));
    assert_eq!(
        usize::from(a.0 == StatusCode::OK) + usize::from(b.0 == StatusCode::OK),
        1
    );
    assert!(a.0 == StatusCode::CONFLICT || b.0 == StatusCode::CONFLICT);
    let search = f
        .api(
            "GET",
            &format!(
                "{}/items?kind=document&text={}",
                path(workspace),
                utf8_percent_encode("标题", NON_ALPHANUMERIC)
            ),
            Some(&admin),
            Value::Null,
        )
        .await;
    assert_eq!(search.0, StatusCode::OK);
    assert_eq!(search.1["items"].as_array().unwrap().len(), 1);
    let before = f.state(&admin, workspace).await;
    let bad = Command::Batch {
        commands: vec![
            Command::Create {
                item: item(Kind::Document, "应回滚", None, None),
            },
            Command::Transition {
                id,
                version: 2,
                target: "invalid".into(),
                note: "失败".into(),
            },
        ],
    };
    assert_eq!(
        f.api(
            "POST",
            &route,
            Some(&admin),
            json!({"id":Id::new_v4(),"command":bad})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        serde_json::to_value(f.state(&admin, workspace).await).unwrap(),
        serde_json::to_value(before).unwrap()
    );
    let mut large = item(Kind::Document, "大于登录接口请求上限的正文", None, None);
    large.description = "中".repeat(20_000);
    f.insert(&admin, workspace, large).await;
    assert_eq!(f.api("POST",&route,Some(&admin),json!({"id":Id::new_v4(),"command":{"type":"create","item":{"description":"x".repeat(2*1024*1024)}}})).await.0,StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn account_disabling_and_concurrent_owner_changes_cannot_strand_workspaces() {
    let f = F::new().await;
    let admin = f.admin().await;
    let (first, owner) = f.ready(&admin, "first_owner", false).await;
    let (second, other) = f.ready(&admin, "second_owner", false).await;
    let workspace = f.workspace(&owner).await;
    let user_path = format!("/api/users/{}", first["id"].as_str().unwrap());
    let disable = json!({"version":2,"name":"首位所有者","department":"研发部","active":false,"is_admin":false});
    let blocked = f
        .api("PATCH", &user_path, Some(&admin), disable.clone())
        .await;
    assert_eq!(blocked.0, StatusCode::BAD_REQUEST);
    assert!(blocked.1["message"].as_str().unwrap().contains("所有者"));
    f.add(&owner, workspace, &second, &[Role::Owner]).await;
    let state = f.state(&owner, workspace).await;
    let second_id = Id::parse_str(second["id"].as_str().unwrap()).unwrap();
    let mut demoted = state.members[&second_id].clone();
    demoted.roles = BTreeSet::from([Role::Viewer]);
    let commands = format!("{}/commands", path(workspace));
    let (disabled,changed)=tokio::join!(f.api("PATCH",&user_path,Some(&admin),disable),f.api("POST",&commands,Some(&other),json!({"id":Id::new_v4(),"expected_revision":state.revision,"command":Command::UpdateMember{member:demoted}})));
    assert_eq!(
        usize::from(disabled.0 == StatusCode::OK) + usize::from(changed.0 == StatusCode::OK),
        1
    );
    let owners:i64=query_scalar("SELECT COUNT(*) FROM ff_workspace_members m JOIN ff_accounts a ON a.id=m.user_id WHERE m.workspace_id=$1 AND m.owner AND m.active AND a.active").bind(workspace).fetch_one(&f.pool).await.unwrap();
    assert_eq!(owners, 1);
    if disabled.0 == StatusCode::OK {
        assert_eq!(
            f.api("GET", &path(workspace), Some(&owner), Value::Null)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        let snapshot = f
            .api("GET", &path(workspace), Some(&other), Value::Null)
            .await
            .1;
        assert_eq!(
            snapshot["accounts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|account| account["id"] == first["id"])
                .unwrap()["active"],
            false
        );
    }
}

#[tokio::test]
async fn disabled_service_accounts_cannot_receive_work_or_mentions_and_viewers_cannot_mutate() {
    let f = F::new().await;
    let admin = f.admin().await;
    let workspace = f.workspace(&admin).await;
    let (user, token) = f.ready(&admin, "member", false).await;
    f.add(&admin, workspace, &user, &[Role::Viewer]).await;
    let document = f
        .insert(
            &admin,
            workspace,
            item(Kind::Document, "可读文档", None, None),
        )
        .await;
    let id = Id::parse_str(user["id"].as_str().unwrap()).unwrap();
    assert_eq!(
        f.api("GET", &path(workspace), Some(&token), Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        f.api(
            "GET",
            &format!("{}/people", path(workspace)),
            Some(&token),
            Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let route = format!("{}/commands", path(workspace));
    assert_eq!(f.api("POST",&route,Some(&token),json!({"id":Id::new_v4(),"command":Command::Create{item:item(Kind::Document,"越权",None,None)}})).await.0,StatusCode::FORBIDDEN);
    assert_eq!(f.api("PATCH",&format!("/api/users/{id}"),Some(&admin),json!({"version":2,"name":"停用成员","department":"研发部","active":false,"is_admin":false})).await.0,StatusCode::OK);
    let mut assigned = item(Kind::Document, "不能指派停用账号", None, None);
    assigned.assigned_to = Some(id);
    assert_eq!(
        f.api(
            "POST",
            &route,
            Some(&admin),
            json!({"id":Id::new_v4(),"command":Command::Create{item:assigned}})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(f.api("POST",&route,Some(&admin),json!({"id":Id::new_v4(),"command":Command::AddComment{id:Id::new_v4(),item_id:document,body:"不能提及已停用账户".into(),parent_id:None,mentions:BTreeSet::from([id]),attachments:BTreeSet::new()}})).await.0,StatusCode::BAD_REQUEST);
    let state = f.state(&admin, workspace).await;
    assert_eq!(f.api("POST",&route,Some(&admin),json!({"id":Id::new_v4(),"expected_revision":state.revision,"command":Command::UpdateMember{member:state.members[&id].clone()}})).await.0,StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn attachment_bytes_are_scoped_deduplicated_retryable_and_verified_before_download() {
    let f = F::new().await;
    let admin = f.admin().await;
    let workspace = f.workspace(&admin).await;
    let (viewer, token) = f.ready(&admin, "reader", false).await;
    f.add(&admin, workspace, &viewer, &[Role::Viewer]).await;
    let document = f
        .insert(
            &admin,
            workspace,
            item(Kind::Document, "附件文档", None, None),
        )
        .await;
    let attachment = Id::new_v4();
    let operation = Id::new_v4();
    let uri = upload_url(
        workspace,
        operation,
        attachment,
        document,
        "复测证据 研流.txt",
    );
    let bytes = "真实附件内容".as_bytes().to_vec();
    assert_eq!(
        f.raw("POST", &uri, None, bytes.clone()).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        f.raw("POST", &uri, Some(&token), bytes.clone()).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        f.raw("POST", &uri, Some(&admin), Vec::new()).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        f.raw("POST", &uri, Some(&admin), vec![0; 10 * 1024 * 1024 + 1])
            .await
            .0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let receipt = f.raw("POST", &uri, Some(&admin), bytes.clone()).await;
    assert_eq!(receipt.0, StatusCode::OK);
    assert_eq!(
        f.raw("POST", &uri, Some(&admin), bytes.clone()).await.2,
        receipt.2
    );
    let state = f.state(&admin, workspace).await;
    let metadata = state.attachments[&attachment].clone();
    let blob = f
        .files
        .join("workspaces")
        .join(workspace.to_string())
        .join("attachments")
        .join(&metadata.sha256[..2])
        .join(&metadata.sha256);
    fs::remove_file(&blob).unwrap();
    assert_eq!(
        f.raw("POST", &uri, Some(&admin), bytes.clone()).await.0,
        StatusCode::OK
    );
    assert!(blob.exists());
    let second = upload_url(
        workspace,
        Id::new_v4(),
        Id::new_v4(),
        document,
        "第二个文件名.txt",
    );
    assert_eq!(
        f.raw("POST", &second, Some(&admin), bytes.clone()).await.0,
        StatusCode::OK
    );
    assert_eq!(fs::read_dir(blob.parent().unwrap()).unwrap().count(), 1);
    let download = format!("{}/attachments/{attachment}", path(workspace));
    let loaded = f.raw("GET", &download, Some(&token), Vec::new()).await;
    assert_eq!(loaded.0, StatusCode::OK);
    assert_eq!(loaded.2, bytes);
    assert_eq!(loaded.1["content-type"], "application/octet-stream");
    assert!(loaded.1["content-disposition"]
        .to_str()
        .unwrap()
        .contains("filename*=UTF-8''"));
    let other = f.workspace(&admin).await;
    assert_eq!(
        f.raw(
            "GET",
            &format!("{}/attachments/{attachment}", path(other)),
            Some(&admin),
            Vec::new()
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.raw("POST", &uri, Some(&admin), b"different".to_vec())
            .await
            .0,
        StatusCode::CONFLICT
    );
    let forged = Command::Batch {
        commands: vec![Command::AddAttachment {
            id: Id::new_v4(),
            item_id: document,
            name: "伪造".into(),
            mime: "text/plain".into(),
            sha256: metadata.sha256.clone(),
            size: metadata.size,
        }],
    };
    assert_eq!(
        f.api(
            "POST",
            &format!("{}/commands", path(workspace)),
            Some(&admin),
            json!({"id":Id::new_v4(),"command":forged})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let maximum = vec![7; 10 * 1024 * 1024];
    let maximum_id = Id::new_v4();
    let maximum_url = upload_url(
        workspace,
        Id::new_v4(),
        maximum_id,
        document,
        "上限附件.bin",
    );
    assert_eq!(
        f.raw("POST", &maximum_url, Some(&admin), maximum.clone())
            .await
            .0,
        StatusCode::OK
    );
    let maximum_download = format!("{}/attachments/{maximum_id}", path(workspace));
    let downloaded = f
        .raw("GET", &maximum_download, Some(&admin), Vec::new())
        .await;
    assert_eq!(downloaded.0, StatusCode::OK);
    assert_eq!(downloaded.2, maximum);
    fs::write(&blob, b"corrupt").unwrap();
    assert_eq!(
        f.raw("GET", &download, Some(&admin), Vec::new()).await.0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(
        f.raw("POST", &uri, Some(&admin), bytes).await.0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
}

#[tokio::test]
async fn journal_failure_rolls_back_state_and_projection_corruption_is_detected() {
    let f = F::new().await;
    let admin = f.admin().await;
    let workspace = f.workspace(&admin).await;
    let state = f.state(&admin, workspace).await;
    query(
        "ALTER TABLE ff_workspace_operations ADD CONSTRAINT reject_operations CHECK (revision<0)",
    )
    .execute(&f.pool)
    .await
    .unwrap();
    let route = format!("{}/commands", path(workspace));
    let request = json!({"id":Id::new_v4(),"command":Command::Create{item:item(Kind::Document,"不能半次保存",None,None)}});
    assert_eq!(
        f.api("POST", &route, Some(&admin), request.clone()).await.0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(
        serde_json::to_value(f.state(&admin, workspace).await).unwrap(),
        serde_json::to_value(&state).unwrap()
    );
    query("ALTER TABLE ff_workspace_operations DROP CONSTRAINT reject_operations")
        .execute(&f.pool)
        .await
        .unwrap();
    assert_eq!(
        f.api("POST", &route, Some(&admin), request).await.0,
        StatusCode::OK
    );
    query("UPDATE ff_workspaces SET checksum='corrupt' WHERE id=$1")
        .bind(workspace)
        .execute(&f.pool)
        .await
        .unwrap();
    assert_eq!(
        f.api("GET", &path(workspace), Some(&admin), Value::Null)
            .await
            .0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
}

#[tokio::test]
async fn account_directory_uses_literal_search_and_cursor_pagination() {
    let f = F::new().await;
    let admin = f.admin().await;
    let workspace = f.workspace(&admin).await;
    let mut names: Vec<_> = (0..105).map(|index| format!("person_{index:03}")).collect();
    names.push("personxignore".into());
    let ids: Vec<_> = names.iter().map(|_| Id::new_v4()).collect();
    query("INSERT INTO ff_accounts(id,account,name,department,password_hash,active,is_admin,must_change_password,version) SELECT id,account,'目录成员','研发部',(SELECT password_hash FROM ff_accounts WHERE account='owner'),true,false,true,1 FROM UNNEST($1::uuid[],$2::text[]) AS seeded(id,account)")
        .bind(ids).bind(names).execute(&f.pool).await.unwrap();
    let uri = format!("{}/people?text=person_", path(workspace));
    let first = f.api("GET", &uri, Some(&admin), Value::Null).await;
    assert_eq!(first.0, StatusCode::OK);
    assert_eq!(first.1["accounts"].as_array().unwrap().len(), 100);
    let next = first.1["next"].as_str().unwrap();
    let second = f
        .api(
            "GET",
            &format!("{uri}&after={next}"),
            Some(&admin),
            Value::Null,
        )
        .await
        .1;
    assert_eq!(second["accounts"].as_array().unwrap().len(), 5);
    assert!(second["next"].is_null());
    let collected: BTreeSet<_> = first.1["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .chain(second["accounts"].as_array().unwrap())
        .map(|value| value["id"].as_str().unwrap())
        .collect();
    assert_eq!(collected.len(), 105);
    assert_eq!(
        f.api(
            "GET",
            &format!("{}/people?text={}", path(workspace), "x".repeat(121)),
            Some(&admin),
            Value::Null
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert!(f
        .api(
            "GET",
            &format!("{}/people?text=not-found", path(workspace)),
            Some(&admin),
            Value::Null
        )
        .await
        .1["accounts"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn attachment_database_failure_keeps_only_an_orphan_and_archives_or_links_block_downloads() {
    let f = F::new().await;
    let admin = f.admin().await;
    let workspace = f.workspace(&admin).await;
    let document = f
        .insert(
            &admin,
            workspace,
            item(Kind::Document, "附件事务", None, None),
        )
        .await;
    let attachment = Id::new_v4();
    let uri = upload_url(
        workspace,
        Id::new_v4(),
        attachment,
        document,
        "恢复证据.txt",
    );
    let bytes = b"recoverable attachment".to_vec();
    query("ALTER TABLE ff_workspace_operations ADD CONSTRAINT reject_new_operations CHECK (revision<0) NOT VALID").execute(&f.pool).await.unwrap();
    let before = f.state(&admin, workspace).await;
    assert_eq!(
        f.raw("POST", &uri, Some(&admin), bytes.clone()).await.0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(
        serde_json::to_value(f.state(&admin, workspace).await).unwrap(),
        serde_json::to_value(&before).unwrap()
    );
    query("ALTER TABLE ff_workspace_operations DROP CONSTRAINT reject_new_operations")
        .execute(&f.pool)
        .await
        .unwrap();
    assert_eq!(
        f.raw("POST", &uri, Some(&admin), bytes).await.0,
        StatusCode::OK
    );
    let state = f.state(&admin, workspace).await;
    assert_eq!(state.revision, before.revision + 1);
    let metadata = &state.attachments[&attachment];
    let blob = f
        .files
        .join("workspaces")
        .join(workspace.to_string())
        .join("attachments")
        .join(&metadata.sha256[..2])
        .join(&metadata.sha256);
    let download = format!("{}/attachments/{attachment}", path(workspace));
    f.command(
        &admin,
        workspace,
        Command::Archive {
            id: document,
            version: state.items[&document].version,
            note: "结束文档归档".into(),
        },
        None,
    )
    .await;
    assert_eq!(
        f.raw("GET", &download, Some(&admin), Vec::new()).await.0,
        StatusCode::NOT_FOUND
    );
    assert!(blob.exists());
    f.command(
        &admin,
        workspace,
        Command::Restore {
            id: document,
            version: f.state(&admin, workspace).await.items[&document].version,
        },
        None,
    )
    .await;
    assert_eq!(
        f.raw("GET", &download, Some(&admin), Vec::new()).await.0,
        StatusCode::OK
    );
    f.command(
        &admin,
        workspace,
        Command::RemoveAttachment { id: attachment },
        None,
    )
    .await;
    assert_eq!(
        f.raw("GET", &download, Some(&admin), Vec::new()).await.0,
        StatusCode::NOT_FOUND
    );
    assert!(blob.exists());
    #[cfg(unix)]
    {
        let other = f.workspace(&admin).await;
        let object = f
            .insert(&admin, other, item(Kind::Document, "路径保护", None, None))
            .await;
        let directory = f
            .files
            .join("workspaces")
            .join(other.to_string())
            .join("attachments");
        fs::remove_dir(&directory).unwrap();
        let outside = f.files.join("outside");
        fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, &directory).unwrap();
        let request = upload_url(
            other,
            Id::new_v4(),
            Id::new_v4(),
            object,
            "不能写入外部.txt",
        );
        assert_eq!(
            f.raw("POST", &request, Some(&admin), b"content".to_vec())
                .await
                .0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(fs::read_dir(outside).unwrap().count(), 0);
        assert!(f.state(&admin, other).await.attachments.is_empty());
    }
}

#[tokio::test]
async fn reused_actor_ids_and_damaged_journal_receipts_never_forge_success() {
    let f = F::new().await;
    let admin = f.admin().await;
    let workspace = f.workspace(&admin).await;
    let (dev, developer) = f.ready(&admin, "journal_reader", false).await;
    f.add(&admin, workspace, &dev, &[Role::Developer]).await;
    let op = Id::new_v4();
    let route = format!("{}/commands", path(workspace));
    let request =
        json!({"id":op,"command":Command::Create{item:item(Kind::Document,"原始操作",None,None)}});
    assert_eq!(
        f.api("POST", &route, Some(&admin), request.clone()).await.0,
        StatusCode::OK
    );
    assert_eq!(
        f.api("POST", &route, Some(&developer), request.clone())
            .await
            .0,
        StatusCode::CONFLICT
    );
    query("UPDATE ff_workspace_operations SET receipt=jsonb_set(receipt::jsonb,'{revision}','999')::text WHERE workspace_id=$1 AND id=$2").bind(workspace).bind(op).execute(&f.pool).await.unwrap();
    assert_eq!(
        f.api("POST", &route, Some(&admin), request.clone()).await.0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    query("DELETE FROM ff_workspace_operations WHERE workspace_id=$1 AND id=$2")
        .bind(workspace)
        .bind(op)
        .execute(&f.pool)
        .await
        .unwrap();
    assert_eq!(
        f.api("POST", &route, Some(&admin), request).await.0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    let state = f.state(&admin, workspace).await;
    let unknown = Member {
        id: Id::new_v4(),
        account: "unknown".into(),
        name: "未知账户".into(),
        department: "研发部".into(),
        active: true,
        roles: BTreeSet::from([Role::Viewer]),
    };
    assert_eq!(f.api("POST",&route,Some(&admin),json!({"id":Id::new_v4(),"expected_revision":state.revision,"command":Command::AddMember{member:unknown}})).await.0,StatusCode::NOT_FOUND);
    let mut assigned = item(Kind::Document, "不能引用其他空间成员", None, None);
    assigned.assigned_to = Some(Id::new_v4());
    assert_eq!(
        f.api(
            "POST",
            &route,
            Some(&admin),
            json!({"id":Id::new_v4(),"command":Command::Create{item:assigned}})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn exact_numeric_payloads_keep_the_same_core_fingerprint_after_storage() {
    let f = F::new().await;
    let admin = f.admin().await;
    let workspace = f.workspace(&admin).await;
    let mut document = item(Kind::Document, "数值边界", None, None);
    document
        .attributes
        .insert("custom_score".into(), json!(-0.0));
    document.description.push_str("\n第二行\t制表");
    let id = document.id;
    let route = format!("{}/commands", path(workspace));
    let request = json!({"id":Id::new_v4(),"command":Command::Create{item:document}});
    let saved = f.api("POST", &route, Some(&admin), request.clone()).await;
    assert_eq!(saved.0, StatusCode::OK, "{}", saved.1);
    let state = f.state(&admin, workspace).await;
    assert_eq!(
        serde_json::to_string(&state.items[&id].attributes["custom_score"]).unwrap(),
        "-0.0"
    );
    assert!(state.items[&id].description.ends_with("\n第二行\t制表"));
    assert_eq!(
        f.api("POST", &route, Some(&admin), request.clone()).await,
        saved
    );
    let mut changed = request;
    changed["command"]["item"]["attributes"]["custom_score"] = json!(0.0);
    assert_eq!(
        f.api("POST", &route, Some(&admin), changed).await.0,
        StatusCode::CONFLICT
    );
    query("ALTER TABLE ff_workspaces ALTER COLUMN genesis TYPE JSONB USING genesis::jsonb")
        .execute(&f.pool)
        .await
        .unwrap();
    assert!(Server::connect(&f.url, &f.files).await.is_err());
}

#[tokio::test]
async fn full_delivery_runs_through_service_roles_and_replays_from_postgres_after_reconnect() {
    let mut f = F::new().await;
    let owner = f.admin().await;
    let mut running = start_service(&mut f);
    let workspace = f.workspace(&owner).await;
    let (dev, developer) = f.ready(&owner, "dev_flow", false).await;
    let (tester_user, tester) = f.ready(&owner, "test_flow", false).await;
    f.add(&owner, workspace, &dev, &[Role::Developer]).await;
    f.add(&owner, workspace, &tester_user, &[Role::Tester])
        .await;
    let product = f
        .insert(&owner, workspace, item(Kind::Product, "研流", None, None))
        .await;
    let mut project = item(Kind::Project, "首版研发", None, None);
    project
        .attributes
        .insert("product_ids".into(), json!([product]));
    let project = f.insert(&owner, workspace, project).await;
    let iteration = f
        .insert(
            &owner,
            workspace,
            item(Kind::Iteration, "首轮迭代", None, Some(project)),
        )
        .await;
    let inbox = f
        .insert(
            &owner,
            workspace,
            item(Kind::Inbox, "收集初始化需求", Some(product), None),
        )
        .await;
    let requirement = Id::new_v4();
    f.command(
        &owner,
        workspace,
        Command::ConvertInbox {
            id: inbox,
            version: 1,
            new_id: requirement,
            kind: Kind::Requirement,
            acceptance: "完成初始化和重开验证".into(),
        },
        None,
    )
    .await;
    f.approve(&owner, workspace, requirement).await;
    let mut task = item(Kind::Task, "实现初始化", Some(product), Some(project));
    task.iteration_id = Some(iteration);
    task.assigned_to = Some(Id::parse_str(dev["id"].as_str().unwrap()).unwrap());
    task.links.insert(Link {
        target: requirement,
        relation: Relation::Implements,
        content_version: None,
    });
    let task = f.insert(&owner, workspace, task).await;
    f.transition(&developer, workspace, task, "doing").await;
    f.transition(&developer, workspace, task, "verify").await;
    assert_eq!(f.api("POST",&format!("{}/commands",path(workspace)),Some(&developer),json!({"id":Id::new_v4(),"command":Command::Transition{id:task,version:f.state(&owner,workspace).await.items[&task].version,target:"closed".into(),note:"不能开发自验收".into()}})).await.0,StatusCode::FORBIDDEN);
    f.transition(&tester, workspace, task, "closed").await;
    let mut case = item(Kind::TestCase, "初始化用例", Some(product), None);
    case.attributes.insert(
        "steps".into(),
        json!([{"action":"创建并重开工作空间","expected":"所有数据保留"}]),
    );
    case.links.insert(Link {
        target: requirement,
        relation: Relation::Verifies,
        content_version: None,
    });
    let case = f.insert(&tester, workspace, case).await;
    f.approve(&tester, workspace, case).await;
    let mut builds = Vec::new();
    for name in ["rc.1", "rc.2"] {
        let mut build = item(Kind::Build, name, Some(product), None);
        build.attributes = BTreeMap::from([
            ("version_name".into(), json!(name)),
            ("code_ref".into(), json!("commit-123")),
        ]);
        let build = f.insert(&developer, workspace, build).await;
        f.transition(&developer, workspace, build, "ready").await;
        builds.push(build);
    }
    let mut plans = Vec::new();
    for build in &builds {
        let mut plan = item(Kind::TestPlan, "测试单", Some(product), None);
        plan.attributes = BTreeMap::from([
            ("build_id".into(), json!(build)),
            ("case_ids".into(), json!([case])),
        ]);
        let plan = f.insert(&tester, workspace, plan).await;
        f.transition(&tester, workspace, plan, "doing").await;
        plans.push(plan);
    }
    let failed = Id::new_v4();
    f.command(
        &tester,
        workspace,
        Command::RunTest {
            id: failed,
            plan_id: plans[0],
            case_id: case,
            case_version: 1,
            result: "fail".into(),
            actual: "初始化失败".into(),
        },
        None,
    )
    .await;
    let bug = Id::new_v4();
    f.command(
        &tester,
        workspace,
        Command::BugFromTest {
            id: bug,
            run_id: failed,
            title: "初始化问题".into(),
            actual: "错误提示不清楚".into(),
            environment: "测试环境".into(),
        },
        None,
    )
    .await;
    f.transition(&developer, workspace, bug, "active").await;
    f.transition(&developer, workspace, bug, "doing").await;
    let version = f.state(&owner, workspace).await.items[&bug].version;
    f.command(
        &developer,
        workspace,
        Command::ResolveBug {
            id: bug,
            version,
            resolution: "fixed".into(),
            build_id: Some(builds[1]),
            duplicate_id: None,
            note: "已修复错误提示".into(),
        },
        None,
    )
    .await;
    let passed = Id::new_v4();
    f.command(
        &tester,
        workspace,
        Command::RunTest {
            id: passed,
            plan_id: plans[1],
            case_id: case,
            case_version: 1,
            result: "pass".into(),
            actual: "实际回归通过".into(),
        },
        None,
    )
    .await;
    let version = f.state(&owner, workspace).await.items[&bug].version;
    f.command(
        &tester,
        workspace,
        Command::RetestBug {
            id: bug,
            version,
            build_id: Some(builds[1]),
            run_id: Some(passed),
            result: "pass".into(),
            note: "复测通过".into(),
        },
        None,
    )
    .await;
    f.transition(&tester, workspace, plans[1], "done").await;
    let version = f.state(&owner, workspace).await.items[&requirement].version;
    f.command(
        &owner,
        workspace,
        Command::AcceptRequirement {
            id: requirement,
            version,
            accepted: true,
            note: "逐项产品验收通过".into(),
        },
        None,
    )
    .await;
    let mut release = item(Kind::Release, "首版发布", Some(product), None);
    release.attributes = BTreeMap::from([
        ("version_name".into(), json!("0.1.0")),
        ("build_ids".into(), json!([builds[1]])),
        ("test_plan_ids".into(), json!([plans[1]])),
    ]);
    for target in [requirement, bug] {
        release.links.insert(Link {
            target,
            relation: Relation::Contains,
            content_version: None,
        });
    }
    let release = f.insert(&owner, workspace, release).await;
    f.transition(&owner, workspace, release, "wait").await;
    let version = f.state(&owner, workspace).await.items[&release].version;
    f.command(
        &owner,
        workspace,
        Command::Publish {
            id: release,
            version,
            note: "核定范围后发布".into(),
        },
        None,
    )
    .await;
    let attachment = Id::new_v4();
    let url = upload_url(
        workspace,
        Id::new_v4(),
        attachment,
        requirement,
        "验收记录.txt",
    );
    assert_eq!(
        f.raw("POST", &url, Some(&owner), b"acceptance evidence".to_vec())
            .await
            .0,
        StatusCode::OK
    );
    let comment = Id::new_v4();
    f.command(
        &owner,
        workspace,
        Command::AddComment {
            id: comment,
            item_id: requirement,
            body: "验收证据已补充".into(),
            parent_id: None,
            mentions: BTreeSet::from([Id::parse_str(tester_user["id"].as_str().unwrap()).unwrap()]),
            attachments: BTreeSet::from([attachment]),
        },
        None,
    )
    .await;
    f.command(
        &tester,
        workspace,
        Command::AddComment {
            id: Id::new_v4(),
            item_id: requirement,
            body: "测试确认".into(),
            parent_id: Some(comment),
            mentions: BTreeSet::new(),
            attachments: BTreeSet::new(),
        },
        None,
    )
    .await;
    let before = f.state(&owner, workspace).await;
    let frozen = before.items[&release].attributes["snapshot"].clone();
    assert_eq!(before.stage(requirement).unwrap(), "released");
    f.command(
        &owner,
        workspace,
        Command::Update {
            id: requirement,
            version: before.items[&requirement].version,
            patch: Patch {
                description: Some("后续新需求版本".into()),
                ..Default::default()
            },
        },
        None,
    )
    .await;
    let final_state = f.state(&owner, workspace).await;
    assert_eq!(final_state.items[&release].attributes["snapshot"], frozen);
    assert_eq!(final_state.stage(requirement).unwrap(), "wait");
    stop_service(&mut running);
    let mut restarted = start_service(&mut f);
    assert_eq!(
        serde_json::to_value(f.state(&owner, workspace).await).unwrap(),
        serde_json::to_value(&final_state).unwrap()
    );
    let genesis: String = query_scalar("SELECT genesis FROM ff_workspaces WHERE id=$1")
        .bind(workspace)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    let mut replay: State = serde_json::from_str(&genesis).unwrap();
    let requests = query(
        "SELECT request FROM ff_workspace_operations WHERE workspace_id=$1 ORDER BY revision",
    )
    .bind(workspace)
    .fetch_all(&f.pool)
    .await
    .unwrap();
    for row in &requests {
        replay
            .execute(serde_json::from_str::<Request>(&row.get::<String, _>("request")).unwrap())
            .unwrap();
    }
    assert_eq!(
        serde_json::to_value(replay).unwrap(),
        serde_json::to_value(&final_state).unwrap()
    );
    let backup = f.files.with_extension("full-delivery-backup");
    f.server.backup(&owner, &backup).await.unwrap();
    let mut restored = F::new().await;
    Server::restore(&backup, &restored.url, &restored.files)
        .await
        .unwrap();
    assert_eq!(
        restored
            .api("GET", "/api/auth/me", Some(&owner), Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let restored_owner = restored.login("owner", PASSWORD).await;
    let mut restored_process = start_service(&mut restored);
    assert_eq!(
        serde_json::to_string(&restored.state(&restored_owner, workspace).await).unwrap(),
        serde_json::to_string(&final_state).unwrap()
    );
    assert_eq!(
        restored
            .raw(
                "GET",
                &format!("{}/attachments/{attachment}", path(workspace)),
                Some(&restored_owner),
                vec![]
            )
            .await
            .2,
        b"acceptance evidence"
    );
    restored
        .command(
            &restored_owner,
            workspace,
            Command::AddComment {
                id: Id::new_v4(),
                item_id: requirement,
                body: "恢复后继续协作".into(),
                parent_id: Some(comment),
                mentions: BTreeSet::new(),
                attachments: BTreeSet::new(),
            },
            None,
        )
        .await;
    let service_package = f.files.with_extension("three-mode-service-package");
    f.server
        .export_copy(&owner, workspace, &service_package)
        .await
        .unwrap();
    let git_path = f.files.with_extension("three-mode-git");
    let (git, _) =
        farflow_core::GitWorkspace::import_copy(&service_package, &git_path, Id::new_v4()).unwrap();
    let old_owner = Id::parse_str(
        f.api("GET", "/api/auth/me", Some(&owner), Value::Null)
            .await
            .1["id"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let git_package = f.files.with_extension("three-mode-git-package");
    git.export_copy(old_owner, &git_package).unwrap();
    let local_path = f.files.with_extension("three-mode-local");
    let (mut local, _) =
        farflow_core::LocalWorkspace::import_copy(&git_package, &local_path, Id::new_v4()).unwrap();
    local
        .execute(Request {
            id: Id::new_v4(),
            actor_id: old_owner,
            at: 1,
            command: Command::AddComment {
                id: Id::new_v4(),
                item_id: requirement,
                body: "三模式保留历史后继续".into(),
                parent_id: Some(comment),
                mentions: BTreeSet::new(),
                attachments: BTreeSet::new(),
            },
        })
        .unwrap();
    let local_state = local.snapshot(old_owner).unwrap();
    let local_package = f.files.with_extension("three-mode-local-package");
    local.export_copy(old_owner, &local_package).unwrap();
    let mut destination = F::new().await;
    let destination_admin = destination.admin().await;
    let real_owner = Id::parse_str(
        destination
            .api("GET", "/api/auth/me", Some(&destination_admin), Value::Null)
            .await
            .1["id"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let mut mappings = BTreeMap::from([(old_owner, real_owner)]);
    for member in local_state.members.values().filter(|m| m.id != old_owner) {
        let (user, _) = destination
            .ready(
                &destination_admin,
                &format!("migrated_{}", member.account),
                false,
            )
            .await;
        mappings.insert(
            member.id,
            Id::parse_str(user["id"].as_str().unwrap()).unwrap(),
        );
    }
    let destination_id = Id::new_v4();
    destination
        .server
        .import_copy(
            &destination_admin,
            &local_package,
            destination_id,
            mappings.clone(),
        )
        .await
        .unwrap();
    let mut destination_process = start_service(&mut destination);
    let mut migrated = destination.state(&destination_admin, destination_id).await;
    migrated.id = local_state.id;
    assert_eq!(
        serde_json::to_string(&migrated).unwrap(),
        serde_json::to_string(&local_state).unwrap()
    );
    assert_eq!(
        destination
            .raw(
                "GET",
                &format!("{}/attachments/{attachment}", path(destination_id)),
                Some(&destination_admin),
                vec![]
            )
            .await
            .2,
        b"acceptance evidence"
    );
    destination
        .command(
            &destination_admin,
            destination_id,
            Command::AddComment {
                id: Id::new_v4(),
                item_id: requirement,
                body: "新服务真实账户继续".into(),
                parent_id: Some(comment),
                mentions: BTreeSet::new(),
                attachments: BTreeSet::new(),
            },
            None,
        )
        .await;
    let destination_package = destination.files.with_extension("final-service-package");
    destination
        .server
        .export_copy(&destination_admin, destination_id, &destination_package)
        .await
        .unwrap();
    assert!(
        farflow_core::MigrationArchive::open(&destination_package)
            .unwrap()
            .copied_history(Id::new_v4())
            .unwrap()
            .clients
            .len()
            > requests.len()
    );
    let before_formal = f.state(&owner, workspace).await;
    let prepared = f
        .server
        .prepare_transfer(
            &owner,
            workspace,
            Id::new_v4(),
            farflow_core::Mode::Git,
            f.files.with_extension("released-before-formal"),
        )
        .await
        .unwrap();
    assert_eq!(f.api("POST",&format!("{}/commands",path(workspace)),Some(&owner),json!({"id":Id::new_v4(),"command":Command::AddComment{id:Id::new_v4(),item_id:requirement,body:"冻结后不能继续写".into(),parent_id:None,mentions:BTreeSet::new(),attachments:BTreeSet::new()}})).await.0,StatusCode::CONFLICT);
    let formal = f.files.with_extension("released-formal-package");
    f.server
        .export_transfer(&owner, workspace, &formal)
        .await
        .unwrap();
    let (formal_git, _, receipt) = farflow_core::GitWorkspace::stage_transfer(
        &formal,
        f.files.with_extension("released-formal-git"),
        old_owner,
        prepared.plan.prepared_at,
    )
    .unwrap();
    let proof = f
        .server
        .complete_transfer(&owner, workspace, &receipt)
        .await
        .unwrap();
    formal_git
        .activate_transfer(old_owner, prepared.plan.prepared_at + 1, &proof)
        .unwrap();
    let git_target_proof = proof.clone();
    assert_eq!(
        serde_json::to_string(&formal_git.snapshot(old_owner).unwrap().state).unwrap(),
        serde_json::to_string(&before_formal).unwrap()
    );
    formal_git
        .execute(Request {
            id: Id::new_v4(),
            actor_id: old_owner,
            at: prepared.plan.prepared_at + 2,
            command: Command::AddComment {
                id: Id::new_v4(),
                item_id: requirement,
                body: "正式 Git 交接后继续".into(),
                parent_id: None,
                mentions: BTreeSet::new(),
                attachments: BTreeSet::new(),
            },
        })
        .unwrap();
    assert_eq!(
        formal_git.read_attachment(old_owner, attachment).unwrap(),
        b"acceptance evidence"
    );
    let local_prepared = local
        .prepare_transfer(
            old_owner,
            Id::new_v4(),
            1_000_000,
            farflow_core::Mode::Service,
            f.files.with_extension("local-before-service-formal"),
        )
        .unwrap();
    let formal = f.files.with_extension("local-service-formal-package");
    local.export_transfer(old_owner, &formal).unwrap();
    let (report, receipt) = destination
        .server
        .stage_transfer(&destination_admin, &formal, mappings.clone())
        .await
        .unwrap();
    assert_eq!(report.target.unwrap().id, local_state.id);
    let staged = destination
        .api(
            "GET",
            &path(local_state.id),
            Some(&destination_admin),
            Value::Null,
        )
        .await;
    assert_eq!(staged.1["transfer"]["phase"], "staged");
    let proof = local
        .complete_transfer(old_owner, local_prepared.plan.prepared_at + 1, &receipt)
        .unwrap();
    destination
        .server
        .activate_transfer(&destination_admin, local_state.id, &proof)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_string(&destination.state(&destination_admin, local_state.id).await)
            .unwrap(),
        serde_json::to_string(&local_state).unwrap()
    );
    destination
        .command(
            &destination_admin,
            local_state.id,
            Command::AddComment {
                id: Id::new_v4(),
                item_id: requirement,
                body: "正式服务交接后真实 HTTP 续写".into(),
                parent_id: Some(comment),
                mentions: BTreeSet::new(),
                attachments: BTreeSet::new(),
            },
            None,
        )
        .await;
    assert_eq!(
        destination
            .state(&destination_admin, local_state.id)
            .await
            .items[&release]
            .attributes["snapshot"],
        local_state.items[&release].attributes["snapshot"]
    );
    let git_before = formal_git.snapshot(old_owner).unwrap().state;
    let reused_backup = destination
        .files
        .with_extension("reused-target-plan-backup");
    assert!(formal_git
        .prepare_transfer(
            old_owner,
            prepared.plan.id,
            2_000_000,
            farflow_core::Mode::Service,
            &reused_backup
        )
        .is_err());
    assert!(!reused_backup.exists());
    let git_plan = formal_git
        .prepare_transfer(
            old_owner,
            Id::new_v4(),
            2_000_000,
            farflow_core::Mode::Service,
            destination
                .files
                .with_extension("released-git-before-service"),
        )
        .unwrap();
    assert!(formal_git
        .activate_transfer(old_owner, prepared.plan.prepared_at + 5, &git_target_proof)
        .is_err());
    for member in git_before.members.values().filter(|m| {
        m.active
            && m.roles.iter().any(|r| {
                [
                    Role::Owner,
                    Role::Manager,
                    Role::Product,
                    Role::Developer,
                    Role::Tester,
                ]
                .contains(r)
            })
    }) {
        formal_git
            .acknowledge_transfer(
                member.id,
                git_plan.plan.prepared_at + 1,
                git_plan.plan.id,
                "已核对并停止本人全部副本",
            )
            .unwrap();
    }
    let git_formal = destination
        .files
        .with_extension("released-git-service-package");
    formal_git.export_transfer(old_owner, &git_formal).unwrap();
    let (report, receipt) = destination
        .server
        .stage_transfer(&destination_admin, &git_formal, mappings)
        .await
        .unwrap();
    assert_eq!(report.target.unwrap().id, workspace);
    assert_eq!(
        destination
            .api(
                "GET",
                &path(workspace),
                Some(&destination_admin),
                Value::Null
            )
            .await
            .1["transfer"]["phase"],
        "staged"
    );
    let git_proof = formal_git
        .complete_transfer(old_owner, git_plan.plan.prepared_at + 2, &receipt)
        .unwrap();
    destination
        .server
        .activate_transfer(&destination_admin, workspace, &git_proof)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_string(&destination.state(&destination_admin, workspace).await).unwrap(),
        serde_json::to_string(&git_before).unwrap()
    );
    destination
        .command(
            &destination_admin,
            workspace,
            Command::AddComment {
                id: Id::new_v4(),
                item_id: requirement,
                body: "Git 正式交接后真实 HTTP 续写".into(),
                parent_id: Some(comment),
                mentions: BTreeSet::new(),
                attachments: BTreeSet::new(),
            },
            None,
        )
        .await;
    assert_eq!(
        destination.state(&destination_admin, workspace).await.items[&release].attributes
            ["snapshot"],
        git_before.items[&release].attributes["snapshot"]
    );
    assert_eq!(
        destination
            .raw(
                "GET",
                &format!("{}/attachments/{attachment}", path(workspace)),
                Some(&destination_admin),
                vec![]
            )
            .await
            .2,
        b"acceptance evidence"
    );
    let before_local = destination.state(&destination_admin, local_state.id).await;
    let to_local = destination
        .server
        .prepare_transfer(
            &destination_admin,
            local_state.id,
            Id::new_v4(),
            farflow_core::Mode::Local,
            destination.files.with_extension("released-before-local"),
        )
        .await
        .unwrap();
    let local_package = destination
        .files
        .with_extension("released-formal-local-package");
    destination
        .server
        .export_transfer(&destination_admin, local_state.id, &local_package)
        .await
        .unwrap();
    let local_destination = destination.files.with_extension("released-formal-local");
    let (mut offline, _, receipt) = farflow_core::LocalWorkspace::stage_transfer(
        &local_package,
        &local_destination,
        old_owner,
        to_local.plan.prepared_at,
    )
    .unwrap();
    assert_eq!(
        serde_json::to_string(&offline.snapshot(old_owner).unwrap()).unwrap(),
        serde_json::to_string(&before_local).unwrap()
    );
    assert!(offline
        .execute(Request {
            id: Id::new_v4(),
            actor_id: old_owner,
            at: to_local.plan.prepared_at,
            command: Command::AddComment {
                id: Id::new_v4(),
                item_id: requirement,
                body: "暂存不能提前写".into(),
                parent_id: None,
                mentions: BTreeSet::new(),
                attachments: BTreeSet::new()
            }
        })
        .is_err());
    let proof = destination
        .server
        .complete_transfer(&destination_admin, local_state.id, &receipt)
        .await
        .unwrap();
    assert_eq!(destination.api("POST",&format!("{}/commands",path(local_state.id)),Some(&destination_admin),json!({"id":Id::new_v4(),"command":Command::AddComment{id:Id::new_v4(),item_id:requirement,body:"已交接服务不能再写".into(),parent_id:None,mentions:BTreeSet::new(),attachments:BTreeSet::new()}})).await.0,StatusCode::CONFLICT);
    offline
        .activate_transfer(old_owner, to_local.plan.prepared_at + 1, &proof)
        .unwrap();
    stop_service(&mut destination_process);
    offline
        .execute(Request {
            id: Id::new_v4(),
            actor_id: old_owner,
            at: to_local.plan.prepared_at + 2,
            command: Command::AddComment {
                id: Id::new_v4(),
                item_id: requirement,
                body: "停掉服务后本地继续".into(),
                parent_id: Some(comment),
                mentions: BTreeSet::new(),
                attachments: BTreeSet::new(),
            },
        })
        .unwrap();
    assert_eq!(
        offline.read_attachment(old_owner, attachment).unwrap(),
        b"acceptance evidence"
    );
    assert_eq!(
        offline.snapshot(old_owner).unwrap().items[&release].attributes["snapshot"],
        before_local.items[&release].attributes["snapshot"]
    );
    let offline_backup = destination.files.with_extension("released-local-backup");
    offline.backup(old_owner, &offline_backup).unwrap();
    let restored_local = farflow_core::LocalWorkspace::restore(
        &offline_backup,
        destination.files.with_extension("released-local-restored"),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_string(&restored_local.snapshot(old_owner).unwrap()).unwrap(),
        serde_json::to_string(&offline.snapshot(old_owner).unwrap()).unwrap()
    );
    stop_service(&mut restored_process);
    stop_service(&mut restarted);
}
