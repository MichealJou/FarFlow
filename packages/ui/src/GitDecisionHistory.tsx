import { useMemo, useState } from 'react';
import type { GitDecisionReview, Member } from '@farflow/client';
import { actionNames, dateText } from './catalog';

export function GitDecisionHistory({decisions,members}:{decisions:GitDecisionReview[];members:Record<string,Member>}) {
  const [text,setText]=useState('');const [status,setStatus]=useState('all');const [page,setPage]=useState(0);
  const ordered=useMemo(()=>[...decisions].sort((a,b)=>b.operation.change.resolution.at-a.operation.change.resolution.at||a.operation.id.localeCompare(b.operation.id)),[decisions]);
  const filtered=ordered.filter(entry=>{
    const resolution=entry.operation.change.resolution,phase=entry.applied?'applied':entry.issue?'pending':'handled';
    return (status==='all'||phase===status)&&`${JSON.stringify(entry)} ${members[resolution.actor_id]?.name}`.toLocaleLowerCase().includes(text.trim().toLocaleLowerCase());
  });
  const pages=Math.max(1,Math.ceil(filtered.length/20));const current=Math.min(page,pages-1);
  return <section aria-label="决策历史" className="git-review"><h2>决策历史</h2><p className="hint">保留全部原始决策和目标提案。状态以本次重放结果为准，记录时间用于浏览排序，父级记录保留因果关系。</p>
    <div className="form-grid"><label>搜索决策<input value={text} onChange={event=>{setText(event.target.value);setPage(0);}} placeholder="成员、说明、操作或设备编号"/></label><label>决策状态<select value={status} onChange={event=>{setStatus(event.target.value);setPage(0);}}><option value="all">全部</option><option value="applied">当前重放已生效</option><option value="pending">等待处理</option><option value="handled">已被后续决策处理</option></select></label></div>
    <p aria-live="polite">共 {decisions.length} 条决策，符合条件 {filtered.length} 条，第 {current+1} / {pages} 页</p>
    {!filtered.length&&<p>{decisions.length?'没有符合条件的决策。':'当前还没有冲突决策。'}</p>}
    {filtered.slice(current*20,current*20+20).map(entry=>{
      const operation=entry.operation,resolution=operation.change.resolution;
      return <details className="git-proposal" key={operation.id}><summary>{dateText(resolution.at)} · {members[resolution.actor_id]?.name||resolution.actor_id} · {entry.applied?'当前重放已生效':entry.issue?'等待处理':'已被后续决策处理'} · {operation.id.slice(0,8)}</summary>
        <p>{resolution.note}</p><p className="hint mono">决策 {operation.id}<br/>实际成员 {resolution.actor_id}<br/>设备 {operation.device_id}<br/>父级记录 {operation.parents.join('、')||'无'}</p>{entry.issue&&<p className="callout">{entry.issue.message}</p>}
        <h3>本次决策命令</h3>{resolution.command?<><p>{actionNames[resolution.command.type]||resolution.command.type}</p><pre>{JSON.stringify(resolution.command,null,2)}</pre></>:<p>不采纳目标操作，保留当时已生效内容。</p>}
        <h3>目标原始提案</h3>{Object.values(entry.targets).map(target=>{
          const origin=target.change.type==='apply'?target.change.request:target.change.resolution;
          return <details key={target.id}><summary>目标 {target.id} · {members[origin.actor_id]?.name||origin.actor_id}</summary><p>{dateText(origin.at)} · 设备 {target.device_id}</p><pre>{JSON.stringify(target,null,2)}</pre></details>;
        })}<details><summary>完整原始决策</summary><pre>{JSON.stringify(operation,null,2)}</pre></details>
      </details>;
    })}
    {pages>1&&<div className="row-actions" aria-label="决策历史分页"><button disabled={current===0} onClick={()=>setPage(current-1)}>上一页</button><button disabled={current+1===pages} onClick={()=>setPage(current+1)}>下一页</button></div>}
  </section>;
}
