import { useState, type FormEvent } from 'react';
import type { Command, Item, Kind, Snapshot } from '@farflow/client';
import { Dialog } from './Dialog';
import { displayStatus, transitions } from './catalog';

export function actionChoices(item: Item): Array<{ value: string; label: string }> {
  if (item.deleted) return [{ value: 'restore', label: '恢复归档' }];
  const choices = transitions(item).map(t => ({ value: `transition:${t.value}`, label: t.label }));
  if (['requirement','test_case'].includes(item.kind) && item.status === 'reviewing') choices.push({ value: 'review', label: '提交评审结论' });
  if (item.kind === 'requirement' && item.status === 'active') choices.push({ value: 'accept_requirement', label: '产品验收' });
  if (item.kind === 'inbox' && item.status === 'accepted') choices.push({ value: 'convert_inbox', label: '转为需求 / Bug' });
  if (item.kind === 'test_plan' && item.status === 'doing') choices.push({ value: 'run_test', label: '执行测试' });
  if (item.kind === 'test_run' && ['fail','blocked'].includes(item.status)) choices.push({ value: 'bug_from_test', label: '由结果提交 Bug' });
  if (item.kind === 'bug' && ['active','doing'].includes(item.status)) choices.push({ value: 'resolve_bug', label: '提交解决方案' });
  if (item.kind === 'bug' && item.status === 'resolved') choices.push({ value: 'retest_bug', label: '提交独立复测' });
  if (item.kind === 'release' && item.status === 'wait') choices.push({ value: 'publish', label: '确认发布' });
  if (item.kind !== 'test_run') choices.push({ value: 'archive', label: '归档' });
  return choices;
}

export function ActionForm({ action, item, snapshot, onSave, onClose }: { action: string; item: Item; snapshot: Snapshot; onSave: (command: Command) => Promise<void>; onClose: () => void }) {
  const [error, setError] = useState(''); const [busy, setBusy] = useState(false);
  const [resolution, setResolution] = useState('fixed');
  const [caseId, setCaseId] = useState(action === 'run_test' ? (item.attributes.case_ids as string[])[0] : '');
  const pinnedVersion = action === 'run_test' ? (item.attributes.case_versions as Record<string,number>)[caseId] : 0;
  const pinnedCase = action === 'run_test' ? snapshot.state.versions.find(v => v.item_id === caseId && v.version === pinnedVersion) || (snapshot.state.items[caseId]?.content_version === pinnedVersion ? snapshot.state.items[caseId] : undefined) : undefined;
  const all = Object.values(snapshot.state.items).filter(i => !i.deleted && i.product_id === item.product_id);
  const select = (name: string, label: string, kind: Kind) => <label>{label}<select name={name} required><option value="">请选择</option>{all.filter(i => i.kind === kind).map(i => <option key={i.id} value={i.id}>{i.title}</option>)}</select></label>;
  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault(); setError(''); setBusy(true);
    const data = new FormData(event.currentTarget); const get = (name: string) => String(data.get(name) || '').trim();
    const base = { id: item.id, version: item.version, note: get('note') };
    let command: Command;
    if (action.startsWith('transition:')) command = { type: 'transition', ...base, target: action.slice(11) };
    else if (action === 'review') command = { type: action, ...base, decision: get('decision') };
    else if (action === 'accept_requirement') command = { type: action, ...base, accepted: get('accepted') === 'true' };
    else if (action === 'convert_inbox') command = { type: action, id: item.id, version: item.version, new_id: crypto.randomUUID(), kind: get('kind'), acceptance: get('acceptance') };
    else if (action === 'run_test') {
      const id = get('case_id'); const versions = item.attributes.case_versions as Record<string, number>;
      command = { type: action, id: crypto.randomUUID(), plan_id: item.id, case_id: id, case_version: versions[id], result: get('result'), actual: get('note') };
    } else if (action === 'bug_from_test') command = { type: action, id: crypto.randomUUID(), run_id: item.id, title: get('title'), actual: get('note'), environment: get('environment') };
    else if (action === 'resolve_bug') command = { type: action, ...base, resolution, build_id: resolution === 'fixed' ? get('build_id') : null, duplicate_id: resolution === 'duplicate' ? get('duplicate_id') : null };
    else if (action === 'retest_bug') command = { type: action, ...base, result: get('result'), build_id: item.attributes.resolution === 'fixed' ? get('build_id') : null, run_id: get('run_id') || null };
    else command = { type: action, ...base };
    try { await onSave(command); onClose(); } catch (reason) { setError((reason as Error).message); } finally { setBusy(false); }
  }
  return <Dialog title={actionChoices(item).find(c => c.value === action)!.label} description="本次操作写入动态；版本冲突、角色权限和业务门禁由共享核心核验。" onClose={onClose}><form className="form-stack" onSubmit={submit}>
    <div className="callout">{item.title} · 内容 V{item.content_version}</div>
    {action === 'review' && <label>评审结论<select name="decision"><option value="approve">批准</option><option value="reject">退回草稿</option><option value="change">要求变更</option></select></label>}
    {action === 'accept_requirement' && <label>验收结论<select name="accepted"><option value="true">通过</option><option value="false">不通过</option></select></label>}
    {action === 'convert_inbox' && <><label>目标类型<select name="kind"><option value="requirement">需求</option><option value="bug">Bug</option></select></label><label>验收标准<textarea name="acceptance" required/></label></>}
    {action === 'run_test' && <><label>测试用例（固定版本）<select name="case_id" required value={caseId} onChange={e => setCaseId(e.target.value)}>{(item.attributes.case_ids as string[]).map(id => <option key={id} value={id}>{snapshot.state.items[id].title} · V{(item.attributes.case_versions as Record<string, number>)[id]}</option>)}</select></label>{pinnedCase ? <section className="callout"><h3>{pinnedCase.title} · V{pinnedVersion}</h3>{pinnedCase.description && <p className="prose">{pinnedCase.description}</p>}<ol className="steps">{(pinnedCase.attributes.steps as Array<{action:string;expected:string}>).map((step,index) => <li key={index}><p>{step.action}</p><small>预期：{step.expected}</small></li>)}</ol></section> : <p role="alert" className="error">固定的用例内容尚未读取，请刷新空间后重试。</p>}</>}
    {action === 'bug_from_test' && <><label>Bug 标题<input name="title" required defaultValue={item.title}/></label><label>环境信息<input name="environment" required/></label></>}
    {action === 'resolve_bug' && <><label>解决方案<select value={resolution} onChange={e => setResolution(e.target.value)}><option value="fixed">已修复</option><option value="duplicate">重复 Bug</option><option value="by_design">设计如此</option><option value="not_reproducible">无法复现</option><option value="external">外部原因</option><option value="postponed">延期处理</option><option value="will_not_fix">不予修复</option><option value="to_requirement">转为需求</option></select></label>{resolution === 'fixed' && select('build_id','修复构建','build')}{resolution === 'duplicate' && select('duplicate_id','原 Bug','bug')}</>}
    {action === 'retest_bug' && item.attributes.resolution === 'fixed' && <>{select('build_id','修复构建','build')}<label>独立执行记录<select name="run_id"><option value="">无来源用例时可不选</option>{all.filter(i => i.kind === 'test_run' && i.attributes.build_id === item.attributes.resolved_build).map(i => <option key={i.id} value={i.id}>{i.title} · {displayStatus(i)} · {i.id.slice(0,8)}</option>)}</select></label><p className="hint">仅列出修复构建的执行记录。有来源用例时，先单独执行原用例，再选择新执行记录。</p></>}
    {['run_test','retest_bug'].includes(action) && <label>结果<select name="result"><option value="pass">通过</option><option value="fail">失败</option><option value="blocked">阻塞</option>{action === 'run_test' && <option value="skipped">跳过</option>}</select></label>}
    {action !== 'restore' && <label>{action === 'run_test' ? '实际执行记录' : '交接说明'}<textarea name="note" required maxLength={8000} rows={4} autoFocus/></label>}
    {action === 'publish' && <p className="hint">发布前核对已就绪构建、已结束测试单、最新测试结果和需求验收。成功后固化发布快照。</p>}
    {error && <p role="alert" className="error">{error}</p>}<footer className="form-actions"><button type="button" onClick={onClose}>取消</button><button className="primary" disabled={busy || (action === 'run_test' && !pinnedCase)}>{busy ? '提交中…' : '确认提交'}</button></footer>
  </form></Dialog>;
}
