use farflow_core::{GitChange, GitWorkspace, Id, Mode, Request};
use farflow_desktop::{Desktop, Profile, ResolutionInput};
use serde_json::json;

fn input(value: serde_json::Value) -> ResolutionInput {
    serde_json::from_value(value).unwrap()
}

#[test]
fn desktop_conflict_review_binds_actual_member_and_reuses_index_and_decision_rules() {
    let temp = tempfile::tempdir().unwrap();
    let mut host = Desktop::default();
    let directory = host.select_directory(temp.path()).unwrap();
    let opened = host
        .create(
            directory.token,
            "git",
            Mode::Git,
            "桌面冲突".into(),
            Profile {
                account: "owner".into(),
                name: "实际所有者".into(),
                department: "研发".into(),
            },
        )
        .unwrap();
    let handle = opened.handle;
    let actor = opened.identities[0].id;
    assert!(host.git_review(handle).is_err());
    assert!(host.git_rebuild(handle).is_err());
    assert!(host.git_review(Id::new_v4()).is_err());
    host.select_identity(handle, actor).unwrap();
    let viewer = Id::new_v4();
    host.execute(handle,Id::new_v4(),serde_json::from_value(json!({"type":"add_member","member":{"id":viewer,"account":"viewer","name":"只读","department":"研发","roles":["viewer"],"active":true}})).unwrap(),0,100).unwrap();
    let item = Id::new_v4();
    host.execute(
        handle,
        Id::new_v4(),
        serde_json::from_value(
            json!({"type":"create","item":{"id":item,"kind":"document","title":"基础文档"}}),
        )
        .unwrap(),
        1,
        100,
    )
    .unwrap();
    let path = temp.path().join("git");
    let workspace = GitWorkspace::open(&path).unwrap();
    let pack = temp.path().join("copy.pack");
    workspace.export_pack(actor, &pack).unwrap();
    let peer = GitWorkspace::from_pack(&pack, temp.path().join("peer")).unwrap();
    for (target, body) in [(&workspace, "甲正文"), (&peer, "乙正文")] {
        target
            .execute(Request {
                id: Id::new_v4(),
                actor_id: actor,
                at: 101,
                command: serde_json::from_value(
                    json!({"type":"update","id":item,"version":1,"patch":{"description":body}}),
                )
                .unwrap(),
            })
            .unwrap();
    }
    workspace
        .merge_from(actor, temp.path().join("peer"))
        .unwrap();
    let review = host.git_review(handle).unwrap();
    assert_eq!(review.issues.len(), 2);
    std::fs::write(
        path.join(".git/farflow/index.sqlite"),
        "损坏派生索引".as_bytes(),
    )
    .unwrap();
    assert_eq!(host.git_rebuild(handle).unwrap().issues.len(), 2);
    let command = review
        .proposals
        .values()
        .find_map(|operation| match &operation.change {
            GitChange::Apply { request } => Some(request.command.clone()),
            _ => None,
        })
        .unwrap();
    let decision = json!({"id":Id::new_v4(),"token":review.token,"targets":review.issues.keys().collect::<Vec<_>>(),"command":command,"note":"比较后采用原始提案","at":200});
    host.select_identity(handle, viewer).unwrap();
    assert_eq!(host.git_review(handle).unwrap().issues.len(), 2);
    assert!(host.git_resolve(handle, input(decision.clone())).is_err());
    assert_eq!(host.git_rebuild(handle).unwrap().issues.len(), 2);
    host.select_identity(handle, actor).unwrap();
    let saved = host.git_resolve(handle, input(decision.clone())).unwrap();
    assert_eq!(saved.receipt.revision, 3);
    assert!(host.git_review(handle).unwrap().issues.is_empty());
    host.select_identity(handle, viewer).unwrap();
    let history = host.git_review(handle).unwrap();
    assert_eq!(history.decisions.len(), 1);
    assert!(history.decisions[0].applied);
    assert_eq!(history.decisions[0].targets.len(), 2);
    assert!(
        matches!(&history.decisions[0].operation.change,GitChange::Resolve{resolution} if resolution.actor_id==actor&&resolution.note=="比较后采用原始提案")
    );
    host.select_identity(handle, actor).unwrap();
    assert!(host.git_resolve(handle, input(decision.clone())).is_ok());
    let mut spoofed = decision.clone();
    spoofed["actor_id"] = json!(viewer);
    assert!(serde_json::from_value::<ResolutionInput>(spoofed).is_err());
    let local = host
        .create(
            directory.token,
            "local",
            Mode::Local,
            "本地".into(),
            Profile {
                account: "owner".into(),
                name: "本地成员".into(),
                department: "研发".into(),
            },
        )
        .unwrap();
    host.select_identity(local.handle, local.identities[0].id)
        .unwrap();
    assert!(host.git_review(local.handle).is_err());
    assert!(host.git_rebuild(local.handle).is_err());
    assert!(host
        .git_resolve(local.handle, input(decision.clone()))
        .is_err());
    host.close(handle);
    assert!(host.git_review(handle).is_err());
    assert!(host.git_resolve(handle, input(decision)).is_err());
}
