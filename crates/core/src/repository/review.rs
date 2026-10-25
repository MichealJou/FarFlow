use super::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitReview {
    pub token: String,
    pub state: State,
    pub issues: BTreeMap<Id, crate::GitIssue>,
    pub proposals: BTreeMap<Id, GitOperation>,
    pub decisions: Vec<GitDecisionReview>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitDecisionReview {
    pub operation: GitOperation,
    pub applied: bool,
    pub issue: Option<crate::GitIssue>,
    pub targets: BTreeMap<Id, GitOperation>,
}

impl GitWorkspace {
    pub fn review(&self, actor: Id) -> Result<GitReview> {
        self.locked(|| {
            let loaded = load(&self.root, true)?;
            let view = loaded.log.view()?;
            member(&view.state, actor)?;
            let proposals = view
                .issues
                .keys()
                .map(|id| (*id, loaded.log.operations()[id].clone()))
                .collect();
            let decisions = loaded
                .log
                .operations()
                .values()
                .filter_map(|operation| {
                    let GitChange::Resolve { resolution } = &operation.change else {
                        return None;
                    };
                    Some(GitDecisionReview {
                        operation: operation.clone(),
                        applied: view.receipts.contains_key(&operation.id),
                        issue: view.issues.get(&operation.id).cloned(),
                        targets: resolution
                            .targets
                            .iter()
                            .map(|id| (*id, loaded.log.operations()[id].clone()))
                            .collect(),
                    })
                })
                .collect();
            Ok(GitReview {
                token: digest(&serde_json::to_vec(&loaded.files).map_err(storage)?),
                state: view.state,
                issues: view.issues,
                proposals,
                decisions,
            })
        })
    }

    pub fn resolve_reviewed(&self, resolution: GitResolution, token: &str) -> Result<GitSave> {
        self.save_resolution(resolution, Some(token))
    }

    pub(super) fn save_resolution(
        &self,
        resolution: GitResolution,
        token: Option<&str>,
    ) -> Result<GitSave> {
        self.locked(|| {
            let mut loaded = load(&self.root, true)?;
            loaded.ensure_writable()?;
            // 不采纳提案可能不改变业务修订，必须核对完整记录集合，不能只比较修订号。
            if !loaded.log.operations().contains_key(&resolution.id)
                && token.is_some_and(|token| {
                    serde_json::to_vec(&loaded.files)
                        .map(|bytes| digest(&bytes) != token)
                        .unwrap_or(true)
                })
            {
                return Err(Error::Conflict(
                    "记录已更新，请重新比较冲突后提交决策".into(),
                ));
            }
            let receipt = loaded.log.resolve(self.device, resolution.clone())?;
            validate_blobs(&self.root, &loaded.log)?;
            persist_operation(&self.root, &loaded.log.operations()[&resolution.id])?;
            finish_save(&self.root, receipt)
        })
    }
}
