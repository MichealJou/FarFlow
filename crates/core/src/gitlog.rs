use crate::{
    Command, Error, Id, Kind, Member, Mode, Receipt, Request, Result, Role, State, WorkspaceInfo,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitLog {
    genesis: State,
    operations: BTreeMap<Id, GitOperation>,
    #[serde(skip)]
    cache: OnceLock<GitView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitOperation {
    pub schema: u32,
    pub workspace_id: Id,
    pub id: Id,
    pub device_id: Id,
    pub parents: BTreeSet<Id>,
    pub change: GitChange,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum GitChange {
    Apply { request: Request },
    Resolve { resolution: GitResolution },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitResolution {
    pub id: Id,
    pub actor_id: Id,
    pub at: i64,
    pub targets: BTreeSet<Id>,
    pub command: Option<Command>,
    pub note: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitIssueKind {
    Concurrent,
    Blocked,
    Constraint,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitIssue {
    pub operation_id: Id,
    pub kind: GitIssueKind,
    pub peers: BTreeSet<Id>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitView {
    pub state: State,
    pub issues: BTreeMap<Id, GitIssue>,
    pub receipts: BTreeMap<Id, Receipt>,
}

struct Dag {
    order: Vec<Id>,
    clocks: BTreeMap<Id, BTreeMap<Id, u64>>,
}

type Pairs = BTreeSet<(Id, Id)>;

impl GitLog {
    pub fn new(id: Id, name: String, owner: Member) -> Result<Self> {
        Ok(Self {
            genesis: State::new(id, name, owner)?,
            operations: BTreeMap::new(),
            cache: OnceLock::new(),
        })
    }

    pub fn info(&self) -> WorkspaceInfo {
        WorkspaceInfo {
            schema: 1,
            id: self.genesis.id,
            name: self.genesis.name.clone(),
            mode: Mode::Git,
        }
    }

    pub fn operations(&self) -> &BTreeMap<Id, GitOperation> {
        &self.operations
    }

    pub(crate) fn genesis(&self) -> &State {
        &self.genesis
    }

    pub(crate) fn from_parts(
        genesis: State,
        operations: BTreeMap<Id, GitOperation>,
    ) -> Result<Self> {
        let log = Self {
            genesis,
            operations,
            cache: OnceLock::new(),
        };
        log.view()?;
        Ok(log)
    }

    pub fn view(&self) -> Result<GitView> {
        if let Some(view) = self.cache.get() {
            return Ok(view.clone());
        }
        self.validate_genesis()?;
        let dag = Dag::new(self)?;
        let mut pairs = Pairs::new();
        let mut previous = BTreeSet::new();
        let mut heads = BTreeSet::new();
        let mut prefix = self.empty_view();
        for id in &dag.order {
            let operation = &self.operations[id];
            if heads.iter().all(|head| dag.ancestor(self, *head, *id)) {
                apply_causal(operation, &mut prefix)?;
                previous.insert(*id);
                for parent in &operation.parents {
                    heads.remove(parent);
                }
                heads.insert(*id);
                continue;
            }
            let ancestors: BTreeSet<_> = previous
                .iter()
                .copied()
                .filter(|p| dag.ancestor(self, *p, *id))
                .collect();
            let basis = if heads.iter().all(|head| dag.ancestor(self, *head, *id)) {
                prefix.clone()
            } else {
                self.materialize(&dag, &pairs, &ancestors)
            };
            self.validate_change(operation, &basis).map_err(|error| {
                Error::Validation(format!("操作 {id} 在原始上下文中无效：{error}"))
            })?;
            let mut changed = false;
            for other in &previous {
                if dag.ancestor(self, *other, *id) {
                    continue;
                }
                let peer = &self.operations[other];
                let common: BTreeSet<_> = previous
                    .iter()
                    .copied()
                    .filter(|p| {
                        (p != other && dag.ancestor(self, *p, *other)) || ancestors.contains(p)
                    })
                    .collect();
                let common_view = self.materialize(&dag, &pairs, &common);
                if !commutes(operation, peer, &common_view.state) {
                    pairs.insert(ordered(*id, *other));
                    changed = true;
                }
            }
            previous.insert(*id);
            if changed {
                prefix = self.materialize(&dag, &pairs, &previous);
            } else {
                self.apply_to_view(&dag, &pairs, &previous, operation, &mut prefix);
            }
            for parent in &operation.parents {
                heads.remove(parent);
            }
            heads.insert(*id);
        }
        let _ = self.cache.set(prefix.clone());
        Ok(prefix)
    }

    pub fn execute(&mut self, device_id: Id, request: Request) -> Result<Receipt> {
        if let Some(existing) = self.operations.get(&request.id) {
            if matches!(&existing.change,GitChange::Apply{request:old} if encoded(old).ok()==encoded(&request).ok())
            {
                return self
                    .view()?
                    .receipts
                    .get(&request.id)
                    .cloned()
                    .ok_or_else(|| Error::Conflict("该操作处于同步冲突中".into()));
            }
            return Err(Error::Conflict("操作标识已被不同内容使用".into()));
        }
        let current = self.view()?;
        if !current.issues.is_empty() {
            return Err(Error::Conflict("请先处理同步冲突，再提交新业务操作".into()));
        }
        let id = request.id;
        let operation = GitOperation {
            schema: 1,
            workspace_id: self.genesis.id,
            id,
            device_id,
            parents: self.heads(),
            change: GitChange::Apply { request },
        };
        self.append(operation)?;
        self.view()?
            .receipts
            .get(&id)
            .cloned()
            .ok_or_else(|| Error::Conflict("新操作发生同步冲突".into()))
    }

    pub fn resolve(&mut self, device_id: Id, resolution: GitResolution) -> Result<Receipt> {
        if let Some(existing) = self.operations.get(&resolution.id) {
            if matches!(&existing.change,GitChange::Resolve{resolution:old} if encoded(old).ok()==encoded(&resolution).ok())
            {
                return self
                    .view()?
                    .receipts
                    .get(&resolution.id)
                    .cloned()
                    .ok_or_else(|| Error::Conflict("该冲突决策本身需要处理".into()));
            }
            return Err(Error::Conflict("决策标识已被不同内容使用".into()));
        }
        let id = resolution.id;
        self.append(GitOperation {
            schema: 1,
            workspace_id: self.genesis.id,
            id,
            device_id,
            parents: self.heads(),
            change: GitChange::Resolve { resolution },
        })?;
        self.view()?
            .receipts
            .get(&id)
            .cloned()
            .ok_or_else(|| Error::Conflict("冲突决策尚未生效".into()))
    }

    pub fn merge(&mut self, incoming: &Self) -> Result<GitView> {
        if encoded(&self.genesis)? != encoded(&incoming.genesis)? {
            return Err(Error::Conflict("不能合并不同初始工作空间的数据".into()));
        }
        let mut next = self.clone();
        let mut changed = false;
        for (id, operation) in &incoming.operations {
            if let Some(existing) = next.operations.get(id) {
                if encoded(existing)? != encoded(operation)? {
                    return Err(Error::Conflict(format!("同一操作 {id} 内容不同")));
                }
            } else {
                next.operations.insert(*id, operation.clone());
                changed = true;
            }
        }
        if !changed {
            return self.view();
        }
        next.cache.take();
        let view = next.view()?;
        *self = next;
        Ok(view)
    }

    fn append(&mut self, operation: GitOperation) -> Result<()> {
        if operation.id.is_nil() || operation.device_id.is_nil() {
            return invalid("操作与设备标识不能为空");
        }
        let mut view = self.view()?;
        apply_causal(&operation, &mut view)?;
        self.operations.insert(operation.id, operation);
        self.cache.take();
        let _ = self.cache.set(view);
        Ok(())
    }

    fn heads(&self) -> BTreeSet<Id> {
        let mut heads: BTreeSet<_> = self.operations.keys().copied().collect();
        for operation in self.operations.values() {
            for parent in &operation.parents {
                heads.remove(parent);
            }
        }
        heads
    }

    fn validate_genesis(&self) -> Result<()> {
        let owner = self
            .genesis
            .members
            .values()
            .next()
            .cloned()
            .ok_or_else(|| Error::Validation("工作空间缺少初始所有者".into()))?;
        let expected = State::new(self.genesis.id, self.genesis.name.clone(), owner)?;
        if encoded(&expected)? != encoded(&self.genesis)? {
            return invalid("初始工作空间内容无效");
        }
        Ok(())
    }

    fn empty_view(&self) -> GitView {
        GitView {
            state: self.genesis.clone(),
            issues: BTreeMap::new(),
            receipts: BTreeMap::new(),
        }
    }

    fn validate_change(&self, operation: &GitOperation, basis: &GitView) -> Result<()> {
        match &operation.change {
            GitChange::Apply { request } => {
                if !basis.issues.is_empty() {
                    return invalid("业务操作依赖尚未解决的冲突");
                }
                basis.state.clone().execute(request.clone())?;
            }
            GitChange::Resolve { resolution } => {
                validate_resolution(resolution, &basis.state)?;
                if resolution.targets.is_empty()
                    || resolution
                        .targets
                        .iter()
                        .any(|id| !basis.issues.contains_key(id))
                {
                    return invalid("冲突决策必须引用当前尚未解决的问题");
                }
                if let Some(command) = &resolution.command {
                    basis.state.clone().execute(Request {
                        id: resolution.id,
                        actor_id: resolution.actor_id,
                        at: resolution.at,
                        command: command.clone(),
                    })?;
                }
            }
        }
        Ok(())
    }

    fn materialize(&self, dag: &Dag, pairs: &Pairs, included: &BTreeSet<Id>) -> GitView {
        let mut view = self.empty_view();
        for id in &dag.order {
            if included.contains(id) {
                self.apply_to_view(dag, pairs, included, &self.operations[id], &mut view);
            }
        }
        view
    }

    fn apply_to_view(
        &self,
        dag: &Dag,
        pairs: &Pairs,
        included: &BTreeSet<Id>,
        operation: &GitOperation,
        view: &mut GitView,
    ) {
        let peers: BTreeSet<_> = pairs
            .iter()
            .filter_map(|(a, b)| {
                if *a == operation.id && included.contains(b) {
                    Some(*b)
                } else if *b == operation.id && included.contains(a) {
                    Some(*a)
                } else {
                    None
                }
            })
            .collect();
        if !peers.is_empty() {
            view.issues.insert(
                operation.id,
                GitIssue {
                    operation_id: operation.id,
                    kind: GitIssueKind::Concurrent,
                    peers,
                    message: "并发操作的执行顺序会改变业务结果，需要明确决策".into(),
                },
            );
            return;
        }
        let acknowledged: BTreeSet<_> = self
            .operations
            .values()
            .filter_map(|node| {
                if view.receipts.contains_key(&node.id) && dag.ancestor(self, node.id, operation.id)
                {
                    if let GitChange::Resolve { resolution } = &node.change {
                        return Some(resolution.targets.iter().copied());
                    }
                }
                None
            })
            .flatten()
            .collect();
        // 并发决策不能替一个尚未见过该决策的旧分支确认前置冲突。
        let blocked: BTreeSet<_> = included
            .iter()
            .copied()
            .filter(|id| {
                *id != operation.id
                    && dag.ancestor(self, *id, operation.id)
                    && !view.receipts.contains_key(id)
                    && !acknowledged.contains(id)
            })
            .collect();
        match &operation.change {
            GitChange::Apply { request } => {
                if !blocked.is_empty() {
                    view.issues.insert(
                        operation.id,
                        GitIssue {
                            operation_id: operation.id,
                            kind: GitIssueKind::Blocked,
                            peers: blocked,
                            message: "前置操作发生冲突，原始后续操作保留但暂不执行".into(),
                        },
                    );
                    return;
                }
                match execute_rebased(&mut view.state, request) {
                    Ok(receipt) => {
                        view.receipts.insert(operation.id, receipt);
                    }
                    Err(error) => {
                        view.issues.insert(
                            operation.id,
                            GitIssue {
                                operation_id: operation.id,
                                kind: GitIssueKind::Constraint,
                                peers: BTreeSet::new(),
                                message: error.to_string(),
                            },
                        );
                    }
                }
            }
            GitChange::Resolve { resolution } => {
                let result = validate_resolution(resolution, &view.state).and_then(|()| {
                    if resolution
                        .targets
                        .iter()
                        .any(|id| !view.issues.contains_key(id))
                    {
                        return invalid("所引用的问题已变化，请重新核对冲突");
                    }
                    if let Some(command) = &resolution.command {
                        execute_rebased(
                            &mut view.state,
                            &Request {
                                id: resolution.id,
                                actor_id: resolution.actor_id,
                                at: resolution.at,
                                command: command.clone(),
                            },
                        )
                    } else {
                        Ok(Receipt {
                            operation_id: operation.id,
                            revision: view.state.revision,
                            affected: Vec::new(),
                        })
                    }
                });
                match result {
                    Ok(receipt) => {
                        for id in &resolution.targets {
                            view.issues.remove(id);
                        }
                        view.receipts.insert(operation.id, receipt);
                    }
                    Err(error) => {
                        view.issues.insert(
                            operation.id,
                            GitIssue {
                                operation_id: operation.id,
                                kind: GitIssueKind::Constraint,
                                peers: resolution.targets.clone(),
                                message: error.to_string(),
                            },
                        );
                    }
                }
            }
        }
    }
}

impl Dag {
    fn new(log: &GitLog) -> Result<Self> {
        let mut children: BTreeMap<Id, Vec<Id>> = BTreeMap::new();
        let mut remaining = BTreeMap::new();
        let mut ready = BTreeSet::new();
        for (id, operation) in &log.operations {
            if id.is_nil()
                || operation.id != *id
                || operation.device_id.is_nil()
                || operation.schema != 1
                || operation.workspace_id != log.genesis.id
            {
                return invalid("操作标识、设备或格式版本无效");
            }
            let change_id = match &operation.change {
                GitChange::Apply { request } => request.id,
                GitChange::Resolve { resolution } => resolution.id,
            };
            if change_id != *id {
                return invalid("操作包和业务命令的标识不一致");
            }
            for parent in &operation.parents {
                if !log.operations.contains_key(parent) {
                    return invalid(&format!("操作 {id} 缺少依赖 {parent}"));
                }
                children.entry(*parent).or_default().push(*id);
            }
            remaining.insert(*id, operation.parents.len());
            if operation.parents.is_empty() {
                ready.insert(*id);
            }
        }
        let mut order = Vec::new();
        let mut clocks: BTreeMap<Id, BTreeMap<Id, u64>> = BTreeMap::new();
        let mut positions = BTreeSet::new();
        while let Some(id) = ready.pop_first() {
            let operation = &log.operations[&id];
            let mut clock = BTreeMap::<Id, u64>::new();
            for parent in &operation.parents {
                for (device, counter) in &clocks[parent] {
                    clock
                        .entry(*device)
                        .and_modify(|v| *v = (*v).max(*counter))
                        .or_insert(*counter);
                }
            }
            let sequence = clock.get(&operation.device_id).copied().unwrap_or(0) + 1;
            if !positions.insert((operation.device_id, sequence)) {
                return Err(Error::Conflict(
                    "设备身份被不同副本重复使用，请为新副本建立独立设备身份".into(),
                ));
            }
            clock.insert(operation.device_id, sequence);
            clocks.insert(id, clock);
            order.push(id);
            if let Some(children) = children.get(&id) {
                for child in children {
                    let count = remaining.get_mut(child).expect("依赖节点已验证");
                    *count -= 1;
                    if *count == 0 {
                        ready.insert(*child);
                    }
                }
            }
        }
        if order.len() != log.operations.len() {
            return invalid("操作依赖存在循环");
        }
        Ok(Self { order, clocks })
    }

    fn ancestor(&self, log: &GitLog, ancestor: Id, descendant: Id) -> bool {
        let device = log.operations[&ancestor].device_id;
        self.clocks[&descendant].get(&device).copied().unwrap_or(0)
            >= self.clocks[&ancestor][&device]
    }
}

fn apply_causal(operation: &GitOperation, view: &mut GitView) -> Result<()> {
    let receipt = match &operation.change {
        GitChange::Apply { request } => {
            if !view.issues.is_empty() {
                return invalid("业务操作依赖尚未解决的冲突");
            }
            view.state.execute(request.clone())?
        }
        GitChange::Resolve { resolution } => {
            validate_resolution(resolution, &view.state)?;
            if resolution.targets.is_empty()
                || resolution
                    .targets
                    .iter()
                    .any(|id| !view.issues.contains_key(id))
            {
                return invalid("冲突决策必须引用当前尚未解决的问题");
            }
            let receipt = if let Some(command) = &resolution.command {
                view.state.execute(Request {
                    id: resolution.id,
                    actor_id: resolution.actor_id,
                    at: resolution.at,
                    command: command.clone(),
                })?
            } else {
                Receipt {
                    operation_id: operation.id,
                    revision: view.state.revision,
                    affected: Vec::new(),
                }
            };
            for id in &resolution.targets {
                view.issues.remove(id);
            }
            receipt
        }
    };
    view.receipts.insert(operation.id, receipt.clone());
    Ok(())
}

fn validate_resolution(resolution: &GitResolution, state: &State) -> Result<()> {
    if resolution.id.is_nil()
        || resolution.at < 0
        || resolution.note.trim().is_empty()
        || resolution.note.chars().count() > 8000
        || resolution.note.contains('\0')
    {
        return invalid("冲突决策需要有效标识、时间和说明");
    }
    let member = state
        .members
        .get(&resolution.actor_id)
        .filter(|m| m.active)
        .ok_or_else(|| Error::Forbidden("冲突决策账户未启用".into()))?;
    if !member
        .roles
        .iter()
        .any(|r| [Role::Owner, Role::Manager].contains(r))
    {
        return Err(Error::Forbidden(
            "只有所有者或项目经理可以处理同步冲突".into(),
        ));
    }
    Ok(())
}

fn commutes(a: &GitOperation, b: &GitOperation, basis: &State) -> bool {
    if let (GitChange::Resolve { resolution: a }, GitChange::Resolve { resolution: b }) =
        (&a.change, &b.change)
    {
        if !a.targets.is_disjoint(&b.targets) {
            return false;
        }
    }
    let run = |first: &GitOperation, second: &GitOperation| -> Result<Value> {
        let mut state = basis.clone();
        simulate(first, &mut state)?;
        simulate(second, &mut state)?;
        semantic(&state)
    };
    matches!((run(a,b),run(b,a)),(Ok(left),Ok(right)) if left==right)
}

fn simulate(operation: &GitOperation, state: &mut State) -> Result<()> {
    match &operation.change {
        GitChange::Apply { request } => {
            execute_rebased(state, request)?;
        }
        GitChange::Resolve { resolution } => {
            validate_resolution(resolution, state)?;
            if let Some(command) = &resolution.command {
                execute_rebased(
                    state,
                    &Request {
                        id: resolution.id,
                        actor_id: resolution.actor_id,
                        at: resolution.at,
                        command: command.clone(),
                    },
                )?;
            }
        }
    }
    Ok(())
}

fn execute_rebased(state: &mut State, original: &Request) -> Result<Receipt> {
    let mut request = original.clone();
    rebase_versions(&mut request.command, state);
    let receipt = state.execute(request)?;
    // 规范投影中的修订号可以变化，幂等指纹始终指向不可变的原始操作。
    if let Some(applied) = state.operations.get_mut(&original.id) {
        applied.fingerprint = format!("{:x}", Sha256::digest(encoded(original)?));
    }
    Ok(receipt)
}

fn rebase_versions(command: &mut Command, state: &State) {
    match command {
        Command::Update { id, version, .. }
        | Command::Review { id, version, .. }
        | Command::Transition { id, version, .. }
        | Command::AcceptRequirement { id, version, .. }
        | Command::ConvertInbox { id, version, .. }
        | Command::ResolveBug { id, version, .. }
        | Command::RetestBug { id, version, .. }
        | Command::Publish { id, version, .. }
        | Command::Archive { id, version, .. }
        | Command::Restore { id, version } => {
            if let Some(item) = state.items.get(id) {
                *version = item.version;
            }
        }
        Command::EditComment { id, version, .. } | Command::DeleteComment { id, version } => {
            if let Some(comment) = state.comments.get(id) {
                *version = comment.version;
            }
        }
        Command::Batch { commands } => {
            for command in commands {
                rebase_versions(command, state);
            }
        }
        _ => {}
    }
}

fn semantic(state: &State) -> Result<Value> {
    let mut items = serde_json::to_value(&state.items).map_err(storage)?;
    let mut latest = BTreeMap::<String, (u64, Id)>::new();
    for item in state.items.values() {
        if let Some(value) = items
            .get_mut(item.id.to_string())
            .and_then(Value::as_object_mut)
        {
            value.remove("updated_at");
            if item.kind == Kind::TestRun {
                let ordinal = item
                    .attributes
                    .get("ordinal")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let key = format!(
                    "{}:{}",
                    item.attributes["plan_id"], item.attributes["case_id"]
                );
                latest
                    .entry(key)
                    .and_modify(|v| *v = (*v).max((ordinal, item.id)))
                    .or_insert((ordinal, item.id));
                if let Some(attributes) = value.get_mut("attributes").and_then(Value::as_object_mut)
                {
                    attributes.remove("ordinal");
                }
            }
        }
    }
    let mut versions = state.versions.clone();
    versions.sort_by_key(|v| (v.item_id, v.version));
    let mut versions = serde_json::to_value(versions).map_err(storage)?;
    if let Some(versions) = versions.as_array_mut() {
        for version in versions {
            if let Some(value) = version.as_object_mut() {
                value.remove("actor_id");
                value.remove("at");
                if value
                    .get("item_id")
                    .and_then(Value::as_str)
                    .and_then(|id| Id::parse_str(id).ok())
                    .and_then(|id| state.items.get(&id))
                    .is_some_and(|item| item.kind == Kind::TestRun)
                {
                    if let Some(attributes) =
                        value.get_mut("attributes").and_then(Value::as_object_mut)
                    {
                        attributes.remove("ordinal");
                    }
                }
            }
        }
    }
    let latest: BTreeMap<_, _> = latest.into_iter().map(|(key, (_, id))| (key, id)).collect();
    Ok(
        json!({"items":items,"versions":versions,"members":state.members,"comments":state.comments,"attachments":state.attachments,"latest_runs":latest}),
    )
}

fn ordered(a: Id, b: Id) -> (Id, Id) {
    if a < b {
        (a, b)
    } else {
        (b, a)
    }
}
fn encoded(value: &impl Serialize) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(storage)
}
fn storage(error: impl std::fmt::Display) -> Error {
    Error::Storage(error.to_string())
}
fn invalid<T>(message: &str) -> Result<T> {
    Err(Error::Validation(message.into()))
}
