import { useState, type FormEvent } from 'react';
import type { Command, Item, Kind, Link, Snapshot } from '@farflow/client';
import { kindNames } from './catalog';
import { Dialog } from './Dialog';

export function ItemForm({ kind, item, snapshot, onSave, onClose, draft=false }: { kind: Kind; item?: Item; snapshot: Snapshot; onSave: (command: Command) => Promise<void>; onClose: () => void; draft?:boolean }) {
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  const [product, setProduct] = useState(item?.product_id || '');
  const [project, setProject] = useState(item?.project_id || '');
  const all = Object.values(snapshot.state.items).filter(i => !i.deleted);
  const objects = (target: Kind) => all.filter(i => i.kind === target && (!product || !i.product_id || i.product_id === product));
  const values = item?.attributes || {};
  const text = (key: string) => typeof values[key] === 'string' ? values[key] as string : '';
  const multi = (key: string) => Array.isArray(values[key]) ? values[key] as string[] : [];
  const links = item?.links || [];
  const [knownBugs, setKnownBugs] = useState(links.filter(l => l.relation === 'known_issue').map(l => l.target));
  const relatedKind = kind === 'task' || kind === 'test_case' || kind === 'release' ? 'requirement' : null;
  const relation = kind === 'task' ? 'implements' : kind === 'test_case' ? 'verifies' : 'contains';

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault(); setError(''); setBusy(true);
    const data = new FormData(event.currentTarget);
    const get = (name: string) => String(data.get(name) || '').trim();
    const ids = (name: string) => data.getAll(name).map(String);
    const attributes: Record<string, unknown> = {};
    if (kind === 'project') attributes.product_ids = ids('product_ids');
    if (kind === 'iteration') attributes.iteration_type = get('iteration_type');
    if (kind === 'bug') { for (const key of ['steps', 'expected', 'actual', 'environment']) attributes[key] = get(key); attributes.affected_build_ids = ids('affected_build_ids'); }
    if (kind === 'test_case') {
      const actions = data.getAll('step_action').map(String); const expected = data.getAll('step_expected').map(String);
      attributes.steps = actions.map((action, index) => ({ action: action.trim(), expected: expected[index].trim() }));
    }
    if (kind === 'build') for (const key of ['version_name', 'code_ref']) attributes[key] = get(key);
    if (kind === 'test_plan' && (!item || item.status === 'wait')) { attributes.build_id = get('build_id'); attributes.case_ids = ids('case_ids'); }
    if (kind === 'release') { attributes.version_name = get('version_name'); attributes.build_ids = ids('build_ids'); attributes.test_plan_ids = ids('test_plan_ids'); attributes.known_issue_notes = Object.fromEntries(knownBugs.map(id => [id,get(`known_note_${id}`)])); }
    const newLinks: Link[] = relatedKind ? links.filter(link => link.relation !== relation && !(kind === 'release' && link.relation === 'known_issue')) : [...links];
    for (const id of ids('related')) newLinks.push({ target: id, relation, content_version: snapshot.state.items[id].content_version });
    if (kind === 'release') { for (const id of ids('fixed_bugs')) newLinks.push({target:id,relation:'contains',content_version:snapshot.state.items[id].content_version}); for (const id of knownBugs) newLinks.push({target:id,relation:'known_issue',content_version:snapshot.state.items[id].content_version}); }
    const fields = { title: get('title'), description: get('description'), acceptance: kind === 'requirement' ? get('acceptance') : item?.acceptance || '', priority: Number(get('priority')), severity: Number(get('severity') || 3), assigned_to: get('assigned_to') || null, links: newLinks, attributes };
    try {
      await onSave(item ? { type: 'update', id: item.id, version: item.version, patch: { ...fields, clear_assignee: !fields.assigned_to } } : { type: 'create', item: { ...fields, id: crypto.randomUUID(), kind, product_id: product || null, project_id: project || null, iteration_id: get('iteration_id') || null, parent_id: null } });
      onClose();
    } catch (reason) { setError((reason as Error).message); } finally { setBusy(false); }
  }

  const selectObjects = (name: string, label: string, target: Kind, multiple = false, required = false) => <label>{label}<select name={name} multiple={multiple} required={required} defaultValue={multiple ? multi(name) : text(name)}><option value="" disabled={multiple}>请选择</option>{objects(target).map(i => <option key={i.id} value={i.id}>{i.title}</option>)}</select></label>;
  const initialSteps = Array.isArray(values.steps) ? values.steps as Array<{ action: string; expected: string }> : [{ action: '', expected: '' }];
  const [steps, setSteps] = useState(initialSteps);
  return <Dialog title={`${draft?'合并':item ? '编辑' : '新建'}${kindNames[kind]}`} description={draft?'这里只准备合并内容，返回冲突比较后确认才保存决策。':'保存后记录操作者和内容版本。业务状态通过独立动作流转。'} onClose={onClose}><form onSubmit={submit} className="form-stack">
    <label>标题<input name="title" required maxLength={240} autoFocus defaultValue={item?.title}/></label>
    {!item && !['product','project'].includes(kind) && <div className="form-grid"><label>所属产品<select aria-label="所属产品" value={product} required={['module','requirement','plan','test_case','build','bug','release','test_plan'].includes(kind)} onChange={e => setProduct(e.target.value)}><option value="">不指定</option>{objects('product').map(i => <option key={i.id} value={i.id}>{i.title}</option>)}</select></label><label>所属项目<select value={project} required={kind === 'task' || kind === 'iteration'} onChange={e => setProject(e.target.value)}><option value="">不指定</option>{objects('project').map(i => <option key={i.id} value={i.id}>{i.title}</option>)}</select></label>{project && <label>所属迭代<select name="iteration_id" defaultValue=""><option value="">不指定</option>{objects('iteration').filter(i => i.project_id === project).map(i => <option key={i.id} value={i.id}>{i.title}</option>)}</select></label>}</div>}
    <div className="form-grid"><label>负责人<select name="assigned_to" defaultValue={item?.assigned_to || ''}><option value="">未指派</option>{Object.values(snapshot.state.members).filter(m => m.active).map(m => <option key={m.id} value={m.id}>{m.name}</option>)}</select></label><label>优先级<select name="priority" defaultValue={item?.priority || 3}>{[1,2,3,4].map(p => <option key={p} value={p}>P{p} · {['','紧急','高','中','低'][p]}</option>)}</select></label>{kind === 'bug' && <label>严重度<select name="severity" defaultValue={item?.severity || 3}>{[1,2,3,4].map(p => <option key={p} value={p}>S{p}</option>)}</select></label>}</div>
    <label>描述<textarea name="description" rows={4} maxLength={64000} defaultValue={item?.description}/></label>
    {kind === 'requirement' && <label>验收标准<textarea name="acceptance" required rows={3} maxLength={16000} defaultValue={item?.acceptance}/></label>}
    {kind === 'project' && selectObjects('product_ids', '关联产品（可多选）', 'product', true, true)}
    {kind === 'iteration' && <label>迭代类型<select name="iteration_type" defaultValue={text('iteration_type') || 'sprint'}><option value="sprint">冲刺</option><option value="stage">阶段</option><option value="kanban">看板</option></select></label>}
    {kind === 'bug' && <>{[['steps','复现步骤'],['expected','预期结果'],['actual','实际结果'],['environment','环境信息']].map(([key,label]) => <label key={key}>{label}<textarea name={key} required={key !== 'environment'} defaultValue={text(key)} rows={2}/></label>)}{selectObjects('affected_build_ids','影响构建（可多选）','build',true)}</>}
    {kind === 'test_case' && <fieldset><legend>测试步骤</legend>{steps.map((step, index) => <div className="form-grid" key={index}><label>步骤 {index + 1}<textarea name="step_action" defaultValue={step.action} required/></label><label>预期 {index + 1}<textarea name="step_expected" defaultValue={step.expected} required/></label></div>)}<button type="button" disabled={steps.length >= 100} onClick={() => setSteps([...steps, { action: '', expected: '' }])}>添加步骤</button></fieldset>}
    {['build','release'].includes(kind) && <label>版本名称<input name="version_name" required defaultValue={text('version_name')}/></label>}
    {kind === 'build' && <label>代码版本 / Commit<input name="code_ref" required defaultValue={text('code_ref')}/></label>}
    {kind === 'test_plan' && ((!item || item.status === 'wait') ? <>{selectObjects('build_id', '测试构建', 'build', false, true)}{selectObjects('case_ids', '测试用例（可多选）', 'test_case', true, true)}</> : <p className="callout">测试已开始，构建与用例范围已固定。</p>)}
    {kind === 'release' && <>{selectObjects('build_ids', '构建（可多选）', 'build', true, true)}{selectObjects('test_plan_ids', '测试单（可多选）', 'test_plan', true, true)}</>}
    {kind === 'release' && <><label>修复 Bug（可多选）<select name="fixed_bugs" multiple defaultValue={links.filter(l => l.relation === 'contains' && snapshot.state.items[l.target].kind === 'bug').map(l => l.target)}>{objects('bug').filter(i => i.status === 'closed' && i.attributes.resolution === 'fixed').map(i => <option key={i.id} value={i.id}>{i.title}</option>)}</select></label><label>遗留 Bug（可多选）<select multiple value={knownBugs} onChange={e => setKnownBugs([...e.target.selectedOptions].map(o => o.value))}>{objects('bug').map(i => <option key={i.id} value={i.id}>{i.title}</option>)}</select></label>{knownBugs.map(id => <label key={id}>遗留说明：{snapshot.state.items[id].title}<textarea name={`known_note_${id}`} required defaultValue={(values.known_issue_notes as Record<string,string> | undefined)?.[id] || ''}/></label>)}</>}
    {relatedKind && <label>关联需求（可多选）<select name="related" multiple defaultValue={links.filter(l => l.relation === relation).map(l => l.target)}>{objects(relatedKind).map(i => <option key={i.id} value={i.id}>{i.title} · V{i.content_version}</option>)}</select></label>}
    {error && <p role="alert" className="error">{error}</p>}<footer className="form-actions"><button type="button" onClick={onClose}>取消</button><button className="primary" disabled={busy}>{busy ? '保存中…' : draft?'采用合并内容':'保存'}</button></footer>
  </form></Dialog>;
}
