use farflow_core::*;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, Stdio};
use std::time::{Duration, Instant};

struct F {
    directory: tempfile::TempDir,
    workspace: GitWorkspace,
    owner: Member,
    remote: PathBuf,
    document: Id,
}
fn member(account: &str, role: Role) -> Member {
    Member {
        id: Id::new_v4(),
        account: account.into(),
        name: account.into(),
        department: "研发".into(),
        roles: BTreeSet::from([role]),
        active: true,
    }
}
fn document(title: &str) -> NewItem {
    serde_json::from_value(json!({"id":Id::new_v4(),"kind":"document","title":title})).unwrap()
}
fn request(actor: Id, command: Command) -> Request {
    Request {
        id: Id::new_v4(),
        actor_id: actor,
        at: 100,
        command,
    }
}
fn value(v: &impl serde::Serialize) -> Value {
    serde_json::to_value(v).unwrap()
}
fn git(root: &Path, args: &[&str]) -> String {
    let output = Process::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
impl F {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let owner = member("owner", Role::Owner);
        let workspace = GitWorkspace::create(
            directory.path().join("work"),
            Id::new_v4(),
            "远端协作测试".into(),
            owner.clone(),
        )
        .unwrap();
        let item = document("基础内容");
        let id = item.id;
        workspace
            .execute(request(owner.id, Command::Create { item }))
            .unwrap();
        let remote = directory.path().join("remote.git");
        fs::create_dir(&remote).unwrap();
        git(
            &remote,
            &["init", "--bare", "--quiet", "--initial-branch=main"],
        );
        Self {
            directory,
            workspace,
            owner,
            remote,
            document: id,
        }
    }
    fn url(&self) -> &str {
        self.remote.to_str().unwrap()
    }
    fn add(&self, workspace: &GitWorkspace, title: &str) -> Id {
        let item = document(title);
        let id = item.id;
        workspace
            .execute(request(self.owner.id, Command::Create { item }))
            .unwrap();
        id
    }
    fn fork(&self, name: &str) -> GitWorkspace {
        GitWorkspace::from_remote(self.url(), self.directory.path().join(name)).unwrap()
    }
}

#[test]
fn empty_remote_sync_clones_and_roundtrips_typed_results() {
    let f = F::new();
    let sync = f.workspace.sync(f.owner.id, f.url()).unwrap();
    assert!(sync.push.pushed);
    assert!(!sync.pending_local);
    assert!(sync.push.failure.is_none());
    assert_eq!(sync.view.state.items.len(), 1);
    let head = git(&f.remote, &["rev-parse", "refs/heads/main"]);
    assert_eq!(head.trim(), sync.push.commit);
    let decoded: GitSync = serde_json::from_value(value(&sync)).unwrap();
    assert!(decoded.push.pushed);
    let decoded: GitPush = serde_json::from_value(value(&sync.push)).unwrap();
    assert_eq!(decoded.commit, sync.push.commit);
    let copy = f.fork("copy");
    assert_eq!(
        value(&copy.snapshot(f.owner.id).unwrap()),
        value(&sync.view)
    );
    assert_eq!(copy.info().mode, Mode::Git);
}

#[test]
fn cancelled_transport_rejects_every_entry_before_mutation_and_fresh_retry_succeeds() {
    let f = F::new();
    let before = value(&f.workspace.snapshot(f.owner.id).unwrap());
    let control = GitTransport::default();
    control.cancel();
    let destination = f.directory.path().join("cancelled-copy");
    assert!(GitWorkspace::from_remote_with_control(f.url(), &destination, &control).is_err());
    assert!(!destination.exists());
    for error in [
        f.workspace
            .pull_with_control(f.owner.id, f.url(), &control)
            .unwrap_err(),
        f.workspace
            .push_with_control(f.owner.id, f.url(), &control)
            .unwrap_err(),
        f.workspace
            .sync_with_control(f.owner.id, f.url(), &control)
            .unwrap_err(),
    ] {
        assert!(error.to_string().contains("已取消"));
    }
    assert_eq!(value(&f.workspace.snapshot(f.owner.id).unwrap()), before);
    assert!(
        f.workspace
            .sync_with_control(f.owner.id, f.url(), &GitTransport::default())
            .unwrap()
            .push
            .pushed
    );
    let copy =
        GitWorkspace::from_remote_with_control(f.url(), &destination, &GitTransport::default())
            .unwrap();
    assert_eq!(value(&copy.snapshot(f.owner.id).unwrap()), before);
    assert!(f
        .workspace
        .pull_with_control(f.owner.id, f.url(), &GitTransport::default())
        .unwrap()
        .issues
        .is_empty());
}

#[test]
fn plain_git_clones_restore_empty_directories_without_hiding_missing_committed_data() {
    let temp = tempfile::tempdir().unwrap();
    let owner = member("owner", Role::Owner);
    let source = GitWorkspace::create(
        temp.path().join("source"),
        Id::new_v4(),
        "空空间".into(),
        owner.clone(),
    )
    .unwrap();
    let remote = temp.path().join("remote.git");
    fs::create_dir(&remote).unwrap();
    git(
        &remote,
        &["init", "--bare", "--quiet", "--initial-branch=main"],
    );
    source.push(owner.id, remote.to_str().unwrap()).unwrap();
    let copy = temp.path().join("copy");
    assert!(Process::new("git")
        .args(["clone", "--quiet", "--branch", "main"])
        .arg(&remote)
        .arg(&copy)
        .status()
        .unwrap()
        .success());
    assert!(!copy.join("operations").exists());
    assert!(!copy.join("attachments").exists());
    let opened = GitWorkspace::open(&copy).unwrap();
    assert_eq!(opened.snapshot(owner.id).unwrap().state.revision, 0);
    let item = document("普通克隆后可以续写");
    let item_id = item.id;
    opened
        .execute(request(owner.id, Command::Create { item }))
        .unwrap();
    let attachment = Id::new_v4();
    opened
        .upload(
            Upload {
                operation_id: Id::new_v4(),
                actor_id: owner.id,
                at: 100,
                id: attachment,
                item_id,
                name: "附件.txt".into(),
                mime: "text/plain".into(),
            },
            b"real bytes",
        )
        .unwrap();
    opened.push(owner.id, remote.to_str().unwrap()).unwrap();
    for name in ["operations", "attachments"] {
        let path = temp.path().join(format!("missing-{name}"));
        assert!(Process::new("git")
            .args(["clone", "--quiet", "--branch", "main"])
            .arg(&remote)
            .arg(&path)
            .status()
            .unwrap()
            .success());
        fs::remove_dir_all(path.join(name)).unwrap();
        assert!(GitWorkspace::open(&path).is_err());
        #[cfg(unix)]
        {
            fs::remove_dir(path.join(name)).unwrap();
            std::os::unix::fs::symlink(temp.path().join("copy").join(name), path.join(name))
                .unwrap();
            assert!(GitWorkspace::open(&path).is_err());
        }
    }
}

fn stalled_http() -> (
    String,
    std::sync::mpsc::Receiver<()>,
    std::thread::JoinHandle<()>,
) {
    use std::io::Read;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("http://{}/repo.git", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let (sent, connected) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let until = Instant::now() + Duration::from_secs(3);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < until =>
                {
                    std::thread::sleep(Duration::from_millis(10))
                }
                other => panic!("没有收到本机 Git 请求：{other:?}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut bytes = [0; 4096];
        assert!(stream.read(&mut bytes).unwrap() > 0);
        let _ = sent.send(());
        loop {
            match stream.read(&mut bytes) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error)
                    if [
                        std::io::ErrorKind::ConnectionReset,
                        std::io::ErrorKind::ConnectionAborted,
                    ]
                    .contains(&error.kind()) =>
                {
                    break
                }
                Err(error) => panic!("Git 取消后连接未及时释放：{error}"),
            }
        }
    });
    (address, connected, server)
}

#[test]
fn real_stalled_http_times_out_or_cancels_and_preserves_business_for_retry() {
    let f = F::new();
    let before = value(&f.workspace.snapshot(f.owner.id).unwrap());
    let (address, _, server) = stalled_http();
    let destination = f.directory.path().join("timed-out-copy");
    let at = Instant::now();
    let error = GitWorkspace::from_remote_with_control(
        &address,
        &destination,
        &GitTransport::new(Duration::from_millis(400)).unwrap(),
    )
    .err()
    .unwrap();
    assert!(
        error.to_string().contains("已超时"),
        "{error}，已耗时 {:?}",
        at.elapsed()
    );
    assert!(at.elapsed() < Duration::from_secs(2));
    server.join().unwrap();
    assert!(!destination.exists());
    let (address, connected, server) = stalled_http();
    let control = GitTransport::default();
    std::thread::scope(|scope| {
        let task = scope.spawn(|| {
            f.workspace
                .sync_with_control(f.owner.id, &address, &control)
        });
        let reached = connected.recv_timeout(Duration::from_secs(3));
        control.cancel();
        assert!(task
            .join()
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("已取消"));
        reached.unwrap();
    });
    server.join().unwrap();
    let (address, _, server) = stalled_http();
    assert!(f
        .workspace
        .push_with_control(
            f.owner.id,
            &address,
            &GitTransport::new(Duration::from_millis(400)).unwrap()
        )
        .unwrap_err()
        .to_string()
        .contains("已超时"));
    server.join().unwrap();
    assert_eq!(value(&f.workspace.snapshot(f.owner.id).unwrap()), before);
    assert!(f.workspace.sync(f.owner.id, f.url()).unwrap().push.pushed);
}

#[test]
#[cfg(unix)]
fn cancelled_upload_keeps_the_local_commit_and_can_be_retried_after_remote_confirmation() {
    let f = F::new();
    f.workspace.sync(f.owner.id, f.url()).unwrap();
    let remote_before = git(&f.remote, &["rev-parse", "refs/heads/main"]);
    let item = f.add(&f.workspace, "取消期间仍保留的业务");
    let marker = f.directory.path().join("receiving");
    let hook = f.remote.join("hooks/pre-receive");
    executable_hook(
        &hook,
        &format!(
            "#!/bin/sh\nprintf started > {}\nsleep 30\n",
            shell_path(&marker)
        ),
    );
    let control = GitTransport::default();
    std::thread::scope(|scope| {
        let task = scope.spawn(|| f.workspace.push_with_control(f.owner.id, f.url(), &control));
        let until = Instant::now() + Duration::from_secs(3);
        while !marker.exists() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        control.cancel();
        assert!(task
            .join()
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("远端结果"));
        assert!(marker.exists());
    });
    assert_eq!(
        git(&f.remote, &["rev-parse", "refs/heads/main"]),
        remote_before
    );
    assert!(f
        .workspace
        .snapshot(f.owner.id)
        .unwrap()
        .state
        .items
        .contains_key(&item));
    fs::remove_file(hook).unwrap();
    assert!(f.workspace.sync(f.owner.id, f.url()).unwrap().push.pushed);
    assert!(f
        .fork("retried-copy")
        .snapshot(f.owner.id)
        .unwrap()
        .state
        .items
        .contains_key(&item));
}

#[test]
fn two_clients_pull_merge_push_and_receive_real_attachment_bytes() {
    let f = F::new();
    f.workspace.sync(f.owner.id, f.url()).unwrap();
    let other = f.fork("other");
    let left = f.add(&f.workspace, "第一位离线新增");
    let right = f.add(&other, "第二位离线新增");
    let upload = Upload {
        operation_id: Id::new_v4(),
        actor_id: f.owner.id,
        at: 101,
        id: Id::new_v4(),
        item_id: f.document,
        name: "联网证据.txt".into(),
        mime: "text/plain".into(),
    };
    other
        .upload(upload.clone(), b"real synchronized content")
        .unwrap();
    f.workspace.sync(f.owner.id, f.url()).unwrap();
    let merged = other.sync(f.owner.id, f.url()).unwrap();
    assert!(merged.push.pushed);
    assert!(merged.view.state.items.contains_key(&left));
    assert!(merged.view.state.items.contains_key(&right));
    let view = f.workspace.pull(f.owner.id, f.url()).unwrap();
    assert_eq!(value(&view), value(&merged.view));
    assert_eq!(
        f.workspace.read_attachment(f.owner.id, upload.id).unwrap(),
        b"real synchronized content"
    );
    let refs = git(
        &f.directory.path().join("work"),
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/farflow/incoming",
        ],
    );
    assert!(refs.trim().is_empty());
}

#[test]
fn non_fast_forward_push_preserves_both_sides_until_sync_merges_them() {
    let f = F::new();
    f.workspace.sync(f.owner.id, f.url()).unwrap();
    let other = f.fork("other");
    let a = f.add(&f.workspace, "远端先到的记录");
    let b = f.add(&other, "本机待重试的记录");
    let first = f.workspace.sync(f.owner.id, f.url()).unwrap();
    let rejected = other.push(f.owner.id, f.url()).unwrap();
    assert!(!rejected.pushed);
    assert_eq!(rejected.failure, Some(GitPushFailure::NonFastForward));
    assert_eq!(
        git(&f.remote, &["rev-parse", "refs/heads/main"]).trim(),
        first.push.commit
    );
    assert!(other
        .snapshot(f.owner.id)
        .unwrap()
        .state
        .items
        .contains_key(&b));
    let retried = other.sync(f.owner.id, f.url()).unwrap();
    assert!(retried.push.pushed);
    assert!(retried.view.state.items.contains_key(&a));
    assert!(retried.view.state.items.contains_key(&b));
    assert!(!retried.pending_local);
}

#[test]
fn remote_unavailable_is_not_reported_as_uploaded_and_retry_succeeds() {
    let f = F::new();
    f.workspace.sync(f.owner.id, f.url()).unwrap();
    let missing = f.directory.path().join("not-a-repo");
    fs::create_dir(&missing).unwrap();
    let id = f.add(&f.workspace, "断连前已保存");
    let outcome = f
        .workspace
        .push(f.owner.id, missing.to_str().unwrap())
        .unwrap();
    assert!(!outcome.pushed);
    assert_eq!(outcome.failure, Some(GitPushFailure::Transport));
    assert!(outcome.warning.unwrap().contains("本机数据保留"));
    assert_eq!(
        git(&f.directory.path().join("work"), &["rev-parse", "HEAD"]).trim(),
        outcome.commit
    );
    assert!(f
        .workspace
        .pull(f.owner.id, missing.to_str().unwrap())
        .is_err());
    assert!(f
        .workspace
        .snapshot(f.owner.id)
        .unwrap()
        .state
        .items
        .contains_key(&id));
    assert!(f.workspace.sync(f.owner.id, f.url()).unwrap().push.pushed);
}

#[test]
fn conflicting_remote_updates_sync_the_original_records_and_require_a_decision() {
    let f = F::new();
    f.workspace.sync(f.owner.id, f.url()).unwrap();
    let other = f.fork("other");
    for (workspace, title) in [(&f.workspace, "一方标题"), (&other, "另一方标题")] {
        workspace
            .execute(request(
                f.owner.id,
                Command::Update {
                    id: f.document,
                    version: 1,
                    patch: Patch {
                        title: Some(title.into()),
                        ..Default::default()
                    },
                },
            ))
            .unwrap();
    }
    f.workspace.sync(f.owner.id, f.url()).unwrap();
    let merged = other.sync(f.owner.id, f.url()).unwrap();
    assert!(merged.push.pushed);
    assert_eq!(merged.view.issues.len(), 2);
    assert_eq!(merged.view.state.items[&f.document].title, "基础内容");
    let decision = GitResolution {
        id: Id::new_v4(),
        actor_id: f.owner.id,
        at: 200,
        targets: merged.view.issues.keys().copied().collect(),
        command: Some(Command::Update {
            id: f.document,
            version: 1,
            patch: Patch {
                title: Some("核定标题".into()),
                ..Default::default()
            },
        }),
        note: "核对远端及本机的原始提案".into(),
    };
    other.resolve(decision).unwrap();
    other.sync(f.owner.id, f.url()).unwrap();
    let view = f.workspace.pull(f.owner.id, f.url()).unwrap();
    assert!(view.issues.is_empty());
    assert_eq!(view.state.items[&f.document].title, "核定标题");
}

#[test]
fn different_workspace_remote_is_rejected_without_changing_local_business() {
    let f = F::new();
    let other = F::new();
    other.workspace.sync(other.owner.id, other.url()).unwrap();
    let before = value(&f.workspace.snapshot(f.owner.id).unwrap());
    assert!(f.workspace.pull(f.owner.id, other.url()).is_err());
    assert_eq!(value(&f.workspace.snapshot(f.owner.id).unwrap()), before);
}

#[test]
fn member_permissions_and_inline_credential_validation_apply_before_network_mutation() {
    let f = F::new();
    let viewer = member("viewer", Role::Viewer);
    f.workspace
        .execute(request(
            f.owner.id,
            Command::AddMember {
                member: viewer.clone(),
            },
        ))
        .unwrap();
    assert!(f.workspace.push(viewer.id, f.url()).is_err());
    assert!(f.workspace.sync(viewer.id, f.url()).is_err());
    assert!(f.workspace.pull(Id::new_v4(), f.url()).is_err());
    let before = value(&f.workspace.snapshot(f.owner.id).unwrap());
    let error = f
        .workspace
        .push(
            f.owner.id,
            "https://user:private-token@example.test/data.git",
        )
        .unwrap_err();
    assert!(!error.to_string().contains("private-token"));
    assert_eq!(value(&f.workspace.snapshot(f.owner.id).unwrap()), before);
}

struct Daemon {
    child: Child,
    url: String,
}
impl Daemon {
    fn start(f: &F) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let executable = PathBuf::from(git(f.directory.path(), &["--exec-path"]).trim()).join(
            if cfg!(windows) {
                "git-daemon.exe"
            } else {
                "git-daemon"
            },
        );
        let mut child = Process::new(executable)
            .args([
                "--reuseaddr",
                "--export-all",
                "--enable=receive-pack",
                "--listen=127.0.0.1",
                &format!("--port={port}"),
                &format!("--base-path={}", f.directory.path().display()),
            ])
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            assert!(child.try_wait().unwrap().is_none(), "Git 测试端点意外退出");
            assert!(Instant::now() < deadline, "Git 测试端点启动超时");
            std::thread::sleep(Duration::from_millis(20));
        }
        Self {
            child,
            url: format!("git://127.0.0.1:{port}/remote.git"),
        }
    }
}

#[cfg(unix)]
fn http_endpoint(f: &F) -> Daemon {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let backend =
        PathBuf::from(git(f.directory.path(), &["--exec-path"]).trim()).join("git-http-backend");
    let script = f.directory.path().join("git_http.py");
    fs::write(&script, include_str!("fixtures/git_http.py")).unwrap();
    let python = Process::new("python3")
        .args(["-c", "import sys; print(sys.executable)"])
        .output()
        .unwrap();
    assert!(python.status.success());
    let executable = String::from_utf8(python.stdout).unwrap();
    let mut child = Process::new(executable.trim())
        .arg(script)
        .arg(f.directory.path())
        .arg(backend)
        .arg(port.to_string())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        assert!(child.try_wait().unwrap().is_none(), "HTTP 测试入口意外退出");
        assert!(Instant::now() < deadline, "HTTP 测试入口启动超时");
        std::thread::sleep(Duration::from_millis(20));
    }
    Daemon {
        child,
        url: format!("http://127.0.0.1:{port}/remote.git"),
    }
}

#[cfg(unix)]
#[test]
fn http_authentication_uses_repository_credential_helper_and_redacts_failures() {
    let f = F::new();
    let server = http_endpoint(&f);
    let denied = f.workspace.push(f.owner.id, &server.url).unwrap();
    assert!(!denied.pushed);
    assert_eq!(denied.failure, Some(GitPushFailure::Authentication));
    assert!(!denied.warning.unwrap().contains("only-test-password"));
    git(
        &f.directory.path().join("work"),
        &[
            "config",
            "credential.helper",
            "!f() { echo username=ff-test; echo password=only-test-password; }; f",
        ],
    );
    let uploaded = f.workspace.sync(f.owner.id, &server.url).unwrap();
    assert!(uploaded.push.pushed);
    f.add(&f.workspace, "HTTP 认证后继续保存");
    assert!(
        f.workspace
            .sync(f.owner.id, &server.url)
            .unwrap()
            .push
            .pushed
    );
    assert!(!value(&f.workspace.snapshot(f.owner.id).unwrap())
        .to_string()
        .contains("only-test-password"));
}

#[cfg(unix)]
#[test]
fn remote_bootstrap_reuses_system_credential_helper_without_global_changes() {
    if let Some(url) = std::env::var_os("FARFLOW_TEST_BOOTSTRAP_URL") {
        let destination =
            PathBuf::from(std::env::var_os("FARFLOW_TEST_BOOTSTRAP_DESTINATION").unwrap());
        let actor = Id::parse_str(&std::env::var("FARFLOW_TEST_BOOTSTRAP_ACTOR").unwrap()).unwrap();
        let copy = GitWorkspace::from_remote(url.to_str().unwrap(), destination).unwrap();
        assert_eq!(copy.snapshot(actor).unwrap().state.items.len(), 1);
        return;
    }
    let f = F::new();
    let server = http_endpoint(&f);
    git(
        &f.directory.path().join("work"),
        &[
            "config",
            "credential.helper",
            "!f() { echo username=ff-test; echo password=only-test-password; }; f",
        ],
    );
    assert!(
        f.workspace
            .sync(f.owner.id, &server.url)
            .unwrap()
            .push
            .pushed
    );
    let config = f.directory.path().join("test-system.gitconfig");
    fs::write(&config,"[credential]\n helper = \"!f() { echo username=ff-test; echo password=only-test-password; }; f\"\n").unwrap();
    let output = Process::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "remote_bootstrap_reuses_system_credential_helper_without_global_changes",
        ])
        .env("FARFLOW_TEST_BOOTSTRAP_URL", &server.url)
        .env(
            "FARFLOW_TEST_BOOTSTRAP_DESTINATION",
            f.directory.path().join("authenticated-copy"),
        )
        .env("FARFLOW_TEST_BOOTSTRAP_ACTOR", f.owner.id.to_string())
        .env("GIT_CONFIG_GLOBAL", config)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn file_remote_urls_delegate_percent_encoded_paths_to_git() {
    let f = F::new();
    let remote = f.directory.path().join("remote with spaces.git");
    fs::rename(&f.remote, &remote).unwrap();
    let url = format!(
        "file://{}",
        remote.display().to_string().replace(' ', "%20")
    );
    assert!(f.workspace.sync(f.owner.id, &url).unwrap().push.pushed);
    let copy = GitWorkspace::from_remote(&url, f.directory.path().join("encoded-copy")).unwrap();
    assert_eq!(copy.snapshot(f.owner.id).unwrap().state.items.len(), 1);
}

#[cfg(unix)]
fn shell_path(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
}

#[cfg(unix)]
fn executable_hook(path: &Path, script: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::write(path, script).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
#[test]
fn sync_refetches_and_merges_a_remote_change_that_arrives_during_upload() {
    let f = F::new();
    f.workspace.sync(f.owner.id, f.url()).unwrap();
    let competitor = f.fork("competitor");
    let foreign = f.add(&competitor, "并发到达的远端需求");
    let foreign_commit = competitor.commit(f.owner.id, "并发远端提交").unwrap();
    git(
        &f.remote,
        &[
            "fetch",
            "--quiet",
            f.directory.path().join("competitor").to_str().unwrap(),
            "HEAD:refs/testing/competing",
        ],
    );
    let local = f.add(&f.workspace, "上传中的本机需求");
    let marker = f.directory.path().join("remote-advanced");
    let script=format!("#!/bin/sh\nif test ! -f {}; then\n touch {}\n env -u GIT_QUARANTINE_PATH -u GIT_OBJECT_DIRECTORY -u GIT_ALTERNATE_OBJECT_DIRECTORIES git -C {} update-ref refs/heads/main {} || exit 1\nfi\nexit 0\n",shell_path(&marker),shell_path(&marker),shell_path(&f.remote),foreign_commit);
    executable_hook(&f.remote.join("hooks/pre-receive"), &script);
    let server = Daemon::start(&f);
    let synced = f.workspace.sync(f.owner.id, &server.url).unwrap();
    assert!(marker.exists());
    assert!(synced.push.pushed, "{:?}", synced.push);
    assert!(synced.view.state.items.contains_key(&local));
    assert!(synced.view.state.items.contains_key(&foreign));
    assert_eq!(
        git(&f.remote, &["rev-parse", "refs/heads/main"]).trim(),
        synced.push.commit
    );
}

#[cfg(unix)]
#[test]
fn continuous_remote_changes_stop_after_the_bounded_retry_and_keep_local_data() {
    let f = F::new();
    f.workspace.sync(f.owner.id, f.url()).unwrap();
    let local = f.add(&f.workspace, "需要后续重试的本机数据");
    let counter = f.directory.path().join("attempts");
    let script=format!("#!/bin/sh\nn=0\nif test -f {}; then n=$(cat {}); fi\nn=$((n+1))\nprintf '%s' \"$n\" > {}\nunset GIT_QUARANTINE_PATH GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES\nold=$(git -C {} rev-parse refs/heads/main) || exit 1\ntree=$(git -C {} rev-parse \"$old^{{tree}}\") || exit 1\nnew=$(printf '并发记录\\n' | git -C {} -c user.name=Test -c user.email=test@localhost commit-tree \"$tree\" -p \"$old\") || exit 1\ngit -C {} update-ref refs/heads/main \"$new\" \"$old\" || exit 1\nexit 0\n",shell_path(&counter),shell_path(&counter),shell_path(&counter),shell_path(&f.remote),shell_path(&f.remote),shell_path(&f.remote),shell_path(&f.remote));
    executable_hook(&f.remote.join("hooks/pre-receive"), &script);
    let server = Daemon::start(&f);
    let result = f.workspace.sync(f.owner.id, &server.url).unwrap();
    assert!(!result.push.pushed);
    assert_eq!(result.push.failure, Some(GitPushFailure::NonFastForward));
    assert_eq!(fs::read_to_string(counter).unwrap(), "2");
    assert!(result.view.state.items.contains_key(&local));
    assert_ne!(
        git(&f.remote, &["rev-parse", "refs/heads/main"]).trim(),
        result.push.commit
    );
}

#[cfg(unix)]
#[test]
fn local_writes_can_continue_during_upload_and_are_reported_as_pending() {
    use std::sync::Arc;
    let f = F::new();
    let marker = f.directory.path().join("upload-started");
    let resume = f.directory.path().join("resume-upload");
    let script=format!("#!/bin/sh\ntouch {}\nn=0\nwhile test ! -f {}; do\n n=$((n+1))\n if test \"$n\" -gt 200; then exit 1; fi\n sleep 0.02\ndone\nexit 0\n",shell_path(&marker),shell_path(&resume));
    executable_hook(&f.remote.join("hooks/pre-receive"), &script);
    let server = Daemon::start(&f);
    let owner = f.owner.id;
    let workspace = Arc::new(GitWorkspace::open(f.directory.path().join("work")).unwrap());
    let worker = workspace.clone();
    let url = server.url.clone();
    let handle = std::thread::spawn(move || worker.sync(owner, &url));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !marker.exists() {
        assert!(Instant::now() < deadline, "上传没有进入远端接收阶段");
        std::thread::sleep(Duration::from_millis(20));
    }
    let item = document("上传期间继续新增");
    let id = item.id;
    workspace
        .execute(request(owner, Command::Create { item }))
        .unwrap();
    fs::write(resume, b"continue").unwrap();
    let result = handle.join().unwrap().unwrap();
    assert!(result.push.pushed);
    assert!(result.pending_local);
    assert!(result.view.state.items.contains_key(&id));
    let remote =
        GitWorkspace::from_remote(&server.url, f.directory.path().join("received")).unwrap();
    assert!(!remote
        .snapshot(owner)
        .unwrap()
        .state
        .items
        .contains_key(&id));
}
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn real_tcp_git_endpoint_supports_sync_clone_and_disconnection_recovery() {
    let f = F::new();
    let server = Daemon::start(&f);
    let url = server.url.clone();
    let first = f.workspace.sync(f.owner.id, &url).unwrap();
    assert!(first.push.pushed);
    let copy = GitWorkspace::from_remote(&url, f.directory.path().join("network-copy")).unwrap();
    f.add(&copy, "真实 TCP 端点新增");
    assert!(copy.sync(f.owner.id, &url).unwrap().push.pushed);
    assert_eq!(
        f.workspace
            .pull(f.owner.id, &url)
            .unwrap()
            .state
            .items
            .len(),
        2
    );
    drop(server);
    f.add(&f.workspace, "断连期间本机继续保存");
    let failed = f.workspace.push(f.owner.id, &url).unwrap();
    assert!(!failed.pushed);
    assert_eq!(failed.failure, Some(GitPushFailure::Transport));
    assert_eq!(
        f.workspace.snapshot(f.owner.id).unwrap().state.items.len(),
        3
    );
}

#[cfg(unix)]
#[test]
fn remote_policy_rejection_retains_the_commit_and_reports_the_correct_failure() {
    use std::os::unix::fs::PermissionsExt;
    let f = F::new();
    let hook = f.remote.join("hooks/pre-receive");
    fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    let server = Daemon::start(&f);
    let outcome = f.workspace.push(f.owner.id, &server.url).unwrap();
    assert!(!outcome.pushed);
    assert_eq!(outcome.failure, Some(GitPushFailure::Rejected));
    assert_eq!(
        f.workspace.snapshot(f.owner.id).unwrap().state.items.len(),
        1
    );
}
