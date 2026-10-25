use farflow_core::{Command, GitTransport, GitWorkspace, Id, Member, Mode, Role};
use farflow_desktop::{Desktop, Profile};
use serde_json::json;
use std::{collections::BTreeSet, fs, process::Command as Process};

fn profile() -> Profile {
    Profile {
        account: "owner".into(),
        name: "创建者".into(),
        department: "研发".into(),
    }
}

#[test]
fn clone_destinations_require_grants_new_names_and_preserve_cancelled_or_existing_paths() {
    let temp = tempfile::tempdir().unwrap();
    let mut host = Desktop::default();
    let directory = host.select_directory(temp.path()).unwrap();
    assert!(host.git_destination(Id::new_v4(), "data").is_err());
    for name in ["../data", "/tmp/data", "CON", "a/b", ""] {
        assert!(host.git_destination(directory.token, name).is_err());
    }
    fs::create_dir(temp.path().join("existing")).unwrap();
    assert!(host.git_destination(directory.token, "existing").is_err());
    let target = host.git_destination(directory.token, "cancelled").unwrap();
    let control = GitTransport::default();
    control.cancel();
    assert!(target.clone_remote("/未访问的远端.git", &control).is_err());
    assert!(!temp.path().join("cancelled").exists());
    let target = host.git_destination(directory.token, "race").unwrap();
    fs::create_dir(temp.path().join("race")).unwrap();
    fs::write(temp.path().join("race/保留.txt"), "已有文件").unwrap();
    assert!(target.clone_remote("", &GitTransport::default()).is_err());
    assert_eq!(
        fs::read_to_string(temp.path().join("race/保留.txt")).unwrap(),
        "已有文件"
    );
    assert!(host
        .git_destination(directory.token, "invalid")
        .unwrap()
        .clone_remote("", &GitTransport::default())
        .is_err());
    assert!(!temp.path().join("invalid").exists());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("missing", temp.path().join("link")).unwrap();
        assert!(host.git_destination(directory.token, "link").is_err());
    }
}

#[test]
fn real_remote_clones_preserve_members_and_require_identity_before_business_access() {
    let temp = tempfile::tempdir().unwrap();
    let mut host = Desktop::default();
    let directory = host.select_directory(temp.path()).unwrap();
    let source = host
        .create(
            directory.token,
            "source",
            Mode::Git,
            "已有研发空间".into(),
            profile(),
        )
        .unwrap();
    let owner = source.identities[0].clone();
    host.select_identity(source.handle, owner.id).unwrap();
    let replacement = Member {
        id: Id::new_v4(),
        account: "replacement".into(),
        name: "现任所有者".into(),
        department: "研发".into(),
        roles: BTreeSet::from([Role::Owner]),
        active: true,
    };
    host.execute(
        source.handle,
        Id::new_v4(),
        Command::AddMember {
            member: replacement.clone(),
        },
        0,
        100,
    )
    .unwrap();
    let item = Id::new_v4();
    host.execute(source.handle,Id::new_v4(),serde_json::from_value(json!({"type":"create","item":{"id":item,"kind":"document","title":"克隆后可读取的文档"}})).unwrap(),1,100).unwrap();
    host.select_identity(source.handle, replacement.id).unwrap();
    host.execute(
        source.handle,
        Id::new_v4(),
        Command::UpdateMember {
            member: Member {
                active: false,
                ..owner.clone()
            },
        },
        2,
        101,
    )
    .unwrap();
    let remote = temp.path().join("remote.git");
    assert!(Process::new("git")
        .args(["init", "--bare", "--quiet", "--initial-branch=main"])
        .arg(&remote)
        .status()
        .unwrap()
        .success());
    let source_git = GitWorkspace::open(temp.path().join("source")).unwrap();
    source_git
        .push(replacement.id, remote.to_str().unwrap())
        .unwrap();
    let cloned = host
        .git_destination(directory.token, "clone")
        .unwrap()
        .clone_remote(remote.to_str().unwrap(), &GitTransport::default())
        .unwrap();
    let opened = host.open_cloned(&cloned).unwrap();
    assert_eq!(opened.workspace, source.workspace);
    assert_eq!(opened.identities.len(), 2);
    assert!(
        !opened
            .identities
            .iter()
            .find(|member| member.id == owner.id)
            .unwrap()
            .active
    );
    assert!(host.snapshot(opened.handle).is_err());
    assert!(host.select_identity(opened.handle, owner.id).is_err());
    let data = host.select_identity(opened.handle, replacement.id).unwrap();
    assert_eq!(data.actor_id, replacement.id);
    assert_eq!(data.state.items[&item].title, "克隆后可读取的文档");
    assert_eq!(data.state.members.len(), 2);
    assert!(Desktop::default().open_cloned(&cloned).is_err());
    fs::rename(temp.path().join("clone"), temp.path().join("kept-clone")).unwrap();
    GitWorkspace::create(
        temp.path().join("clone"),
        Id::new_v4(),
        "其他空间".into(),
        replacement,
    )
    .unwrap();
    assert!(host
        .open_cloned(&cloned)
        .err()
        .unwrap()
        .to_string()
        .contains("已被替换"));
}

#[cfg(unix)]
#[test]
fn prepared_clone_destinations_refuse_parent_symlink_replacement_before_network() {
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().join("parent");
    fs::create_dir(&parent).unwrap();
    let mut host = Desktop::default();
    let directory = host.select_directory(&parent).unwrap();
    let target = host.git_destination(directory.token, "clone").unwrap();
    fs::rename(&parent, temp.path().join("original")).unwrap();
    std::os::unix::fs::symlink(temp.path().join("original"), &parent).unwrap();
    assert!(target
        .clone_remote(
            "https://invalid.example/不应访问.git",
            &GitTransport::default()
        )
        .is_err());
    assert!(!parent.join("clone").exists());
}
