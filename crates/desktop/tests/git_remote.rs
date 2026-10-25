use farflow_core::{Command, GitTransport, GitWorkspace, Id, Member, Mode, Role};
use farflow_desktop::{Desktop, GitRemoteAction, Profile};
use std::{collections::BTreeSet, fs, process::Command as Process};

fn profile() -> Profile {
    Profile {
        account: "owner".into(),
        name: "同步成员".into(),
        department: "研发".into(),
    }
}

#[test]
fn git_sessions_reuse_real_remote_operations_and_keep_local_mode_separate() {
    let temp = tempfile::tempdir().unwrap();
    let mut host = Desktop::default();
    let directory = host.select_directory(temp.path()).unwrap();
    let local = host
        .create(
            directory.token,
            "local",
            Mode::Local,
            "本地".into(),
            profile(),
        )
        .unwrap();
    assert!(host.git_session(local.handle).is_err());
    assert!(host.git_session(Id::new_v4()).is_err());
    let opened = host
        .create(
            directory.token,
            "git",
            Mode::Git,
            "联网协作".into(),
            profile(),
        )
        .unwrap();
    assert!(host.git_session(opened.handle).is_err());
    let actor = opened.identities[0].id;
    host.select_identity(opened.handle, actor).unwrap();
    let session = host.git_session(opened.handle).unwrap();
    let remote = temp.path().join("remote.git");
    fs::create_dir(&remote).unwrap();
    assert!(Process::new("git")
        .arg("-C")
        .arg(&remote)
        .args(["init", "--bare", "--quiet", "--initial-branch=main"])
        .status()
        .unwrap()
        .success());
    let result = session
        .remote(
            GitRemoteAction::Sync,
            remote.to_str().unwrap(),
            &GitTransport::default(),
        )
        .unwrap();
    assert!(result.push.unwrap().pushed);
    assert_eq!(result.pending_local, Some(false));
    let result = session
        .remote(
            GitRemoteAction::Push,
            remote.to_str().unwrap(),
            &GitTransport::default(),
        )
        .unwrap();
    assert!(result.push.unwrap().pushed);
    assert_eq!(result.pending_local, None);
    let result = session
        .remote(
            GitRemoteAction::Pull,
            remote.to_str().unwrap(),
            &GitTransport::default(),
        )
        .unwrap();
    assert!(result.push.is_none());
    assert_eq!(result.pending_local, None);
    let cancelled = GitTransport::default();
    cancelled.cancel();
    assert!(session
        .remote(GitRemoteAction::Sync, remote.to_str().unwrap(), &cancelled)
        .is_err());
    let viewer = Member {
        id: Id::new_v4(),
        account: "viewer".into(),
        name: "只读成员".into(),
        department: "研发".into(),
        roles: BTreeSet::from([Role::Viewer]),
        active: true,
    };
    host.execute(
        opened.handle,
        Id::new_v4(),
        Command::AddMember {
            member: viewer.clone(),
        },
        0,
        1792900000,
    )
    .unwrap();
    host.select_identity(opened.handle, viewer.id).unwrap();
    let viewer_session = host.git_session(opened.handle).unwrap();
    assert!(viewer_session
        .remote(
            GitRemoteAction::Push,
            remote.to_str().unwrap(),
            &GitTransport::default()
        )
        .is_err());
    assert!(viewer_session
        .remote(
            GitRemoteAction::Sync,
            remote.to_str().unwrap(),
            &GitTransport::default()
        )
        .is_err());
    assert!(viewer_session
        .remote(
            GitRemoteAction::Pull,
            remote.to_str().unwrap(),
            &GitTransport::default()
        )
        .is_ok());
}

#[test]
fn prepared_git_sessions_refuse_a_replaced_directory_before_network_access() {
    let temp = tempfile::tempdir().unwrap();
    let mut host = Desktop::default();
    let directory = host.select_directory(temp.path()).unwrap();
    let opened = host
        .create(
            directory.token,
            "git",
            Mode::Git,
            "原空间".into(),
            profile(),
        )
        .unwrap();
    host.select_identity(opened.handle, opened.identities[0].id)
        .unwrap();
    let session = host.git_session(opened.handle).unwrap();
    fs::rename(temp.path().join("git"), temp.path().join("old-git")).unwrap();
    GitWorkspace::create(
        temp.path().join("git"),
        Id::new_v4(),
        "其他空间".into(),
        opened.identities[0].clone(),
    )
    .unwrap();
    assert!(session
        .remote(
            GitRemoteAction::Sync,
            "https://invalid.example/repo.git",
            &GitTransport::default()
        )
        .err()
        .unwrap()
        .to_string()
        .contains("已被替换"));
}
