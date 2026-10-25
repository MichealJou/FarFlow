use super::{
    args, commit_files, extract_bundle, extract_fetched, git, git_process, invalid, load, member,
    storage, tree, working_objects, writable, GitTransport, GitWorkspace,
};
use crate::{Error, GitView, Id, Result};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::process::Output;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitPushFailure {
    NonFastForward,
    Authentication,
    Transport,
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitPush {
    pub commit: String,
    pub pushed: bool,
    pub failure: Option<GitPushFailure>,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitSync {
    pub view: GitView,
    pub push: GitPush,
    pub pending_local: bool,
}

impl GitWorkspace {
    pub fn from_remote(remote: &str, destination: impl AsRef<Path>) -> Result<Self> {
        Self::from_remote_with_control(remote, destination, &GitTransport::default())
    }

    pub fn from_remote_with_control(
        remote: &str,
        destination: impl AsRef<Path>,
        control: &GitTransport,
    ) -> Result<Self> {
        control.check()?;
        let remote = validate_remote(remote)?;
        let extracted = fetch_remote(None, &remote, control)?;
        control.check()?;
        Self::from_extracted(extracted.path(), destination.as_ref())
    }

    pub fn pull(&self, actor: Id, remote: &str) -> Result<GitView> {
        self.pull_with_control(actor, remote, &GitTransport::default())
    }

    pub fn pull_with_control(
        &self,
        actor: Id,
        remote: &str,
        control: &GitTransport,
    ) -> Result<GitView> {
        control.check()?;
        let remote = validate_remote(remote)?;
        member(&self.snapshot(actor)?.state, actor)?;
        let extracted = fetch_remote(Some(self), &remote, control)?;
        control.check()?;
        self.import_extracted(actor, extracted.path())
    }

    pub fn push(&self, actor: Id, remote: &str) -> Result<GitPush> {
        self.push_with_control(actor, remote, &GitTransport::default())
    }

    pub fn push_with_control(
        &self,
        actor: Id,
        remote: &str,
        control: &GitTransport,
    ) -> Result<GitPush> {
        control.check()?;
        let remote = validate_remote(remote)?;
        let commit = self.locked(|| {
            let loaded = load(&self.root, true)?;
            loaded.ensure_transport()?;
            writable(&loaded.log.view()?.state, actor)?;
            commit_files(&self.root, &loaded.files, "FarFlow 远端同步")
        })?;
        let output = network(
            &self.root,
            &[
                "push".into(),
                "--porcelain".into(),
                "--".into(),
                remote,
                format!("{commit}:refs/heads/main").into(),
            ],
            control,
        )?;
        if output.status.success() {
            return Ok(GitPush {
                commit,
                pushed: true,
                failure: None,
                warning: None,
            });
        }
        let failure = classify(&output);
        Ok(GitPush {
            commit,
            pushed: false,
            failure: Some(failure),
            warning: Some(message(failure).into()),
        })
    }

    pub fn sync(&self, actor: Id, remote: &str) -> Result<GitSync> {
        self.sync_with_control(actor, remote, &GitTransport::default())
    }

    pub fn sync_with_control(
        &self,
        actor: Id,
        remote: &str,
        control: &GitTransport,
    ) -> Result<GitSync> {
        control.check()?;
        let address = validate_remote(remote)?;
        writable(&self.snapshot(actor)?.state, actor)?;
        for attempt in 0..2 {
            let output = network(
                &self.root,
                &[
                    "ls-remote".into(),
                    "--heads".into(),
                    "--".into(),
                    address.clone(),
                    "refs/heads/main".into(),
                ],
                control,
            )?;
            remote_success(&output)?;
            if !output.stdout.is_empty() {
                self.pull_with_control(actor, remote, control)?;
            }
            let push = self.push_with_control(actor, remote, control)?;
            if push.failure == Some(GitPushFailure::NonFastForward) && attempt == 0 {
                continue;
            }
            return self.locked(|| {
                let loaded = load(&self.root, true)?;
                let view = loaded.log.view()?;
                member(&view.state, actor)?;
                let pending_local =
                    working_objects(&self.root, &loaded.files)? != tree(&self.root, &push.commit)?;
                Ok(GitSync {
                    view,
                    push,
                    pending_local,
                })
            });
        }
        unreachable!("有界重试在第二次返回结果")
    }
}

fn validate_remote(remote: &str) -> Result<OsString> {
    if remote.is_empty() || remote.len() > 4096 || remote.chars().any(|c| c.is_control()) {
        return invalid("Git 地址为空、过长或含控制字符");
    }
    if Path::new(remote).exists() || Path::new(remote).is_absolute() {
        return Ok(fs::canonicalize(remote).map_err(storage)?.into_os_string());
    }
    if remote.contains(['?', '#']) {
        return invalid("Git 地址不能携带查询令牌或片段");
    }
    if let Some((scheme, rest)) = remote.split_once("://") {
        if scheme == "file" {
            if !rest.starts_with('/') && !rest.starts_with("localhost/") {
                return invalid("本机 Git 文件地址必须是绝对路径");
            }
            return Ok(remote.into());
        }
        if !["https", "http", "ssh", "git"].contains(&scheme) {
            return invalid("Git 地址协议不受支持");
        }
        let authority = rest.split('/').next().unwrap_or("");
        if authority.is_empty() || authority.chars().any(char::is_whitespace) {
            return invalid("Git 服务主机无效");
        }
        if let Some((user, host)) = authority.rsplit_once('@') {
            if scheme != "ssh" || user.is_empty() || user.contains([':', '@']) || host.is_empty() {
                return invalid("Git 地址不能内嵌密码或访问令牌，请使用系统凭据助手");
            }
        }
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        if host.starts_with('-')
            || !host
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b".-_:[]%".contains(&c))
        {
            return invalid("Git 服务主机名不符合规范");
        }
        if rest.chars().any(char::is_whitespace) {
            return invalid("Git 地址中的空格需先进行 URL 编码");
        }
        return Ok(remote.into());
    }
    if let Some((host, path)) = remote.split_once(':') {
        let (user, host) = host
            .split_once('@')
            .map_or((None, host), |(u, h)| (Some(u), h));
        if user.is_none_or(|u| {
            !u.is_empty()
                && u.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
        }) && !host.is_empty()
            && !host.starts_with('-')
            && host
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
            && !path.is_empty()
            && !path.starts_with(':')
            && !path.chars().any(char::is_whitespace)
        {
            return Ok(remote.into());
        }
    }
    invalid("请填写有效的 HTTPS、SSH 或本机 Git 仓库地址")
}

fn network(root: &Path, arguments: &[OsString], control: &GitTransport) -> Result<Output> {
    // 认证仅委托系统 Git；错误文本不回传原始输出，防止凭据助手泄漏令牌。
    super::transport::run(git_process(root, arguments, false, None, true), control)
}

fn remote_success(output: &Output) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let failure = classify(output);
    if failure == GitPushFailure::Authentication {
        Err(Error::Forbidden(message(failure).into()))
    } else {
        Err(Error::Storage(message(failure).into()))
    }
}

fn fetch_remote(
    workspace: Option<&GitWorkspace>,
    remote: &OsString,
    control: &GitTransport,
) -> Result<tempfile::TempDir> {
    if let Some(workspace) = workspace {
        let reference = format!("refs/farflow/incoming/{}", Id::new_v4());
        let result = (|| {
            let output = network(
                &workspace.root,
                &[
                    "fetch".into(),
                    "--quiet".into(),
                    "--no-tags".into(),
                    "--no-write-fetch-head".into(),
                    "--".into(),
                    remote.clone(),
                    format!("refs/heads/main:{reference}").into(),
                ],
                control,
            )?;
            remote_success(&output)?;
            let temporary =
                tempfile::tempdir_in(workspace.root.join(".git/farflow")).map_err(storage)?;
            let bundle = temporary.path().join("incoming.bundle");
            git(
                &workspace.root,
                &[
                    "bundle".into(),
                    "create".into(),
                    bundle.as_os_str().to_owned(),
                    reference.clone().into(),
                ],
                None,
                None,
            )?;
            extract_bundle(&bundle, &reference)
        })();
        let _ = git(
            &workspace.root,
            &["update-ref".into(), "-d".into(), reference.into()],
            None,
            None,
        );
        return result;
    }
    let mut builder = tempfile::Builder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    let stage = builder.tempdir().map_err(storage)?;
    let bare = stage.path().join("objects.git");
    fs::create_dir(&bare).map_err(storage)?;
    git(
        &bare,
        &args(&["init", "--bare", "--quiet", "--object-format=sha1"]),
        None,
        None,
    )?;
    let output = network(
        &bare,
        &[
            "fetch".into(),
            "--quiet".into(),
            "--no-tags".into(),
            "--".into(),
            remote.clone(),
            "refs/heads/main".into(),
        ],
        control,
    )?;
    remote_success(&output)?;
    extract_fetched(stage)
}

fn classify(output: &Output) -> GitPushFailure {
    let text = format!(
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
    .to_lowercase();
    if ["non-fast-forward", "fetch first", "stale info"]
        .iter()
        .any(|s| text.contains(s))
        || (text.contains("cannot lock ref") && text.contains("expected"))
    {
        GitPushFailure::NonFastForward
    } else if [
        "authentication failed",
        "permission denied",
        "could not read username",
        "terminal prompts disabled",
        "no such identity",
        "returned error: 401",
        "returned error: 403",
    ]
    .iter()
    .any(|s| text.contains(s))
    {
        GitPushFailure::Authentication
    } else if ["remote rejected", "hook declined", "pre-receive"]
        .iter()
        .any(|s| text.contains(s))
    {
        GitPushFailure::Rejected
    } else {
        GitPushFailure::Transport
    }
}

fn message(failure: GitPushFailure) -> &'static str {
    match failure {
        GitPushFailure::NonFastForward => {
            "本机已提交，远端已有新记录，请先获取合并后重试；不会强制覆盖"
        }
        GitPushFailure::Authentication => {
            "本机数据保留，Git 认证失败，请检查系统凭据助手或 SSH 配置"
        }
        GitPushFailure::Transport => {
            "本机数据保留，无法完成 Git 远端操作，请检查地址、网络或访问权限"
        }
        GitPushFailure::Rejected => "本机已提交，远端策略拒绝上传，请核对仓库权限或保护规则",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn remote_address_validation_accepts_supported_forms_and_rejects_embedded_secrets() {
        for address in [
            "https://example.test/team/data.git",
            "http://localhost:3000/data.git",
            "ssh://git@example.test:2222/data.git",
            "git@example.test:team/data.git",
            "git://127.0.0.1:1234/data.git",
        ] {
            assert!(validate_remote(address).is_ok(), "{address}");
        }
        for address in [
            "",
            "unknown",
            "https://",
            "ssh://@example.test/x",
            "https://user:token@example.test/data.git",
            "ssh://user:token@example.test/data.git",
            "https://example.test/data.git?token=secret",
            "https://example.test/data.git#secret",
            "ext::sh -c evil",
            "https://a b/data.git",
            "https://example.test/data repo.git",
            "git@:repo",
            "\0",
            "file://relative/data",
        ] {
            assert!(validate_remote(address).is_err());
        }
    }
    #[cfg(unix)]
    #[test]
    fn error_classification_redacts_raw_credentials_and_preserves_useful_categories() {
        use std::os::unix::process::ExitStatusExt;
        for (text, expected) in [
            ("fetch first", GitPushFailure::NonFastForward),
            (
                "Authentication failed: secret-token",
                GitPushFailure::Authentication,
            ),
            ("remote rejected: private detail", GitPushFailure::Rejected),
            ("connection refused", GitPushFailure::Transport),
        ] {
            let output = Output {
                status: std::process::ExitStatus::from_raw(256),
                stdout: Vec::new(),
                stderr: text.as_bytes().to_vec(),
            };
            assert_eq!(classify(&output), expected);
            assert!(!message(expected).contains("secret-token"));
            assert!(remote_success(&output).is_err());
        }
    }
}
