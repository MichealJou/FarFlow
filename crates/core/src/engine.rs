use crate::model::*;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

impl State {
    pub fn new(id: Id, name: String, owner: Member) -> Result<Self> {
        validate_text(&name, 1, 120, "工作空间名称")?;
        validate_member(&owner)?;
        if id.is_nil() || !owner.active || !owner.roles.contains(&Role::Owner) {
            return invalid("工作空间需要有效标识和启用的所有者");
        }
        Ok(Self {
            schema: 1,
            id,
            name,
            revision: 0,
            members: BTreeMap::from([(owner.id, owner)]),
            items: BTreeMap::new(),
            versions: Vec::new(),
            comments: BTreeMap::new(),
            attachments: BTreeMap::new(),
            history: Vec::new(),
            operations: BTreeMap::new(),
        })
    }

    pub fn item(&self, id: Id) -> Result<&Item> {
        self.items
            .get(&id)
            .filter(|item| !item.deleted)
            .ok_or_else(|| Error::NotFound(format!("业务对象 {id}")))
    }

    pub fn execute(&mut self, request: Request) -> Result<Receipt> {
        self.actor(request.actor_id)?;
        if request.id.is_nil() || request.at < 0 {
            return invalid("操作标识和时间无效");
        }
        let encoded = serde_json::to_vec(&request).map_err(storage)?;
        let fingerprint = format!("{:x}", Sha256::digest(encoded));
        if let Some(applied) = self.operations.get(&request.id) {
            return if applied.fingerprint == fingerprint {
                Ok(applied.receipt.clone())
            } else {
                Err(Error::Conflict("同一操作标识携带了不同内容".into()))
            };
        }
        // 失败命令不能留下部分成员、关系或状态变化。
        let mut next = self.clone();
        next.apply(&request, &request.command, 0)?;
        next.reconcile(self, &request)?;
        next.revision += 1;
        let before = self.objects()?;
        let after = next.objects()?;
        let changed: BTreeSet<_> = before
            .keys()
            .chain(after.keys())
            .copied()
            .filter(|id| before.get(id) != after.get(id))
            .collect();
        let note = command_note(&request.command);
        let action = serde_json::to_value(&request.command).map_err(storage)?["type"]
            .as_str()
            .unwrap_or("operation")
            .to_owned();
        for id in &changed {
            next.history.push(Audit {
                operation_id: request.id,
                actor_id: request.actor_id,
                at: request.at,
                action: action.clone(),
                object_id: *id,
                before: before.get(id).cloned().unwrap_or(Value::Null),
                after: after.get(id).cloned().unwrap_or(Value::Null),
                note: note.clone(),
            });
        }
        let receipt = Receipt {
            operation_id: request.id,
            revision: next.revision,
            affected: changed.into_iter().collect(),
        };
        next.operations.insert(
            request.id,
            Applied {
                fingerprint,
                receipt: receipt.clone(),
            },
        );
        *self = next;
        Ok(receipt)
    }

    pub fn search(&self, filter: &Filter) -> Vec<&Item> {
        let text = filter.text.trim().to_lowercase();
        let mut items: Vec<_> = self
            .items
            .values()
            .filter(|item| {
                item.deleted == filter.archived
                    && filter.kind.is_none_or(|v| item.kind == v)
                    && filter.status.as_ref().is_none_or(|v| &item.status == v)
                    && filter
                        .assigned_to
                        .is_none_or(|v| item.assigned_to == Some(v))
                    && filter.product_id.is_none_or(|v| item.product_id == Some(v))
                    && filter.project_id.is_none_or(|v| item.project_id == Some(v))
                    && (text.is_empty()
                        || format!(
                            "{} {} {} {}",
                            item.id, item.title, item.description, item.acceptance
                        )
                        .to_lowercase()
                        .contains(&text)
                        || self.comments.values().any(|c| {
                            c.item_id == item.id
                                && !c.deleted
                                && c.body.to_lowercase().contains(&text)
                        })
                        || self.attachments.values().any(|a| {
                            a.item_id == item.id
                                && !a.deleted
                                && a.name.to_lowercase().contains(&text)
                        }))
            })
            .collect();
        items.sort_by_key(|item| (std::cmp::Reverse(item.updated_at), item.id));
        items
    }

    pub fn metrics(&self, filter: &Filter) -> Metrics {
        let mut result = Metrics::default();
        for item in self.search(filter) {
            match item.kind {
                Kind::Requirement => result.requirements += 1,
                Kind::Task if item.status == "verify" => result.pending_acceptance += 1,
                Kind::Task if ["wait", "doing", "pause"].contains(&item.status.as_str()) => {
                    result.open_tasks += 1
                }
                Kind::Bug if item.status == "resolved" => result.pending_retest += 1,
                Kind::Bug if item.status != "closed" => result.open_bugs += 1,
                Kind::Release if item.status == "normal" => result.releases += 1,
                _ => {}
            }
        }
        result
    }

    pub fn stage(&self, id: Id) -> Result<String> {
        let item = self.item(id)?;
        if item.kind != Kind::Requirement {
            return invalid("仅需求具有研发阶段");
        }
        if item.status == "closed" {
            return Ok("closed".into());
        }
        if self.items.values().any(|r| {
            r.kind == Kind::Release
                && ["normal", "terminate"].contains(&r.status.as_str())
                && r.links
                    .iter()
                    .any(|l| l.target == id && l.relation == Relation::Contains)
                && r.attributes
                    .get("snapshot")
                    .and_then(|s| s.get("items"))
                    .and_then(|items| items.get(id.to_string()))
                    .and_then(|i| i.get("content_version"))
                    .and_then(Value::as_u64)
                    == Some(item.content_version)
        }) {
            return Ok("released".into());
        }
        if accepted(item) {
            return Ok("accepted".into());
        }
        let tasks: Vec<_> = self
            .items
            .values()
            .filter(|t| {
                !t.deleted
                    && t.kind == Kind::Task
                    && t.status != "cancel"
                    && t.links.iter().any(|l| {
                        l.target == id
                            && l.relation == Relation::Implements
                            && l.content_version == Some(item.content_version)
                    })
            })
            .collect();
        if !tasks.is_empty() {
            if tasks.iter().all(|t| t.status == "closed") {
                return Ok("tested".into());
            }
            if tasks.iter().any(|t| t.status == "verify") {
                return Ok("testing".into());
            }
            if tasks
                .iter()
                .any(|t| ["doing", "pause"].contains(&t.status.as_str()))
            {
                return Ok("developing".into());
            }
            return Ok("planned".into());
        }
        if self
            .items
            .values()
            .any(|p| !p.deleted && p.kind == Kind::Plan && p.links.iter().any(|l| l.target == id))
        {
            return Ok("planned".into());
        }
        Ok("wait".into())
    }

    fn actor(&self, id: Id) -> Result<&Member> {
        self.members
            .get(&id)
            .filter(|m| m.active)
            .ok_or_else(|| Error::Forbidden("账户未启用或不是工作空间成员".into()))
    }

    fn roles(&self, actor: Id, allowed: &[Role]) -> Result<()> {
        let member = self.actor(actor)?;
        if member.roles.contains(&Role::Owner) || allowed.iter().any(|r| member.roles.contains(r)) {
            Ok(())
        } else {
            Err(Error::Forbidden("当前角色不能执行此操作".into()))
        }
    }

    fn writable(&self, actor: Id) -> Result<()> {
        self.roles(
            actor,
            &[Role::Product, Role::Manager, Role::Developer, Role::Tester],
        )
    }

    fn unused(&self, id: Id) -> Result<()> {
        if id.is_nil()
            || self.items.contains_key(&id)
            || self.members.contains_key(&id)
            || self.comments.contains_key(&id)
            || self.attachments.contains_key(&id)
        {
            return invalid("对象标识为空或已经存在");
        }
        Ok(())
    }

    fn expected(&self, id: Id, version: u64) -> Result<Item> {
        let item = self.item(id)?;
        if item.version != version {
            return Err(Error::Conflict(format!(
                "对象已更新，当前版本 {}",
                item.version
            )));
        }
        Ok(item.clone())
    }

    fn can_edit(&self, actor: Id, item: &Item) -> Result<()> {
        self.writable(actor)?;
        let member = self.actor(actor)?;
        if member.roles.contains(&Role::Owner) || member.roles.contains(&Role::Manager) {
            return Ok(());
        }
        let allowed = match item.kind {
            Kind::Product | Kind::Module | Kind::Requirement | Kind::Plan | Kind::Release => {
                member.roles.contains(&Role::Product)
            }
            Kind::Project | Kind::Iteration => false,
            Kind::TestCase | Kind::TestPlan => member.roles.contains(&Role::Tester),
            Kind::Task | Kind::Build => {
                member.roles.contains(&Role::Developer)
                    || item.assigned_to == Some(actor)
                    || item.created_by == actor
            }
            Kind::Inbox | Kind::Bug | Kind::Document => true,
            Kind::TestRun => false,
        };
        if allowed {
            Ok(())
        } else {
            Err(Error::Forbidden("不能编辑此类对象".into()))
        }
    }

    fn apply(&mut self, req: &Request, command: &Command, depth: usize) -> Result<()> {
        self.actor(req.actor_id)?;
        if depth > 4 {
            return invalid("批量命令嵌套过深");
        }
        match command {
            Command::Batch { commands } => {
                if commands.is_empty() || commands.len() > 50 {
                    return invalid("批量命令需要 1 至 50 项");
                }
                for command in commands {
                    self.apply(req, command, depth + 1)?;
                }
            }
            Command::Create { item } => self.create(req, item)?,
            Command::Update { id, version, patch } => {
                let mut item = self.expected(*id, *version)?;
                self.can_edit(req.actor_id, &item)?;
                if item.kind == Kind::TestRun
                    || (item.kind == Kind::Release
                        && ["normal", "terminate"].contains(&item.status.as_str()))
                    || (item.kind == Kind::Build && item.status == "ready")
                {
                    return invalid("已固化的测试结果、构建或发布不能直接修改");
                }
                if let Some(v) = &patch.title {
                    item.title = v.clone();
                }
                if let Some(v) = &patch.description {
                    item.description = v.clone();
                }
                if let Some(v) = &patch.acceptance {
                    item.acceptance = v.clone();
                }
                if let Some(v) = patch.priority {
                    item.priority = v;
                }
                if let Some(v) = patch.severity {
                    item.severity = v;
                }
                if let Some(v) = patch.assigned_to {
                    item.assigned_to = Some(v);
                }
                if patch.clear_assignee {
                    item.assigned_to = None;
                }
                if let Some(v) = &patch.links {
                    if item.kind == Kind::Bug
                        && item
                            .links
                            .iter()
                            .any(|link| link.relation == Relation::Source && !v.contains(link))
                    {
                        return invalid("Bug 的原始来源不能通过编辑移除或改写");
                    }
                    item.links = v.clone();
                }
                if let Some(v) = &patch.attributes {
                    validate_user_attributes(v)?;
                    if item.kind == Kind::TestPlan
                        && item.status != "wait"
                        && v.keys()
                            .any(|k| ["case_ids", "build_id"].contains(&k.as_str()))
                    {
                        return invalid("已开始测试单的构建与用例范围不可修改");
                    }
                    for (key, value) in v {
                        item.attributes.insert(key.clone(), value.clone());
                    }
                }
                self.pin_requirements(&mut item)?;
                if item.kind == Kind::Bug
                    && ["resolved", "closed"].contains(&item.status.as_str())
                    && content(&self.items[id]) != content(&item)
                {
                    return invalid("修改已解决或已关闭 Bug 的内容前，需要先重新激活");
                }
                if item.kind == Kind::Project {
                    let products = attr_ids(&item.attributes, "product_ids")?;
                    if self.items.values().any(|child| {
                        !child.deleted
                            && child.project_id == Some(item.id)
                            && child.product_id.is_some_and(|p| !products.contains(&p))
                    }) {
                        return invalid("不能移除已有项目工作项使用的产品");
                    }
                }
                self.validate_item(&item)?;
                if item.kind == Kind::TestPlan && item.status == "wait" {
                    self.pin_cases(&mut item)?;
                }
                self.items.insert(*id, item);
            }
            Command::Review {
                id,
                version,
                decision,
                note,
            } => {
                let mut item = self.expected(*id, *version)?;
                let roles = if item.kind == Kind::TestCase {
                    vec![Role::Tester, Role::Manager]
                } else {
                    vec![Role::Product, Role::Manager]
                };
                self.roles(req.actor_id, &roles)?;
                if ![Kind::Requirement, Kind::TestCase].contains(&item.kind)
                    || item.status != "reviewing"
                {
                    return invalid("当前对象不在可评审状态");
                }
                validate_text(note, 1, 8000, "评审意见")?;
                item.status = match decision {
                    ReviewDecision::Approve => {
                        if item.kind == Kind::Requirement {
                            validate_text(&item.acceptance, 1, 16000, "验收标准")?;
                        }
                        "active"
                    }
                    ReviewDecision::Reject => "draft",
                    ReviewDecision::Change if item.kind == Kind::TestCase => "draft",
                    ReviewDecision::Change => "changing",
                }
                .into();
                item.attributes.insert("review_result".into(), json!({"actor":req.actor_id,"at":req.at,"version":item.content_version,"decision":decision,"note":note}));
                self.items.insert(*id, item);
            }
            Command::Transition {
                id,
                version,
                target,
                note,
            } => self.transition(req, *id, *version, target, note)?,
            Command::AcceptRequirement {
                id,
                version,
                accepted: yes,
                note,
            } => {
                self.roles(req.actor_id, &[Role::Product, Role::Manager])?;
                let mut item = self.expected(*id, *version)?;
                if item.kind != Kind::Requirement || item.status != "active" {
                    return invalid("只能验收已批准需求");
                }
                validate_text(note, 1, 8000, "验收结论")?;
                if *yes
                    && self.items.values().any(|t| {
                        !t.deleted
                            && t.kind == Kind::Task
                            && t.links
                                .iter()
                                .any(|l| l.target == *id && l.relation == Relation::Implements)
                            && !["closed", "cancel"].contains(&t.status.as_str())
                    })
                {
                    return invalid("需求仍有未验收任务");
                }
                item.attributes.insert("acceptance_record".into(), json!({"accepted":yes,"actor":req.actor_id,"at":req.at,"version":item.content_version,"note":note}));
                self.items.insert(*id, item);
            }
            Command::ConvertInbox {
                id,
                version,
                new_id,
                kind,
                acceptance,
            } => {
                self.writable(req.actor_id)?;
                let mut source = self.expected(*id, *version)?;
                if source.kind != Kind::Inbox
                    || source.status == "converted"
                    || ![Kind::Requirement, Kind::Bug].contains(kind)
                {
                    return invalid("收集项只能转化一次为需求或 Bug");
                }
                let mut attributes = source.attributes.clone();
                if *kind == Kind::Bug {
                    attributes
                        .entry("steps".into())
                        .or_insert(json!(source.description));
                    attributes
                        .entry("expected".into())
                        .or_insert(json!(acceptance));
                    attributes
                        .entry("actual".into())
                        .or_insert(json!(source.description));
                }
                self.create(
                    req,
                    &NewItem {
                        id: *new_id,
                        kind: *kind,
                        title: source.title.clone(),
                        description: source.description.clone(),
                        acceptance: acceptance.clone(),
                        priority: source.priority,
                        severity: source.severity,
                        assigned_to: source.assigned_to,
                        product_id: source.product_id,
                        project_id: source.project_id,
                        iteration_id: source.iteration_id,
                        parent_id: None,
                        links: BTreeSet::from([Link {
                            target: *id,
                            relation: Relation::Source,
                            content_version: Some(source.content_version),
                        }]),
                        attributes,
                    },
                )?;
                source.status = "converted".into();
                source
                    .attributes
                    .insert("converted_id".into(), json!(new_id));
                self.items.insert(*id, source);
            }
            Command::RunTest {
                id,
                plan_id,
                case_id,
                case_version,
                result,
                actual,
            } => self.run_test(req, *id, *plan_id, *case_id, *case_version, result, actual)?,
            Command::BugFromTest {
                id,
                run_id,
                title,
                actual,
                environment,
            } => self.bug_from_test(req, *id, *run_id, title, actual, environment)?,
            Command::ResolveBug {
                id,
                version,
                resolution,
                build_id,
                duplicate_id,
                note,
            } => self.resolve_bug(
                req,
                *id,
                *version,
                resolution,
                *build_id,
                *duplicate_id,
                note,
            )?,
            Command::RetestBug {
                id,
                version,
                build_id,
                run_id,
                result,
                note,
            } => self.retest_bug(req, *id, *version, *build_id, *run_id, result, note)?,
            Command::Publish { id, version, note } => self.publish(req, *id, *version, note)?,
            Command::Archive { id, version, note } => {
                let mut item = self.expected(*id, *version)?;
                self.roles(req.actor_id, &[Role::Manager, Role::Product])?;
                validate_text(note, 1, 8000, "归档原因")?;
                if item.kind == Kind::TestRun {
                    return invalid("测试执行事实不能归档隐藏");
                }
                if (item.kind == Kind::Bug && item.status != "closed")
                    || (item.kind == Kind::Task
                        && !["closed", "cancel"].contains(&item.status.as_str()))
                {
                    return invalid("未结束的任务和 Bug 不能通过归档移出交付检查");
                }
                if self
                    .items
                    .values()
                    .any(|other| !other.deleted && other.id != *id && references(other, *id))
                {
                    return invalid("对象仍被有效业务引用，不能归档");
                }
                item.deleted = true;
                self.items.insert(*id, item);
            }
            Command::Restore { id, version } => {
                self.roles(req.actor_id, &[Role::Manager, Role::Product])?;
                let mut item = self
                    .items
                    .get(id)
                    .cloned()
                    .ok_or_else(|| Error::NotFound("归档对象".into()))?;
                if item.version != *version {
                    return Err(Error::Conflict("归档对象已更新".into()));
                }
                if !item.deleted {
                    return invalid("对象尚未归档");
                }
                item.deleted = false;
                self.validate_item(&item)?;
                self.items.insert(*id, item);
            }
            Command::AddComment {
                id,
                item_id,
                body,
                parent_id,
                mentions,
                attachments,
            } => {
                self.writable(req.actor_id)?;
                self.unused(*id)?;
                self.item(*item_id)?;
                if body.trim().is_empty() && attachments.is_empty() {
                    return invalid("评论需要文字或附件");
                }
                validate_text(body, 0, 32000, "评论")?;
                if let Some(parent) = parent_id {
                    if !self
                        .comments
                        .get(parent)
                        .is_some_and(|c| c.item_id == *item_id && !c.deleted)
                    {
                        return invalid("回复目标不属于当前对象或已移除");
                    }
                }
                for member in mentions {
                    self.actor(*member)?;
                }
                for id in attachments {
                    if !self
                        .attachments
                        .get(id)
                        .is_some_and(|a| a.item_id == *item_id && !a.deleted)
                    {
                        return invalid("评论附件不属于当前对象");
                    }
                }
                self.comments.insert(
                    *id,
                    Comment {
                        id: *id,
                        item_id: *item_id,
                        actor_id: req.actor_id,
                        body: body.clone(),
                        parent_id: *parent_id,
                        mentions: mentions.clone(),
                        attachments: attachments.clone(),
                        created_at: req.at,
                        updated_at: req.at,
                        version: 1,
                        deleted: false,
                    },
                );
            }
            Command::EditComment { id, version, body } => {
                let mut comment = self.comment_for_edit(req.actor_id, *id, *version)?;
                validate_text(
                    body,
                    usize::from(comment.attachments.is_empty()),
                    32000,
                    "评论",
                )?;
                comment.body = body.clone();
                comment.version += 1;
                comment.updated_at = req.at;
                self.comments.insert(*id, comment);
            }
            Command::DeleteComment { id, version } => {
                let mut comment = self.comment_for_edit(req.actor_id, *id, *version)?;
                comment.deleted = true;
                comment.version += 1;
                comment.updated_at = req.at;
                self.comments.insert(*id, comment);
            }
            Command::AddAttachment {
                id,
                item_id,
                name,
                mime,
                sha256,
                size,
            } => {
                self.writable(req.actor_id)?;
                self.unused(*id)?;
                self.item(*item_id)?;
                validate_text(name, 1, 240, "附件名称")?;
                validate_text(mime, 1, 200, "文件类型")?;
                if name.contains(['/', '\\', '\0'])
                    || sha256.len() != 64
                    || !sha256
                        .bytes()
                        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
                    || *size == 0
                    || *size > 10 * 1024 * 1024
                {
                    return invalid("附件名称、大小或内容哈希无效");
                }
                self.attachments.insert(
                    *id,
                    Attachment {
                        id: *id,
                        item_id: *item_id,
                        name: name.clone(),
                        mime: mime.clone(),
                        sha256: sha256.clone(),
                        size: *size,
                        actor_id: req.actor_id,
                        at: req.at,
                        deleted: false,
                    },
                );
            }
            Command::RemoveAttachment { id } => {
                self.writable(req.actor_id)?;
                let mut a = self
                    .attachments
                    .get(id)
                    .cloned()
                    .filter(|a| !a.deleted)
                    .ok_or_else(|| Error::NotFound("附件".into()))?;
                if a.actor_id != req.actor_id {
                    self.roles(req.actor_id, &[Role::Manager])?;
                }
                a.deleted = true;
                self.attachments.insert(*id, a);
            }
            Command::AddMember { member } => {
                self.roles(req.actor_id, &[])?;
                self.unused(member.id)?;
                validate_member(member)?;
                self.unique_account(member)?;
                self.members.insert(member.id, member.clone());
            }
            Command::UpdateMember { member } => {
                self.roles(req.actor_id, &[])?;
                validate_member(member)?;
                if !self.members.contains_key(&member.id) {
                    return Err(Error::NotFound("成员".into()));
                }
                self.unique_account(member)?;
                if (!member.active || !member.roles.contains(&Role::Owner))
                    && !self
                        .members
                        .values()
                        .any(|m| m.id != member.id && m.active && m.roles.contains(&Role::Owner))
                {
                    return invalid("至少保留一位启用的所有者");
                }
                self.members.insert(member.id, member.clone());
            }
        }
        Ok(())
    }

    fn create(&mut self, req: &Request, new: &NewItem) -> Result<()> {
        self.unused(new.id)?;
        validate_user_attributes(&new.attributes)?;
        let mut item = Item {
            id: new.id,
            kind: new.kind,
            title: new.title.trim().into(),
            description: new.description.clone(),
            acceptance: new.acceptance.clone(),
            status: initial_status(new.kind).into(),
            priority: new.priority,
            severity: new.severity,
            assigned_to: new.assigned_to,
            product_id: new.product_id,
            project_id: new.project_id,
            iteration_id: new.iteration_id,
            parent_id: new.parent_id,
            links: new.links.clone(),
            attributes: new.attributes.clone(),
            version: 1,
            content_version: 1,
            created_by: req.actor_id,
            created_at: req.at,
            updated_at: req.at,
            deleted: false,
        };
        if item.kind == Kind::TestRun {
            return invalid("测试结果必须通过执行测试生成");
        }
        self.can_edit(req.actor_id, &item)?;
        self.pin_requirements(&mut item)?;
        self.validate_item(&item)?;
        if new.kind == Kind::TestPlan {
            self.pin_cases(&mut item)?;
        }
        self.items.insert(item.id, item);
        Ok(())
    }

    fn pin_requirements(&self, item: &mut Item) -> Result<()> {
        let mut links = BTreeSet::new();
        for mut link in item.links.iter().cloned() {
            if [Relation::Implements, Relation::Verifies].contains(&link.relation)
                && link.content_version.is_none()
            {
                link.content_version = Some(self.item(link.target)?.content_version);
            }
            links.insert(link);
        }
        item.links = links;
        Ok(())
    }

    fn pin_cases(&self, item: &mut Item) -> Result<()> {
        let mut versions = BTreeMap::new();
        for id in attr_ids(&item.attributes, "case_ids")? {
            versions.insert(id.to_string(), self.item(id)?.content_version);
        }
        item.attributes
            .insert("case_versions".into(), json!(versions));
        Ok(())
    }

    fn validate_item(&self, item: &Item) -> Result<()> {
        validate_text(&item.title, 1, 240, "标题")?;
        validate_text(&item.description, 0, 64000, "描述")?;
        validate_text(&item.acceptance, 0, 16000, "验收标准")?;
        if !(1..=4).contains(&item.priority) || !(1..=4).contains(&item.severity) {
            return invalid("优先级和严重度范围为 1 至 4");
        }
        if serde_json::to_vec(&item.attributes).map_err(storage)?.len() > 128000 {
            return invalid("扩展内容过大");
        }
        if let Some(id) = item.assigned_to {
            self.actor(id)?;
        }
        for (id, expected) in [
            (item.product_id, Kind::Product),
            (item.project_id, Kind::Project),
            (item.iteration_id, Kind::Iteration),
        ] {
            if let Some(id) = id {
                if id == item.id || self.item(id)?.kind != expected {
                    return invalid("所属范围类型不匹配");
                }
            }
        }
        if let Some(id) = item.parent_id {
            let parent = self.item(id)?;
            if id == item.id || parent.kind != item.kind || parent.product_id != item.product_id {
                return invalid("父对象类型或产品不匹配");
            }
            let mut next = parent.parent_id;
            let mut visited = BTreeSet::from([item.id, parent.id]);
            while let Some(id) = next {
                if !visited.insert(id) {
                    return invalid("父子关系不能循环");
                }
                next = self.item(id)?.parent_id;
            }
        }
        if let Some(iteration) = item.iteration_id {
            if self.item(iteration)?.project_id != item.project_id {
                return invalid("迭代不属于当前项目");
            }
        }
        if let (Some(product), Some(project)) = (item.product_id, item.project_id) {
            if !attr_ids(&self.item(project)?.attributes, "product_ids")?.contains(&product) {
                return invalid("项目没有关联当前产品");
            }
        }
        for link in &item.links {
            if link.target == item.id {
                return invalid("不能关联自身");
            }
            let other = self.item(link.target)?;
            if item.product_id.is_some()
                && other.product_id.is_some()
                && item.product_id != other.product_id
            {
                return invalid("不能跨产品关联业务对象");
            }
            match link.relation {
                Relation::Implements
                    if item.kind != Kind::Task || other.kind != Kind::Requirement =>
                {
                    return invalid("实现关系只用于任务和需求")
                }
                Relation::Verifies
                    if item.kind != Kind::TestCase || other.kind != Kind::Requirement =>
                {
                    return invalid("验证关系只用于用例和需求")
                }
                Relation::KnownIssue if item.kind != Kind::Release || other.kind != Kind::Bug => {
                    return invalid("遗留问题只用于发布和 Bug")
                }
                Relation::DependsOn => {
                    if item.kind != Kind::Task || other.kind != Kind::Task {
                        return invalid("依赖关系只用于任务");
                    }
                    let mut stack = vec![other.id];
                    let mut visited = BTreeSet::new();
                    while let Some(id) = stack.pop() {
                        if id == item.id {
                            return invalid("任务依赖不能循环");
                        }
                        if visited.insert(id) {
                            stack.extend(
                                self.item(id)?
                                    .links
                                    .iter()
                                    .filter(|l| l.relation == Relation::DependsOn)
                                    .map(|l| l.target),
                            );
                        }
                    }
                }
                _ => {}
            }
            if let Some(version) = link.content_version {
                if version == 0 || version > other.content_version {
                    return invalid("关联内容版本不存在");
                }
            }
        }
        let product_required = [
            Kind::Module,
            Kind::Requirement,
            Kind::Plan,
            Kind::TestCase,
            Kind::Build,
            Kind::Bug,
            Kind::Release,
        ];
        if product_required.contains(&item.kind) && item.product_id.is_none() {
            return invalid("必须选择所属产品");
        }
        if [Kind::Task, Kind::Iteration].contains(&item.kind) && item.project_id.is_none() {
            return invalid("必须选择所属项目");
        }
        match item.kind {
            Kind::Project => {
                let ids = attr_ids(&item.attributes, "product_ids")?;
                if ids.is_empty() {
                    return invalid("项目至少关联一个产品");
                }
                for id in ids {
                    if self.item(id)?.kind != Kind::Product {
                        return invalid("项目产品类型无效");
                    }
                }
            }
            Kind::Iteration => {
                let kind = attr_text(&item.attributes, "iteration_type").unwrap_or("sprint");
                if !["sprint", "stage", "kanban"].contains(&kind) {
                    return invalid("执行类型无效");
                }
            }
            Kind::Bug => {
                required_attr(&item.attributes, "steps", "复现步骤")?;
                required_attr(&item.attributes, "expected", "预期结果")?;
                required_attr(&item.attributes, "actual", "实际结果")?;
            }
            Kind::TestCase => {
                let steps = item
                    .attributes
                    .get("steps")
                    .and_then(Value::as_array)
                    .ok_or_else(|| Error::Validation("用例需要步骤".into()))?;
                if steps.is_empty() || steps.len() > 100 {
                    return invalid("用例需要 1 至 100 个步骤");
                }
                for step in steps {
                    validate_text(step["action"].as_str().unwrap_or(""), 1, 8000, "测试步骤")?;
                    validate_text(step["expected"].as_str().unwrap_or(""), 1, 8000, "预期结果")?;
                }
            }
            Kind::TestPlan => {
                let build = self.item(attr_id(&item.attributes, "build_id")?)?;
                if build.kind != Kind::Build || build.status != "ready" {
                    return invalid("测试单需要就绪构建");
                }
                if build.product_id != item.product_id {
                    return invalid("测试单与构建产品不一致");
                }
                let cases = attr_ids(&item.attributes, "case_ids")?;
                if cases.is_empty() {
                    return invalid("测试单至少包含一个用例");
                }
                for id in cases {
                    let case = self.item(id)?;
                    if case.kind != Kind::TestCase
                        || case.status != "active"
                        || case.product_id != item.product_id
                    {
                        return invalid("测试单只能包含同产品已批准用例");
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn transition(
        &mut self,
        req: &Request,
        id: Id,
        version: u64,
        target: &str,
        note: &str,
    ) -> Result<()> {
        let mut item = self.expected(id, version)?;
        if item.kind == Kind::Task && target == "closed" {
            self.roles(req.actor_id, &[Role::Tester, Role::Manager, Role::Product])?;
        } else {
            self.can_edit(req.actor_id, &item)?;
        }
        let from = item.status.as_str();
        let allowed = match item.kind {
            Kind::Requirement => matches!(
                (from, target),
                ("draft", "reviewing")
                    | ("changing", "reviewing")
                    | ("active", "changing")
                    | ("active", "closed")
                    | ("closed", "active")
            ),
            Kind::Task => matches!(
                (from, target),
                ("wait", "doing")
                    | ("wait", "cancel")
                    | ("doing", "verify")
                    | ("doing", "pause")
                    | ("doing", "cancel")
                    | ("pause", "doing")
                    | ("pause", "cancel")
                    | ("verify", "closed")
                    | ("verify", "doing")
                    | ("closed", "doing")
                    | ("cancel", "doing")
            ),
            Kind::Bug => matches!(
                (from, target),
                ("new", "active")
                    | ("active", "doing")
                    | ("doing", "active")
                    | ("resolved", "active")
                    | ("closed", "active")
            ),
            Kind::TestCase => {
                matches!((from, target), ("draft", "reviewing") | ("active", "draft"))
            }
            Kind::TestPlan => matches!(
                (from, target),
                ("wait", "doing") | ("doing", "done") | ("done", "doing")
            ),
            Kind::Project | Kind::Iteration => matches!(
                (from, target),
                ("wait", "doing")
                    | ("doing", "suspended")
                    | ("suspended", "doing")
                    | ("doing", "closed")
                    | ("suspended", "closed")
                    | ("closed", "doing")
            ),
            Kind::Plan => matches!(
                (from, target),
                ("wait", "doing") | ("doing", "done") | ("done", "doing")
            ),
            Kind::Build => from == "draft" && target == "ready",
            Kind::Release => matches!(
                (from, target),
                ("draft", "wait") | ("wait", "draft") | ("normal", "terminate")
            ),
            Kind::Inbox => matches!(
                (from, target),
                ("new", "clarifying")
                    | ("new", "accepted")
                    | ("clarifying", "accepted")
                    | ("accepted", "clarifying")
                    | ("new", "rejected")
                    | ("clarifying", "rejected")
                    | ("rejected", "new")
            ),
            _ => false,
        };
        if !allowed {
            return invalid("不允许此状态流转，请使用对应的评审、解决、测试或发布动作");
        }
        if ["verify", "closed", "cancel", "rejected", "terminate"].contains(&target)
            || ["closed", "resolved", "cancel"].contains(&from)
        {
            validate_text(note, 1, 8000, "交接或重开说明")?;
        }
        if item.kind == Kind::Task && target == "closed" {
            self.roles(req.actor_id, &[Role::Tester, Role::Manager, Role::Product])?;
        }
        if item.kind == Kind::Requirement && target == "closed" && !accepted(&item) {
            return invalid("需求需先产品验收，再关闭");
        }
        if item.kind == Kind::Build && target == "ready" {
            required_attr(&item.attributes, "code_ref", "代码版本")?;
            required_attr(&item.attributes, "version_name", "构建名称")?;
        }
        if item.kind == Kind::Task && ["doing", "verify"].contains(&target) {
            for link in &item.links {
                let other = self.item(link.target)?;
                if link.relation == Relation::Implements
                    && (other.status != "active"
                        || link.content_version != Some(other.content_version))
                {
                    return invalid("关联需求已变更或未批准，需要确认当前需求版本");
                }
                if link.relation == Relation::DependsOn
                    && !["closed", "cancel"].contains(&other.status.as_str())
                {
                    return invalid("前置依赖任务尚未完成验收");
                }
            }
        }
        if [Kind::Project, Kind::Iteration].contains(&item.kind)
            && target == "closed"
            && self.items.values().any(|t| {
                !t.deleted
                    && t.id != id
                    && (t.project_id == Some(id) || t.iteration_id == Some(id))
                    && [Kind::Task, Kind::Iteration].contains(&t.kind)
                    && !["closed", "cancel"].contains(&t.status.as_str())
            })
        {
            return invalid("仍有未关闭的执行或任务");
        }
        if item.kind == Kind::TestPlan && target == "done" {
            for case_id in attr_ids(&item.attributes, "case_ids")? {
                if self.latest_run(id, case_id).is_none() {
                    return invalid("测试单仍有未执行用例");
                }
            }
        }
        if item.kind == Kind::Bug && target == "active" {
            item.attributes.insert("confirmed".into(), json!(true));
            if ["resolved", "closed"].contains(&from) {
                let count = item
                    .attributes
                    .get("reopen_count")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                item.attributes
                    .insert("reopen_count".into(), json!(count + 1));
                for key in [
                    "resolution",
                    "resolved_build",
                    "duplicate_id",
                    "resolved_by",
                ] {
                    item.attributes.remove(key);
                }
            }
        }
        if item.kind == Kind::Task && target == "verify" {
            item.attributes.insert(
                "finished_by".into(),
                json!({"actor":req.actor_id,"at":req.at,"note":note}),
            );
        }
        item.status = target.into();
        self.items.insert(id, item);
        Ok(())
    }

    // 参数对应同一业务命令，避免为转发字段额外建立一层模型。
    #[allow(clippy::too_many_arguments)]
    fn run_test(
        &mut self,
        req: &Request,
        id: Id,
        plan_id: Id,
        case_id: Id,
        case_version: u64,
        result: &str,
        actual: &str,
    ) -> Result<()> {
        self.roles(req.actor_id, &[Role::Tester, Role::Manager])?;
        self.unused(id)?;
        if !["pass", "fail", "blocked", "skipped"].contains(&result) {
            return invalid("测试结果无效");
        }
        validate_text(actual, 1, 16000, "实际执行记录")?;
        let plan = self.item(plan_id)?;
        if plan.kind != Kind::TestPlan || plan.status != "doing" {
            return invalid("测试单尚未开始或已经结束");
        }
        if !attr_ids(&plan.attributes, "case_ids")?.contains(&case_id) {
            return invalid("用例不在测试单范围内");
        }
        let pinned = plan
            .attributes
            .get("case_versions")
            .and_then(|v| v.get(case_id.to_string()))
            .and_then(Value::as_u64);
        if pinned != Some(case_version) {
            return invalid("必须执行测试单所固定的用例版本");
        }
        let case = self.item(case_id)?;
        let spec = self
            .versions
            .iter()
            .find(|v| v.item_id == case_id && v.version == case_version)
            .ok_or_else(|| Error::NotFound("用例内容版本".into()))?;
        let build_id = attr_id(&plan.attributes, "build_id")?;
        let item = Item {
            id,
            kind: Kind::TestRun,
            title: spec.title.clone(),
            description: spec.description.clone(),
            acceptance: spec.acceptance.clone(),
            status: result.into(),
            priority: case.priority,
            severity: case.severity,
            assigned_to: Some(req.actor_id),
            product_id: plan.product_id,
            project_id: plan.project_id,
            iteration_id: plan.iteration_id,
            parent_id: None,
            links: BTreeSet::from([
                Link {
                    target: case_id,
                    relation: Relation::Related,
                    content_version: Some(case_version),
                },
                Link {
                    target: plan_id,
                    relation: Relation::Related,
                    content_version: None,
                },
            ]),
            attributes: BTreeMap::from([
                ("plan_id".into(), json!(plan_id)),
                ("case_id".into(), json!(case_id)),
                ("case_version".into(), json!(case_version)),
                ("build_id".into(), json!(build_id)),
                ("actual".into(), json!(actual)),
                (
                    "steps".into(),
                    spec.attributes.get("steps").cloned().unwrap_or(Value::Null),
                ),
                (
                    "ordinal".into(),
                    json!(
                        self.items
                            .values()
                            .filter(|i| i.kind == Kind::TestRun)
                            .count()
                            + 1
                    ),
                ),
            ]),
            version: 1,
            content_version: 1,
            created_by: req.actor_id,
            created_at: req.at,
            updated_at: req.at,
            deleted: false,
        };
        self.items.insert(id, item);
        Ok(())
    }

    fn latest_run(&self, plan: Id, case: Id) -> Option<&Item> {
        self.items
            .values()
            .filter(|i| {
                !i.deleted
                    && i.kind == Kind::TestRun
                    && attr_id(&i.attributes, "plan_id").ok() == Some(plan)
                    && attr_id(&i.attributes, "case_id").ok() == Some(case)
            })
            .max_by_key(|i| {
                (
                    i.attributes
                        .get("ordinal")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    i.id,
                )
            })
    }

    fn bug_from_test(
        &mut self,
        req: &Request,
        id: Id,
        run_id: Id,
        title: &str,
        actual: &str,
        environment: &str,
    ) -> Result<()> {
        self.roles(req.actor_id, &[Role::Tester, Role::Manager])?;
        let run = self.item(run_id)?.clone();
        if run.kind != Kind::TestRun || !["fail", "blocked"].contains(&run.status.as_str()) {
            return invalid("只能从失败或阻塞的执行结果创建 Bug");
        }
        let steps = run
            .attributes
            .get("steps")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let expected = steps
            .iter()
            .filter_map(|s| s["expected"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let actions = steps
            .iter()
            .filter_map(|s| s["action"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        self.create(
            req,
            &NewItem {
                id,
                kind: Kind::Bug,
                title: title.into(),
                description: run.description,
                acceptance: String::new(),
                priority: 3,
                severity: 3,
                assigned_to: None,
                product_id: run.product_id,
                project_id: run.project_id,
                iteration_id: run.iteration_id,
                parent_id: None,
                links: BTreeSet::from([Link {
                    target: run_id,
                    relation: Relation::Source,
                    content_version: Some(run.content_version),
                }]),
                attributes: BTreeMap::from([
                    ("steps".into(), json!(actions)),
                    ("expected".into(), json!(expected)),
                    ("actual".into(), json!(actual)),
                    ("environment".into(), json!(environment)),
                    (
                        "affected_build_ids".into(),
                        json!([attr_id(&run.attributes, "build_id")?]),
                    ),
                ]),
            },
        )
    }

    // 参数对应同一业务命令，避免为转发字段额外建立一层模型。
    #[allow(clippy::too_many_arguments)]
    fn resolve_bug(
        &mut self,
        req: &Request,
        id: Id,
        version: u64,
        resolution: &str,
        build_id: Option<Id>,
        duplicate_id: Option<Id>,
        note: &str,
    ) -> Result<()> {
        self.roles(req.actor_id, &[Role::Developer, Role::Manager])?;
        let mut bug = self.expected(id, version)?;
        if bug.kind != Kind::Bug || !["active", "doing"].contains(&bug.status.as_str()) {
            return invalid("只有已确认待处理的 Bug 可以提交解决方案");
        }
        validate_text(note, 1, 8000, "处理说明")?;
        if ![
            "fixed",
            "duplicate",
            "by_design",
            "external",
            "not_reproducible",
            "postponed",
            "will_not_fix",
            "to_requirement",
        ]
        .contains(&resolution)
        {
            return invalid("解决方案无效");
        }
        if (resolution != "fixed" && build_id.is_some())
            || (resolution != "duplicate" && duplicate_id.is_some())
        {
            return invalid("修复构建和重复对象必须与解决方案匹配");
        }
        if resolution == "fixed" {
            let build =
                self.item(build_id.ok_or_else(|| Error::Validation("修复需选择修复构建".into()))?)?;
            if build.kind != Kind::Build
                || build.status != "ready"
                || build.product_id != bug.product_id
            {
                return invalid("修复构建必须是同产品就绪构建");
            }
        }
        if resolution == "duplicate" {
            let other = self.item(
                duplicate_id.ok_or_else(|| Error::Validation("重复 Bug 需要指定原 Bug".into()))?,
            )?;
            if other.id == id || other.kind != Kind::Bug || other.product_id != bug.product_id {
                return invalid("重复 Bug 引用无效");
            }
            let mut next = Some(other.id);
            let mut seen = BTreeSet::from([id]);
            while let Some(other_id) = next {
                if !seen.insert(other_id) {
                    return invalid("重复 Bug 关系不能循环");
                }
                next = attr_id(&self.item(other_id)?.attributes, "duplicate_id").ok();
            }
        }
        bug.status = "resolved".into();
        bug.attributes
            .insert("resolution".into(), json!(resolution));
        bug.attributes.insert(
            "resolved_by".into(),
            json!({"operation_id":req.id,"actor":req.actor_id,"at":req.at,"note":note,"content_version":bug.content_version}),
        );
        if let Some(build) = build_id {
            bug.attributes.insert("resolved_build".into(), json!(build));
        }
        if let Some(other) = duplicate_id {
            bug.attributes.insert("duplicate_id".into(), json!(other));
        }
        self.items.insert(id, bug);
        Ok(())
    }

    // 复测字段直接来自同一命令，保留真实构建与执行证据，避免通用状态切换冒充复测。
    #[allow(clippy::too_many_arguments)]
    fn retest_bug(
        &mut self,
        req: &Request,
        id: Id,
        version: u64,
        build_id: Option<Id>,
        run_id: Option<Id>,
        result: &str,
        note: &str,
    ) -> Result<()> {
        self.roles(req.actor_id, &[Role::Tester, Role::Manager])?;
        let mut bug = self.expected(id, version)?;
        if bug.kind != Kind::Bug || bug.status != "resolved" {
            return invalid("只有已解决待复测的 Bug 可以提交复测结论");
        }
        validate_text(note, 1, 8000, "复测或处理结论确认说明")?;
        if !["pass", "fail", "blocked"].contains(&result) {
            return invalid("复测结果只能是通过、失败或阻塞");
        }
        let resolution = required_attr(&bug.attributes, "resolution", "解决方案")?;
        let resolution_record = bug
            .attributes
            .get("resolved_by")
            .ok_or_else(|| Error::Validation("缺少解决记录".into()))?;
        if resolution_record
            .get("content_version")
            .and_then(Value::as_u64)
            != Some(bug.content_version)
        {
            return invalid("Bug 内容已变更，需要重新激活并提交当前版本的解决方案");
        }
        if resolution == "fixed" {
            let fixed = attr_id(&bug.attributes, "resolved_build")?;
            if build_id != Some(fixed) {
                return invalid("复测必须明确选择提交解决时的修复构建");
            }
            let build = self.item(fixed)?;
            if build.kind != Kind::Build
                || build.status != "ready"
                || build.product_id != bug.product_id
            {
                return invalid("复测构建必须是同产品就绪构建");
            }
            let source_cases: BTreeSet<_> = bug
                .links
                .iter()
                .filter(|link| link.relation == Relation::Source)
                .filter_map(|link| self.items.get(&link.target))
                .filter(|item| item.kind == Kind::TestRun)
                .map(|item| attr_id(&item.attributes, "case_id"))
                .collect::<Result<_>>()?;
            if !source_cases.is_empty() && run_id.is_none() {
                return invalid("测试执行产生的 Bug 必须关联原用例的回归执行记录");
            }
            if let Some(run_id) = run_id {
                let run = self.item(run_id)?;
                if run.kind != Kind::TestRun
                    || run.product_id != bug.product_id
                    || attr_id(&run.attributes, "build_id")? != fixed
                    || run.status != result
                {
                    return invalid("复测执行的构建、产品和结果必须与结论一致");
                }
                let case = attr_id(&run.attributes, "case_id")?;
                let plan = attr_id(&run.attributes, "plan_id")?;
                if (!source_cases.is_empty() && !source_cases.contains(&case))
                    || self.latest_run(plan, case).map(|item| item.id) != Some(run_id)
                {
                    return invalid("复测必须使用原用例的最新执行记录");
                }
                let resolved_operation = resolution_record
                    .get("operation_id")
                    .and_then(Value::as_str)
                    .and_then(|value| Id::parse_str(value).ok())
                    .ok_or_else(|| Error::Validation("缺少解决操作标识".into()))?;
                let resolved_at = self.history.iter().rposition(|audit| {
                    audit.object_id == id && audit.operation_id == resolved_operation
                });
                let executed_at = self
                    .history
                    .iter()
                    .position(|audit| audit.object_id == run_id && audit.before.is_null());
                if !matches!((resolved_at, executed_at), (Some(resolved), Some(executed)) if executed > resolved)
                {
                    return invalid("复测执行必须在提交解决之后单独保存");
                }
            }
        } else if build_id.is_some() || run_id.is_some() {
            return invalid("非修复方案只确认处理结论，不能伪造修复构建或测试证据");
        }
        let record = json!({
            "operation_id":req.id, "actor_id":req.actor_id, "at":req.at,
            "content_version":bug.content_version, "result":result, "note":note,
            "build_id":build_id, "run_id":run_id, "resolution":resolution,
            "resolution_record":resolution_record,
            "duplicate_id":bug.attributes.get("duplicate_id")
        });
        let records = bug
            .attributes
            .entry("retest_records".into())
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| Error::Validation("复测记录格式无效".into()))?;
        records.push(record);
        if result == "pass" {
            bug.status = "closed".into();
        }
        self.items.insert(id, bug);
        if result == "fail" {
            self.transition(req, id, version, "active", note)?;
        }
        Ok(())
    }

    fn publish(&mut self, req: &Request, id: Id, version: u64, note: &str) -> Result<()> {
        self.roles(req.actor_id, &[Role::Product, Role::Manager])?;
        let mut release = self.expected(id, version)?;
        if release.kind != Kind::Release || release.status != "wait" {
            return invalid("仅待发布版本可以正式发布");
        }
        validate_text(note, 1, 16000, "发布说明")?;
        required_attr(&release.attributes, "version_name", "版本号")?;
        let builds = attr_ids(&release.attributes, "build_ids")?;
        let plans = attr_ids(&release.attributes, "test_plan_ids")?;
        if builds.is_empty() || plans.is_empty() {
            return invalid("发布需要构建与测试单");
        }
        let mut snapshot_items = BTreeMap::new();
        for build_id in &builds {
            let build = self.item(*build_id)?;
            if build.kind != Kind::Build
                || build.status != "ready"
                || build.product_id != release.product_id
            {
                return invalid("发布只能包含同产品就绪构建");
            }
            snapshot_items.insert(*build_id, serde_json::to_value(build).map_err(storage)?);
            if !plans.iter().any(|p| {
                self.item(*p)
                    .ok()
                    .is_some_and(|p| attr_id(&p.attributes, "build_id").ok() == Some(*build_id))
            }) {
                return invalid("有构建缺少测试单");
            }
        }
        for plan_id in &plans {
            let plan = self.item(*plan_id)?;
            if plan.kind != Kind::TestPlan
                || plan.status != "done"
                || !builds.contains(&attr_id(&plan.attributes, "build_id")?)
            {
                return invalid("测试单尚未结束或不属于本次构建");
            }
            snapshot_items.insert(*plan_id, serde_json::to_value(plan).map_err(storage)?);
            for case_id in attr_ids(&plan.attributes, "case_ids")? {
                let run = self
                    .latest_run(*plan_id, case_id)
                    .ok_or_else(|| Error::Validation("发布存在未执行用例".into()))?;
                if !["pass", "skipped"].contains(&run.status.as_str()) {
                    return invalid("发布存在失败或阻塞测试");
                }
                snapshot_items.insert(run.id, serde_json::to_value(run).map_err(storage)?);
            }
        }
        let mut known = BTreeSet::new();
        for link in &release.links {
            let item = self.item(link.target)?;
            match (link.relation, item.kind) {
                (Relation::Contains, Kind::Requirement) if !accepted(item) => {
                    return invalid("发布需求尚未通过当前版本的产品验收")
                }
                (Relation::Contains, Kind::Bug)
                    if item.status != "closed"
                        || attr_text(&item.attributes, "resolution") != Some("fixed") =>
                {
                    return invalid("修复清单包含尚未复测关闭的代码修复")
                }
                (Relation::KnownIssue, Kind::Bug) => {
                    let reason = release
                        .attributes
                        .get("known_issue_notes")
                        .and_then(|v| v.get(item.id.to_string()))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    validate_text(reason, 1, 8000, "遗留问题说明")?;
                    known.insert(item.id);
                }
                _ => {}
            }
            snapshot_items.insert(item.id, serde_json::to_value(item).map_err(storage)?);
        }
        for bug in self.items.values().filter(|i| {
            !i.deleted
                && i.kind == Kind::Bug
                && i.product_id == release.product_id
                && i.severity <= 2
                && i.status != "closed"
        }) {
            if !attr_ids(&bug.attributes, "affected_build_ids")?.is_disjoint(&builds)
                && !known.contains(&bug.id)
            {
                return invalid("构建仍有未处理的高严重度 Bug");
            }
        }
        release.status = "normal".into();
        release.attributes.insert(
            "snapshot".into(),
            json!({"items":snapshot_items,"actor":req.actor_id,"at":req.at,"note":note}),
        );
        self.items.insert(id, release);
        Ok(())
    }

    fn unique_account(&self, member: &Member) -> Result<()> {
        if self
            .members
            .values()
            .any(|m| m.id != member.id && m.account.eq_ignore_ascii_case(&member.account))
        {
            return invalid("成员账号已存在");
        }
        Ok(())
    }

    fn comment_for_edit(&self, actor: Id, id: Id, version: u64) -> Result<Comment> {
        self.writable(actor)?;
        let comment = self
            .comments
            .get(&id)
            .filter(|c| !c.deleted)
            .ok_or_else(|| Error::NotFound("评论".into()))?;
        self.item(comment.item_id)?;
        if comment.actor_id != actor {
            self.roles(actor, &[Role::Manager])?;
        }
        if comment.version != version {
            return Err(Error::Conflict("评论已更新".into()));
        }
        Ok(comment.clone())
    }

    fn reconcile(&mut self, before: &State, req: &Request) -> Result<()> {
        for item in self.items.values_mut() {
            let old = before.items.get(&item.id);
            if old == Some(item) {
                continue;
            }
            let changed = old.is_none_or(|old| content(old) != content(item));
            if let Some(old) = old {
                item.version = old.version + 1;
                if changed {
                    item.content_version = old.content_version + 1;
                    item.attributes.remove("acceptance_record");
                    if item.kind == Kind::Requirement
                        && ["active", "reviewing"].contains(&item.status.as_str())
                    {
                        item.status = "changing".into();
                    }
                    if item.kind == Kind::TestCase
                        && ["active", "reviewing"].contains(&item.status.as_str())
                    {
                        item.status = "draft".into();
                    }
                }
            }
            item.updated_at = req.at;
            if changed {
                self.versions.push(ContentVersion {
                    item_id: item.id,
                    version: item.content_version,
                    title: item.title.clone(),
                    description: item.description.clone(),
                    acceptance: item.acceptance.clone(),
                    attributes: item.attributes.clone(),
                    actor_id: req.actor_id,
                    at: req.at,
                });
            }
        }
        Ok(())
    }

    fn objects(&self) -> Result<BTreeMap<Id, Value>> {
        let mut objects = BTreeMap::new();
        for (id, value) in &self.items {
            objects.insert(*id, serde_json::to_value(value).map_err(storage)?);
        }
        for (id, value) in &self.members {
            objects.insert(*id, serde_json::to_value(value).map_err(storage)?);
        }
        for (id, value) in &self.comments {
            objects.insert(*id, serde_json::to_value(value).map_err(storage)?);
        }
        for (id, value) in &self.attachments {
            objects.insert(*id, serde_json::to_value(value).map_err(storage)?);
        }
        Ok(objects)
    }
}

fn content(item: &Item) -> Value {
    let attributes: BTreeMap<_, _> = item
        .attributes
        .iter()
        .filter(|(key, _)| {
            [
                "steps",
                "expected",
                "preconditions",
                "actual",
                "environment",
                "code_ref",
                "version_name",
            ]
            .contains(&key.as_str())
        })
        .collect();
    json!([item.title, item.description, item.acceptance, attributes])
}

fn accepted(item: &Item) -> bool {
    item.attributes.get("acceptance_record").is_some_and(|r| {
        r["accepted"] == true && r["version"].as_u64() == Some(item.content_version)
    })
}

fn references(item: &Item, id: Id) -> bool {
    item.product_id == Some(id)
        || item.project_id == Some(id)
        || item.iteration_id == Some(id)
        || item.parent_id == Some(id)
        || item.links.iter().any(|l| l.target == id)
        || [
            "build_id",
            "plan_id",
            "case_id",
            "resolved_build",
            "duplicate_id",
            "converted_id",
        ]
        .iter()
        .any(|k| attr_id(&item.attributes, k).ok() == Some(id))
        || [
            "product_ids",
            "case_ids",
            "build_ids",
            "test_plan_ids",
            "affected_build_ids",
        ]
        .iter()
        .any(|k| attr_ids(&item.attributes, k).is_ok_and(|ids| ids.contains(&id)))
}

fn initial_status(kind: Kind) -> &'static str {
    match kind {
        Kind::Product | Kind::Module | Kind::Document => "active",
        Kind::Inbox | Kind::Bug => "new",
        Kind::Requirement | Kind::TestCase | Kind::Build | Kind::Release => "draft",
        Kind::TestRun => "pending",
        _ => "wait",
    }
}

fn validate_user_attributes(attrs: &BTreeMap<String, Value>) -> Result<()> {
    for key in [
        "acceptance_record",
        "snapshot",
        "resolution",
        "resolved_build",
        "duplicate_id",
        "confirmed",
        "converted_id",
        "reopen_count",
        "case_versions",
        "review_result",
        "resolved_by",
        "retest_records",
        "finished_by",
    ] {
        if attrs.contains_key(key) {
            return invalid("流程记录只能由对应业务动作生成");
        }
    }
    Ok(())
}

fn validate_member(member: &Member) -> Result<()> {
    validate_text(&member.name, 1, 120, "成员姓名")?;
    validate_text(&member.account, 1, 120, "成员账号")?;
    validate_text(&member.department, 0, 120, "部门")?;
    if member.id.is_nil()
        || member.roles.is_empty()
        || member.account.trim() != member.account
        || member.account.contains(char::is_whitespace)
    {
        return invalid("成员标识、账号或角色无效");
    }
    Ok(())
}

fn validate_text(text: &str, min: usize, max: usize, label: &str) -> Result<()> {
    let len = text.trim().chars().count();
    if len < min || len > max || text.contains('\0') {
        return invalid(&format!("{label}长度应为 {min} 至 {max} 字符"));
    }
    Ok(())
}

fn attr_text<'a>(attrs: &'a BTreeMap<String, Value>, key: &str) -> Option<&'a str> {
    attrs.get(key).and_then(Value::as_str)
}
fn required_attr<'a>(
    attrs: &'a BTreeMap<String, Value>,
    key: &str,
    label: &str,
) -> Result<&'a str> {
    let value = attr_text(attrs, key).unwrap_or("");
    validate_text(value, 1, 16000, label)?;
    Ok(value)
}
fn attr_id(attrs: &BTreeMap<String, Value>, key: &str) -> Result<Id> {
    attr_text(attrs, key)
        .and_then(|s| Id::parse_str(s).ok())
        .filter(|id| !id.is_nil())
        .ok_or_else(|| Error::Validation(format!("{key}需要有效对象标识")))
}
fn attr_ids(attrs: &BTreeMap<String, Value>, key: &str) -> Result<BTreeSet<Id>> {
    let Some(value) = attrs.get(key) else {
        return Ok(BTreeSet::new());
    };
    let array = value
        .as_array()
        .ok_or_else(|| Error::Validation(format!("{key}需要对象列表")))?;
    let mut ids = BTreeSet::new();
    for value in array {
        let id = value
            .as_str()
            .and_then(|s| Id::parse_str(s).ok())
            .filter(|id| !id.is_nil())
            .ok_or_else(|| Error::Validation(format!("{key}包含无效标识")))?;
        if !ids.insert(id) {
            return invalid("对象列表不能重复");
        }
    }
    Ok(ids)
}
fn command_note(command: &Command) -> String {
    match command {
        Command::Review { note, .. }
        | Command::Transition { note, .. }
        | Command::AcceptRequirement { note, .. }
        | Command::ResolveBug { note, .. }
        | Command::RetestBug { note, .. }
        | Command::Publish { note, .. }
        | Command::Archive { note, .. } => note.clone(),
        _ => String::new(),
    }
}
fn invalid<T>(message: &str) -> Result<T> {
    Err(Error::Validation(message.into()))
}
fn storage(error: impl std::fmt::Display) -> Error {
    Error::Storage(error.to_string())
}
