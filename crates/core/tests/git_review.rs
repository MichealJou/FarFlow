use farflow_core::*;
use serde_json::json;
use std::collections::BTreeSet;

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

fn apply(workspace: &GitWorkspace, actor: Id, command: Command) -> Id {
    let id = Id::new_v4();
    workspace
        .execute(Request {
            id,
            actor_id: actor,
            at: 100,
            command,
        })
        .unwrap();
    id
}

fn edit(item: Id, text: &str) -> Command {
    serde_json::from_value(
        json!({"type":"update","id":item,"version":1,"patch":{"description":text}}),
    )
    .unwrap()
}

#[test]
fn reviewed_decisions_compare_full_records_even_when_revision_does_not_change() {
    let temp = tempfile::tempdir().unwrap();
    let owner = member("owner", Role::Owner);
    let workspace = GitWorkspace::create(
        temp.path().join("first"),
        Id::new_v4(),
        "冲突核对".into(),
        owner.clone(),
    )
    .unwrap();
    let viewer = member("viewer", Role::Viewer);
    apply(
        &workspace,
        owner.id,
        Command::AddMember {
            member: viewer.clone(),
        },
    );
    let item = Id::new_v4();
    apply(&workspace,owner.id,serde_json::from_value(json!({"type":"create","item":{"id":item,"kind":"document","title":"基础文档","description":"基础正文"}})).unwrap());
    let empty = workspace.review(owner.id).unwrap();
    assert!(empty.issues.is_empty());
    assert!(empty.proposals.is_empty());
    assert!(empty.decisions.is_empty());
    assert_eq!(empty.token.len(), 64);
    assert!(workspace.review(Id::new_v4()).is_err());
    let pack = temp.path().join("copy.pack");
    workspace.export_pack(owner.id, &pack).unwrap();
    let peer = GitWorkspace::from_pack(&pack, temp.path().join("second")).unwrap();
    let left = apply(&workspace, owner.id, edit(item, "甲正文"));
    let right = apply(&peer, owner.id, edit(item, "乙正文"));
    workspace
        .merge_from(owner.id, temp.path().join("second"))
        .unwrap();
    let review = workspace.review(viewer.id).unwrap();
    assert_eq!(review.issues.len(), 2);
    assert_eq!(review.proposals.len(), 2);
    assert_eq!(review.state.items[&item].description, "基础正文");
    assert!(
        matches!(&review.proposals[&left].change,GitChange::Apply{request} if request.actor_id==owner.id)
    );
    let mut resolution = GitResolution {
        id: Id::new_v4(),
        actor_id: viewer.id,
        at: 200,
        targets: BTreeSet::from([left]),
        command: None,
        note: "比较后不采纳甲提案".into(),
    };
    assert!(workspace
        .resolve_reviewed(resolution.clone(), &review.token)
        .is_err());
    resolution.actor_id = owner.id;
    assert!(workspace
        .resolve_reviewed(resolution.clone(), "过期记录")
        .is_err());
    let mut invalid = resolution.clone();
    invalid.note = " ".into();
    assert!(workspace.resolve_reviewed(invalid, &review.token).is_err());
    let mut invalid = resolution.clone();
    invalid.targets.clear();
    assert!(workspace.resolve_reviewed(invalid, &review.token).is_err());
    let mut invalid = resolution.clone();
    invalid.targets = BTreeSet::from([Id::new_v4()]);
    assert!(workspace.resolve_reviewed(invalid, &review.token).is_err());
    let saved = workspace
        .resolve_reviewed(resolution.clone(), &review.token)
        .unwrap();
    let changed = workspace.review(owner.id).unwrap();
    assert_ne!(changed.token, review.token);
    assert_eq!(changed.state.revision, review.state.revision);
    assert_eq!(changed.issues.len(), 1);
    assert_eq!(changed.decisions.len(), 1);
    assert!(changed.decisions[0].applied);
    assert!(changed.decisions[0].issue.is_none());
    assert!(changed.decisions[0].targets.contains_key(&left));
    assert!(
        matches!(&changed.decisions[0].operation.change,GitChange::Resolve{resolution:old} if old.note==resolution.note)
    );
    assert_eq!(
        workspace
            .resolve_reviewed(resolution.clone(), &review.token)
            .unwrap()
            .receipt
            .operation_id,
        saved.receipt.operation_id
    );
    let mut altered = resolution.clone();
    altered.note = "改写已保存决策".into();
    assert!(workspace.resolve_reviewed(altered, &review.token).is_err());
    let next = GitResolution {
        id: Id::new_v4(),
        targets: BTreeSet::from([right]),
        command: Some(edit(item, "合并后的正文")),
        note: "比较双方并合并正文".into(),
        ..resolution
    };
    assert!(workspace
        .resolve_reviewed(next.clone(), &review.token)
        .err()
        .unwrap()
        .to_string()
        .contains("重新比较"));
    workspace.resolve_reviewed(next, &changed.token).unwrap();
    let reopened = GitWorkspace::open(temp.path().join("first"))
        .unwrap()
        .review(owner.id)
        .unwrap();
    assert!(reopened.issues.is_empty());
    assert_eq!(reopened.state.items[&item].description, "合并后的正文");
    assert_eq!(reopened.state.items[&item].content_version, 2);
    assert_eq!(reopened.decisions.len(), 2);
    assert!(reopened.decisions.iter().all(|entry| entry.applied));
    let serialized = serde_json::to_value(&reopened).unwrap();
    let decoded: GitReview = serde_json::from_value(serialized.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), serialized);
    let operation_files = std::fs::read_dir(temp.path().join("first/operations"))
        .unwrap()
        .map(|d| std::fs::read_dir(d.unwrap().path()).unwrap().count())
        .sum::<usize>();
    assert_eq!(operation_files, 6);
}

#[test]
fn decision_history_keeps_superseded_decisions_their_original_targets_and_pending_results() {
    let temp = tempfile::tempdir().unwrap();
    let owner = member("owner", Role::Owner);
    let workspace = GitWorkspace::create(
        temp.path().join("first"),
        Id::new_v4(),
        "决策历史".into(),
        owner.clone(),
    )
    .unwrap();
    let item = Id::new_v4();
    apply(
        &workspace,
        owner.id,
        serde_json::from_value(
            json!({"type":"create","item":{"id":item,"kind":"document","title":"历史正文"}}),
        )
        .unwrap(),
    );
    let initial = temp.path().join("initial.pack");
    workspace.export_pack(owner.id, &initial).unwrap();
    let peer = GitWorkspace::from_pack(&initial, temp.path().join("peer")).unwrap();
    apply(&workspace, owner.id, edit(item, "原始甲提案"));
    apply(&peer, owner.id, edit(item, "原始乙提案"));
    workspace
        .merge_from(owner.id, temp.path().join("peer"))
        .unwrap();
    let conflict = temp.path().join("conflict.pack");
    workspace.export_pack(owner.id, &conflict).unwrap();
    let reviewer = GitWorkspace::from_pack(&conflict, temp.path().join("reviewer")).unwrap();
    let review = workspace.review(owner.id).unwrap();
    let mut saved = Vec::new();
    for (target, note) in [
        (&workspace, "甲设备保留基础内容"),
        (&reviewer, "乙设备保留基础内容"),
    ] {
        let resolution = GitResolution {
            id: Id::new_v4(),
            actor_id: owner.id,
            at: 200,
            targets: review.issues.keys().copied().collect(),
            command: None,
            note: note.into(),
        };
        target
            .resolve_reviewed(resolution.clone(), &review.token)
            .unwrap();
        saved.push(resolution);
    }
    workspace
        .merge_from(owner.id, temp.path().join("reviewer"))
        .unwrap();
    let pending = workspace.review(owner.id).unwrap();
    assert_eq!(pending.decisions.len(), 2);
    for entry in &pending.decisions {
        assert!(!entry.applied);
        assert!(entry.issue.is_some());
        assert_eq!(entry.targets.len(), 2);
        assert!(entry
            .targets
            .values()
            .all(|target| matches!(target.change, GitChange::Apply { .. })));
    }
    let final_resolution = GitResolution {
        id: Id::new_v4(),
        actor_id: owner.id,
        at: 100,
        targets: pending.issues.keys().copied().collect(),
        command: Some(edit(item, "核对两次决策后合并")),
        note: "时钟时间不替代因果关系".into(),
    };
    workspace
        .resolve_reviewed(final_resolution.clone(), &pending.token)
        .unwrap();
    let complete = workspace.review(owner.id).unwrap();
    assert!(complete.issues.is_empty());
    assert_eq!(complete.decisions.len(), 3);
    let final_entry = complete
        .decisions
        .iter()
        .find(|entry| entry.operation.id == final_resolution.id)
        .unwrap();
    assert!(final_entry.applied);
    assert!(final_entry.issue.is_none());
    for resolution in saved {
        let historical = complete
            .decisions
            .iter()
            .find(|entry| entry.operation.id == resolution.id)
            .unwrap();
        assert!(!historical.applied);
        assert!(historical.issue.is_none());
        assert_eq!(historical.targets.len(), 2);
        assert!(final_entry.targets.contains_key(&resolution.id));
    }
    assert_eq!(
        complete.state.items[&item].description,
        "核对两次决策后合并"
    );
    let (before, after) = (complete.state.revision, workspace.review(owner.id).unwrap());
    assert_eq!(before, after.state.revision);
}
