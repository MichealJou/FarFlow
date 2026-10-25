use crate::{
    migration::{checksum, heads, History},
    store::{digest, owner, valid_hash},
    Error, Id, Mode, Result,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferPlan {
    pub id: Id,
    pub workspace_id: Id,
    pub source_mode: Mode,
    pub target_mode: Mode,
    pub prepared_by: Id,
    pub prepared_at: i64,
    pub heads: BTreeSet<Id>,
    pub history_checksum: String,
    pub activation_hash: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferRole {
    Source,
    Target,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferPhase {
    Frozen,
    Completed,
    Cancelled,
    Staged,
    Active,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferStatus {
    pub plan: TransferPlan,
    pub role: TransferRole,
    pub phase: TransferPhase,
    pub changed_by: Id,
    pub changed_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferReceipt {
    pub plan_checksum: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferActivation {
    pub transfer_id: Id,
    pub plan_checksum: String,
    pub token: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferState {
    pub status: TransferStatus,
    token: Option<String>,
}

impl TransferPlan {
    pub fn validate(&self, history: &History) -> Result<()> {
        if self.id.is_nil()
            || self.workspace_id != history.log.info().id
            || self.source_mode == self.target_mode
            || self.prepared_at < 0
            || !valid_hash(&self.history_checksum)
            || !valid_hash(&self.activation_hash)
        {
            return invalid("迁移计划标识、模式或校验值无效");
        }
        let prefix = history.prefix(&self.heads)?;
        if heads(&prefix.log) != self.heads || checksum(&prefix)? != self.history_checksum {
            return invalid("迁移切点与原始历史不一致");
        }
        owner(&prefix.validate()?, self.prepared_by)
    }

    pub fn receipt(&self) -> Result<TransferReceipt> {
        Ok(TransferReceipt {
            plan_checksum: checksum(self)?,
        })
    }
}

impl TransferState {
    pub fn prepare(
        history: &History,
        source_mode: Mode,
        target_mode: Mode,
        id: Id,
        actor: Id,
        at: i64,
    ) -> Result<Self> {
        owner(&history.validate()?, actor)?;
        let token = format!("{}{}", Id::new_v4().simple(), Id::new_v4().simple());
        let state = Self {
            status: TransferStatus {
                plan: TransferPlan {
                    id,
                    workspace_id: history.log.info().id,
                    source_mode,
                    target_mode,
                    prepared_by: actor,
                    prepared_at: at,
                    heads: heads(&history.log),
                    history_checksum: checksum(history)?,
                    activation_hash: digest(token.as_bytes()),
                },
                role: TransferRole::Source,
                phase: TransferPhase::Frozen,
                changed_by: actor,
                changed_at: at,
            },
            token: Some(token),
        };
        state.validate(history, source_mode)?;
        Ok(state)
    }

    pub fn stage(
        plan: TransferPlan,
        history: &History,
        mode: Mode,
        actor: Id,
        at: i64,
    ) -> Result<Self> {
        let state = Self {
            status: TransferStatus {
                changed_by: actor,
                changed_at: at,
                plan,
                role: TransferRole::Target,
                phase: TransferPhase::Staged,
            },
            token: None,
        };
        state.validate(history, mode)?;
        Ok(state)
    }

    pub fn validate_header(&self, workspace: Id, mode: Mode) -> Result<()> {
        let plan = &self.status.plan;
        if plan.id.is_nil()
            || plan.workspace_id != workspace
            || plan.source_mode == plan.target_mode
            || plan.prepared_by.is_nil()
            || plan.prepared_at < 0
            || !valid_hash(&plan.history_checksum)
            || !valid_hash(&plan.activation_hash)
            || self.status.changed_by.is_nil()
        {
            return invalid("迁移控制头无效");
        }
        let token_valid = self.token.as_ref().is_some_and(|token| {
            token.len() == 64
                && token
                    .bytes()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
                && digest(token.as_bytes()) == self.status.plan.activation_hash
        });
        let valid = match (self.status.role, self.status.phase) {
            (TransferRole::Source, TransferPhase::Frozen | TransferPhase::Completed) => {
                token_valid && mode == self.status.plan.source_mode
            }
            (TransferRole::Source, TransferPhase::Cancelled) => {
                self.token.is_none() && mode == self.status.plan.source_mode
            }
            (TransferRole::Target, TransferPhase::Staged) => {
                self.token.is_none() && mode == self.status.plan.target_mode
            }
            (TransferRole::Target, TransferPhase::Active) => {
                token_valid && mode == self.status.plan.target_mode
            }
            _ => false,
        };
        if !valid || self.status.changed_at < self.status.plan.prepared_at {
            return invalid("迁移阶段、完成凭据或变更时间无效");
        }
        Ok(())
    }

    pub fn validate(&self, history: &History, mode: Mode) -> Result<()> {
        self.validate_header(history.log.info().id, mode)?;
        self.status.validate(history, mode)?;
        if !self.writable() && heads(&history.log) != self.status.plan.heads {
            return invalid("只读迁移空间出现了切点之外的业务写入");
        }
        Ok(())
    }

    pub fn writable(&self) -> bool {
        matches!(
            (self.status.role, self.status.phase),
            (TransferRole::Source, TransferPhase::Cancelled)
                | (TransferRole::Target, TransferPhase::Active)
        )
    }

    pub fn ensure_writable(&self) -> Result<()> {
        if !self.writable() {
            return Err(Error::Conflict("工作空间正在迁移或已交接，当前只读".into()));
        }
        Ok(())
    }

    pub fn complete(
        &mut self,
        receipt: &TransferReceipt,
        actor: Id,
        at: i64,
    ) -> Result<TransferActivation> {
        if self.status.role != TransferRole::Source
            || !matches!(
                self.status.phase,
                TransferPhase::Frozen | TransferPhase::Completed
            )
            || *receipt != self.status.plan.receipt()?
            || at < self.status.changed_at
        {
            return invalid("目标核对回执不符或源迁移已取消");
        }
        let Some(token) = self.token.clone() else {
            return invalid("源端缺少完成凭据");
        };
        if self.status.phase != TransferPhase::Completed {
            self.status.phase = TransferPhase::Completed;
            self.status.changed_by = actor;
            self.status.changed_at = at;
        }
        Ok(TransferActivation {
            transfer_id: self.status.plan.id,
            plan_checksum: receipt.plan_checksum.clone(),
            token,
        })
    }

    pub fn cancel(&mut self, actor: Id, at: i64) -> Result<()> {
        if self.status.role != TransferRole::Source
            || !matches!(
                self.status.phase,
                TransferPhase::Frozen | TransferPhase::Cancelled
            )
            || at < self.status.changed_at
        {
            return invalid("源迁移已完成，不能撤销交接");
        }
        if self.status.phase != TransferPhase::Cancelled {
            self.status.phase = TransferPhase::Cancelled;
            self.status.changed_by = actor;
            self.status.changed_at = at;
            self.token = None;
        }
        Ok(())
    }

    pub fn activate(&mut self, proof: &TransferActivation, actor: Id, at: i64) -> Result<()> {
        if self.status.role != TransferRole::Target
            || !matches!(
                self.status.phase,
                TransferPhase::Staged | TransferPhase::Active
            )
            || proof.transfer_id != self.status.plan.id
            || proof.plan_checksum != self.status.plan.receipt()?.plan_checksum
            || digest(proof.token.as_bytes()) != self.status.plan.activation_hash
            || at < self.status.changed_at
        {
            return invalid("完成凭据不匹配，目标继续只读");
        }
        if self.status.phase != TransferPhase::Active {
            self.status.phase = TransferPhase::Active;
            self.status.changed_by = actor;
            self.status.changed_at = at;
            self.token = Some(proof.token.clone());
        }
        Ok(())
    }
}

impl TransferStatus {
    pub fn validate(&self, history: &History, mode: Mode) -> Result<()> {
        self.plan.validate(history)?;
        let phase_valid = matches!(
            (self.role, self.phase),
            (
                TransferRole::Source,
                TransferPhase::Frozen | TransferPhase::Completed | TransferPhase::Cancelled
            ) | (
                TransferRole::Target,
                TransferPhase::Staged | TransferPhase::Active
            )
        );
        let expected_mode = if self.role == TransferRole::Source {
            self.plan.source_mode
        } else {
            self.plan.target_mode
        };
        if !phase_valid || expected_mode != mode || self.changed_at < self.plan.prepared_at {
            return invalid("迁移状态与模式或时间不符");
        }
        owner(
            &history.prefix(&self.plan.heads)?.validate()?,
            self.changed_by,
        )
    }
}

fn invalid<T>(message: &str) -> Result<T> {
    Err(Error::Validation(message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Command, GitLog, Member, Request, Role};
    use std::collections::BTreeMap;

    fn fixture() -> (History, Member) {
        let owner = Member {
            id: Id::new_v4(),
            account: "owner".into(),
            name: "所有者".into(),
            department: "研发".into(),
            roles: BTreeSet::from([Role::Owner]),
            active: true,
        };
        (
            History {
                log: GitLog::new(Id::new_v4(), "空空间交接".into(), owner.clone()).unwrap(),
                origins: vec![],
                clients: BTreeMap::new(),
            },
            owner,
        )
    }

    #[test]
    fn plan_and_phase_validation_rejects_forged_roles_frontiers_modes_and_tokens() {
        let (history, owner) = fixture();
        let source =
            TransferState::prepare(&history, Mode::Local, Mode::Git, Id::new_v4(), owner.id, 10)
                .unwrap();
        for change in 0..6 {
            let mut plan = source.status.plan.clone();
            match change {
                0 => plan.id = Id::nil(),
                1 => plan.target_mode = Mode::Local,
                2 => plan.heads.insert(Id::new_v4()).then_some(()).unwrap(),
                3 => plan.history_checksum = "0".repeat(64),
                4 => plan.prepared_by = Id::new_v4(),
                _ => plan.activation_hash = "invalid".into(),
            }
            assert!(plan.validate(&history).is_err());
        }
        for phase in [
            TransferPhase::Frozen,
            TransferPhase::Completed,
            TransferPhase::Cancelled,
            TransferPhase::Staged,
            TransferPhase::Active,
        ] {
            let mut state = source.clone();
            state.status.role = TransferRole::Target;
            state.status.phase = phase;
            assert!(state.validate(&history, Mode::Local).is_err());
        }
        let mut invalid = source.clone();
        invalid.token = None;
        assert!(invalid.validate(&history, Mode::Local).is_err());
        assert!(invalid
            .complete(&source.status.plan.receipt().unwrap(), owner.id, 11)
            .is_err());
        invalid = source.clone();
        invalid.status.changed_at = 9;
        assert!(invalid.validate(&history, Mode::Local).is_err());
        assert!(TransferState::stage(
            source.status.plan.clone(),
            &history,
            Mode::Service,
            owner.id,
            10
        )
        .is_err());
        let mut unknown = serde_json::to_value(&source.status).unwrap();
        unknown["token"] = serde_json::json!("不能混入公开状态");
        assert!(serde_json::from_value::<TransferStatus>(unknown).is_err());
    }

    #[test]
    fn activation_is_bound_to_one_plan_and_historical_ownership_survives_later_changes() {
        let (mut history, owner) = fixture();
        let mut source =
            TransferState::prepare(&history, Mode::Local, Mode::Git, Id::new_v4(), owner.id, 10)
                .unwrap();
        let mut target = TransferState::stage(
            source.status.plan.clone(),
            &history,
            Mode::Git,
            owner.id,
            10,
        )
        .unwrap();
        let receipt = source.status.plan.receipt().unwrap();
        assert!(source.complete(&receipt, owner.id, 9).is_err());
        assert!(source.cancel(owner.id, 9).is_err());
        assert!(target.complete(&receipt, owner.id, 11).is_err());
        assert!(target.cancel(owner.id, 11).is_err());
        let proof = source.complete(&receipt, owner.id, 11).unwrap();
        assert!(target.activate(&proof, owner.id, 9).is_err());
        let mut wrong = proof.clone();
        wrong.transfer_id = Id::new_v4();
        assert!(target.activate(&wrong, owner.id, 12).is_err());
        assert!(source.activate(&proof, owner.id, 12).is_err());
        target.activate(&proof, owner.id, 12).unwrap();
        let mut next = owner.clone();
        next.id = Id::new_v4();
        next.account = "next".into();
        let mut inactive = owner.clone();
        inactive.active = false;
        for (actor, command) in [
            (
                owner.id,
                Command::AddMember {
                    member: next.clone(),
                },
            ),
            (next.id, Command::UpdateMember { member: inactive }),
        ] {
            history
                .log
                .execute(
                    Id::new_v4(),
                    Request {
                        id: Id::new_v4(),
                        actor_id: actor,
                        at: 13,
                        command,
                    },
                )
                .unwrap();
        }
        target.validate(&history, Mode::Git).unwrap();
        assert!(source.validate(&history, Mode::Local).is_err());
        assert!(history
            .export_transfer(
                &source.status.plan,
                owner.id,
                std::path::Path::new("unused"),
                std::path::Path::new("unused-target")
            )
            .is_err());
        assert!(target.ensure_writable().is_ok());
    }
}
