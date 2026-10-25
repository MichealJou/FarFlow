import { useState, type FormEvent } from 'react';
import { IconX, IconPaperclip, IconUserCircle, IconDownload, IconFile, IconMessage } from '@tabler/icons-react';
import type { Command, Item, Snapshot, WorkspaceClient } from '@farflow/client';
import { actionNames, dateText, displayStatus, kindNames, resolutionNames } from './catalog';
import { ActionForm, actionChoices } from './ActionForm';
import { ItemForm } from './ItemForm';
import { Dialog } from './Dialog';

export function ItemDetail({ item, snapshot, client, onSave, onRefresh, onClose, onSelect, readOnly }: { item: Item; snapshot: Snapshot; client: WorkspaceClient; onSave: (command: Command) => Promise<void>; onRefresh: () => Promise<void>; onClose: () => void; onSelect: (id: string) => void; readOnly: boolean }) {
  const [tab, setTab] = useState('内容'); const [action, setAction] = useState(''); const [edit, setEdit] = useState(false);
  const [error, setError] = useState(''); const [busy, setBusy] = useState(false); const [reply, setReply] = useState<string | null>(null);
  const [editingComment, setEditingComment] = useState<string | null>(null); const [warning, setWarning] = useState('');
  const [preview, setPreview] = useState<{ name: string; type: 'text'; text: string } | { name: string; type: 'image' | 'document'; url: string } | null>(null);
  const state = snapshot.state; const space = snapshot.workspace.id;
  const acceptance = item.attributes.acceptance_record as { accepted:boolean;version:number;note:string } | undefined;
  const retests = (item.attributes.retest_records || []) as Array<{operation_id:string;actor_id:string;at:number;result:string;note:string}>;
  const published = item.attributes.snapshot as {items:Record<string,Item>;note:string;at:number} | undefined;
  const comments = Object.values(state.comments).filter(c => c.item_id === item.id).sort((a,b) => a.created_at - b.created_at);
  const attachments = Object.values(state.attachments).filter(a => a.item_id === item.id && !a.deleted);
  const name = (id: string) => state.members[id]?.name || '历史成员';
  const choices = actionChoices(item);
  const primaryAction = choices.find(c => !c.value.startsWith('transition:') && !['archive','restore'].includes(c.value)) || choices.find(c => c.value.startsWith('transition:'));
  const editable = !item.deleted && item.kind !== 'test_run' && !['normal','terminate','ready'].includes(item.status) && !(item.kind === 'bug' && ['resolved','closed'].includes(item.status));
  async function run(work: () => Promise<unknown>) {
    setError(''); setWarning(''); setBusy(true); try { await work(); } catch (reason) { setError((reason as Error).message); } finally { setBusy(false); }
  }
  async function comment(event: FormEvent<HTMLFormElement>) {
    event.preventDefault(); const form = event.currentTarget; const data = new FormData(form);
    await run(async () => { await onSave({ type: 'add_comment', id: crypto.randomUUID(), item_id: item.id, body: String(data.get('body')), parent_id: reply, mentions: data.getAll('mentions'), attachments: data.getAll('attachments') }); form.reset(); setReply(null); });
  }
  async function upload(event: FormEvent<HTMLInputElement>) {
    const field = event.currentTarget; const file = field.files?.[0]; if (!file) return;
    await run(async () => {
      const receipt = await client.upload(space, item.id, crypto.randomUUID(), crypto.randomUUID(), file); setWarning(receipt.warning || '');
      try { await onRefresh(); } catch (reason) { throw new Error(`附件已保存，重新读取失败，请刷新：${(reason as Error).message}`); }
    }); field.value = '';
  }
  async function file(id: string, show: boolean) {
    await run(async () => {
      const blob = await client.download(space, id); const attachment = state.attachments[id];
      if (show && attachment.mime.startsWith('text/plain')) {
        setPreview({ name: attachment.name, type: 'text', text: await blob.text() }); return;
      }
      const url = URL.createObjectURL(blob);
      if (show) setPreview({ name: attachment.name, url, type: attachment.mime.startsWith('image/') ? 'image' : 'document' });
      else { const anchor = document.createElement('a'); anchor.href = url; anchor.download = attachment.name; anchor.click(); URL.revokeObjectURL(url); }
    });
  }
  const storedRefs = ['build_id','case_id','plan_id','resolved_build','duplicate_id','build_ids','case_ids','test_plan_ids','product_ids','affected_build_ids'].flatMap(key => { const value=item.attributes[key]; return typeof value === 'string' ? [value] : Array.isArray(value) ? value as string[] : []; });
  const relationIds = [...new Set([...item.links.map(l => l.target), ...storedRefs, ...Object.values(state.items).filter(i => i.links.some(l => l.target === item.id)).map(i => i.id)])].filter(id => state.items[id]);
  return <aside className="detail-panel" aria-label="工作项详情"><div className="detail-top"><span className="mono">{kindNames[item.kind]} · {item.id.slice(0,8)}</span><span className={`status status-${item.status}`}>{displayStatus(item)}</span><button className="icon-button close-detail" aria-label="关闭详情" onClick={onClose}><IconX size={18}/></button></div>
    <h2>{item.title}</h2><div className="detail-info"><p className="metadata"><IconUserCircle size={18}/>{item.assigned_to ? name(item.assigned_to) : '未指派'}<span>内容 V{item.content_version}</span><span>{dateText(item.created_at)} 创建</span></p>
    {!readOnly && (editable || choices.length > 0) && <div className="detail-actions">{primaryAction && <button className="primary" onClick={() => setAction(primaryAction.value)}>{primaryAction.label}</button>}<select aria-label="业务操作" value="" onChange={e => e.target.value === 'edit' ? setEdit(true) : setAction(e.target.value)}><option value="">更多操作</option>{editable && <option value="edit">编辑内容</option>}{choices.map(c => <option key={c.value} value={c.value}>{c.label}</option>)}</select></div>}</div>
    <div className="tabs" role="tablist">{['内容',`评论 ${comments.length}`,`附件 ${attachments.length}`,'动态','关联'].map(label => <button key={label} role="tab" aria-selected={tab === label.split(' ')[0]} onClick={() => setTab(label.split(' ')[0])}>{label}</button>)}</div>
    {error && <p role="alert" className="error">{error}</p>}
    {warning && <p role="status" className="callout">{warning}</p>}
    <div className="detail-body">
    {tab === '内容' && <><div className="prose">{item.description || '暂无描述'}</div>{item.acceptance && <section><h3>验收标准</h3><div className="prose">{item.acceptance}</div></section>}
      {['steps','expected','actual','environment','version_name','code_ref'].map(key => typeof item.attributes[key] === 'string' && <section key={key}><h3>{({ steps:'复现步骤',expected:'预期结果',actual:'实际结果',environment:'环境',version_name:'版本名称',code_ref:'代码版本' } as Record<string,string>)[key]}</h3><p className="prose">{item.attributes[key] as string}</p></section>)}
      {Array.isArray(item.attributes.steps) && <section><h3>测试步骤</h3><ol className="steps">{(item.attributes.steps as Array<{ action: string; expected: string }>).map((step,index) => <li key={index}><p>{step.action}</p><small>预期：{step.expected}</small></li>)}</ol></section>}
      {item.kind === 'test_plan' && <section><h3>测试范围</h3>{(item.attributes.case_ids as string[]).map(id => <button key={id} className="relation-link" onClick={() => onSelect(id)}>{state.items[id].title} · V{(item.attributes.case_versions as Record<string,number>)[id]}</button>)}</section>}
      {acceptance && <section><h3>产品验收 · 内容 V{acceptance.version}</h3><p>{acceptance.accepted ? '通过' : '未通过'}{acceptance.version !== item.content_version && '（历史版本）'}</p><p className="prose">{acceptance.note}</p></section>}
      {item.kind === 'bug' && typeof item.attributes.resolution === 'string' && <section><h3>解决方案</h3><p>{resolutionNames[item.attributes.resolution]}</p>{typeof item.attributes.resolved_build === 'string' && <button className="relation-link" onClick={() => onSelect(item.attributes.resolved_build as string)}>修复构建 · {state.items[item.attributes.resolved_build].title}</button>}</section>}
      {retests.length > 0 && <section><h3>独立复测记录</h3><ol className="activity">{retests.map(record => <li key={record.operation_id}><strong>{name(record.actor_id)} · {({pass:'通过',fail:'失败',blocked:'阻塞'} as Record<string,string>)[record.result]}</strong><time>{dateText(record.at)}</time><p className="prose">{record.note}</p></li>)}</ol></section>}
      {published && <section><h3>发布快照</h3><p className="callout">此发布已固化快照，后续修改其他工作项不会改写此版本。</p><p className="prose">{published.note}</p>{Object.values(published.items).map(fixed => <article className="snapshot-row" key={fixed.id}><small>{kindNames[fixed.kind]} · 内容 V{fixed.content_version}</small><strong>{fixed.title}</strong><span className={`status status-${fixed.status}`}>{displayStatus(fixed)}</span></article>)}</section>}
    </>}
    {tab === '评论' && <><div className="comments">{comments.length === 0 && <p className="empty-inline"><IconMessage size={22}/>还没有评论。记录讨论和交接结论。</p>}{comments.map(c => <article className="comment" key={c.id}><IconUserCircle size={30}/><div><header><strong>{name(c.actor_id)}</strong><time>{dateText(c.created_at)}</time></header>{c.parent_id && <small>回复 {name(state.comments[c.parent_id].actor_id)}</small>}{c.deleted ? <p className="hint">评论已删除</p> : <><p className="prose">{c.body}</p>{c.mentions.length > 0 && <small>提及：{c.mentions.map(name).join('、')}</small>}{c.attachments.map(id => state.attachments[id] && !state.attachments[id].deleted && <div className="file-card" key={id}><IconFile size={25}/><div><button className="file-title" onClick={() => file(id,false)}>{state.attachments[id].name}</button><small>{(state.attachments[id].size/1024).toFixed(1)} KiB</small></div><button className="icon-button" aria-label={`下载 ${state.attachments[id].name}`} onClick={() => file(id,false)}><IconDownload size={16}/></button></div>)}{!readOnly && <div className="comment-actions"><button onClick={() => setReply(c.id)}>回复</button>{c.actor_id === snapshot.actor_id && <><button onClick={() => setEditingComment(c.id)}>编辑</button><button onClick={() => run(() => onSave({ type:'delete_comment',id:c.id,version:c.version }))}>删除</button></>}</div>}</>}</div></article>)}</div>
      {!readOnly && <form className="comment-composer" onSubmit={comment}>{reply && <div className="reply-label">回复 {name(state.comments[reply].actor_id)}<button type="button" onClick={() => setReply(null)}>取消回复</button></div>}<label className="sr-only" htmlFor="comment-body">评论内容</label><textarea id="comment-body" name="body" required maxLength={16000} rows={1} placeholder="写下评论，记录讨论与交接结论"/><details><summary>提及成员 / 引用附件</summary><label>提及成员<select name="mentions" multiple>{Object.values(state.members).filter(m => m.active).map(m => <option key={m.id} value={m.id}>{m.name}</option>)}</select></label>{attachments.map(a => <label className="check-row" key={a.id}><input type="checkbox" name="attachments" value={a.id}/>{a.name}</label>)}</details><div className="composer-footer"><label className="upload-button"><IconPaperclip size={16}/>添加附件<input type="file" aria-label="评论附件" onChange={upload} disabled={busy}/></label><button className="primary" disabled={busy}>发表评论</button></div></form>}
    </>}
    {tab === '附件' && <><p className="hint">单个文件不超过 10 MiB。附件与工作项、操作者共同保存。</p>{attachments.length === 0 && <p className="empty-inline">暂无附件</p>}{attachments.map(a => <article className="file-card" key={a.id}><IconFile size={26}/><div><strong>{a.name}</strong><small>{(a.size / 1024).toFixed(1)} KiB · {name(a.actor_id)}</small></div><div className="file-actions">{(a.mime.startsWith('image/') || a.mime === 'application/pdf' || a.mime.startsWith('text/plain')) && <button onClick={() => file(a.id,true)}>预览</button>}<button aria-label={`下载 ${a.name}`} onClick={() => file(a.id,false)}><IconDownload size={16}/></button>{!readOnly && <button onClick={() => run(() => onSave({type:'remove_attachment',id:a.id}))}>删除</button>}</div></article>)}{!readOnly && <label className="upload-button"><IconPaperclip size={16}/>上传附件<input type="file" aria-label="上传附件" onChange={upload} disabled={busy}/></label>}</>}
    {tab === '动态' && <ol className="activity">{state.history.filter(h => h.object_id === item.id || state.comments[h.object_id]?.item_id === item.id || state.attachments[h.object_id]?.item_id === item.id).map((h,index) => <li key={`${h.operation_id}-${index}`}><strong>{name(h.actor_id)}</strong><time>{dateText(h.at)}</time><p>{actionNames[h.action] || h.action}</p>{h.note && <p className="prose">{h.note}</p>}</li>)}</ol>}
    {tab === '关联' && <>{relationIds.length === 0 && <p className="empty-inline">暂无关联工作项</p>}{relationIds.map(id => <button className="relation-link" key={id} onClick={() => onSelect(id)}>{kindNames[state.items[id].kind]} · {state.items[id].title}</button>)}</>}
    </div>
    {edit && <ItemForm kind={item.kind} item={item} snapshot={snapshot} onSave={onSave} onClose={() => setEdit(false)}/>}
    {action && <ActionForm action={action} item={item} snapshot={snapshot} onSave={onSave} onClose={() => setAction('')}/>}
    {editingComment && <Dialog title="编辑评论" description="保留评论标识，修改后更新版本。" onClose={() => setEditingComment(null)}><form className="form-stack" onSubmit={async event => { event.preventDefault(); const body = String(new FormData(event.currentTarget).get('body')); const c = state.comments[editingComment]; await run(async () => { await onSave({type:'edit_comment',id:c.id,version:c.version,body}); setEditingComment(null); }); }}><label>评论内容<textarea name="body" required defaultValue={state.comments[editingComment].body}/></label><button className="primary" disabled={busy}>保存评论</button></form></Dialog>}
    {preview && <Dialog title={preview.name} description="附件预览" onClose={() => { if (preview.type !== 'text') URL.revokeObjectURL(preview.url); setPreview(null); }}>{preview.type === 'text' ? <pre className="attachment-preview prose" aria-label={preview.name}>{preview.text}</pre> : preview.type === 'image' ? <img className="attachment-preview" src={preview.url} alt={preview.name}/> : <iframe className="attachment-preview" sandbox="" title={preview.name} src={preview.url}/>}</Dialog>}
  </aside>;
}
