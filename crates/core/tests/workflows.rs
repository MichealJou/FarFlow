use farflow_core::*;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

struct F {
    backing: Option<(tempfile::TempDir, LocalWorkspace)>,
    git: Option<(GitLog, Id)>,
    repository: Option<(tempfile::TempDir, GitWorkspace)>,
    state: State,
    owner: Id,
    dev: Id,
    tester: Id,
    viewer: Id,
    product: Id,
    project: Id,
    iteration: Id,
}

enum Backend {
    Memory,
    Sqlite,
    Git,
    Repository,
}

fn member(name: &str, roles: &[Role]) -> Member {
    Member {
        id: Id::new_v4(),
        account: name.into(),
        name: name.into(),
        roles: roles.iter().copied().collect(),
        department: "研发".into(),
        active: true,
    }
}

impl F {
    fn new() -> Self {
        Self::with_storage(false)
    }
    fn with_storage(persisted: bool) -> Self {
        Self::with_backend(if persisted {
            Backend::Sqlite
        } else {
            Backend::Memory
        })
    }
    fn with_backend(backend: Backend) -> Self {
        let owner = member("owner", &[Role::Owner]);
        let mut f = Self {
            backing: None,
            git: None,
            repository: None,
            state: State::new(Id::new_v4(), "研流测试".into(), owner.clone()).unwrap(),
            owner: owner.id,
            dev: Id::nil(),
            tester: Id::nil(),
            viewer: Id::nil(),
            product: Id::nil(),
            project: Id::nil(),
            iteration: Id::nil(),
        };
        if matches!(backend, Backend::Sqlite) {
            let directory = tempfile::tempdir().unwrap();
            let workspace = LocalWorkspace::create(
                directory.path().join("workspace"),
                f.state.id,
                f.state.name.clone(),
                owner.clone(),
            )
            .unwrap();
            f.backing = Some((directory, workspace));
        }
        if matches!(backend, Backend::Git) {
            f.git = Some((
                GitLog::new(f.state.id, f.state.name.clone(), owner.clone()).unwrap(),
                Id::new_v4(),
            ));
        }
        if matches!(backend, Backend::Repository) {
            let directory = tempfile::tempdir().unwrap();
            let workspace = GitWorkspace::create(
                directory.path().join("repo"),
                f.state.id,
                f.state.name.clone(),
                owner,
            )
            .unwrap();
            f.repository = Some((directory, workspace));
        }
        for (name, role) in [
            ("dev", Role::Developer),
            ("tester", Role::Tester),
            ("viewer", Role::Viewer),
        ] {
            let m = member(name, &[role]);
            let id = m.id;
            f.ok(Command::AddMember { member: m });
            match role {
                Role::Developer => f.dev = id,
                Role::Tester => f.tester = id,
                _ => f.viewer = id,
            }
        }
        f.product = f.create(Kind::Product, "产品");
        f.project = f.create(Kind::Project, "项目");
        f.iteration = f.create(Kind::Iteration, "迭代");
        f
    }
    fn request(&self, actor: Id, command: Command) -> Request {
        Request {
            id: Id::new_v4(),
            actor_id: actor,
            at: self.state.revision as i64 + 100,
            command,
        }
    }
    fn as_actor(&mut self, actor: Id, command: Command) -> Result<Receipt> {
        let request = self.request(actor, command);
        if let Some((_, workspace)) = &mut self.backing {
            let result = workspace.execute(request);
            self.state = workspace.snapshot(self.owner).unwrap();
            result
        } else if let Some((log, device)) = &mut self.git {
            let result = log.execute(*device, request);
            self.state = log.view().unwrap().state;
            result
        } else if let Some((_, workspace)) = &self.repository {
            let result = workspace.execute(request).map(|saved| saved.receipt);
            self.state = workspace.snapshot(self.owner).unwrap().state;
            result
        } else {
            self.state.execute(request)
        }
    }
    fn ok(&mut self, command: Command) -> Receipt {
        self.as_actor(self.owner, command).unwrap()
    }
    fn new_item(&self, kind: Kind, title: &str) -> NewItem {
        let product_id = if [
            Kind::Product,
            Kind::Project,
            Kind::Iteration,
            Kind::Document,
        ]
        .contains(&kind)
        {
            None
        } else {
            Some(self.product)
        };
        let project_id = if [Kind::Task, Kind::Iteration].contains(&kind) {
            Some(self.project)
        } else {
            None
        };
        let mut attributes = BTreeMap::new();
        match kind {
            Kind::Project => {
                attributes.insert("product_ids".into(), json!([self.product]));
            }
            Kind::TestCase => {
                attributes.insert(
                    "steps".into(),
                    json!([{"action":"打开页面","expected":"正确显示"}]),
                );
            }
            Kind::Bug => {
                for key in ["steps", "expected", "actual"] {
                    attributes.insert(key.into(), json!("可复现内容"));
                }
            }
            Kind::Build => {
                attributes.insert("version_name".into(), json!(title));
                attributes.insert("code_ref".into(), json!("a123456"));
            }
            _ => {}
        }
        NewItem {
            id: Id::new_v4(),
            kind,
            title: title.into(),
            description: "详细内容".into(),
            acceptance: "验收标准".into(),
            priority: 3,
            severity: 3,
            assigned_to: None,
            product_id,
            project_id,
            iteration_id: if kind == Kind::Task {
                Some(self.iteration)
            } else {
                None
            },
            parent_id: None,
            links: BTreeSet::new(),
            attributes,
        }
    }
    fn insert(&mut self, new: NewItem) -> Id {
        let id = new.id;
        self.ok(Command::Create { item: new });
        id
    }
    fn create(&mut self, kind: Kind, title: &str) -> Id {
        self.insert(self.new_item(kind, title))
    }
    fn version(&self, id: Id) -> u64 {
        self.state.items[&id].version
    }
    fn transition_as(&mut self, actor: Id, id: Id, target: &str) -> Result<Receipt> {
        self.as_actor(
            actor,
            Command::Transition {
                id,
                version: self.version(id),
                target: target.into(),
                note: "操作和交接说明".into(),
            },
        )
    }
    fn transition(&mut self, id: Id, target: &str) {
        self.transition_as(self.owner, id, target).unwrap();
    }
    fn retest_as(
        &mut self,
        actor: Id,
        id: Id,
        build_id: Option<Id>,
        run_id: Option<Id>,
        result: &str,
    ) -> Result<Receipt> {
        self.as_actor(
            actor,
            Command::RetestBug {
                id,
                version: self.version(id),
                build_id,
                run_id,
                result: result.into(),
                note: "按复现步骤核对实际结果，保存复测结论".into(),
            },
        )
    }
    fn resolve_fixed(&mut self, id: Id, build: Id) -> Receipt {
        self.as_actor(
            self.dev,
            Command::ResolveBug {
                id,
                version: self.version(id),
                resolution: "fixed".into(),
                build_id: Some(build),
                duplicate_id: None,
                note: "修复内容已进入所选构建".into(),
            },
        )
        .unwrap()
    }
    fn approve(&mut self, id: Id) {
        self.transition(id, "reviewing");
        self.ok(Command::Review {
            id,
            version: self.version(id),
            decision: ReviewDecision::Approve,
            note: "内容和标准明确".into(),
        });
    }
    fn update(&mut self, id: Id, patch: Patch) -> Result<Receipt> {
        self.as_actor(
            self.owner,
            Command::Update {
                id,
                version: self.version(id),
                patch,
            },
        )
    }
    fn requirement(&mut self) -> Id {
        let id = self.create(Kind::Requirement, "离线需求");
        self.approve(id);
        id
    }
    fn case(&mut self, requirement: Id) -> Id {
        let mut new = self.new_item(Kind::TestCase, "验证离线功能");
        new.links.insert(Link {
            target: requirement,
            relation: Relation::Verifies,
            content_version: None,
        });
        let id = self.insert(new);
        self.approve(id);
        id
    }
    fn build(&mut self, name: &str) -> Id {
        let id = self.create(Kind::Build, name);
        self.transition(id, "ready");
        id
    }
    fn plan(&mut self, build: Id, case: Id) -> Id {
        let mut new = self.new_item(Kind::TestPlan, "回归测试单");
        new.attributes = BTreeMap::from([
            ("build_id".into(), json!(build)),
            ("case_ids".into(), json!([case])),
        ]);
        let id = self.insert(new);
        self.transition(id, "doing");
        id
    }
    fn run(&mut self, plan: Id, case: Id, result: &str) -> Id {
        let id = Id::new_v4();
        let version = self.state.items[&plan].attributes["case_versions"][case.to_string()]
            .as_u64()
            .unwrap();
        self.as_actor(
            self.tester,
            Command::RunTest {
                id,
                plan_id: plan,
                case_id: case,
                case_version: version,
                result: result.into(),
                actual: "实际操作结果记录".into(),
            },
        )
        .unwrap();
        id
    }
    fn accept(&mut self, id: Id) {
        self.ok(Command::AcceptRequirement {
            id,
            version: self.version(id),
            accepted: true,
            note: "逐项验收通过".into(),
        });
    }
    fn release(&mut self, req: Id, build: Id, plan: Id) -> Id {
        let mut new = self.new_item(Kind::Release, "首版发布");
        new.attributes = BTreeMap::from([
            ("version_name".into(), json!("0.1.0")),
            ("build_ids".into(), json!([build])),
            ("test_plan_ids".into(), json!([plan])),
        ]);
        new.links.insert(Link {
            target: req,
            relation: Relation::Contains,
            content_version: None,
        });
        let id = self.insert(new);
        self.transition(id, "wait");
        id
    }
    fn publish(&mut self, id: Id) -> Result<Receipt> {
        self.as_actor(
            self.owner,
            Command::Publish {
                id,
                version: self.version(id),
                note: "发布范围已核对".into(),
            },
        )
    }
    fn ready_release(&mut self) -> (Id, Id, Id, Id) {
        let req = self.requirement();
        let case = self.case(req);
        let build = self.build("rc.1");
        let plan = self.plan(build, case);
        self.run(plan, case, "pass");
        self.transition(plan, "done");
        self.accept(req);
        let release = self.release(req, build, plan);
        (release, req, plan, case)
    }
}

#[test]
fn complete_delivery_flow_preserves_history_and_snapshots() {
    complete_delivery(F::new());
}

#[test]
fn complete_delivery_flow_is_persisted_in_sqlite() {
    complete_delivery(F::with_storage(true));
}

#[test]
fn complete_delivery_flow_replays_from_git_operations() {
    complete_delivery(F::with_backend(Backend::Git));
}

#[test]
fn complete_delivery_flow_survives_a_real_repository_and_bundle_roundtrip() {
    complete_delivery(F::with_backend(Backend::Repository));
}

fn complete_delivery(mut f: F) {
    let inbox = f.create(Kind::Inbox, "初始化三种模式");
    let req = Id::new_v4();
    f.ok(Command::ConvertInbox {
        id: inbox,
        version: f.version(inbox),
        new_id: req,
        kind: Kind::Requirement,
        acceptance: "三种模式可以初始化和重开".into(),
    });
    assert_eq!(f.state.items[&inbox].status, "converted");
    f.approve(req);
    let mut task = f.new_item(Kind::Task, "实现初始化");
    task.assigned_to = Some(f.dev);
    task.links.insert(Link {
        target: req,
        relation: Relation::Implements,
        content_version: None,
    });
    let task = f.insert(task);
    assert_eq!(f.state.stage(req).unwrap(), "planned");
    f.transition_as(f.dev, task, "doing").unwrap();
    assert_eq!(f.state.stage(req).unwrap(), "developing");
    f.transition_as(f.dev, task, "verify").unwrap();
    assert_eq!(f.state.stage(req).unwrap(), "testing");
    assert!(f.transition_as(f.dev, task, "closed").is_err());
    f.transition_as(f.tester, task, "closed").unwrap();
    assert_eq!(f.state.stage(req).unwrap(), "tested");
    let case = f.case(req);
    let first = f.build("rc.1");
    let plan = f.plan(first, case);
    let run = f.run(plan, case, "fail");
    let bug = Id::new_v4();
    f.as_actor(
        f.tester,
        Command::BugFromTest {
            id: bug,
            run_id: run,
            title: "初始化目录不可写时提示不清楚".into(),
            actual: "显示空白错误".into(),
            environment: "测试环境".into(),
        },
    )
    .unwrap();
    f.transition_as(f.dev, bug, "active").unwrap();
    f.transition_as(f.dev, bug, "doing").unwrap();
    let fixed = f.build("rc.2");
    f.as_actor(
        f.dev,
        Command::ResolveBug {
            id: bug,
            version: f.version(bug),
            resolution: "fixed".into(),
            build_id: Some(fixed),
            duplicate_id: None,
            note: "补充明确目录错误信息".into(),
        },
    )
    .unwrap();
    assert_eq!(f.state.metrics(&Filter::default()).pending_retest, 1);
    assert!(f.transition_as(f.dev, bug, "closed").is_err());
    assert!(f.transition_as(f.tester, bug, "closed").is_err());
    let retest = f.plan(fixed, case);
    let passed = f.run(retest, case, "pass");
    f.retest_as(f.tester, bug, Some(fixed), Some(passed), "pass")
        .unwrap();
    assert_eq!(f.state.items[&bug].status, "closed");
    assert_eq!(
        f.state.items[&bug].attributes["retest_records"][0]["run_id"],
        json!(passed)
    );
    f.transition(retest, "done");
    f.accept(req);
    assert_eq!(f.state.stage(req).unwrap(), "accepted");
    let release = f.release(req, fixed, retest);
    let mut links = f.state.items[&release].links.clone();
    links.insert(Link {
        target: bug,
        relation: Relation::Contains,
        content_version: None,
    });
    f.update(
        release,
        Patch {
            links: Some(links),
            ..Default::default()
        },
    )
    .unwrap();
    f.publish(release).unwrap();
    assert_eq!(f.state.stage(req).unwrap(), "released");
    let snapshot = f.state.items[&release].attributes["snapshot"].clone();
    assert_eq!(snapshot["items"][passed.to_string()]["status"], "pass");
    f.update(
        req,
        Patch {
            description: Some("后续需求变更".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(f.state.items[&req].status, "changing");
    assert_eq!(f.state.stage(req).unwrap(), "wait");
    assert!(!f.state.items[&req]
        .attributes
        .contains_key("acceptance_record"));
    assert_eq!(f.state.items[&release].attributes["snapshot"], snapshot);
    assert!(f
        .update(
            release,
            Patch {
                title: Some("偷偷改发布".into()),
                ..Default::default()
            }
        )
        .is_err());
    assert!(f
        .state
        .history
        .iter()
        .any(|a| a.object_id == bug && a.action == "resolve_bug"));
    let data = serde_json::to_vec(&f.state).unwrap();
    let reopened: State = serde_json::from_slice(&data).unwrap();
    assert_eq!(reopened.items[&release].attributes["snapshot"], snapshot);
    if let Some((directory, workspace)) = f.backing.take() {
        drop(workspace);
        let reopened = LocalWorkspace::open(directory.path().join("workspace")).unwrap();
        assert_eq!(
            serde_json::to_value(reopened.snapshot(f.owner).unwrap()).unwrap(),
            serde_json::to_value(&f.state).unwrap()
        );
        let backup = directory.path().join("delivery-backup");
        reopened.backup(f.owner, &backup).unwrap();
        let restored =
            LocalWorkspace::restore(&backup, directory.path().join("delivery-restored")).unwrap();
        assert_eq!(
            serde_json::to_value(restored.snapshot(f.owner).unwrap()).unwrap(),
            serde_json::to_value(&f.state).unwrap()
        );
        assert_eq!(
            restored.snapshot(f.owner).unwrap().items[&release].attributes["snapshot"],
            snapshot
        );
        let package = directory.path().join("delivery-copy");
        restored.export_copy(f.owner, &package).unwrap();
        let (git, _) = GitWorkspace::import_copy(
            &package,
            directory.path().join("delivery-git-copy"),
            Id::new_v4(),
        )
        .unwrap();
        let mut actual = git.snapshot(f.owner).unwrap().state;
        actual.id = f.state.id;
        assert_eq!(
            serde_json::to_string(&actual).unwrap(),
            serde_json::to_string(&f.state).unwrap()
        );
        let second = directory.path().join("delivery-git-package");
        git.export_copy(f.owner, &second).unwrap();
        let (mut local, _) = LocalWorkspace::import_copy(
            &second,
            directory.path().join("delivery-local-copy"),
            Id::new_v4(),
        )
        .unwrap();
        let mut actual = local.snapshot(f.owner).unwrap();
        actual.id = f.state.id;
        assert_eq!(
            serde_json::to_string(&actual).unwrap(),
            serde_json::to_string(&f.state).unwrap()
        );
        let original = local.snapshot(f.owner).unwrap();
        local
            .prepare_transfer(
                f.owner,
                Id::new_v4(),
                1_000_000,
                Mode::Git,
                directory.path().join("before-formal-transfer"),
            )
            .unwrap();
        let formal = directory.path().join("formal-transfer");
        local.export_transfer(f.owner, &formal).unwrap();
        let (target, _, receipt) = GitWorkspace::stage_transfer(
            &formal,
            directory.path().join("formal-git-target"),
            f.owner,
            1_000_001,
        )
        .unwrap();
        let proof = local
            .complete_transfer(f.owner, 1_000_002, &receipt)
            .unwrap();
        target
            .activate_transfer(f.owner, 1_000_003, &proof)
            .unwrap();
        assert_eq!(
            serde_json::to_string(&target.snapshot(f.owner).unwrap().state).unwrap(),
            serde_json::to_string(&original).unwrap()
        );
        target
            .execute(Request {
                id: Id::new_v4(),
                actor_id: f.owner,
                at: 1_000_003,
                command: Command::AddComment {
                    id: Id::new_v4(),
                    item_id: req,
                    body: "正式交接后继续协作".into(),
                    parent_id: None,
                    mentions: BTreeSet::new(),
                    attachments: BTreeSet::new(),
                },
            })
            .unwrap();
        assert_eq!(
            target.snapshot(f.owner).unwrap().state.items[&release].attributes["snapshot"],
            snapshot
        );
        assert!(local
            .execute(Request {
                id: Id::new_v4(),
                actor_id: f.owner,
                at: 1_000_004,
                command: Command::Create {
                    item: f.new_item(Kind::Document, "旧源不能继续写")
                }
            })
            .is_err());
    }
    if let Some((log, _)) = f.git.take() {
        let reopened: GitLog = serde_json::from_slice(&serde_json::to_vec(&log).unwrap()).unwrap();
        assert!(reopened.view().unwrap().issues.is_empty());
        assert_eq!(
            serde_json::to_value(reopened.view().unwrap().state).unwrap(),
            serde_json::to_value(&f.state).unwrap()
        );
    }
    if let Some((directory, workspace)) = f.repository.take() {
        let pack = directory.path().join("delivery.farflowpack");
        workspace.export_pack(f.owner, &pack).unwrap();
        drop(workspace);
        let reopened = GitWorkspace::open(directory.path().join("repo")).unwrap();
        assert_eq!(
            serde_json::to_value(reopened.snapshot(f.owner).unwrap().state).unwrap(),
            serde_json::to_value(&f.state).unwrap()
        );
        let copy = GitWorkspace::from_pack(&pack, directory.path().join("restored")).unwrap();
        assert_eq!(
            serde_json::to_value(copy.snapshot(f.owner).unwrap().state).unwrap(),
            serde_json::to_value(&f.state).unwrap()
        );
        let remote = directory.path().join("remote.git");
        let initialized = std::process::Command::new("git")
            .args(["init", "--bare", "--initial-branch=main"])
            .arg(&remote)
            .output()
            .unwrap();
        assert!(initialized.status.success());
        assert!(
            copy.sync(f.owner, remote.to_str().unwrap())
                .unwrap()
                .push
                .pushed
        );
        let fetched =
            GitWorkspace::from_remote(remote.to_str().unwrap(), directory.path().join("fetched"))
                .unwrap();
        assert_eq!(
            serde_json::to_value(fetched.snapshot(f.owner).unwrap().state).unwrap(),
            serde_json::to_value(&f.state).unwrap()
        );
    }
}

#[test]
fn public_model_serialization_defaults_and_error_messages() {
    for mode in [Mode::Local, Mode::Git, Mode::Service] {
        assert_eq!(
            serde_json::from_str::<Mode>(&serde_json::to_string(&mode).unwrap()).unwrap(),
            mode
        );
    }
    for kind in [
        Kind::Product,
        Kind::Module,
        Kind::Project,
        Kind::Iteration,
        Kind::Inbox,
        Kind::Requirement,
        Kind::Plan,
        Kind::Task,
        Kind::TestCase,
        Kind::TestPlan,
        Kind::TestRun,
        Kind::Build,
        Kind::Bug,
        Kind::Release,
        Kind::Document,
    ] {
        assert_eq!(
            serde_json::from_value::<Kind>(serde_json::to_value(kind).unwrap()).unwrap(),
            kind
        );
    }
    let item: NewItem =
        serde_json::from_value(json!({"id":Id::new_v4(),"kind":"product","title":"产品"})).unwrap();
    assert_eq!(item.priority, 3);
    assert!(item.attributes.is_empty());
    let patch: Patch = serde_json::from_value(json!({"clear_assignee":true})).unwrap();
    assert!(patch.clear_assignee);
    for error in [
        Error::Validation("字段".into()),
        Error::NotFound("对象".into()),
        Error::Conflict("版本".into()),
        Error::Storage("磁盘".into()),
        Error::Forbidden("角色".into()),
    ] {
        assert!(!error.to_string().is_empty());
    }
    assert!(serde_json::from_value::<Command>(json!({"type":"execute_arbitrary_sql"})).is_err());
}

#[test]
fn workspace_and_member_boundaries() {
    let mut owner = member("owner", &[Role::Owner]);
    assert!(State::new(Id::new_v4(), " ".into(), owner.clone()).is_err());
    assert!(State::new(Id::nil(), "空间".into(), owner.clone()).is_err());
    owner.active = false;
    assert!(State::new(Id::new_v4(), "空间".into(), owner.clone()).is_err());
    owner.active = true;
    owner.roles.clear();
    assert!(State::new(Id::new_v4(), "空间".into(), owner).is_err());
    let mut f = F::new();
    let original = serde_json::to_value(&f.state).unwrap();
    let mut last = f.state.members[&f.owner].clone();
    last.active = false;
    assert!(f
        .as_actor(f.owner, Command::UpdateMember { member: last })
        .is_err());
    assert_eq!(serde_json::to_value(&f.state).unwrap(), original);
    for modify in 0..6 {
        let mut m = member("new", &[Role::Developer]);
        match modify {
            0 => m.id = Id::nil(),
            1 => m.name = " ".into(),
            2 => m.account = "has space".into(),
            3 => m.roles.clear(),
            4 => m.account = "OWNER".into(),
            _ => m.department = "x".repeat(121),
        };
        assert!(f
            .as_actor(f.owner, Command::AddMember { member: m })
            .is_err());
    }
    let mut disabled = f.state.members[&f.dev].clone();
    disabled.active = false;
    f.ok(Command::UpdateMember { member: disabled });
    assert!(f
        .as_actor(
            f.dev,
            Command::Create {
                item: f.new_item(Kind::Document, "文档")
            }
        )
        .is_err());
    let other = member("second-owner", &[Role::Owner]);
    f.ok(Command::AddMember { member: other });
    let mut old = f.state.members[&f.owner].clone();
    old.roles = BTreeSet::from([Role::Viewer]);
    f.ok(Command::UpdateMember { member: old });
}

#[test]
fn roles_and_unknown_identities_are_enforced() {
    let mut f = F::new();
    for actor in [f.viewer, Id::new_v4()] {
        assert!(f
            .as_actor(
                actor,
                Command::Create {
                    item: f.new_item(Kind::Product, "产品")
                }
            )
            .is_err());
    }
    for actor in [f.dev, f.tester, f.viewer] {
        assert!(f
            .as_actor(
                actor,
                Command::AddMember {
                    member: member("other", &[Role::Viewer])
                }
            )
            .is_err());
    }
    assert!(f
        .as_actor(
            f.dev,
            Command::Create {
                item: f.new_item(Kind::Project, "越权项目")
            }
        )
        .is_err());
    assert!(f
        .as_actor(
            f.tester,
            Command::Create {
                item: f.new_item(Kind::Requirement, "越权需求")
            }
        )
        .is_err());
    let m = member("product", &[Role::Product]);
    let id = m.id;
    f.ok(Command::AddMember { member: m });
    f.as_actor(
        id,
        Command::Create {
            item: f.new_item(Kind::Requirement, "产品需求"),
        },
    )
    .unwrap();
    assert!(f
        .as_actor(
            f.owner,
            Command::UpdateMember {
                member: member("missing", &[Role::Viewer])
            }
        )
        .is_err());
}

#[test]
fn idempotency_conflicts_and_atomic_batches() {
    let mut f = F::new();
    let req = f.request(
        f.owner,
        Command::Create {
            item: f.new_item(Kind::Document, "幂等文档"),
        },
    );
    let first = f.state.execute(req.clone()).unwrap();
    let revision = f.state.revision;
    assert_eq!(f.state.execute(req.clone()).unwrap(), first);
    assert_eq!(f.state.revision, revision);
    let mut changed = req.clone();
    changed.at += 1;
    assert!(matches!(f.state.execute(changed), Err(Error::Conflict(_))));
    let mut invalid = req.clone();
    invalid.id = Id::nil();
    assert!(f.state.execute(invalid).is_err());
    let mut invalid = req.clone();
    invalid.id = Id::new_v4();
    invalid.at = -1;
    assert!(f.state.execute(invalid).is_err());
    let before = serde_json::to_value(&f.state).unwrap();
    let one = f.new_item(Kind::Document, "不应部分创建");
    let mut two = f.new_item(Kind::Document, "");
    two.title.clear();
    assert!(f
        .as_actor(
            f.owner,
            Command::Batch {
                commands: vec![Command::Create { item: one }, Command::Create { item: two }]
            }
        )
        .is_err());
    assert_eq!(serde_json::to_value(&f.state).unwrap(), before);
    assert!(f
        .as_actor(f.owner, Command::Batch { commands: vec![] })
        .is_err());
    assert!(f
        .as_actor(
            f.owner,
            Command::Batch {
                commands: (0..51)
                    .map(|_| Command::Create {
                        item: f.new_item(Kind::Document, "文档")
                    })
                    .collect()
            }
        )
        .is_err());
    let mut nested = Command::Create {
        item: f.new_item(Kind::Document, "嵌套"),
    };
    for _ in 0..6 {
        nested = Command::Batch {
            commands: vec![nested],
        };
    }
    assert!(f.as_actor(f.owner, nested).is_err());
    let id = f.create(Kind::Document, "版本校验");
    assert!(matches!(
        f.as_actor(
            f.owner,
            Command::Update {
                id,
                version: 999,
                patch: Patch::default()
            }
        ),
        Err(Error::Conflict(_))
    ));
    assert!(f.state.item(Id::new_v4()).is_err());
}

#[test]
fn creation_and_attribute_boundaries() {
    let mut f = F::new();
    for mode in 0..10 {
        let mut item = f.new_item(Kind::Requirement, "正常");
        match mode {
            0 => item.id = Id::nil(),
            1 => item.title = " ".into(),
            2 => item.title = "字".repeat(241),
            3 => item.description = "x".repeat(64001),
            4 => item.acceptance = "x".repeat(16001),
            5 => item.priority = 0,
            6 => item.severity = 5,
            7 => item.product_id = None,
            8 => item.assigned_to = Some(Id::new_v4()),
            _ => {
                item.attributes.insert("snapshot".into(), json!({}));
            }
        }
        assert!(
            f.as_actor(f.owner, Command::Create { item }).is_err(),
            "{mode}"
        );
    }
    let mut item = f.new_item(Kind::Document, "大字段");
    item.attributes
        .insert("large".into(), json!("a".repeat(128001)));
    assert!(f.as_actor(f.owner, Command::Create { item }).is_err());
    let id = f.create(Kind::Document, "文档");
    let mut dup = f.new_item(Kind::Document, "重复");
    dup.id = id;
    assert!(f.as_actor(f.owner, Command::Create { item: dup }).is_err());
    let mut run = f.new_item(Kind::TestRun, "伪造测试");
    run.attributes.clear();
    assert!(f.as_actor(f.owner, Command::Create { item: run }).is_err());
    assert!(f
        .update(
            id,
            Patch {
                attributes: Some(BTreeMap::from([(
                    "acceptance_record".into(),
                    json!({"accepted":true})
                )])),
                ..Default::default()
            }
        )
        .is_err());
}

#[test]
fn relationships_and_scope_are_validated() {
    let mut f = F::new();
    let document = f.create(Kind::Document, "其他");
    for mode in 0..8 {
        let mut item = f.new_item(Kind::Task, "关系");
        match mode {
            0 => item.product_id = Some(document),
            1 => item.project_id = Some(document),
            2 => item.iteration_id = Some(document),
            3 => item.parent_id = Some(document),
            4 => {
                item.links = BTreeSet::from([Link {
                    target: item.id,
                    relation: Relation::Related,
                    content_version: None,
                }])
            }
            5 => {
                item.links = BTreeSet::from([Link {
                    target: Id::new_v4(),
                    relation: Relation::Related,
                    content_version: None,
                }])
            }
            6 => item.project_id = None,
            _ => {
                item.links = BTreeSet::from([Link {
                    target: document,
                    relation: Relation::Implements,
                    content_version: None,
                }])
            }
        };
        assert!(
            f.as_actor(f.owner, Command::Create { item }).is_err(),
            "{mode}"
        );
    }
    let other_product = f.create(Kind::Product, "另一产品");
    let mut item = f.new_item(Kind::Task, "跨产品");
    item.product_id = Some(other_product);
    assert!(f.as_actor(f.owner, Command::Create { item }).is_err());
    let mut project = f.new_item(Kind::Project, "非法列表");
    for value in [
        json!([]),
        json!("bad"),
        json!(["not-an-id"]),
        json!([document]),
        json!([f.product, f.product]),
    ] {
        project.attributes.insert("product_ids".into(), value);
        assert!(f
            .as_actor(
                f.owner,
                Command::Create {
                    item: project.clone()
                }
            )
            .is_err());
    }
    let mut iteration = f.new_item(Kind::Iteration, "类型");
    iteration
        .attributes
        .insert("iteration_type".into(), json!("unknown"));
    assert!(f
        .as_actor(f.owner, Command::Create { item: iteration })
        .is_err());
    let req = f.requirement();
    let task = f.create(Kind::Task, "普通任务");
    assert!(f
        .update(
            task,
            Patch {
                links: Some(BTreeSet::from([Link {
                    target: req,
                    relation: Relation::Verifies,
                    content_version: None
                }])),
                ..Default::default()
            }
        )
        .is_err());
    assert!(f
        .update(
            task,
            Patch {
                links: Some(BTreeSet::from([Link {
                    target: req,
                    relation: Relation::KnownIssue,
                    content_version: None
                }])),
                ..Default::default()
            }
        )
        .is_err());
    assert!(f
        .update(
            task,
            Patch {
                links: Some(BTreeSet::from([Link {
                    target: req,
                    relation: Relation::Related,
                    content_version: Some(999)
                }])),
                ..Default::default()
            }
        )
        .is_err());
}

#[test]
fn task_dependencies_and_requirement_changes_block_stale_handoffs() {
    let mut f = F::new();
    let req = f.requirement();
    let first = f.create(Kind::Task, "前置任务");
    let mut new = f.new_item(Kind::Task, "依赖任务");
    new.links = BTreeSet::from([
        Link {
            target: first,
            relation: Relation::DependsOn,
            content_version: None,
        },
        Link {
            target: req,
            relation: Relation::Implements,
            content_version: None,
        },
    ]);
    let second = f.insert(new);
    assert!(f.transition_as(f.owner, second, "doing").is_err());
    assert!(f
        .update(
            first,
            Patch {
                links: Some(BTreeSet::from([Link {
                    target: second,
                    relation: Relation::DependsOn,
                    content_version: None
                }])),
                ..Default::default()
            }
        )
        .is_err());
    for status in ["doing", "verify", "closed"] {
        f.transition(first, status);
    }
    f.transition(second, "doing");
    f.update(
        req,
        Patch {
            description: Some("修改验收范围".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(f.state.items[&req].status, "changing");
    assert!(f.transition_as(f.owner, second, "verify").is_err());
    f.approve(req);
    let mut links = f.state.items[&second].links.clone();
    links.retain(|l| l.relation != Relation::Implements);
    links.insert(Link {
        target: req,
        relation: Relation::Implements,
        content_version: None,
    });
    f.update(
        second,
        Patch {
            links: Some(links),
            ..Default::default()
        },
    )
    .unwrap();
    f.transition(second, "verify");
    assert!(f
        .as_actor(
            f.owner,
            Command::AcceptRequirement {
                id: req,
                version: f.version(req),
                accepted: true,
                note: "验收".into()
            }
        )
        .is_err());
}

#[test]
fn review_and_acceptance_rules() {
    let mut f = F::new();
    let id = f.create(Kind::Requirement, "评审");
    assert!(f
        .as_actor(
            f.owner,
            Command::Review {
                id,
                version: f.version(id),
                decision: ReviewDecision::Approve,
                note: "未提交".into()
            }
        )
        .is_err());
    f.update(
        id,
        Patch {
            acceptance: Some(" ".into()),
            ..Default::default()
        },
    )
    .unwrap();
    f.transition(id, "reviewing");
    assert!(f
        .as_actor(
            f.owner,
            Command::Review {
                id,
                version: f.version(id),
                decision: ReviewDecision::Approve,
                note: "同意".into()
            }
        )
        .is_err());
    f.ok(Command::Review {
        id,
        version: f.version(id),
        decision: ReviewDecision::Reject,
        note: "补充验收标准".into(),
    });
    assert_eq!(f.state.items[&id].status, "draft");
    f.update(
        id,
        Patch {
            acceptance: Some("真实验收".into()),
            ..Default::default()
        },
    )
    .unwrap();
    f.approve(id);
    assert!(f.transition_as(f.owner, id, "closed").is_err());
    f.accept(id);
    f.transition(id, "closed");
    assert_eq!(f.state.stage(id).unwrap(), "closed");
    f.transition(id, "active");
    f.ok(Command::AcceptRequirement {
        id,
        version: f.version(id),
        accepted: false,
        note: "验收退回".into(),
    });
    assert_eq!(f.state.stage(id).unwrap(), "wait");
    assert!(f
        .as_actor(
            f.owner,
            Command::AcceptRequirement {
                id: f.product,
                version: f.version(f.product),
                accepted: true,
                note: "不适用".into()
            }
        )
        .is_err());
    assert!(f.state.stage(f.product).is_err());
}

#[test]
fn lifecycle_branches_are_explicit_and_notes_required() {
    let mut f = F::new();
    let task = f.create(Kind::Task, "状态");
    assert!(f.transition_as(f.owner, task, "closed").is_err());
    f.transition(task, "doing");
    f.transition(task, "pause");
    f.transition(task, "doing");
    assert!(f
        .as_actor(
            f.owner,
            Command::Transition {
                id: task,
                version: f.version(task),
                target: "verify".into(),
                note: " ".into()
            }
        )
        .is_err());
    f.transition(task, "cancel");
    f.transition(task, "doing");
    f.transition(task, "verify");
    f.transition(task, "doing");
    f.transition(task, "verify");
    f.transition(task, "closed");
    f.transition(task, "doing");
    for id in [f.project, f.iteration] {
        f.transition(id, "doing");
        f.transition(id, "suspended");
        f.transition(id, "doing");
        assert!(f.transition_as(f.owner, id, "closed").is_err());
    }
    f.transition(task, "cancel");
    f.transition(f.iteration, "closed");
    f.transition(f.project, "closed");
    f.transition(f.project, "doing");
    let plan = f.create(Kind::Plan, "计划");
    for status in ["doing", "done", "doing"] {
        f.transition(plan, status);
    }
    let inbox = f.create(Kind::Inbox, "收集");
    for status in ["clarifying", "accepted", "clarifying", "rejected", "new"] {
        f.transition(inbox, status);
    }
    assert!(f.transition_as(f.owner, f.product, "closed").is_err());
}

#[test]
fn build_and_case_boundaries() {
    let mut f = F::new();
    let mut b = f.new_item(Kind::Build, "缺少提交");
    b.attributes.clear();
    let b = f.insert(b);
    assert!(f.transition_as(f.owner, b, "ready").is_err());
    let b2 = f.build("固化构建");
    assert!(f
        .update(
            b2,
            Patch {
                title: Some("偷偷修改".into()),
                ..Default::default()
            }
        )
        .is_err());
    for value in [
        json!([]),
        json!("bad"),
        json!([{"action":"","expected":"结果"}]),
        json!([{"action":"动作"}]),
    ] {
        let mut case = f.new_item(Kind::TestCase, "错误步骤");
        case.attributes.insert("steps".into(), value);
        assert!(f.as_actor(f.owner, Command::Create { item: case }).is_err());
    }
    let req = f.requirement();
    let case = f.case(req);
    f.update(
        case,
        Patch {
            title: Some("新版用例".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(f.state.items[&case].status, "draft");
    f.transition(case, "reviewing");
    f.ok(Command::Review {
        id: case,
        version: f.version(case),
        decision: ReviewDecision::Change,
        note: "退回补充".into(),
    });
    assert_eq!(f.state.items[&case].status, "draft");
}

#[test]
fn test_runs_pin_old_versions_and_cannot_be_forged() {
    let mut f = F::new();
    let req = f.requirement();
    let case = f.case(req);
    let build = f.build("测试构建");
    let plan = f.plan(build, case);
    f.update(
        case,
        Patch {
            title: Some("用例新版".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let run = f.run(plan, case, "fail");
    assert_eq!(f.state.items[&run].title, "验证离线功能");
    assert_eq!(f.state.items[&run].attributes["case_version"], 1);
    assert!(f
        .update(
            run,
            Patch {
                title: Some("伪造结果".into()),
                ..Default::default()
            }
        )
        .is_err());
    for (case_id, version, result, actual) in [
        (case, 2, "pass", "记录"),
        (Id::new_v4(), 1, "pass", "记录"),
        (case, 1, "unknown", "记录"),
        (case, 1, "pass", ""),
    ] {
        assert!(f
            .as_actor(
                f.tester,
                Command::RunTest {
                    id: Id::new_v4(),
                    plan_id: plan,
                    case_id,
                    case_version: version,
                    result: result.into(),
                    actual: actual.into()
                }
            )
            .is_err());
    }
    assert!(f
        .update(
            plan,
            Patch {
                attributes: Some(BTreeMap::from([("case_ids".into(), json!([]))])),
                ..Default::default()
            }
        )
        .is_err());
    assert!(f
        .as_actor(
            f.dev,
            Command::RunTest {
                id: Id::new_v4(),
                plan_id: plan,
                case_id: case,
                case_version: 1,
                result: "pass".into(),
                actual: "记录".into()
            }
        )
        .is_err());
    f.transition(plan, "done");
    assert!(f
        .as_actor(
            f.tester,
            Command::RunTest {
                id: Id::new_v4(),
                plan_id: plan,
                case_id: case,
                case_version: 1,
                result: "pass".into(),
                actual: "记录".into()
            }
        )
        .is_err());
}

#[test]
fn test_plan_validation_and_pending_scope_repin() {
    let mut f = F::new();
    let req = f.requirement();
    let case = f.case(req);
    let build = f.build("构建");
    for attrs in [
        BTreeMap::new(),
        BTreeMap::from([("build_id".into(), json!(build))]),
        BTreeMap::from([
            ("build_id".into(), json!(f.product)),
            ("case_ids".into(), json!([case])),
        ]),
        BTreeMap::from([
            ("build_id".into(), json!(build)),
            ("case_ids".into(), json!([f.product])),
        ]),
    ] {
        let mut new = f.new_item(Kind::TestPlan, "无效测试单");
        new.attributes = attrs;
        assert!(f.as_actor(f.owner, Command::Create { item: new }).is_err());
    }
    let mut new = f.new_item(Kind::TestPlan, "待启动测试");
    new.attributes = BTreeMap::from([
        ("build_id".into(), json!(build)),
        ("case_ids".into(), json!([case])),
    ]);
    let plan = f.insert(new);
    let case2 = f.case(req);
    f.update(
        plan,
        Patch {
            attributes: Some(BTreeMap::from([("case_ids".into(), json!([case, case2]))])),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(f.state.items[&plan].attributes["case_versions"]
        .get(case2.to_string())
        .is_some());
    f.transition(plan, "doing");
    assert!(f.transition_as(f.owner, plan, "done").is_err());
    f.run(plan, case, "pass");
    assert!(f.transition_as(f.owner, plan, "done").is_err());
    f.run(plan, case2, "skipped");
    f.transition(plan, "done");
}

#[test]
fn manual_bug_retests_are_atomic_audited_idempotent_and_preserve_each_attempt() {
    let mut f = F::new();
    let bug = f.create(Kind::Bug, "手工提交缺陷");
    let build = f.build("fix.1");
    assert!(f
        .retest_as(f.tester, bug, Some(build), None, "pass")
        .is_err());
    f.transition(bug, "active");
    let fixed = f.resolve_fixed(bug, build);
    let before = serde_json::to_value(&f.state).unwrap();
    for actor in [f.dev, f.viewer] {
        assert!(f.retest_as(actor, bug, Some(build), None, "pass").is_err());
    }
    for (target, selected, result, note, version) in [
        (bug, None, "pass", "确认", f.version(bug)),
        (bug, Some(f.product), "pass", "确认", f.version(bug)),
        (bug, Some(build), "skipped", "确认", f.version(bug)),
        (bug, Some(build), "pass", " ", f.version(bug)),
        (bug, Some(build), "pass", "确认", f.version(bug) - 1),
        (f.product, Some(build), "pass", "确认", f.version(f.product)),
    ] {
        assert!(f
            .as_actor(
                f.tester,
                Command::RetestBug {
                    id: target,
                    version,
                    build_id: selected,
                    run_id: None,
                    result: result.into(),
                    note: note.into(),
                }
            )
            .is_err());
    }
    assert_eq!(serde_json::to_value(&f.state).unwrap(), before);
    let request = f.request(
        f.tester,
        Command::RetestBug {
            id: bug,
            version: f.version(bug),
            build_id: Some(build),
            run_id: None,
            result: "blocked".into(),
            note: "环境不可用，保留待复测".into(),
        },
    );
    let serialized: Request =
        serde_json::from_value(serde_json::to_value(&request).unwrap()).unwrap();
    let receipt = f.state.execute(serialized).unwrap();
    assert_eq!(f.state.execute(request.clone()).unwrap(), receipt);
    let after = serde_json::to_value(&f.state).unwrap();
    let mut changed = request;
    if let Command::RetestBug { result, .. } = &mut changed.command {
        *result = "pass".into();
    }
    assert!(matches!(f.state.execute(changed), Err(Error::Conflict(_))));
    assert_eq!(serde_json::to_value(&f.state).unwrap(), after);
    assert_eq!(f.state.items[&bug].status, "resolved");
    let record = f.state.items[&bug].attributes["retest_records"][0].clone();
    assert_eq!(
        record["resolution_record"]["operation_id"],
        json!(fixed.operation_id)
    );
    assert_eq!(record["actor_id"], json!(f.tester));
    let audit = f.state.history.last().unwrap();
    assert_eq!(audit.action, "retest_bug");
    assert_eq!(audit.note, "环境不可用，保留待复测");
    f.retest_as(f.tester, bug, Some(build), None, "fail")
        .unwrap();
    assert_eq!(f.state.items[&bug].status, "active");
    assert_eq!(f.state.items[&bug].attributes["reopen_count"], 1);
    assert!(!f.state.items[&bug]
        .attributes
        .contains_key("resolved_build"));
    let second = f.build("fix.2");
    f.resolve_fixed(bug, second);
    assert!(f
        .retest_as(f.tester, bug, Some(build), None, "pass")
        .is_err());
    f.retest_as(f.tester, bug, Some(second), None, "pass")
        .unwrap();
    assert_eq!(f.state.items[&bug].status, "closed");
    let records = f.state.items[&bug].attributes["retest_records"]
        .as_array()
        .unwrap();
    assert_eq!(records.len(), 3);
    assert_eq!(records[0], record);
    assert_eq!(records[2]["build_id"], json!(second));
    assert!(f
        .retest_as(f.tester, bug, Some(second), None, "pass")
        .is_err());
    assert!(f
        .update(
            bug,
            Patch {
                description: Some("改变复现事实".into()),
                ..Default::default()
            }
        )
        .is_err());
    f.transition(bug, "active");
    assert_eq!(f.state.items[&bug].attributes["retest_records"][0], record);
    assert!(f
        .update(
            bug,
            Patch {
                attributes: Some(BTreeMap::from([("retest_records".into(), json!([]))])),
                ..Default::default()
            }
        )
        .is_err());
    let mut forged = f.new_item(Kind::Bug, "伪造复测记录");
    forged.attributes.insert("retest_records".into(), json!([]));
    assert!(f
        .as_actor(f.owner, Command::Create { item: forged })
        .is_err());
}

#[test]
fn test_origin_bugs_require_matching_fresh_and_latest_regression_evidence() {
    let mut f = F::new();
    let requirement = f.requirement();
    let case = f.case(requirement);
    let first = f.build("rc.1");
    let plan = f.plan(first, case);
    let failed = f.run(plan, case, "fail");
    let bug = Id::new_v4();
    f.as_actor(
        f.tester,
        Command::BugFromTest {
            id: bug,
            run_id: failed,
            title: "执行失败".into(),
            actual: "结果不符".into(),
            environment: "测试环境".into(),
        },
    )
    .unwrap();
    assert!(f
        .update(
            bug,
            Patch {
                links: Some(BTreeSet::new()),
                ..Default::default()
            }
        )
        .is_err());
    f.transition(bug, "active");
    let fixed = f.build("rc.2");
    let regression = f.plan(fixed, case);
    let old_pass = f.run(regression, case, "pass");
    f.resolve_fixed(bug, fixed);
    let other_case = f.case(requirement);
    let other_plan = f.plan(fixed, other_case);
    let unrelated = f.run(other_plan, other_case, "pass");
    let before = serde_json::to_value(&f.state).unwrap();
    for (build, run) in [
        (fixed, None),
        (fixed, Some(old_pass)),
        (fixed, Some(unrelated)),
        (fixed, Some(failed)),
        (first, Some(old_pass)),
        (fixed, Some(f.product)),
    ] {
        assert!(f
            .retest_as(f.tester, bug, Some(build), run, "pass")
            .is_err());
    }
    assert_eq!(serde_json::to_value(&f.state).unwrap(), before);
    let blocked = f.run(regression, case, "blocked");
    f.retest_as(f.tester, bug, Some(fixed), Some(blocked), "blocked")
        .unwrap();
    assert_eq!(f.state.items[&bug].status, "resolved");
    let passed = f.run(regression, case, "pass");
    let failed_again = f.run(regression, case, "fail");
    assert!(f
        .retest_as(f.tester, bug, Some(fixed), Some(passed), "pass")
        .is_err());
    assert!(f
        .retest_as(f.tester, bug, Some(fixed), Some(failed_again), "pass")
        .is_err());
    f.retest_as(f.tester, bug, Some(fixed), Some(failed_again), "fail")
        .unwrap();
    assert_eq!(f.state.items[&bug].status, "active");
    f.resolve_fixed(bug, fixed);
    assert!(f
        .retest_as(f.tester, bug, Some(fixed), Some(failed_again), "fail")
        .is_err());
    let final_run = f.run(regression, case, "pass");
    f.retest_as(f.tester, bug, Some(fixed), Some(final_run), "pass")
        .unwrap();
    assert_eq!(f.state.items[&bug].status, "closed");
    assert_eq!(
        f.state.items[&bug].attributes["retest_records"][2]["run_id"],
        json!(final_run)
    );
}

#[test]
fn non_fixed_resolutions_are_confirmed_without_inventing_a_fixed_build() {
    let mut f = F::new();
    let bug = f.create(Kind::Bug, "设计确认");
    f.transition(bug, "active");
    let build = f.build("不应冒充修复");
    f.ok(Command::ResolveBug {
        id: bug,
        version: f.version(bug),
        resolution: "by_design".into(),
        build_id: None,
        duplicate_id: None,
        note: "符合批准的原需求".into(),
    });
    assert!(f
        .retest_as(f.tester, bug, Some(build), None, "pass")
        .is_err());
    assert!(f
        .retest_as(f.tester, bug, None, Some(build), "pass")
        .is_err());
    assert!(f.transition_as(f.owner, bug, "closed").is_err());
    assert!(f
        .update(
            bug,
            Patch {
                description: Some("改写处理依据".into()),
                ..Default::default()
            }
        )
        .is_err());
    f.retest_as(f.tester, bug, None, None, "blocked").unwrap();
    f.retest_as(f.tester, bug, None, None, "fail").unwrap();
    assert_eq!(f.state.items[&bug].status, "active");
    f.ok(Command::ResolveBug {
        id: bug,
        version: f.version(bug),
        resolution: "by_design".into(),
        build_id: None,
        duplicate_id: None,
        note: "补充设计证据".into(),
    });
    f.retest_as(f.tester, bug, None, None, "pass").unwrap();
    let record = &f.state.items[&bug].attributes["retest_records"][2];
    assert_eq!(record["resolution"], "by_design");
    assert!(record["build_id"].is_null());
    assert!(record["run_id"].is_null());
}

#[test]
fn bug_resolution_retest_reopen_and_duplicates() {
    let mut f = F::new();
    let bug = f.create(Kind::Bug, "缺陷");
    assert!(f
        .as_actor(
            f.dev,
            Command::ResolveBug {
                id: bug,
                version: f.version(bug),
                resolution: "fixed".into(),
                build_id: None,
                duplicate_id: None,
                note: "修复".into()
            }
        )
        .is_err());
    f.transition(bug, "active");
    for (resolution, build, duplicate, note) in [
        ("fixed", None, None, "修复"),
        ("fixed", Some(f.product), None, "修复"),
        ("duplicate", None, None, "重复"),
        ("duplicate", None, Some(bug), "重复"),
        ("unknown", None, None, "处理"),
        ("will_not_fix", None, None, ""),
        ("will_not_fix", Some(f.product), None, "不应携带构建"),
        ("external", None, Some(bug), "不应携带重复对象"),
    ] {
        assert!(f
            .as_actor(
                f.dev,
                Command::ResolveBug {
                    id: bug,
                    version: f.version(bug),
                    resolution: resolution.into(),
                    build_id: build,
                    duplicate_id: duplicate,
                    note: note.into()
                }
            )
            .is_err());
    }
    let build = f.build("修复构建");
    f.as_actor(
        f.dev,
        Command::ResolveBug {
            id: bug,
            version: f.version(bug),
            resolution: "fixed".into(),
            build_id: Some(build),
            duplicate_id: None,
            note: "修复说明".into(),
        },
    )
    .unwrap();
    f.transition(bug, "active");
    assert_eq!(f.state.items[&bug].attributes["reopen_count"], 1);
    assert!(!f.state.items[&bug].attributes.contains_key("resolution"));
    let other = f.create(Kind::Bug, "原缺陷");
    f.transition(other, "active");
    f.ok(Command::ResolveBug {
        id: bug,
        version: f.version(bug),
        resolution: "duplicate".into(),
        build_id: None,
        duplicate_id: Some(other),
        note: "原缺陷相同".into(),
    });
    assert!(f
        .as_actor(
            f.owner,
            Command::ResolveBug {
                id: other,
                version: f.version(other),
                resolution: "duplicate".into(),
                build_id: None,
                duplicate_id: Some(bug),
                note: "循环引用".into()
            }
        )
        .is_err());
    f.retest_as(f.tester, bug, None, None, "pass").unwrap();
    f.transition(bug, "active");
    assert_eq!(f.state.items[&bug].attributes["reopen_count"], 2);
    for resolution in [
        "by_design",
        "external",
        "not_reproducible",
        "postponed",
        "will_not_fix",
        "to_requirement",
    ] {
        f.ok(Command::ResolveBug {
            id: bug,
            version: f.version(bug),
            resolution: resolution.into(),
            build_id: None,
            duplicate_id: None,
            note: "处理依据".into(),
        });
        f.transition(bug, "active");
    }
}

#[test]
fn release_requires_current_acceptance_tests_and_immutable_builds() {
    let mut f = F::new();
    let (release, req, plan, case) = f.ready_release();
    f.ok(Command::AcceptRequirement {
        id: req,
        version: f.version(req),
        accepted: false,
        note: "退回验收".into(),
    });
    assert!(f.publish(release).is_err());
    f.accept(req);
    f.transition(plan, "doing");
    f.run(plan, case, "blocked");
    f.transition(plan, "done");
    assert!(f.publish(release).is_err());
    f.transition(plan, "doing");
    f.run(plan, case, "pass");
    f.transition(plan, "done");
    f.publish(release).unwrap();
    f.transition(release, "terminate");
    assert_eq!(f.state.stage(req).unwrap(), "released");
    assert!(f.publish(release).is_err());
}

#[test]
fn release_missing_scope_and_known_issue_rules() {
    let mut f = F::new();
    let req = f.requirement();
    f.accept(req);
    let release = f.create(Kind::Release, "无范围发布");
    f.transition(release, "wait");
    assert!(f.publish(release).is_err());
    let (release, _, plan, _) = f.ready_release();
    let build = Id::parse_str(
        f.state.items[&plan].attributes["build_id"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let mut new = f.new_item(Kind::Bug, "阻塞性遗留");
    new.severity = 1;
    new.attributes
        .insert("affected_build_ids".into(), json!([build]));
    let bug = f.insert(new);
    assert!(f.publish(release).is_err());
    let mut links = f.state.items[&release].links.clone();
    links.insert(Link {
        target: bug,
        relation: Relation::KnownIssue,
        content_version: None,
    });
    f.update(
        release,
        Patch {
            links: Some(links),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(f.publish(release).is_err());
    f.update(
        release,
        Patch {
            attributes: Some(BTreeMap::from([(
                "known_issue_notes".into(),
                json!({bug.to_string():"限制影响已评估并明确交付"}),
            )])),
            ..Default::default()
        },
    )
    .unwrap();
    f.publish(release).unwrap();
}

#[test]
fn comments_replies_attachments_and_history() {
    let mut f = F::new();
    let item = f.create(Kind::Document, "评论附件");
    let attachment = Id::new_v4();
    f.ok(Command::AddAttachment {
        id: attachment,
        item_id: item,
        name: "证据.png".into(),
        mime: "image/png".into(),
        sha256: "a".repeat(64),
        size: 128,
    });
    let first = Id::new_v4();
    f.as_actor(
        f.dev,
        Command::AddComment {
            id: first,
            item_id: item,
            body: "请复核 @tester".into(),
            parent_id: None,
            mentions: BTreeSet::from([f.tester]),
            attachments: BTreeSet::from([attachment]),
        },
    )
    .unwrap();
    let reply = Id::new_v4();
    f.as_actor(
        f.tester,
        Command::AddComment {
            id: reply,
            item_id: item,
            body: "验证通过".into(),
            parent_id: Some(first),
            mentions: BTreeSet::new(),
            attachments: BTreeSet::new(),
        },
    )
    .unwrap();
    assert!(f
        .as_actor(
            f.tester,
            Command::EditComment {
                id: first,
                version: 1,
                body: "篡改他人".into()
            }
        )
        .is_err());
    f.as_actor(
        f.dev,
        Command::EditComment {
            id: first,
            version: 1,
            body: "补充说明".into(),
        },
    )
    .unwrap();
    assert!(f
        .as_actor(
            f.dev,
            Command::DeleteComment {
                id: first,
                version: 1
            }
        )
        .is_err());
    f.as_actor(
        f.dev,
        Command::DeleteComment {
            id: first,
            version: 2,
        },
    )
    .unwrap();
    assert!(f.state.comments[&first].deleted);
    assert_eq!(f.state.comments[&reply].parent_id, Some(first));
    assert!(f
        .as_actor(f.tester, Command::RemoveAttachment { id: attachment })
        .is_err());
    f.ok(Command::RemoveAttachment { id: attachment });
    assert!(f.state.attachments[&attachment].deleted);
    assert!(f.state.history.iter().any(|a| a.object_id == first
        && a.before["body"] == "请复核 @tester"
        && a.after["body"] == "补充说明"));
}

#[test]
fn comment_and_attachment_boundaries_are_atomic() {
    let mut f = F::new();
    let item = f.create(Kind::Document, "附件边界");
    for mode in 0..8 {
        let mut name = "证据.txt".to_owned();
        let mut hash = "a".repeat(64);
        let mut size = 1;
        let mut mime = "text/plain".to_owned();
        match mode {
            0 => name = "".into(),
            1 => name = "../file".into(),
            2 => hash = "A".repeat(64),
            3 => hash = "z".repeat(64),
            4 => size = 0,
            5 => size = 10 * 1024 * 1024 + 1,
            6 => mime.clear(),
            _ => hash = "a".repeat(63),
        };
        assert!(f
            .as_actor(
                f.owner,
                Command::AddAttachment {
                    id: Id::new_v4(),
                    item_id: item,
                    name,
                    mime,
                    sha256: hash,
                    size
                }
            )
            .is_err());
    }
    for (body, parent, mentions, attachments) in [
        ("", None, BTreeSet::new(), BTreeSet::new()),
        ("正文", Some(Id::new_v4()), BTreeSet::new(), BTreeSet::new()),
        (
            "正文",
            None,
            BTreeSet::from([Id::new_v4()]),
            BTreeSet::new(),
        ),
        (
            "正文",
            None,
            BTreeSet::new(),
            BTreeSet::from([Id::new_v4()]),
        ),
    ] {
        assert!(f
            .as_actor(
                f.owner,
                Command::AddComment {
                    id: Id::new_v4(),
                    item_id: item,
                    body: body.into(),
                    parent_id: parent,
                    mentions,
                    attachments
                }
            )
            .is_err());
    }
    assert!(f
        .as_actor(
            f.owner,
            Command::EditComment {
                id: Id::new_v4(),
                version: 1,
                body: "内容".into()
            }
        )
        .is_err());
    assert!(f
        .as_actor(f.owner, Command::RemoveAttachment { id: Id::new_v4() })
        .is_err());
}

#[test]
fn search_filters_metrics_and_archives() {
    let mut f = F::new();
    let req = f.requirement();
    let task = f.create(Kind::Task, "中文检索");
    f.update(
        task,
        Patch {
            assigned_to: Some(f.dev),
            priority: Some(1),
            severity: Some(2),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        f.state
            .search(&Filter {
                kind: Some(Kind::Task),
                status: Some("wait".into()),
                assigned_to: Some(f.dev),
                product_id: Some(f.product),
                project_id: Some(f.project),
                text: "中文".into(),
                ..Default::default()
            })
            .len(),
        1
    );
    f.update(
        task,
        Patch {
            clear_assignee: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(f.state.items[&task].assigned_to, None);
    assert!(f
        .state
        .search(&Filter {
            kind: Some(Kind::Task),
            status: Some("doing".into()),
            ..Default::default()
        })
        .is_empty());
    let doc = f.create(Kind::Document, "备份文档");
    f.ok(Command::AddComment {
        id: Id::new_v4(),
        item_id: doc,
        body: "评论中的检索词".into(),
        parent_id: None,
        mentions: BTreeSet::new(),
        attachments: BTreeSet::new(),
    });
    assert_eq!(
        f.state
            .search(&Filter {
                text: "检索词".into(),
                ..Default::default()
            })
            .len(),
        1
    );
    f.ok(Command::AddAttachment {
        id: Id::new_v4(),
        item_id: doc,
        name: "附件检索.pdf".into(),
        mime: "application/pdf".into(),
        sha256: "b".repeat(64),
        size: 2,
    });
    assert_eq!(
        f.state
            .search(&Filter {
                text: "附件检索".into(),
                ..Default::default()
            })
            .len(),
        1
    );
    f.ok(Command::Archive {
        id: doc,
        version: f.version(doc),
        note: "归档".into(),
    });
    assert!(f.state.item(doc).is_err());
    assert_eq!(
        f.state
            .search(&Filter {
                archived: true,
                ..Default::default()
            })
            .len(),
        1
    );
    f.ok(Command::Restore {
        id: doc,
        version: f.version(doc),
    });
    assert!(f.state.item(doc).is_ok());
    assert!(f
        .as_actor(
            f.owner,
            Command::Restore {
                id: doc,
                version: f.version(doc)
            }
        )
        .is_err());
    assert!(f
        .as_actor(
            f.owner,
            Command::Archive {
                id: f.product,
                version: f.version(f.product),
                note: "仍有引用".into()
            }
        )
        .is_err());
    assert_eq!(f.state.stage(req).unwrap(), "wait");
    let mut plan = f.new_item(Kind::Plan, "计划关联");
    plan.links.insert(Link {
        target: req,
        relation: Relation::Contains,
        content_version: None,
    });
    f.insert(plan);
    assert_eq!(f.state.stage(req).unwrap(), "planned");
    let bug = f.create(Kind::Bug, "未确认");
    assert_eq!(f.state.metrics(&Filter::default()).open_bugs, 1);
    f.transition(bug, "active");
    f.ok(Command::ResolveBug {
        id: bug,
        version: f.version(bug),
        resolution: "will_not_fix".into(),
        build_id: None,
        duplicate_id: None,
        note: "业务说明".into(),
    });
    assert_eq!(f.state.metrics(&Filter::default()).pending_retest, 1);
    f.transition(task, "doing");
    assert_eq!(f.state.metrics(&Filter::default()).open_tasks, 1);
    f.transition(task, "verify");
    assert_eq!(f.state.metrics(&Filter::default()).pending_acceptance, 1);
}

#[test]
fn test_execution_facts_cannot_be_hidden_by_archiving() {
    let mut f = F::new();
    let req = f.requirement();
    let case = f.case(req);
    let build = f.build("构建");
    let plan = f.plan(build, case);
    let run = f.run(plan, case, "fail");
    let before = serde_json::to_value(&f.state).unwrap();
    assert!(f
        .as_actor(
            f.owner,
            Command::Archive {
                id: run,
                version: f.version(run),
                note: "试图隐藏失败".into()
            }
        )
        .is_err());
    assert_eq!(serde_json::to_value(&f.state).unwrap(), before);
    f.transition(plan, "done");
    f.accept(req);
    let release = f.release(req, build, plan);
    assert!(f.publish(release).is_err());
}

#[test]
fn archives_and_end_of_support_do_not_bypass_delivery_records() {
    let mut f = F::new();
    let task = f.create(Kind::Task, "未交付任务");
    let bug = f.create(Kind::Bug, "尚未解决缺陷");
    for id in [task, bug] {
        let before = serde_json::to_value(&f.state).unwrap();
        assert!(f
            .as_actor(
                f.owner,
                Command::Archive {
                    id,
                    version: f.version(id),
                    note: "试图移出检查".into()
                }
            )
            .is_err());
        assert_eq!(serde_json::to_value(&f.state).unwrap(), before);
    }
    f.transition(task, "doing");
    f.transition(task, "verify");
    assert!(f
        .as_actor(
            f.owner,
            Command::Archive {
                id: task,
                version: f.version(task),
                note: "仍待验收".into()
            }
        )
        .is_err());
    f.transition(task, "closed");
    f.ok(Command::Archive {
        id: task,
        version: f.version(task),
        note: "完成后归档".into(),
    });
    f.transition(bug, "active");
    f.ok(Command::ResolveBug {
        id: bug,
        version: f.version(bug),
        resolution: "by_design".into(),
        build_id: None,
        duplicate_id: None,
        note: "符合原设计".into(),
    });
    assert!(f
        .as_actor(
            f.owner,
            Command::Archive {
                id: bug,
                version: f.version(bug),
                note: "仍待确认".into()
            }
        )
        .is_err());
    f.retest_as(f.tester, bug, None, None, "pass").unwrap();
    f.ok(Command::Archive {
        id: bug,
        version: f.version(bug),
        note: "确认后归档".into(),
    });
    let (release, req, _, _) = f.ready_release();
    f.publish(release).unwrap();
    let snapshot = f.state.items[&release].attributes["snapshot"].clone();
    f.transition(release, "terminate");
    for patch in [
        Patch {
            title: Some("改写发布标题".into()),
            ..Default::default()
        },
        Patch {
            links: Some(BTreeSet::new()),
            ..Default::default()
        },
    ] {
        assert!(f.update(release, patch).is_err());
    }
    assert_eq!(f.state.items[&release].attributes["snapshot"], snapshot);
    assert_eq!(f.state.stage(req).unwrap(), "released");
}

#[test]
fn paused_tasks_are_open_and_old_versions_do_not_mark_new_requirements_delivered() {
    let mut f = F::new();
    let req = f.requirement();
    let mut new = f.new_item(Kind::Task, "旧版实现");
    new.links.insert(Link {
        target: req,
        relation: Relation::Implements,
        content_version: None,
    });
    let task = f.insert(new);
    f.transition(task, "doing");
    f.transition(task, "pause");
    assert_eq!(f.state.metrics(&Filter::default()).open_tasks, 1);
    f.transition(task, "doing");
    f.transition(task, "verify");
    f.transition(task, "closed");
    assert_eq!(f.state.stage(req).unwrap(), "tested");
    f.update(
        req,
        Patch {
            acceptance: Some("新的验收条件".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(f.state.stage(req).unwrap(), "wait");
    let (release, published, _, _) = f.ready_release();
    f.publish(release).unwrap();
    assert_eq!(f.state.stage(published).unwrap(), "released");
    f.update(
        published,
        Patch {
            description: Some("发布后的新版正文".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(f.state.stage(published).unwrap(), "wait");
}

#[test]
fn git_publish_cannot_ignore_a_concurrent_critical_bug() {
    let mut f = F::with_backend(Backend::Git);
    let (release, _, plan, _) = f.ready_release();
    let build = Id::parse_str(
        f.state.items[&plan].attributes["build_id"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let (base, _) = f.git.take().unwrap();
    let mut publisher = base.clone();
    let mut tester = base.clone();
    let publish = f.request(
        f.owner,
        Command::Publish {
            id: release,
            version: f.version(release),
            note: "离线检查已通过".into(),
        },
    );
    let publish_id = publish.id;
    publisher.execute(Id::new_v4(), publish).unwrap();
    let mut bug = f.new_item(Kind::Bug, "同一构建的严重缺陷");
    bug.severity = 1;
    bug.attributes
        .insert("affected_build_ids".into(), json!([build]));
    let new_bug = bug.id;
    let submit = f.request(f.tester, Command::Create { item: bug.clone() });
    let submit_id = submit.id;
    tester.execute(Id::new_v4(), submit).unwrap();
    let view = publisher.merge(&tester).unwrap();
    assert!(view.issues.contains_key(&publish_id));
    assert!(view.issues.contains_key(&submit_id));
    assert_eq!(view.state.items[&release].status, "wait");
    assert!(!view.state.items.contains_key(&new_bug));
    let decision = GitResolution {
        id: Id::new_v4(),
        actor_id: f.owner,
        at: 999,
        targets: view.issues.keys().copied().collect(),
        command: Some(Command::Create { item: bug }),
        note: "采纳严重缺陷，暂停发布".into(),
    };
    let device = Id::new_v4();
    publisher.resolve(device, decision).unwrap();
    let req = Request {
        id: Id::new_v4(),
        actor_id: f.owner,
        at: 1000,
        command: Command::Publish {
            id: release,
            version: publisher.view().unwrap().state.items[&release].version,
            note: "再次尝试发布".into(),
        },
    };
    assert!(publisher.execute(device, req).is_err());
    assert_eq!(
        publisher.view().unwrap().state.items[&release].status,
        "wait"
    );
}

#[test]
fn git_retests_merge_metadata_but_require_a_decision_for_opposite_outcomes() {
    let mut f = F::with_backend(Backend::Git);
    let bug = f.create(Kind::Bug, "并发复测");
    f.transition(bug, "active");
    let build = f.build("fix.concurrent");
    f.resolve_fixed(bug, build);
    let (base, _) = f.git.take().unwrap();
    for opposite in [false, true] {
        let mut left = base.clone();
        let mut right = base.clone();
        let pass = f.request(
            f.tester,
            Command::RetestBug {
                id: bug,
                version: f.version(bug),
                build_id: Some(build),
                run_id: None,
                result: "pass".into(),
                note: "实际复测通过".into(),
            },
        );
        let other = f.request(
            if opposite { f.tester } else { f.owner },
            if opposite {
                Command::RetestBug {
                    id: bug,
                    version: f.version(bug),
                    build_id: Some(build),
                    run_id: None,
                    result: "fail".into(),
                    note: "实际复测仍失败".into(),
                }
            } else {
                Command::Update {
                    id: bug,
                    version: f.version(bug),
                    patch: Patch {
                        priority: Some(1),
                        ..Default::default()
                    },
                }
            },
        );
        left.execute(Id::new_v4(), pass.clone()).unwrap();
        right.execute(Id::new_v4(), other.clone()).unwrap();
        let view = left.merge(&right).unwrap();
        let reverse = right.merge(&left).unwrap();
        assert_eq!(
            serde_json::to_value(&view).unwrap(),
            serde_json::to_value(reverse).unwrap()
        );
        if opposite {
            assert_eq!(view.issues.len(), 2);
            assert_eq!(view.state.items[&bug].status, "resolved");
            assert!(left.operations().contains_key(&pass.id));
            assert!(left.operations().contains_key(&other.id));
            left.resolve(
                Id::new_v4(),
                GitResolution {
                    id: Id::new_v4(),
                    actor_id: f.owner,
                    at: 1000,
                    targets: view.issues.keys().copied().collect(),
                    command: Some(Command::RetestBug {
                        id: bug,
                        version: view.state.items[&bug].version,
                        build_id: Some(build),
                        run_id: None,
                        result: "pass".into(),
                        note: "核对双方证据后复测通过".into(),
                    }),
                    note: "核对双方原始复测证据".into(),
                },
            )
            .unwrap();
        } else {
            assert!(view.issues.is_empty());
            assert_eq!(view.state.items[&bug].priority, 1);
        }
        let reopened: GitLog = serde_json::from_slice(&serde_json::to_vec(&left).unwrap()).unwrap();
        let final_view = reopened.view().unwrap();
        assert!(final_view.issues.is_empty());
        assert_eq!(final_view.state.items[&bug].status, "closed");
        assert_eq!(
            final_view.state.items[&bug].attributes["retest_records"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
}

#[test]
fn git_test_results_merge_across_cases_and_conflict_for_the_same_case() {
    let mut f = F::with_backend(Backend::Git);
    let req = f.requirement();
    let case1 = f.case(req);
    let case2 = f.case(req);
    let build = f.build("并发测试构建");
    let mut new = f.new_item(Kind::TestPlan, "并发测试单");
    new.attributes = BTreeMap::from([
        ("build_id".into(), json!(build)),
        ("case_ids".into(), json!([case1, case2])),
    ]);
    let plan = f.insert(new);
    f.transition(plan, "doing");
    let (base, _) = f.git.take().unwrap();
    for same_case in [false, true] {
        let mut left = base.clone();
        let mut right = base.clone();
        for (log, case, result) in [
            (&mut left, case1, "pass"),
            (&mut right, if same_case { case1 } else { case2 }, "fail"),
        ] {
            log.execute(
                Id::new_v4(),
                f.request(
                    f.tester,
                    Command::RunTest {
                        id: Id::new_v4(),
                        plan_id: plan,
                        case_id: case,
                        case_version: 1,
                        result: result.into(),
                        actual: "独立执行记录".into(),
                    },
                ),
            )
            .unwrap();
        }
        let mut reverse = right.clone();
        let expected = left.clone();
        let view = left.merge(&right).unwrap();
        reverse.merge(&expected).unwrap();
        assert_eq!(
            serde_json::to_value(&view).unwrap(),
            serde_json::to_value(reverse.view().unwrap()).unwrap()
        );
        if same_case {
            assert_eq!(view.issues.len(), 2);
            assert_eq!(
                view.state
                    .items
                    .values()
                    .filter(|i| i.kind == Kind::TestRun)
                    .count(),
                0
            );
        } else {
            assert!(view.issues.is_empty());
            assert_eq!(
                view.state
                    .items
                    .values()
                    .filter(|i| i.kind == Kind::TestRun)
                    .count(),
                2
            );
        }
    }
}
