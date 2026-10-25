import { useState, type FormEvent } from 'react';
import type { Command, GitClient, GitResolutionInput, GitReview, Snapshot } from '@farflow/client';
import { actionNames, dateText, displayStatus } from './catalog';
import { ItemForm } from './ItemForm';
import { GitDecisionHistory } from './GitDecisionHistory';

export function GitConflicts({client,snapshot,onRefresh,disabled}:{client:GitClient;snapshot:Snapshot;onRefresh:()=>Promise<void>;disabled:boolean}) {
  const [review,setReview]=useState<GitReview|null>(null);const [targets,setTargets]=useState<string[]>([]);
  const [choice,setChoice]=useState('');const [note,setNote]=useState('');const [confirmed,setConfirmed]=useState(false);
  const [draft,setDraft]=useState<Command|null>(null);const [editing,setEditing]=useState('');
  const [pending,setPending]=useState<GitResolutionInput|null>(null);const [busy,setBusy]=useState(false);const [error,setError]=useState('');const [notice,setNotice]=useState('');
  const member=snapshot.state.members[snapshot.actor_id];const canResolve=member.active&&member.roles.some(role=>role==='owner'||role==='manager');
  const writable=!snapshot.transfer||['active','cancelled'].includes(snapshot.transfer.phase);
  const merged=draft?.patch as Record<string,unknown>|undefined;
  const blocked=disabled||busy;const proposals=review?Object.values(review.proposals):[];
  const selected=proposals.filter(proposal=>targets.includes(proposal.id));
  const commandOf=(id:string)=>{const change=review!.proposals[id].change;return change.type==='apply'?change.request.command:change.resolution.command;};
  const editable=review?[...new Set(selected.map(proposal=>commandOf(proposal.id)).filter(command=>command?.type==='update').map(command=>String(command!.id)))].map(id=>review.state.items[id]).filter(item=>item&&!item.deleted):[];

  async function load(){
    setBusy(true);setError('');setNotice('');
    try{setReview(await client.review());setTargets([]);setChoice('');setDraft(null);setNote('');setConfirmed(false);setPending(null);}catch(reason){setError((reason as Error).message);}finally{setBusy(false);}
  }
  async function rebuild(){
    setBusy(true);setError('');setNotice('');
    try{await client.rebuild();setNotice('本机索引已从完整记录重建，业务记录保持原样。');await onRefresh();}catch(reason){setError((reason as Error).message);}finally{setBusy(false);}
  }
  function changeTargets(id:string,checked:boolean){setTargets(current=>checked?[...current,id]:current.filter(value=>value!==id));setChoice('');setDraft(null);setConfirmed(false);setPending(null);}
  async function submit(event:FormEvent){
    event.preventDefault();if(!review)return;
    if(!targets.length||!choice||!confirmed||!note.trim()||(choice==='merge'&&!draft)){setError('请选择待处理操作、处理方式，填写说明并确认比较结果。');return;}
    const input=pending||{id:crypto.randomUUID(),token:review.token,targets,command:choice==='keep'?null:choice==='merge'?draft:commandOf(choice),note:note.trim(),at:Math.floor(Date.now()/1000)};
    setPending(input);setBusy(true);setError('');setNotice('');
    try{const saved=await client.resolve(input);setNotice(saved.warning||'决策已保存在本机，原始提案继续保留。请同步到其他副本。');setReview(null);setPending(null);await onRefresh();}catch(reason){setError((reason as Error).message);}finally{setBusy(false);}
  }
  return <section aria-label="冲突处理与本机索引"><h2>冲突处理与本机索引</h2><p className="hint">比较双方原始提案后，由所有者或项目经理明确决策。决策仍需通过正常业务权限和流程校验。</p>
    <div className="row-actions"><button onClick={load} disabled={blocked}>{review?'重新读取并比较':'查看冲突与决策历史'}</button><button onClick={rebuild} disabled={blocked}>重建本机索引</button></div>
    {error&&<p className="error" role="alert">{error}</p>}{notice&&<p className="callout" role="status">{notice}</p>}
    {review&&<><p className="hint">当前共有 {proposals.length} 条待处理记录。重新读取会清空未提交的选择。</p>{!proposals.length&&<p>当前没有待处理冲突。</p>}
      {!!proposals.length&&<form onSubmit={submit} className="form-stack git-review"><fieldset disabled={blocked||!canResolve||!writable}><legend>比较与决策</legend>{proposals.map(proposal=>{
        const issue=review.issues[proposal.id],change=proposal.change,origin=change.type==='apply'?change.request:change.resolution,command=commandOf(proposal.id);
        const current=command&&typeof command.id==='string'?review.state.items[command.id]:undefined;
        return <article key={proposal.id} className="git-proposal"><label className="check-row"><input type="checkbox" aria-label={`纳入决策 ${proposal.id}`} checked={targets.includes(proposal.id)} onChange={event=>changeTargets(proposal.id,event.target.checked)}/><strong>{change.type==='resolve'?'既有冲突决策':actionNames[command!.type]}</strong><span className="mono">{proposal.id.slice(0,8)}</span></label>
          <p>{review.state.members[origin.actor_id]?.name||origin.actor_id} · {dateText(origin.at)} · {issue.kind==='concurrent'?'并发冲突':issue.kind==='blocked'?'前置操作受阻':'业务约束'}</p><p>{issue.message}</p><p className="hint mono">操作 {proposal.id}<br/>设备 {proposal.device_id}<br/>关联操作 {issue.peers.join('、')||'无'}</p>
          {current&&<div className="git-comparison"><div><h3>当前已生效内容</h3><strong>{current.title}</strong><p>{displayStatus(current)} · V{current.content_version}</p><pre>{current.description}</pre>{current.acceptance&&<pre>{current.acceptance}</pre>}</div><div><h3>原始提案</h3>{command?.type==='update'?<dl>{Object.entries(command.patch as Record<string,unknown>).filter(([,value])=>value!==null&&value!==false).map(([field,value])=><div key={field}><dt>{{title:'标题',description:'描述',acceptance:'验收标准',priority:'优先级',severity:'严重度',assigned_to:'负责人',attributes:'业务字段',links:'关联关系',clear_assignee:'清除负责人',product_id:'所属产品',project_id:'所属项目',iteration_id:'所属迭代',parent_id:'父级工作项'}[field]||field}</dt><dd><pre>{typeof value==='string'?value:JSON.stringify(value,null,2)}</pre></dd></div>)}</dl>:<pre>{JSON.stringify(command,null,2)}</pre>}</div></div>}
          {change.type==='resolve'&&<p>原决策说明：{change.resolution.note}</p>}<details><summary>完整原始操作</summary><pre>{JSON.stringify(proposal,null,2)}</pre></details>
        </article>;
      })}
      <><label>处理方式<select required value={choice} onChange={event=>{setChoice(event.target.value);setConfirmed(false);setPending(null);}}><option value="">请选择</option><option value="keep">不采纳所选操作，保留当前已生效内容</option>{selected.map(proposal=><option value={proposal.id} key={proposal.id}>采用原始操作 {proposal.id.slice(0,8)}</option>)}{!!editable.length&&<option value="merge">编辑合并内容</option>}</select></label>
        {choice==='merge'&&<><p className="hint">以当前内容为基础编辑，准备的内容经下方确认后才保存。</p><div className="row-actions">{editable.map(item=><button type="button" key={item.id} onClick={()=>setEditing(item.id)}>编辑合并：{item.title}</button>)}</div>{draft&&<div className="git-merge-preview"><h3>待确认的合并内容</h3><strong>{String(merged!.title)}</strong><p>{String(merged!.description)}</p>{!!merged!.acceptance&&<p>{String(merged!.acceptance)}</p>}<details><summary>查看完整合并字段</summary><pre>{JSON.stringify(draft,null,2)}</pre></details></div>}</>}
        <label>处理说明<textarea required maxLength={8000} value={note} onChange={event=>{setNote(event.target.value);setPending(null);setConfirmed(false);}}/></label><label className="check-row"><input type="checkbox" checked={confirmed} onChange={event=>setConfirmed(event.target.checked)}/>已比较所选操作，确认记录以上决策</label><p className="hint">只处理明确勾选的操作，其他冲突与后续受阻记录会继续保留。保存后可用新的决策或业务操作继续调整。</p><button className="primary" disabled={!targets.length||!choice||!confirmed||(choice==='merge'&&!draft)}>{busy?'正在保存…':'保存冲突决策'}</button></>
      </fieldset>{!canResolve&&<p className="callout">当前成员只能查看，需要所有者或项目经理处理冲突。</p>}{!writable&&<p className="callout">空间正在交接，当前不能保存冲突决策。</p>}</form>}
      <GitDecisionHistory key={review.token} decisions={review.decisions} members={review.state.members}/>
    </>}
    {editing&&review&&<ItemForm kind={review.state.items[editing].kind} item={review.state.items[editing]} snapshot={{...snapshot,state:review.state}} draft onSave={async command=>{setDraft(command);setPending(null);setConfirmed(false);}} onClose={()=>setEditing('')}/>}
  </section>;
}
