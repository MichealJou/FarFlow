use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

pub type Id = Uuid;
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("未找到：{0}")]
    NotFound(String),
    #[error("没有权限：{0}")]
    Forbidden(String),
    #[error("数据校验失败：{0}")]
    Validation(String),
    #[error("版本冲突：{0}")]
    Conflict(String),
    #[error("存储错误：{0}")]
    Storage(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Local,
    Git,
    Service,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Owner,
    Product,
    Manager,
    Developer,
    Tester,
    Viewer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Member {
    pub id: Id,
    pub account: String,
    pub name: String,
    pub roles: BTreeSet<Role>,
    pub department: String,
    pub active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Product,
    Module,
    Project,
    Iteration,
    Inbox,
    Requirement,
    Plan,
    Task,
    TestCase,
    TestPlan,
    TestRun,
    Build,
    Bug,
    Release,
    Document,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Relation {
    Related,
    Source,
    Implements,
    Verifies,
    Contains,
    KnownIssue,
    DependsOn,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Link {
    pub target: Id,
    pub relation: Relation,
    #[serde(default)]
    pub content_version: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Item {
    pub id: Id,
    pub kind: Kind,
    pub title: String,
    pub description: String,
    pub acceptance: String,
    pub status: String,
    pub priority: u8,
    pub severity: u8,
    pub assigned_to: Option<Id>,
    pub product_id: Option<Id>,
    pub project_id: Option<Id>,
    pub iteration_id: Option<Id>,
    pub parent_id: Option<Id>,
    pub links: BTreeSet<Link>,
    pub attributes: BTreeMap<String, Value>,
    pub version: u64,
    pub content_version: u64,
    pub created_by: Id,
    pub created_at: i64,
    pub updated_at: i64,
    pub deleted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewItem {
    pub id: Id,
    pub kind: Kind,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub acceptance: String,
    #[serde(default = "default_priority")]
    pub priority: u8,
    #[serde(default = "default_priority")]
    pub severity: u8,
    #[serde(default)]
    pub assigned_to: Option<Id>,
    #[serde(default)]
    pub product_id: Option<Id>,
    #[serde(default)]
    pub project_id: Option<Id>,
    #[serde(default)]
    pub iteration_id: Option<Id>,
    #[serde(default)]
    pub parent_id: Option<Id>,
    #[serde(default)]
    pub links: BTreeSet<Link>,
    #[serde(default)]
    pub attributes: BTreeMap<String, Value>,
}

fn default_priority() -> u8 {
    3
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Patch {
    pub title: Option<String>,
    pub description: Option<String>,
    pub acceptance: Option<String>,
    pub priority: Option<u8>,
    pub severity: Option<u8>,
    pub assigned_to: Option<Id>,
    #[serde(default)]
    pub clear_assignee: bool,
    pub links: Option<BTreeSet<Link>>,
    pub attributes: Option<BTreeMap<String, Value>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentVersion {
    pub item_id: Id,
    pub version: u64,
    pub title: String,
    pub description: String,
    pub acceptance: String,
    pub attributes: BTreeMap<String, Value>,
    pub actor_id: Id,
    pub at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Comment {
    pub id: Id,
    pub item_id: Id,
    pub actor_id: Id,
    pub body: String,
    pub parent_id: Option<Id>,
    pub mentions: BTreeSet<Id>,
    pub attachments: BTreeSet<Id>,
    pub created_at: i64,
    pub updated_at: i64,
    pub version: u64,
    pub deleted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attachment {
    pub id: Id,
    pub item_id: Id,
    pub name: String,
    pub mime: String,
    pub sha256: String,
    pub size: u64,
    pub actor_id: Id,
    pub at: i64,
    pub deleted: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Approve,
    Reject,
    Change,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    Batch {
        commands: Vec<Command>,
    },
    Create {
        item: NewItem,
    },
    Update {
        id: Id,
        version: u64,
        patch: Patch,
    },
    Review {
        id: Id,
        version: u64,
        decision: ReviewDecision,
        note: String,
    },
    Transition {
        id: Id,
        version: u64,
        target: String,
        note: String,
    },
    AcceptRequirement {
        id: Id,
        version: u64,
        accepted: bool,
        note: String,
    },
    ConvertInbox {
        id: Id,
        version: u64,
        new_id: Id,
        kind: Kind,
        acceptance: String,
    },
    RunTest {
        id: Id,
        plan_id: Id,
        case_id: Id,
        case_version: u64,
        result: String,
        actual: String,
    },
    BugFromTest {
        id: Id,
        run_id: Id,
        title: String,
        actual: String,
        environment: String,
    },
    ResolveBug {
        id: Id,
        version: u64,
        resolution: String,
        build_id: Option<Id>,
        duplicate_id: Option<Id>,
        note: String,
    },
    RetestBug {
        id: Id,
        version: u64,
        build_id: Option<Id>,
        run_id: Option<Id>,
        result: String,
        note: String,
    },
    Publish {
        id: Id,
        version: u64,
        note: String,
    },
    Archive {
        id: Id,
        version: u64,
        note: String,
    },
    Restore {
        id: Id,
        version: u64,
    },
    AddComment {
        id: Id,
        item_id: Id,
        body: String,
        parent_id: Option<Id>,
        mentions: BTreeSet<Id>,
        attachments: BTreeSet<Id>,
    },
    EditComment {
        id: Id,
        version: u64,
        body: String,
    },
    DeleteComment {
        id: Id,
        version: u64,
    },
    AddAttachment {
        id: Id,
        item_id: Id,
        name: String,
        mime: String,
        sha256: String,
        size: u64,
    },
    RemoveAttachment {
        id: Id,
    },
    AddMember {
        member: Member,
    },
    UpdateMember {
        member: Member,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub id: Id,
    pub actor_id: Id,
    pub at: i64,
    pub command: Command,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientOperation {
    pub id: Id,
    #[serde(default)]
    pub expected_revision: Option<u64>,
    pub command: Command,
}
impl ClientOperation {
    pub fn verify_effective(&self, request: &Request) -> Result<()> {
        fn normalize(client: &mut Command, effective: &Command) -> bool {
            match (client, effective) {
                (Command::Batch { commands: a }, Command::Batch { commands: b })
                    if a.len() == b.len() =>
                {
                    a.iter_mut()
                        .zip(b)
                        .map(|(a, b)| normalize(a, b))
                        .fold(false, |a, b| a | b)
                }
                (Command::AddMember { member: a }, Command::AddMember { member: b })
                | (Command::UpdateMember { member: a }, Command::UpdateMember { member: b }) => {
                    a.account = b.account.clone();
                    a.name = b.name.clone();
                    a.department = b.department.clone();
                    true
                }
                _ => false,
            }
        }
        let mut command = self.command.clone();
        let membership = normalize(&mut command, &request.command);
        let (Ok(client_bytes), Ok(effective_bytes)) = (
            serde_json::to_vec(&command),
            serde_json::to_vec(&request.command),
        ) else {
            return Err(Error::Storage("命令编码失败".into()));
        };
        if self.id != request.id
            || (membership && self.expected_revision.is_none())
            || client_bytes != effective_bytes
        {
            return Err(Error::Validation("客户端请求与实际操作不一致".into()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub operation_id: Id,
    pub revision: u64,
    pub affected: Vec<Id>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Audit {
    pub operation_id: Id,
    pub actor_id: Id,
    pub at: i64,
    pub action: String,
    pub object_id: Id,
    pub before: Value,
    pub after: Value,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Applied {
    pub fingerprint: String,
    pub receipt: Receipt,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub schema: u32,
    pub id: Id,
    pub name: String,
    pub revision: u64,
    pub members: BTreeMap<Id, Member>,
    pub items: BTreeMap<Id, Item>,
    pub versions: Vec<ContentVersion>,
    pub comments: BTreeMap<Id, Comment>,
    pub attachments: BTreeMap<Id, Attachment>,
    pub history: Vec<Audit>,
    pub operations: BTreeMap<Id, Applied>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Filter {
    pub kind: Option<Kind>,
    pub status: Option<String>,
    pub assigned_to: Option<Id>,
    pub product_id: Option<Id>,
    pub project_id: Option<Id>,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub archived: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metrics {
    pub requirements: usize,
    pub open_tasks: usize,
    pub pending_acceptance: usize,
    pub open_bugs: usize,
    pub pending_retest: usize,
    pub releases: usize,
}
