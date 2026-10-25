use super::*;
use crate::TransferPlan;
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitTransferView {
    pub status: TransferStatus,
    pub origin_device: Id,
    pub required_members: BTreeSet<Id>,
    pub confirmations: BTreeMap<Id, BTreeSet<Id>>,
    pub late_operations: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    id: Id,
    device: Id,
    change: Change,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Change {
    Freeze {
        plan: TransferPlan,
        previous: BTreeSet<Id>,
        required: BTreeSet<Id>,
    },
    Confirm {
        transfer_id: Id,
        actor: Id,
        at: i64,
        note: String,
    },
    Finish {
        status: TransferStatus,
        activation: Option<TransferActivation>,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Recovery {
    schema: u32,
    freeze: Record,
    control: TransferState,
}

pub(super) struct Handoffs {
    records: BTreeMap<Id, Record>,
    views: BTreeMap<Id, GitTransferView>,
}

impl Handoffs {
    pub(super) fn read(
        root: &Path,
        history: &History,
        files: &mut BTreeMap<String, String>,
    ) -> Result<Self> {
        let directory = root.join("transfers");
        let mut records = BTreeMap::new();
        if fs::symlink_metadata(&directory).is_ok() {
            regular_directory(&directory)?;
            for prefix in fs::read_dir(directory).map_err(storage)? {
                let prefix = prefix.map_err(storage)?;
                regular_directory(&prefix.path())?;
                for entry in fs::read_dir(prefix.path()).map_err(storage)? {
                    let entry = entry.map_err(storage)?;
                    let record: Record = canonical_json(&entry.path(), 2 * 1024 * 1024)?;
                    let relative = record_path(record.id);
                    if record.id.is_nil()
                        || record.device.is_nil()
                        || entry.path() != root.join(&relative)
                        || records.insert(record.id, record).is_some()
                    {
                        return invalid("Git 交接记录标识或路径无效");
                    }
                    files.insert(relative, hash_file(&entry.path())?);
                }
            }
        }
        let mut handoffs = Self {
            records,
            views: BTreeMap::new(),
        };
        handoffs.validate(history)?;
        Ok(handoffs)
    }

    fn validate(&mut self, history: &History) -> Result<()> {
        for record in self.records.values() {
            if let Change::Freeze { plan, required, .. } = &record.change {
                plan.validate(history)?;
                let state = history.prefix(&plan.heads)?.validate()?;
                if record.id != plan.id
                    || plan.source_mode != Mode::Git
                    || *required != writers(&state)
                {
                    return invalid("Git 冻结计划、模式或确认成员不符");
                }
                self.views.insert(
                    plan.id,
                    GitTransferView {
                        status: TransferStatus {
                            plan: plan.clone(),
                            role: TransferRole::Source,
                            phase: TransferPhase::Frozen,
                            changed_by: plan.prepared_by,
                            changed_at: plan.prepared_at,
                        },
                        origin_device: record.device,
                        required_members: required.clone(),
                        confirmations: BTreeMap::new(),
                        late_operations: migration::heads(&history.log) != plan.heads,
                    },
                );
            }
        }
        let mut finished = BTreeSet::new();
        for record in self.records.values() {
            match &record.change {
                Change::Freeze { .. } => {}
                Change::Confirm {
                    transfer_id,
                    actor,
                    at,
                    note,
                } => {
                    let view = self
                        .views
                        .get_mut(transfer_id)
                        .ok_or_else(|| Error::Validation("确认记录缺少对应冻结计划".into()))?;
                    if !view.required_members.contains(actor)
                        || *at < view.status.plan.prepared_at
                        || note.trim().is_empty()
                        || note.len() > 8000
                        || note.contains('\0')
                    {
                        return invalid("冻结确认成员、时间或说明无效");
                    }
                    view.confirmations
                        .entry(*actor)
                        .or_default()
                        .insert(record.device);
                }
                Change::Finish { status, activation } => {
                    status.validate(history, Mode::Git)?;
                    let view = self
                        .views
                        .get_mut(&status.plan.id)
                        .ok_or_else(|| Error::Validation("结束记录缺少对应冻结计划".into()))?;
                    if record.device != view.origin_device
                        || status.plan != view.status.plan
                        || status.role != TransferRole::Source
                        || !matches!(
                            status.phase,
                            TransferPhase::Cancelled | TransferPhase::Completed
                        )
                        || !finished.insert(status.plan.id)
                    {
                        return invalid("Git 交接结束记录冲突或发起副本不符");
                    }
                    match (status.phase, activation) {
                        (TransferPhase::Cancelled, None) => {}
                        (TransferPhase::Completed, Some(proof)) => {
                            verify_proof(&status.plan, proof)?;
                            let acknowledged: BTreeSet<_> = self
                                .records
                                .values()
                                .filter_map(|r| match &r.change {
                                    Change::Confirm {
                                        transfer_id,
                                        actor,
                                        at,
                                        ..
                                    } if *transfer_id == status.plan.id
                                        && *at <= status.changed_at =>
                                    {
                                        Some(*actor)
                                    }
                                    _ => None,
                                })
                                .collect();
                            if acknowledged != view.required_members {
                                return invalid("完成交接前缺少全部成员的冻结确认");
                            }
                        }
                        _ => return invalid("Git 交接结束阶段与凭据不符"),
                    }
                    view.status = status.clone();
                }
            }
        }
        for record in self.records.values() {
            if let Change::Freeze { plan, previous, .. } = &record.change {
                if previous.contains(&plan.id) {
                    return invalid("冻结计划不能引用自身");
                }
                for id in previous {
                    let old = self
                        .views
                        .get(id)
                        .ok_or_else(|| Error::Validation("冻结计划缺少前次交接记录".into()))?;
                    let Change::Freeze {
                        previous: older, ..
                    } = &self.freeze(*id)?.change
                    else {
                        unreachable!();
                    };
                    if old.status.phase != TransferPhase::Cancelled
                        || old.status.changed_at > plan.prepared_at
                        || !older.is_subset(previous)
                    {
                        return invalid("前次迁移尚未取消或交接顺序无效");
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) fn ensure_writable(&self) -> Result<()> {
        if self
            .views
            .values()
            .any(|v| v.status.phase != TransferPhase::Cancelled)
        {
            return Err(Error::Conflict("Git 空间正在迁移或已交接，业务只读".into()));
        }
        Ok(())
    }

    pub(super) fn has_records(&self) -> bool {
        !self.records.is_empty()
    }

    pub(super) fn authorize_read(&self, history: &History, actor: Id) -> Result<()> {
        let current = member(&history.log.view()?.state, actor).map(|_| ());
        if current.is_ok() {
            return current;
        }
        for view in self
            .views
            .values()
            .filter(|v| v.status.phase != TransferPhase::Cancelled)
        {
            if owner(&history.prefix(&view.status.plan.heads)?.validate()?, actor).is_ok() {
                return Ok(());
            }
        }
        current
    }

    pub(super) fn current_status(&self) -> Result<Option<TransferStatus>> {
        let active: Vec<_> = self
            .views
            .values()
            .filter(|v| v.status.phase != TransferPhase::Cancelled)
            .collect();
        if active.len() > 1 {
            return Err(Error::Conflict(
                "存在并行冻结计划，请查看并逐个取消未完成计划".into(),
            ));
        }
        let latest = active.first().copied().or_else(|| {
            self.views.values().max_by_key(|v| {
                let depth = match &self.records[&v.status.plan.id].change {
                    Change::Freeze { previous, .. } => previous.len(),
                    _ => 0,
                };
                (depth, v.status.plan.prepared_at, v.status.plan.id)
            })
        });
        Ok(latest.map(|v| v.status.clone()))
    }

    fn freeze(&self, id: Id) -> Result<&Record> {
        self.records
            .get(&id)
            .filter(|r| matches!(r.change, Change::Freeze { .. }))
            .ok_or_else(|| Error::NotFound("Git 冻结计划".into()))
    }

    fn single(&self, id: Id) -> Result<&GitTransferView> {
        let status = self
            .current_status()?
            .ok_or_else(|| Error::Validation("Git 源尚未冻结".into()))?;
        if status.plan.id != id || status.phase == TransferPhase::Cancelled {
            return invalid("迁移标识不符或计划已取消");
        }
        Ok(&self.views[&id])
    }
}

impl GitWorkspace {
    pub fn source_transfers(&self, actor: Id) -> Result<Vec<GitTransferView>> {
        self.locked(|| {
            let loaded = load(&self.root, true)?;
            loaded
                .handoffs
                .authorize_read(&loaded_history(&loaded), actor)?;
            Ok(loaded.handoffs.views.into_values().collect())
        })
    }

    pub fn prepare_transfer(
        &self,
        actor: Id,
        id: Id,
        at: i64,
        target: Mode,
        backup_destination: impl AsRef<Path>,
    ) -> Result<TransferStatus> {
        self.locked(|| {
            let loaded = load(&self.root, true)?;
            owner(&loaded.log.view()?.state, actor)?;
            if id.is_nil()
                || target == Mode::Git
                || at < 0
                || loaded
                    .transfer
                    .as_ref()
                    .is_some_and(|control| control.status.plan.id == id)
            {
                return invalid("迁移标识、目标模式或时间无效");
            }
            if let Some(view) = loaded.handoffs.views.get(&id) {
                if view.status.phase == TransferPhase::Frozen
                    && view.status.plan.target_mode == target
                    && view.origin_device == self.device
                {
                    return Ok(view.status.clone());
                }
                return invalid("不能重用已存在或其他副本的迁移标识");
            }
            if loaded.handoffs.records.contains_key(&id) {
                return invalid("迁移标识已属于历史交接记录");
            }
            loaded.ensure_writable()?;
            if loaded
                .handoffs
                .views
                .values()
                .any(|v| at < v.status.changed_at)
            {
                return invalid("迁移时间早于前次交接");
            }
            let history = loaded_history(&loaded);
            let control = TransferState::prepare(&history, Mode::Git, target, id, actor, at)?;
            let freeze = Record {
                id,
                device: self.device,
                change: Change::Freeze {
                    plan: control.status.plan.clone(),
                    previous: loaded.handoffs.views.keys().copied().collect(),
                    required: writers(&history.validate()?),
                },
            };
            backup_source(
                &self.root,
                &loaded,
                &freeze,
                &control,
                backup_destination.as_ref(),
            )?;
            write_private(&self.root, &control)?;
            persist_checked(&self.root, &loaded, &freeze)?;
            Ok(control.status)
        })
    }

    pub fn acknowledge_transfer(
        &self,
        actor: Id,
        at: i64,
        id: Id,
        note: &str,
    ) -> Result<GitTransferView> {
        self.locked(|| {
            let loaded = load(&self.root, true)?;
            writable(&loaded.log.view()?.state, actor)?;
            let view = loaded.handoffs.single(id)?;
            if view.status.phase != TransferPhase::Frozen
                || view.late_operations
                || !view.required_members.contains(&actor)
                || at < view.status.plan.prepared_at
                || note.trim().is_empty()
                || note.len() > 8000
                || note.contains('\0')
            {
                return invalid("请核对固定切点并说明所有本人副本已同步停写");
            }
            if view
                .confirmations
                .get(&actor)
                .is_some_and(|devices| devices.contains(&self.device))
            {
                return Ok(view.clone());
            }
            persist_checked(
                &self.root,
                &loaded,
                &Record {
                    id: Id::new_v4(),
                    device: self.device,
                    change: Change::Confirm {
                        transfer_id: id,
                        actor,
                        at,
                        note: note.into(),
                    },
                },
            )?;
            Ok(load(&self.root, true)?
                .handoffs
                .views
                .remove(&id)
                .expect("已核对的冻结计划存在"))
        })
    }

    pub fn export_transfer(
        &self,
        actor: Id,
        destination: impl AsRef<Path>,
    ) -> Result<MigrationReport> {
        self.locked(|| {
            let loaded = load(&self.root, true)?;
            let status = loaded
                .handoffs
                .current_status()?
                .ok_or_else(|| Error::Validation("Git 源尚未冻结".into()))?;
            let view = loaded.handoffs.single(status.plan.id)?;
            transfer_owner(&loaded, &status.plan, actor)?;
            let history = if status.phase == TransferPhase::Completed {
                loaded_history(&loaded).prefix(&status.plan.heads)?
            } else {
                confirmed(view)?;
                loaded_history(&loaded)
            };
            history.export_transfer(&view.status.plan, actor, &self.root, destination)
        })
    }

    pub fn complete_transfer(
        &self,
        actor: Id,
        at: i64,
        receipt: &TransferReceipt,
    ) -> Result<TransferActivation> {
        self.locked(|| {
            let loaded = load(&self.root, true)?;
            let status = loaded
                .handoffs
                .current_status()?
                .ok_or_else(|| Error::Validation("Git 源尚未冻结".into()))?;
            let view = loaded.handoffs.single(status.plan.id)?;
            transfer_owner(&loaded, &status.plan, actor)?;
            if *receipt != status.plan.receipt()? || at < status.changed_at {
                return invalid("目标核对回执或完成时间不符");
            }
            if status.phase == TransferPhase::Completed {
                return loaded
                    .handoffs
                    .records
                    .values()
                    .find_map(|r| match &r.change {
                        Change::Finish {
                            status: end,
                            activation: Some(proof),
                        } if end.plan.id == status.plan.id => Some(proof.clone()),
                        _ => None,
                    })
                    .ok_or_else(|| Error::Validation("Git 完成记录缺少凭据".into()));
            }
            confirmed(view)?;
            if view.origin_device != self.device {
                return Err(Error::Forbidden(
                    "仅发起副本可确认源完成，请返回发起设备或从私有备份恢复".into(),
                ));
            }
            let mut control: TransferState =
                read_json(&private_path(&self.root, status.plan.id), MANIFEST_LIMIT)?;
            control.validate(&loaded_history(&loaded), Mode::Git)?;
            if control.status != status {
                return invalid("私有源控制与公开冻结记录不一致");
            }
            let proof = control.complete(receipt, actor, at)?;
            persist_finish(
                &self.root,
                &loaded,
                self.device,
                control.status,
                Some(proof.clone()),
            )?;
            Ok(proof)
        })
    }

    pub fn cancel_transfer(&self, actor: Id, at: i64, id: Id) -> Result<TransferStatus> {
        self.locked(|| {
            let loaded = load(&self.root, true)?;
            let view = loaded
                .handoffs
                .views
                .get(&id)
                .ok_or_else(|| Error::NotFound("Git 冻结计划".into()))?;
            transfer_owner(&loaded, &view.status.plan, actor)?;
            if view.origin_device != self.device
                || view.status.phase == TransferPhase::Completed
                || at < view.status.changed_at
            {
                return invalid("仅发起副本可取消未完成迁移，时间不能倒退");
            }
            if view.status.phase == TransferPhase::Cancelled {
                return Ok(view.status.clone());
            }
            let status = TransferStatus {
                phase: TransferPhase::Cancelled,
                changed_by: actor,
                changed_at: at,
                ..view.status.clone()
            };
            status.validate(&loaded_history(&loaded), Mode::Git)?;
            persist_finish(&self.root, &loaded, self.device, status.clone(), None)?;
            Ok(status)
        })
    }

    pub fn restore_transfer(
        source: impl AsRef<Path>,
        destination: impl AsRef<Path>,
        actor: Id,
    ) -> Result<Self> {
        let source = fs::canonicalize(source).map_err(storage)?;
        regular_directory(&source)?;
        let recovery: Recovery = read_json(&source.join("recovery.json"), MANIFEST_LIMIT)?;
        let extracted = extract_pack(&source.join("workspace.farflowpack"))?;
        let loaded = load(&extracted.path().join("data"), false)?;
        validate_recovery(&recovery, &loaded, actor)?;
        let destination = destination.as_ref();
        let workspace = if fs::symlink_metadata(destination).is_ok() {
            regular_directory(destination)?;
            Self::open(destination)?
        } else {
            Self::from_extracted(extracted.path(), destination)?
        };
        workspace.locked(|| {
            let loaded = load(&workspace.root, true)?;
            validate_recovery(&recovery, &loaded, actor)?;
            write_private(&workspace.root, &recovery.control)?;
            let device_file = workspace.root.join(".git/farflow/device.json");
            regular_file(&device_file)?;
            let mut temporary =
                tempfile::NamedTempFile::new_in(workspace.root.join(".git/farflow"))
                    .map_err(storage)?;
            temporary
                .write_all(&serde_json::to_vec_pretty(&recovery.freeze.device).map_err(storage)?)
                .map_err(storage)?;
            temporary.as_file().sync_all().map_err(storage)?;
            temporary
                .persist(device_file)
                .map_err(|e| storage(e.error))?;
            sync_directory(&workspace.root.join(".git/farflow"))
        })?;
        Self::open(destination)
    }
}

pub(super) fn loaded_history(loaded: &Loaded) -> History {
    History {
        log: loaded.log.clone(),
        origins: loaded.origins.clone(),
        clients: loaded.clients.clone(),
    }
}

fn transfer_owner(loaded: &Loaded, plan: &TransferPlan, actor: Id) -> Result<()> {
    owner(
        &loaded_history(loaded).prefix(&plan.heads)?.validate()?,
        actor,
    )
}

fn writers(state: &State) -> BTreeSet<Id> {
    state
        .members
        .keys()
        .copied()
        .filter(|id| writable(state, *id).is_ok())
        .collect()
}

fn confirmed(view: &GitTransferView) -> Result<()> {
    if view.late_operations
        || view.confirmations.keys().copied().collect::<BTreeSet<_>>() != view.required_members
    {
        return Err(Error::Conflict(
            "源存在迟到业务或缺少成员冻结确认，请取消、合并并重新迁移".into(),
        ));
    }
    Ok(())
}

fn verify_proof(plan: &TransferPlan, proof: &TransferActivation) -> Result<()> {
    if proof.transfer_id != plan.id
        || proof.plan_checksum != plan.receipt()?.plan_checksum
        || proof.token.len() != 64
        || !proof
            .token
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        || digest(proof.token.as_bytes()) != plan.activation_hash
    {
        return invalid("Git 完成凭据与冻结计划不符");
    }
    Ok(())
}

pub(super) fn record_path(id: Id) -> String {
    let text = id.to_string();
    format!("transfers/{}/{}.json", &text[..2], text)
}

fn persist_record(root: &Path, record: &Record) -> Result<()> {
    let path = root.join(record_path(record.id));
    for dir in [
        root.join("transfers"),
        path.parent().expect("交接路径有父目录").to_owned(),
    ] {
        if fs::symlink_metadata(&dir).is_err() {
            fs::create_dir(&dir).map_err(storage)?;
            sync_directory(dir.parent().expect("交接目录有父目录"))?;
        }
        regular_directory(&dir)?;
    }
    atomic_json(&path, record)
}

fn persist_finish(
    root: &Path,
    loaded: &Loaded,
    device: Id,
    status: TransferStatus,
    activation: Option<TransferActivation>,
) -> Result<()> {
    persist_checked(
        root,
        loaded,
        &Record {
            id: Id::new_v4(),
            device,
            change: Change::Finish { status, activation },
        },
    )?;
    Ok(())
}

fn persist_checked(root: &Path, loaded: &Loaded, record: &Record) -> Result<()> {
    let mut candidate = Handoffs {
        records: loaded.handoffs.records.clone(),
        views: BTreeMap::new(),
    };
    if candidate
        .records
        .insert(record.id, record.clone())
        .is_some()
    {
        return invalid("交接记录标识已存在");
    }
    candidate.validate(&loaded_history(loaded))?;
    persist_record(root, record)?;
    load(root, true)?;
    Ok(())
}

fn private_path(root: &Path, id: Id) -> PathBuf {
    root.join(".git/farflow/source-transfers")
        .join(format!("{id}.json"))
}

fn write_private(root: &Path, control: &TransferState) -> Result<()> {
    let path = private_path(root, control.status.plan.id);
    let parent = path.parent().expect("私有交接路径有父目录");
    if fs::symlink_metadata(parent).is_err() {
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(parent).map_err(storage)?;
        sync_directory(parent.parent().expect("私有交接目录有父目录"))?;
    }
    regular_directory(parent)?;
    if fs::symlink_metadata(&path).is_ok() {
        let old: TransferState = read_json(&path, MANIFEST_LIMIT)?;
        if serde_json::to_string(&old).map_err(storage)?
            != serde_json::to_string(control).map_err(storage)?
        {
            return invalid("私有源控制已经属于不同交接");
        }
        return Ok(());
    }
    write_json(&path, control)?;
    sync_directory(parent)
}

fn validate_recovery(recovery: &Recovery, loaded: &Loaded, actor: Id) -> Result<()> {
    let history = loaded_history(loaded);
    owner(&history.validate()?, actor)?;
    recovery.control.validate(&history, Mode::Git)?;
    let id = recovery.control.status.plan.id;
    let view = loaded.handoffs.single(id)?;
    if recovery.schema != 1
        || recovery.control.status.role != TransferRole::Source
        || recovery.control.status.phase != TransferPhase::Frozen
        || view.status != recovery.control.status
        || serde_json::to_string(loaded.handoffs.freeze(id)?).map_err(storage)?
            != serde_json::to_string(&recovery.freeze).map_err(storage)?
    {
        return invalid("私有备份与 Git 冻结记录不一致");
    }
    Ok(())
}

fn backup_source(
    root: &Path,
    loaded: &Loaded,
    freeze: &Record,
    control: &TransferState,
    destination: &Path,
) -> Result<()> {
    migration::check_destination(root, destination)?;
    let backup = new_destination(destination)?;
    commit_files(root, &loaded.files, "迁移前固定 Git 源历史")?;
    let temporary = tempfile::tempdir_in(backup.path()).map_err(storage)?;
    for (relative, hash) in &loaded.files {
        let output = temporary.path().join(relative);
        fs::create_dir_all(output.parent().expect("规范文件有父目录")).map_err(storage)?;
        fs::copy(root.join(relative), &output).map_err(storage)?;
        if hash_file(&output)? != *hash {
            return invalid("Git 源在备份期间变化");
        }
    }
    for directory in ["operations", "attachments"] {
        fs::create_dir_all(temporary.path().join(directory)).map_err(storage)?;
    }
    git(
        temporary.path(),
        &args(&[
            "init",
            "--quiet",
            "--initial-branch=main",
            "--object-format=sha1",
        ]),
        None,
        None,
    )?;
    let metadata = temporary.path().join(".git/farflow");
    fs::create_dir(&metadata).map_err(storage)?;
    let parent = fetch_parent(temporary.path(), root)?;
    remember_parent(temporary.path(), &parent)?;
    persist_record(temporary.path(), freeze)?;
    let frozen = load(temporary.path(), true)?;
    export_pack_files(
        temporary.path(),
        &frozen.files,
        &backup.path().join("workspace.farflowpack"),
    )?;
    write_json(
        &backup.path().join("recovery.json"),
        &Recovery {
            schema: 1,
            freeze: freeze.clone(),
            control: control.clone(),
        },
    )?;
    drop(temporary);
    sync_directory(backup.path())?;
    promote(backup.path(), destination)
}

pub(super) fn copy_records(
    source: &Path,
    destination: &Path,
    files: &BTreeMap<String, String>,
) -> Result<()> {
    for (relative, hash) in files
        .iter()
        .filter(|(path, _)| path.starts_with("transfers/"))
    {
        let output = destination.join(relative);
        fs::create_dir_all(output.parent().expect("交接记录有父目录")).map_err(storage)?;
        atomic_bytes(
            &output,
            &read_file(&source.join(relative), 2 * 1024 * 1024)?,
        )?;
        if hash_file(&output)? != *hash {
            return invalid("交接记录在复制期间变化");
        }
        sync_directory(
            output
                .parent()
                .and_then(Path::parent)
                .expect("交接记录有分类目录"),
        )?;
    }
    sync_directory(destination)
}
