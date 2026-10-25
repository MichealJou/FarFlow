use farflow_core::{Command, Error, Id, LocalWorkspace, Member, Mode, Role};
use farflow_desktop::{AttachmentInput, Desktop, Profile};
use std::collections::BTreeSet;

const AT: i64 = 1792900000;
fn profile() -> Profile {
    Profile {
        account: "owner".into(),
        name: "本机成员".into(),
        department: "研发".into(),
    }
}
fn document(id: Id) -> Command {
    serde_json::from_value(serde_json::json!({"type":"create","item":{"id":id,"kind":"document","title":"本机文档","description":"保存后可重新打开"}})).unwrap()
}
fn upload(operation_id: Id, id: Id, item_id: Id) -> AttachmentInput {
    AttachmentInput {
        operation_id,
        id,
        item_id,
        name: "验收附件.txt".into(),
        mime: "text/plain".into(),
    }
}

#[test]
fn local_and_git_sessions_save_real_data_and_require_an_explicit_active_identity() {
    for mode in [Mode::Local, Mode::Git] {
        let temp = tempfile::tempdir().unwrap();
        let mut host = Desktop::default();
        let directory = host.select_directory(temp.path()).unwrap();
        let opened = host
            .create(
                directory.token,
                "研发资料",
                mode,
                "研发空间".into(),
                profile(),
            )
            .unwrap();
        let handle = opened.handle;
        let owner = opened.identities[0].id;
        assert_eq!(opened.workspace.mode, mode);
        assert_eq!(opened.identities[0].name, "本机成员");
        assert!(matches!(host.snapshot(handle), Err(Error::Forbidden(_))));
        assert!(matches!(
            host.attachment(handle, Id::new_v4()),
            Err(Error::Forbidden(_))
        ));
        assert!(matches!(
            host.execute(handle, Id::new_v4(), document(Id::new_v4()), 0, AT),
            Err(Error::Forbidden(_))
        ));
        assert!(matches!(
            host.select_identity(handle, Id::new_v4()),
            Err(Error::Forbidden(_))
        ));
        let initial = host.select_identity(handle, owner).unwrap();
        assert_eq!(initial.state.revision, 0);
        assert!(initial.issues.is_empty());
        let id = Id::new_v4();
        let operation = Id::new_v4();
        let receipt = host
            .execute(handle, operation, document(id), 0, AT)
            .unwrap();
        assert!(receipt.warning.is_none());
        assert_eq!(receipt.receipt.revision, 1);
        assert_eq!(
            host.execute(handle, operation, document(id), 0, AT)
                .unwrap()
                .receipt,
            receipt.receipt
        );
        assert!(matches!(
            host.execute(handle, Id::new_v4(), document(Id::new_v4()), 0, AT),
            Err(Error::Conflict(_))
        ));
        let attachment = Id::new_v4();
        let uploaded = Id::new_v4();
        host.upload(
            handle,
            upload(uploaded, attachment, id),
            "真实附件字节".as_bytes(),
            AT,
        )
        .unwrap();
        assert_eq!(
            host.upload(
                handle,
                upload(uploaded, attachment, id),
                "真实附件字节".as_bytes(),
                AT
            )
            .unwrap()
            .receipt
            .revision,
            2
        );
        assert_eq!(
            host.attachment(handle, attachment).unwrap(),
            "真实附件字节".as_bytes()
        );
        host.execute(
            handle,
            Id::new_v4(),
            Command::AddComment {
                id: Id::new_v4(),
                item_id: id,
                body: "可重新读取的评论".into(),
                parent_id: None,
                mentions: BTreeSet::new(),
                attachments: BTreeSet::from([attachment]),
            },
            2,
            AT,
        )
        .unwrap();
        let viewer = Member {
            id: Id::new_v4(),
            account: "viewer".into(),
            name: "只读成员".into(),
            department: "".into(),
            roles: BTreeSet::from([Role::Viewer]),
            active: true,
        };
        host.execute(
            handle,
            Id::new_v4(),
            Command::AddMember {
                member: viewer.clone(),
            },
            3,
            AT,
        )
        .unwrap();
        let read_only = host.select_identity(handle, viewer.id).unwrap();
        assert_eq!(read_only.state.comments.len(), 1);
        assert_eq!(
            host.attachment(handle, attachment).unwrap(),
            "真实附件字节".as_bytes()
        );
        assert!(matches!(
            host.execute(handle, Id::new_v4(), document(Id::new_v4()), 4, AT),
            Err(Error::Forbidden(_))
        ));
        host.select_identity(handle, owner).unwrap();
        let mut inactive = viewer;
        inactive.id = Id::new_v4();
        inactive.account = "inactive".into();
        inactive.active = false;
        host.execute(
            handle,
            Id::new_v4(),
            Command::AddMember {
                member: inactive.clone(),
            },
            4,
            AT,
        )
        .unwrap();
        let folder = host.select_directory(&opened.path).unwrap();
        let again = host.inspect(folder.token, mode).unwrap();
        assert_ne!(handle, again.handle);
        assert_eq!(again.identities.len(), 3);
        assert!(matches!(
            host.select_identity(again.handle, inactive.id),
            Err(Error::Forbidden(_))
        ));
        assert!(matches!(
            host.snapshot(again.handle),
            Err(Error::Forbidden(_))
        ));
        assert_eq!(host.snapshot(handle).unwrap().actor_id, owner);
        let disk = host.select_identity(again.handle, owner).unwrap();
        assert_eq!(disk.state.revision, 5);
        assert_eq!(disk.state.items[&id].title, "本机文档");
        assert_eq!(disk.state.attachments.len(), 1);
        drop(host);
        let mut cold = Desktop::default();
        let selected = cold.select_directory(&opened.path).unwrap();
        let existing = cold.inspect(selected.token, mode).unwrap();
        let restored = cold.select_identity(existing.handle, owner).unwrap();
        assert_eq!(restored.state.comments.len(), 1);
        assert_eq!(
            cold.attachment(existing.handle, attachment).unwrap(),
            "真实附件字节".as_bytes()
        );
        cold.close(existing.handle);
        cold.close(existing.handle);
        assert!(matches!(
            cold.snapshot(existing.handle),
            Err(Error::Forbidden(_))
        ));
    }
}

#[test]
fn directories_folder_names_and_storage_modes_are_validated_without_overwriting_files() {
    let temp = tempfile::tempdir().unwrap();
    let mut host = Desktop::default();
    let directory = host.select_directory(temp.path()).unwrap();
    assert!(host.select_directory(&temp.path().join("不存在")).is_err());
    std::fs::write(temp.path().join("普通文件"), b"original").unwrap();
    assert!(host
        .select_directory(&temp.path().join("普通文件"))
        .is_err());
    assert!(matches!(
        host.create(
            Id::new_v4(),
            "新目录",
            Mode::Local,
            "新空间".into(),
            profile()
        ),
        Err(Error::Forbidden(_))
    ));
    assert!(matches!(
        host.inspect(Id::new_v4(), Mode::Local),
        Err(Error::Forbidden(_))
    ));
    for folder in [
        "",
        ".",
        "..",
        "../outside",
        "a/b",
        "a\\b",
        " a",
        "a ",
        "a.",
        "a?b",
        "a\nb",
        "CON",
        "con.txt",
        "PRN",
        "NUL",
        "AUX",
        "COM1",
        "lpt9",
    ] {
        assert!(
            host.create(
                directory.token,
                folder,
                Mode::Local,
                "研发".into(),
                profile()
            )
            .is_err(),
            "{folder:?}"
        );
    }
    assert!(host
        .create(
            directory.token,
            &"字".repeat(100),
            Mode::Local,
            "研发".into(),
            profile()
        )
        .is_err());
    assert!(host
        .create(
            directory.token,
            "服务",
            Mode::Service,
            "服务".into(),
            profile()
        )
        .is_err());
    assert!(!temp.path().join("服务").exists());
    assert!(host.inspect(directory.token, Mode::Service).is_err());
    let mut invalid = profile();
    invalid.account.clear();
    assert!(host
        .create(
            directory.token,
            "无效成员",
            Mode::Local,
            "研发".into(),
            invalid
        )
        .is_err());
    assert!(!temp.path().join("无效成员").exists());
    let opened = host
        .create(
            directory.token,
            "COM10",
            Mode::Local,
            "研发空间".into(),
            profile(),
        )
        .unwrap();
    assert!(host
        .create(
            directory.token,
            "COM10",
            Mode::Local,
            "覆盖".into(),
            profile()
        )
        .is_err());
    assert_eq!(
        std::fs::read(temp.path().join("普通文件")).unwrap(),
        b"original"
    );
    assert_eq!(
        LocalWorkspace::open(&opened.path).unwrap().info().name,
        "研发空间"
    );
    assert!(matches!(
        host.select_identity(Id::new_v4(), Id::new_v4()),
        Err(Error::Forbidden(_))
    ));
}

#[test]
fn attachment_limits_missing_files_invalid_clocks_and_replaced_directories_fail_explicitly() {
    for mode in [Mode::Local, Mode::Git] {
        let temp = tempfile::tempdir().unwrap();
        let mut host = Desktop::default();
        let directory = host.select_directory(temp.path()).unwrap();
        let opened = host
            .create(directory.token, "原目录", mode, "原空间".into(), profile())
            .unwrap();
        let owner = opened.identities[0].id;
        host.select_identity(opened.handle, owner).unwrap();
        let id = Id::new_v4();
        assert!(host
            .execute(opened.handle, Id::new_v4(), document(id), 0, -1)
            .is_err());
        assert_eq!(host.snapshot(opened.handle).unwrap().state.revision, 0);
        host.execute(opened.handle, Id::new_v4(), document(id), 0, AT)
            .unwrap();
        assert!(host.attachment(opened.handle, Id::new_v4()).is_err());
        let large = vec![0; 10 * 1024 * 1024 + 1];
        assert!(matches!(
            host.upload(
                opened.handle,
                upload(Id::new_v4(), Id::new_v4(), id),
                &large,
                AT
            ),
            Err(Error::Validation(_))
        ));
        assert!(host
            .upload(
                opened.handle,
                upload(Id::new_v4(), Id::new_v4(), Id::new_v4()),
                b"none",
                AT
            )
            .is_err());
        assert_eq!(host.snapshot(opened.handle).unwrap().state.revision, 1);
        std::fs::rename(&opened.path, temp.path().join("原目录备份")).unwrap();
        host.create(
            directory.token,
            "原目录",
            mode,
            "另一个空间".into(),
            profile(),
        )
        .unwrap();
        assert!(matches!(
            host.snapshot(opened.handle),
            Err(Error::Conflict(_))
        ));
        assert!(matches!(
            host.execute(opened.handle, Id::new_v4(), document(Id::new_v4()), 1, AT),
            Err(Error::Conflict(_))
        ));
    }
}
