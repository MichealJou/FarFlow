use farflow_core::{Id, Mode};
use farflow_desktop::{AttachmentInput, Desktop, Profile};
use serde_json::json;
use std::fs;

fn profile() -> Profile {
    Profile {
        account: "owner".into(),
        name: "文件包所有者".into(),
        department: "研发".into(),
    }
}

#[test]
fn offline_packs_preserve_business_attachments_members_and_allow_idempotent_roundtrips() {
    let temp = tempfile::tempdir().unwrap();
    let mut host = Desktop::default();
    let directory = host.select_directory(temp.path()).unwrap();
    let opened = host
        .create(
            directory.token,
            "source",
            Mode::Git,
            "离线协作".into(),
            profile(),
        )
        .unwrap();
    let owner = opened.identities[0].id;
    host.select_identity(opened.handle, owner).unwrap();
    let item = Id::new_v4();
    host.execute(
        opened.handle,
        Id::new_v4(),
        serde_json::from_value(
            json!({"type":"create","item":{"id":item,"kind":"document","title":"完整文件包往返"}}),
        )
        .unwrap(),
        0,
        100,
    )
    .unwrap();
    let attachment = Id::new_v4();
    let bytes = "离线附件原始字节".as_bytes();
    host.upload(
        opened.handle,
        AttachmentInput {
            operation_id: Id::new_v4(),
            id: attachment,
            item_id: item,
            name: "说明.txt".into(),
            mime: "text/plain".into(),
        },
        bytes,
        101,
    )
    .unwrap();
    let comment = Id::new_v4();
    host.execute(opened.handle,Id::new_v4(),serde_json::from_value(json!({"type":"add_comment","id":comment,"item_id":item,"body":"离线评论保留","parent_id":null,"mentions":[],"attachments":[attachment]})).unwrap(),2,102).unwrap();
    let viewer = Id::new_v4();
    host.execute(opened.handle,Id::new_v4(),serde_json::from_value(json!({"type":"add_member","member":{"id":viewer,"account":"viewer","name":"只读成员","department":"研发","roles":["viewer"],"active":true}})).unwrap(),3,103).unwrap();
    let original = host.snapshot(opened.handle).unwrap();
    let session = host.git_session(opened.handle).unwrap();
    let pack = temp.path().join("original.farflowpack");
    let result = session.export_pack(&pack).unwrap();
    assert_eq!(serde_json::to_value(&result).unwrap()["action"], "export");
    assert!(result.bytes.unwrap() > 0);
    assert!(result.issues.is_none());
    assert!(host.pack_source(directory.token).is_err());
    assert!(host.select_pack(temp.path()).is_err());
    let selected = host.select_pack(&pack).unwrap();
    assert_eq!(selected.path, fs::canonicalize(&pack).unwrap());
    let created = host
        .git_destination(directory.token, "copy")
        .unwrap()
        .clone_pack(host.pack_source(selected.token).unwrap())
        .unwrap();
    let copy = host.open_cloned(&created).unwrap();
    assert_eq!(copy.workspace, opened.workspace);
    assert!(host.snapshot(copy.handle).is_err());
    let state = host.select_identity(copy.handle, owner).unwrap();
    assert_eq!(state.state.revision, original.state.revision);
    assert_eq!(state.state.comments[&comment].body, "离线评论保留");
    assert_eq!(host.attachment(copy.handle, attachment).unwrap(), bytes);
    assert_eq!(state.state.members.len(), 2);
    host.select_identity(copy.handle, viewer).unwrap();
    let read_session = host.git_session(copy.handle).unwrap();
    let forbidden = temp.path().join("forbidden.farflowpack");
    assert!(read_session.export_pack(&forbidden).is_err());
    assert!(!forbidden.exists());
    let imported = read_session
        .import_pack(host.pack_source(selected.token).unwrap())
        .unwrap();
    assert_eq!(serde_json::to_value(imported).unwrap()["action"], "import");
    assert_eq!(host.snapshot(copy.handle).unwrap().state.revision, 4);
    let snapshot = host.select_identity(copy.handle, owner).unwrap();
    host.execute(copy.handle,Id::new_v4(),serde_json::from_value(json!({"type":"update","id":item,"version":snapshot.state.items[&item].version,"patch":{"description":"从离线副本带回的正文"}})).unwrap(),snapshot.state.revision,104).unwrap();
    let returning = temp.path().join("return.farflowpack");
    host.git_session(copy.handle)
        .unwrap()
        .export_pack(&returning)
        .unwrap();
    let back = host.select_pack(&returning).unwrap();
    let result = session
        .import_pack(host.pack_source(back.token).unwrap())
        .unwrap();
    assert_eq!(result.issues, Some(0));
    assert!(result.bytes.is_none());
    let state = host.snapshot(opened.handle).unwrap();
    assert_eq!(state.state.items[&item].description, "从离线副本带回的正文");
    assert_eq!(host.attachment(opened.handle, attachment).unwrap(), bytes);
    let revision = state.state.revision;
    session
        .import_pack(host.pack_source(back.token).unwrap())
        .unwrap();
    assert_eq!(
        host.snapshot(opened.handle).unwrap().state.revision,
        revision
    );
}

#[test]
fn invalid_unauthorized_and_replaced_pack_paths_never_overwrite_business_or_files() {
    let temp = tempfile::tempdir().unwrap();
    let mut host = Desktop::default();
    let directory = host.select_directory(temp.path()).unwrap();
    assert!(host.pack_source(Id::new_v4()).is_err());
    assert!(host.select_pack(&temp.path().join("missing")).is_err());
    let opened = host
        .create(
            directory.token,
            "git",
            Mode::Git,
            "校验拒绝".into(),
            profile(),
        )
        .unwrap();
    host.select_identity(opened.handle, opened.identities[0].id)
        .unwrap();
    let session = host.git_session(opened.handle).unwrap();
    let invalid = temp.path().join("invalid.farflowpack");
    fs::write(&invalid, "无效文件包").unwrap();
    let bad = host.select_pack(&invalid).unwrap();
    assert!(session
        .import_pack(host.pack_source(bad.token).unwrap())
        .is_err());
    assert!(host
        .git_destination(directory.token, "bad-copy")
        .unwrap()
        .clone_pack(host.pack_source(bad.token).unwrap())
        .is_err());
    assert!(!temp.path().join("bad-copy").exists());
    assert_eq!(host.snapshot(opened.handle).unwrap().state.revision, 0);
    assert!(session
        .export_pack(&temp.path().join("git/inside.farflowpack"))
        .is_err());
    assert!(session.export_pack(&invalid).is_err());
    assert_eq!(fs::read_to_string(&invalid).unwrap(), "无效文件包");
    let parent = temp.path().join("packs");
    fs::create_dir(&parent).unwrap();
    let good = parent.join("good.farflowpack");
    session.export_pack(&good).unwrap();
    let source = host.select_pack(&good).unwrap();
    let destination = host.git_destination(directory.token, "raced").unwrap();
    fs::create_dir(temp.path().join("raced")).unwrap();
    assert!(destination
        .clone_pack(host.pack_source(source.token).unwrap())
        .is_err());
    #[cfg(unix)]
    {
        fs::rename(&parent, temp.path().join("kept-packs")).unwrap();
        std::os::unix::fs::symlink(temp.path().join("kept-packs"), &parent).unwrap();
        assert!(session
            .import_pack(host.pack_source(source.token).unwrap())
            .err()
            .unwrap()
            .to_string()
            .contains("路径已变化"));
        fs::remove_file(&parent).unwrap();
        fs::rename(temp.path().join("kept-packs"), &parent).unwrap();
        fs::rename(&good, parent.join("kept.farflowpack")).unwrap();
        std::os::unix::fs::symlink(parent.join("kept.farflowpack"), &good).unwrap();
        assert!(session
            .import_pack(host.pack_source(source.token).unwrap())
            .is_err());
    }
    assert_eq!(host.snapshot(opened.handle).unwrap().state.revision, 0);
}
