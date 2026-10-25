use farflow_core::*;
use serde_json::{json, Value};
use std::collections::BTreeSet;

struct F {
    log: GitLog,
    owner: Member,
    device: Id,
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
    serde_json::from_value(
        json!({"id":Id::new_v4(),"kind":"document","title":title,"description":"基础正文"}),
    )
    .unwrap()
}
fn request(actor: Id, command: Command) -> Request {
    Request {
        id: Id::new_v4(),
        actor_id: actor,
        at: 100,
        command,
    }
}
fn encoded(value: &impl serde::Serialize) -> Value {
    serde_json::to_value(value).unwrap()
}
fn update(id: Id, version: u64, patch: Patch) -> Command {
    Command::Update { id, version, patch }
}
fn merge_resolution(log: &GitLog, actor: Id, command: Option<Command>) -> GitResolution {
    GitResolution {
        id: Id::new_v4(),
        actor_id: actor,
        at: 200,
        targets: log.view().unwrap().issues.keys().copied().collect(),
        command,
        note: "已比较双方原始内容并决定合并方式".into(),
    }
}

impl F {
    fn new() -> Self {
        let owner = member("owner", Role::Owner);
        let mut log = GitLog::new(Id::new_v4(), "Git 操作测试".into(), owner.clone()).unwrap();
        let device = Id::new_v4();
        let new = document("基础文档");
        let id = new.id;
        log.execute(device, request(owner.id, Command::Create { item: new }))
            .unwrap();
        Self {
            log,
            owner,
            device,
            document: id,
        }
    }
    fn split(&self) -> (GitLog, GitLog, Id, Id) {
        (
            self.log.clone(),
            self.log.clone(),
            Id::new_v4(),
            Id::new_v4(),
        )
    }
    fn conflict(&self) -> (GitLog, Id, Id) {
        let (mut a, mut b, da, db) = self.split();
        let left = request(
            self.owner.id,
            update(
                self.document,
                1,
                Patch {
                    description: Some("左侧修改".into()),
                    ..Default::default()
                },
            ),
        );
        let right = request(
            self.owner.id,
            update(
                self.document,
                1,
                Patch {
                    description: Some("右侧修改".into()),
                    ..Default::default()
                },
            ),
        );
        let ids = (left.id, right.id);
        a.execute(da, left).unwrap();
        b.execute(db, right).unwrap();
        a.merge(&b).unwrap();
        (a, ids.0, ids.1)
    }
}

#[test]
fn independent_creations_and_comments_converge_in_either_merge_order() {
    let f = F::new();
    let (mut a, mut b, da, db) = f.split();
    let item_a = document("离线任务甲");
    let item_b = document("离线任务乙");
    let aid = item_a.id;
    let bid = item_b.id;
    a.execute(da, request(f.owner.id, Command::Create { item: item_a }))
        .unwrap();
    b.execute(db, request(f.owner.id, Command::Create { item: item_b }))
        .unwrap();
    for (log, device, text) in [(&mut a, da, "甲评论"), (&mut b, db, "乙评论")] {
        log.execute(
            device,
            request(
                f.owner.id,
                Command::AddComment {
                    id: Id::new_v4(),
                    item_id: f.document,
                    body: text.into(),
                    parent_id: None,
                    mentions: BTreeSet::new(),
                    attachments: BTreeSet::new(),
                },
            ),
        )
        .unwrap();
    }
    let mut left = a.clone();
    let mut right = b.clone();
    left.merge(&b).unwrap();
    right.merge(&a).unwrap();
    assert_eq!(
        encoded(&left.view().unwrap()),
        encoded(&right.view().unwrap())
    );
    let view = left.view().unwrap();
    assert!(view.issues.is_empty());
    assert!(view.state.items.contains_key(&aid));
    assert!(view.state.items.contains_key(&bid));
    assert_eq!(view.state.comments.len(), 2);
    assert_eq!(left.info().mode, Mode::Git);
    assert_eq!(left.info().name, "Git 操作测试");
    assert_eq!(left.operations().len(), 5);
    let before = encoded(&view);
    left.merge(&right).unwrap();
    assert_eq!(encoded(&left.view().unwrap()), before);
}

#[test]
fn independent_metadata_fields_merge_without_overwriting_content() {
    let mut f = F::new();
    let developer = member("developer", Role::Developer);
    let developer_id = developer.id;
    f.log
        .execute(
            f.device,
            request(f.owner.id, Command::AddMember { member: developer }),
        )
        .unwrap();
    let (mut a, mut b, da, db) = f.split();
    a.execute(
        da,
        request(
            f.owner.id,
            update(
                f.document,
                1,
                Patch {
                    priority: Some(1),
                    ..Default::default()
                },
            ),
        ),
    )
    .unwrap();
    b.execute(
        db,
        request(
            f.owner.id,
            update(
                f.document,
                1,
                Patch {
                    assigned_to: Some(developer_id),
                    ..Default::default()
                },
            ),
        ),
    )
    .unwrap();
    let view = a.merge(&b).unwrap();
    assert!(view.issues.is_empty());
    let item = &view.state.items[&f.document];
    assert_eq!(item.priority, 1);
    assert_eq!(item.assigned_to, Some(developer_id));
    assert_eq!(item.content_version, 1);
    assert_eq!(item.version, 3);
}

#[test]
fn content_and_metadata_merge_but_two_content_revisions_require_a_decision() {
    let f = F::new();
    let (mut a, mut b, da, db) = f.split();
    a.execute(
        da,
        request(
            f.owner.id,
            update(
                f.document,
                1,
                Patch {
                    description: Some("新的正文".into()),
                    ..Default::default()
                },
            ),
        ),
    )
    .unwrap();
    b.execute(
        db,
        request(
            f.owner.id,
            update(
                f.document,
                1,
                Patch {
                    priority: Some(1),
                    ..Default::default()
                },
            ),
        ),
    )
    .unwrap();
    let view = a.merge(&b).unwrap();
    assert!(view.issues.is_empty());
    assert_eq!(view.state.items[&f.document].description, "新的正文");
    assert_eq!(view.state.items[&f.document].priority, 1);
    let (mut a, mut b, da, db) = f.split();
    a.execute(
        da,
        request(
            f.owner.id,
            update(
                f.document,
                1,
                Patch {
                    title: Some("另一个标题".into()),
                    ..Default::default()
                },
            ),
        ),
    )
    .unwrap();
    b.execute(
        db,
        request(
            f.owner.id,
            update(
                f.document,
                1,
                Patch {
                    description: Some("另一个正文".into()),
                    ..Default::default()
                },
            ),
        ),
    )
    .unwrap();
    let view = a.merge(&b).unwrap();
    assert_eq!(view.issues.len(), 2);
    assert!(view
        .issues
        .values()
        .all(|i| i.kind == GitIssueKind::Concurrent));
    assert_eq!(view.state.items[&f.document].content_version, 1);
}

#[test]
fn same_content_edit_can_coalesce_and_original_operations_are_preserved() {
    let f = F::new();
    let (mut a, mut b, da, db) = f.split();
    for (log, device, at) in [(&mut a, da, 100), (&mut b, db, 200)] {
        let mut req = request(
            f.owner.id,
            update(
                f.document,
                1,
                Patch {
                    description: Some("相同内容".into()),
                    ..Default::default()
                },
            ),
        );
        req.at = at;
        log.execute(device, req).unwrap();
    }
    let view = a.merge(&b).unwrap();
    assert!(view.issues.is_empty());
    assert_eq!(view.state.items[&f.document].content_version, 2);
    assert_eq!(a.operations().len(), 3);
    assert_eq!(view.state.operations.len(), 3);
}

#[test]
fn conflicting_values_never_choose_a_winner_by_clock_or_import_order() {
    let f = F::new();
    let (mut a, mut b, da, db) = f.split();
    let mut ra = request(
        f.owner.id,
        update(
            f.document,
            1,
            Patch {
                priority: Some(1),
                ..Default::default()
            },
        ),
    );
    ra.at = 9_999_999;
    let rb = request(
        f.owner.id,
        update(
            f.document,
            1,
            Patch {
                priority: Some(4),
                ..Default::default()
            },
        ),
    );
    let ids = BTreeSet::from([ra.id, rb.id]);
    a.execute(da, ra).unwrap();
    b.execute(db, rb).unwrap();
    let mut first = a.clone();
    let mut second = b.clone();
    first.merge(&b).unwrap();
    second.merge(&a).unwrap();
    let view = first.view().unwrap();
    assert_eq!(view.issues.keys().copied().collect::<BTreeSet<_>>(), ids);
    assert_eq!(view.state.items[&f.document].priority, 3);
    assert_eq!(encoded(&view), encoded(&second.view().unwrap()));
    assert!(first
        .execute(
            Id::new_v4(),
            request(
                f.owner.id,
                Command::Create {
                    item: document("冲突未处理时的新写入")
                }
            )
        )
        .is_err());
}

#[test]
fn explicit_resolution_reuses_business_validation_and_survives_replay() {
    let f = F::new();
    let (mut log, left, right) = f.conflict();
    let original_left = encoded(&log.operations()[&left]);
    let original_right = encoded(&log.operations()[&right]);
    let resolution = merge_resolution(
        &log,
        f.owner.id,
        Some(update(
            f.document,
            1,
            Patch {
                description: Some("确认后的合并正文".into()),
                ..Default::default()
            },
        )),
    );
    let device = Id::new_v4();
    let receipt = log.resolve(device, resolution.clone()).unwrap();
    assert_eq!(log.resolve(device, resolution.clone()).unwrap(), receipt);
    assert!(log.view().unwrap().issues.is_empty());
    assert_eq!(
        log.view().unwrap().state.items[&f.document].description,
        "确认后的合并正文"
    );
    assert_eq!(encoded(&log.operations()[&left]), original_left);
    assert_eq!(encoded(&log.operations()[&right]), original_right);
    assert!(log.operations().contains_key(&resolution.id));
    let reopened: GitLog = serde_json::from_value(encoded(&log)).unwrap();
    assert_eq!(
        encoded(&log.view().unwrap()),
        encoded(&reopened.view().unwrap())
    );
    log.execute(
        device,
        request(
            f.owner.id,
            Command::Create {
                item: document("继续正常工作"),
            },
        ),
    )
    .unwrap();
    assert!(log.view().unwrap().issues.is_empty());
    let mut altered = resolution;
    altered.note = "同一标识不同决策".into();
    assert!(log.resolve(device, altered).is_err());
}

#[test]
fn abandon_resolution_keeps_base_and_is_recorded_without_a_fake_business_change() {
    let f = F::new();
    let (mut log, _, _) = f.conflict();
    let before = log.view().unwrap().state;
    let decision = merge_resolution(&log, f.owner.id, None);
    let id = decision.id;
    let receipt = log.resolve(Id::new_v4(), decision).unwrap();
    let view = log.view().unwrap();
    assert!(view.issues.is_empty());
    assert_eq!(encoded(&view.state), encoded(&before));
    assert_eq!(receipt.revision, before.revision);
    assert!(view.receipts.contains_key(&id));
    assert!(log.operations().contains_key(&id));
}

#[test]
fn invalid_or_unauthorized_resolutions_are_atomic() {
    let mut f = F::new();
    let viewer = member("viewer", Role::Viewer);
    let viewer_id = viewer.id;
    f.log
        .execute(
            f.device,
            request(f.owner.id, Command::AddMember { member: viewer }),
        )
        .unwrap();
    let (mut log, _, _) = f.conflict();
    for index in 0..9 {
        let before = encoded(&log);
        let mut decision = merge_resolution(&log, f.owner.id, None);
        match index {
            0 => decision.actor_id = viewer_id,
            1 => decision.actor_id = Id::new_v4(),
            2 => decision.targets.clear(),
            3 => {
                decision.targets.insert(Id::new_v4());
            }
            4 => decision.note.clear(),
            5 => decision.at = -1,
            6 => decision.id = Id::nil(),
            7 => {
                decision.command = Some(update(
                    f.document,
                    999,
                    Patch {
                        title: Some("过期覆盖".into()),
                        ..Default::default()
                    },
                ))
            }
            _ => decision.note = "x".repeat(8001),
        };
        assert!(log.resolve(Id::new_v4(), decision).is_err(), "场景 {index}");
        assert_eq!(encoded(&log), before);
    }
}

#[test]
fn concurrent_resolutions_must_themselves_be_resolved_explicitly() {
    let f = F::new();
    let (log, _, _) = f.conflict();
    let mut a = log.clone();
    let mut b = log.clone();
    let ra = merge_resolution(&a, f.owner.id, None);
    let rb = merge_resolution(&b, f.owner.id, None);
    a.resolve(Id::new_v4(), ra.clone()).unwrap();
    b.resolve(Id::new_v4(), rb.clone()).unwrap();
    a.merge(&b).unwrap();
    let issues = a.view().unwrap().issues;
    assert_eq!(issues.len(), 4);
    assert!(issues.contains_key(&ra.id));
    assert!(issues.contains_key(&rb.id));
    let decision = merge_resolution(
        &a,
        f.owner.id,
        Some(update(
            f.document,
            1,
            Patch {
                title: Some("重新核定".into()),
                ..Default::default()
            },
        )),
    );
    a.resolve(Id::new_v4(), decision).unwrap();
    assert!(a.view().unwrap().issues.is_empty());
    assert_eq!(a.view().unwrap().state.items[&f.document].title, "重新核定");
}

#[test]
fn descendants_of_conflicts_are_retained_and_need_their_own_acknowledgement() {
    let f = F::new();
    let (mut a, mut b, da, db) = f.split();
    let ra = request(
        f.owner.id,
        update(
            f.document,
            1,
            Patch {
                priority: Some(1),
                ..Default::default()
            },
        ),
    );
    let rb = request(
        f.owner.id,
        update(
            f.document,
            1,
            Patch {
                priority: Some(4),
                ..Default::default()
            },
        ),
    );
    a.execute(da, ra).unwrap();
    let child = request(
        f.owner.id,
        Command::Create {
            item: document("依赖旧决策的后续内容"),
        },
    );
    let child_id = child.id;
    a.execute(da, child).unwrap();
    b.execute(db, rb).unwrap();
    a.merge(&b).unwrap();
    let view = a.view().unwrap();
    assert_eq!(view.issues[&child_id].kind, GitIssueKind::Blocked);
    assert!(a.operations().contains_key(&child_id));
    let decision = merge_resolution(&a, f.owner.id, None);
    a.resolve(Id::new_v4(), decision).unwrap();
    assert!(a.view().unwrap().issues.is_empty());
    assert_eq!(a.view().unwrap().state.items.len(), 1);
}

#[test]
fn journal_ids_are_idempotent_and_different_payloads_never_replace_them() {
    let mut f = F::new();
    let req = request(
        f.owner.id,
        Command::Create {
            item: document("幂等内容"),
        },
    );
    let receipt = f.log.execute(f.device, req.clone()).unwrap();
    assert_eq!(f.log.execute(Id::new_v4(), req.clone()).unwrap(), receipt);
    let before = encoded(&f.log);
    let mut changed = req;
    changed.at += 1;
    assert!(f.log.execute(f.device, changed).is_err());
    assert_eq!(encoded(&f.log), before);
    let mut incoming: Value = encoded(&f.log);
    let id = receipt.operation_id.to_string();
    incoming["operations"][&id]["change"]["request"]["at"] = json!(999);
    let incoming: GitLog = serde_json::from_value(incoming).unwrap();
    assert!(f.log.merge(&incoming).is_err());
    assert_eq!(encoded(&f.log), before);
}

#[test]
fn malformed_graphs_missing_dependencies_cycles_and_device_reuse_are_rejected() {
    let f = F::new();
    let id = *f.log.operations().keys().next().unwrap();
    for index in 0..10 {
        let mut data = encoded(&f.log);
        let node = &mut data["operations"][id.to_string()];
        match index {
            0 => node["schema"] = json!(2),
            1 => node["workspace_id"] = json!(Id::new_v4()),
            2 => node["id"] = json!(Id::new_v4()),
            3 => node["device_id"] = json!(Id::nil()),
            4 => node["parents"] = json!([Id::new_v4()]),
            5 => node["parents"] = json!([id]),
            6 => node["change"]["request"]["id"] = json!(Id::new_v4()),
            7 => node["change"]["request"]["at"] = json!(-1),
            8 => node["change"]["request"]["actor_id"] = json!(Id::new_v4()),
            _ => node["change"]["request"]["command"]["item"]["title"] = json!(""),
        };
        let forged: GitLog = serde_json::from_value(data).unwrap();
        assert!(forged.view().is_err(), "场景 {index}");
        let mut clean = f.log.clone();
        assert!(clean.merge(&forged).is_err());
        assert_eq!(encoded(&clean), encoded(&f.log));
    }
    let (mut a, mut b, device, _) = f.split();
    a.execute(
        device,
        request(
            f.owner.id,
            Command::Create {
                item: document("同设备甲"),
            },
        ),
    )
    .unwrap();
    b.execute(
        device,
        request(
            f.owner.id,
            Command::Create {
                item: document("同设备乙"),
            },
        ),
    )
    .unwrap();
    assert!(matches!(a.merge(&b), Err(Error::Conflict(_))));
}

#[test]
fn genesis_and_unknown_protocol_fields_cannot_be_silently_reinterpreted() {
    let mut f = F::new();
    assert!(GitLog::new(Id::nil(), "无效".into(), f.owner.clone()).is_err());
    assert!(f
        .log
        .execute(
            Id::nil(),
            request(
                f.owner.id,
                Command::Create {
                    item: document("无设备")
                }
            )
        )
        .is_err());
    let other = GitLog::new(Id::new_v4(), "其他空间".into(), f.owner.clone()).unwrap();
    assert!(f.log.merge(&other).is_err());
    let mut data = encoded(&f.log);
    data["genesis"]["revision"] = json!(1);
    assert!(serde_json::from_value::<GitLog>(data)
        .unwrap()
        .view()
        .is_err());
    let mut data = encoded(&f.log);
    data["unknown"] = json!(true);
    assert!(serde_json::from_value::<GitLog>(data).is_err());
    let id = f.log.operations().keys().next().unwrap().to_string();
    let mut data = encoded(&f.log);
    data["operations"][id]["unknown"] = json!(true);
    assert!(serde_json::from_value::<GitLog>(data).is_err());
}

#[test]
fn higher_order_business_constraints_are_not_lost_in_pairwise_checks() {
    let owner = member("owner-a", Role::Owner);
    let second = member("owner-b", Role::Owner);
    let third = member("owner-c", Role::Owner);
    let mut base = GitLog::new(Id::new_v4(), "三方校验".into(), owner.clone()).unwrap();
    let device = Id::new_v4();
    for m in [second.clone(), third.clone()] {
        base.execute(device, request(owner.id, Command::AddMember { member: m }))
            .unwrap();
    }
    let mut logs = Vec::new();
    for mut m in [owner, second, third] {
        let mut fork = base.clone();
        let actor = m.id;
        m.active = false;
        fork.execute(
            Id::new_v4(),
            request(actor, Command::UpdateMember { member: m }),
        )
        .unwrap();
        logs.push(fork);
    }
    let mut combined = logs[0].clone();
    combined.merge(&logs[1]).unwrap();
    let view = combined.merge(&logs[2]).unwrap();
    assert_eq!(view.issues.len(), 1);
    assert_eq!(
        view.issues.values().next().unwrap().kind,
        GitIssueKind::Constraint
    );
    let active: Vec<_> = view.state.members.values().filter(|m| m.active).collect();
    assert_eq!(active.len(), 1);
    let decision = merge_resolution(&combined, active[0].id, None);
    combined.resolve(Id::new_v4(), decision).unwrap();
    assert!(combined.view().unwrap().issues.is_empty());
}

#[test]
fn disabling_a_member_and_their_concurrent_write_requires_a_security_decision() {
    let mut f = F::new();
    let dev = member("developer", Role::Developer);
    let dev_id = dev.id;
    f.log
        .execute(
            f.device,
            request(
                f.owner.id,
                Command::AddMember {
                    member: dev.clone(),
                },
            ),
        )
        .unwrap();
    let (mut a, mut b, da, db) = f.split();
    let mut disabled = dev;
    disabled.active = false;
    a.execute(
        da,
        request(f.owner.id, Command::UpdateMember { member: disabled }),
    )
    .unwrap();
    b.execute(
        db,
        request(
            dev_id,
            Command::Create {
                item: document("停用并发写入"),
            },
        ),
    )
    .unwrap();
    let view = a.merge(&b).unwrap();
    assert_eq!(view.issues.len(), 2);
    assert!(view.state.members[&dev_id].active);
    assert_eq!(view.state.items.len(), 1);
}

#[test]
fn late_old_branch_cannot_borrow_a_concurrent_resolution_as_acknowledgement() {
    let f = F::new();
    let (mut a, mut b, da, db) = f.split();
    a.execute(
        da,
        request(
            f.owner.id,
            update(
                f.document,
                1,
                Patch {
                    priority: Some(1),
                    ..Default::default()
                },
            ),
        ),
    )
    .unwrap();
    let mut old_branch = a.clone();
    let child = document("稍后到达的旧分支内容");
    let child_item = child.id;
    let req = request(f.owner.id, Command::Create { item: child });
    let child_op = req.id;
    old_branch.execute(da, req).unwrap();
    b.execute(
        db,
        request(
            f.owner.id,
            update(
                f.document,
                1,
                Patch {
                    priority: Some(4),
                    ..Default::default()
                },
            ),
        ),
    )
    .unwrap();
    a.merge(&b).unwrap();
    let decision = merge_resolution(&a, f.owner.id, None);
    let decision_id = decision.id;
    a.resolve(Id::new_v4(), decision).unwrap();
    assert!(a.view().unwrap().issues.is_empty());
    let view = a.merge(&old_branch).unwrap();
    assert!(view.receipts.contains_key(&decision_id));
    assert_eq!(view.issues[&child_op].kind, GitIssueKind::Blocked);
    assert!(!view.state.items.contains_key(&child_item));
    let decision = merge_resolution(
        &a,
        f.owner.id,
        Some(Command::Create {
            item: document("明确重提的后续内容"),
        }),
    );
    a.resolve(Id::new_v4(), decision).unwrap();
    assert!(a.view().unwrap().issues.is_empty());
}

#[test]
fn three_independent_branches_converge_for_all_import_orders() {
    let mut f = F::new();
    let assignee = member("assignee", Role::Developer);
    let assignee_id = assignee.id;
    f.log
        .execute(
            f.device,
            request(f.owner.id, Command::AddMember { member: assignee }),
        )
        .unwrap();
    let patches = [
        Patch {
            priority: Some(1),
            ..Default::default()
        },
        Patch {
            severity: Some(2),
            ..Default::default()
        },
        Patch {
            assigned_to: Some(assignee_id),
            ..Default::default()
        },
    ];
    let mut branches = Vec::new();
    for patch in patches {
        let mut branch = f.log.clone();
        branch
            .execute(
                Id::new_v4(),
                request(f.owner.id, update(f.document, 1, patch)),
            )
            .unwrap();
        branches.push(branch);
    }
    let mut expected = None;
    for order in [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ] {
        let mut target = f.log.clone();
        for index in order {
            target.merge(&branches[index]).unwrap();
        }
        let view = target.view().unwrap();
        assert!(view.issues.is_empty());
        let data = encoded(&view);
        if let Some(expected) = &expected {
            assert_eq!(&data, expected);
        } else {
            expected = Some(data);
        }
    }
}

#[test]
fn removing_a_comment_and_replying_concurrently_preserves_both_proposals_as_conflicts() {
    let mut f = F::new();
    let comment = Id::new_v4();
    f.log
        .execute(
            f.device,
            request(
                f.owner.id,
                Command::AddComment {
                    id: comment,
                    item_id: f.document,
                    body: "原始评论".into(),
                    parent_id: None,
                    mentions: BTreeSet::new(),
                    attachments: BTreeSet::new(),
                },
            ),
        )
        .unwrap();
    let (mut a, mut b, da, db) = f.split();
    a.execute(
        da,
        request(
            f.owner.id,
            Command::DeleteComment {
                id: comment,
                version: 1,
            },
        ),
    )
    .unwrap();
    b.execute(
        db,
        request(
            f.owner.id,
            Command::AddComment {
                id: Id::new_v4(),
                item_id: f.document,
                body: "并发回复".into(),
                parent_id: Some(comment),
                mentions: BTreeSet::new(),
                attachments: BTreeSet::new(),
            },
        ),
    )
    .unwrap();
    let view = a.merge(&b).unwrap();
    assert_eq!(view.issues.len(), 2);
    assert!(!view.state.comments[&comment].deleted);
    assert_eq!(view.state.comments.len(), 1);
}

#[test]
fn cached_projection_and_cold_replay_match_for_a_longer_linear_history() {
    let mut f = F::new();
    for index in 0..128 {
        f.log
            .execute(
                f.device,
                request(
                    f.owner.id,
                    Command::Create {
                        item: document(&format!("线性文档 {index}")),
                    },
                ),
            )
            .unwrap();
    }
    let cached = encoded(&f.log.view().unwrap());
    let cold: GitLog = serde_json::from_value(encoded(&f.log)).unwrap();
    assert_eq!(encoded(&cold.view().unwrap()), cached);
    assert_eq!(f.log.operations().len(), 129);
    assert!(!encoded(&f.log).as_object().unwrap().contains_key("cache"));
}

#[test]
fn manager_can_resolve_but_cannot_bypass_the_replacement_commands_permissions() {
    let mut f = F::new();
    let manager = member("manager", Role::Manager);
    let actor = manager.id;
    f.log
        .execute(
            f.device,
            request(f.owner.id, Command::AddMember { member: manager }),
        )
        .unwrap();
    let (mut log, _, _) = f.conflict();
    let invalid = merge_resolution(
        &log,
        actor,
        Some(Command::AddMember {
            member: member("should-not-be-added", Role::Owner),
        }),
    );
    let before = encoded(&log);
    assert!(log.resolve(Id::new_v4(), invalid).is_err());
    assert_eq!(encoded(&log), before);
    let valid = merge_resolution(
        &log,
        actor,
        Some(update(
            f.document,
            1,
            Patch {
                title: Some("经理确认后的标题".into()),
                ..Default::default()
            },
        )),
    );
    log.resolve(Id::new_v4(), valid).unwrap();
    assert!(log.view().unwrap().issues.is_empty());
}

#[test]
fn nested_unknown_command_fields_cannot_disappear_during_deserialization() {
    let f = F::new();
    let id = f.log.operations().keys().next().unwrap().to_string();
    for target in ["request", "command", "item"] {
        let mut data = encoded(&f.log);
        let value = &mut data["operations"][&id]["change"]["request"];
        let object = match target {
            "request" => value,
            "command" => &mut value["command"],
            _ => &mut value["command"]["item"],
        };
        object["unimplemented_field"] = json!("不能被静默忽略");
        assert!(serde_json::from_value::<GitLog>(data).is_err());
    }
}

#[test]
fn retrying_a_conflicted_original_or_decision_returns_conflict_without_new_records() {
    let f = F::new();
    let (mut log, left, _) = f.conflict();
    let original = match &log.operations()[&left].change {
        GitChange::Apply { request } => request.clone(),
        _ => unreachable!(),
    };
    let before = encoded(&log);
    assert!(log.execute(Id::new_v4(), original).is_err());
    assert_eq!(encoded(&log), before);
    let mut other = log.clone();
    let a = merge_resolution(&log, f.owner.id, None);
    let b = merge_resolution(&other, f.owner.id, None);
    log.resolve(Id::new_v4(), a.clone()).unwrap();
    other.resolve(Id::new_v4(), b).unwrap();
    log.merge(&other).unwrap();
    let before = encoded(&log);
    assert!(log.resolve(Id::new_v4(), a).is_err());
    assert_eq!(encoded(&log), before);
    let existing = log.operations().keys().next().copied().unwrap();
    let mut invalid = merge_resolution(&log, f.owner.id, None);
    invalid.id = existing;
    assert!(log.resolve(Id::new_v4(), invalid).is_err());
}
