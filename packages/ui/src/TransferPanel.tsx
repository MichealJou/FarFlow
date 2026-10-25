import { useEffect, useState, type FormEvent } from 'react';
import type { DirectoryGrant, Mode, PackFile, Snapshot, TransferAction, TransferClient, TransferResult } from '@farflow/client';
import { dateText, modeNames } from './catalog';

const phases = {frozen:'源已冻结',completed:'源已完成交接',cancelled:'源已取消迁移',staged:'目标已暂存',active:'目标已激活'};

export function TransferPanel({client,snapshot,onRefresh}:{client:TransferClient;snapshot:Snapshot;onRefresh:()=>Promise<void>}) {
  const [view,setView]=useState<TransferResult|null>(null);const [error,setError]=useState('');const [notice,setNotice]=useState('');const [busy,setBusy]=useState(false);const [retry,setRetry]=useState(0);
  const [directory,setDirectory]=useState<DirectoryGrant|null>(null);const [file,setFile]=useState<PackFile|null>(null);
  const [id,setId]=useState(()=>crypto.randomUUID());const [target,setTarget]=useState<Mode>(snapshot.workspace.mode==='local'?'git':'local');
  const [confirmed,setConfirmed]=useState(false);const [cancelConfirmed,setCancelConfirmed]=useState(false);const [acknowledged,setAcknowledged]=useState(false);const [note,setNote]=useState('');const [outputName,setOutputName]=useState('');
  const actor=snapshot.state.members[snapshot.actor_id];const owner=actor.active&&actor.roles.includes('owner');
  const status=view?.status||snapshot.transfer;const current=status?.plan.id||id;
  const source=status?.role==='source';const frozen=source&&status.phase==='frozen';const completed=source&&status.phase==='completed';const staged=status?.role==='target'&&status.phase==='staged';
  useEffect(()=>{
    let disposed=false;setError('');
    client.view().then(result=>{if(!disposed)setView(result);}).catch(reason=>{if(!disposed)setError((reason as Error).message);});
    return()=>{disposed=true;};
  },[client,retry]);
  async function choose(kind:'directory'|'file') {
    setBusy(true);setError('');
    try {if(kind==='directory'){const chosen=await client.selectDirectory();if(chosen)setDirectory(chosen);}else{const chosen=await client.selectFile();if(chosen)setFile(chosen);}}
    catch(reason){setError((reason as Error).message);}finally{setBusy(false);}
  }
  async function run(action:TransferAction,message:string) {
    setBusy(true);setError('');setNotice('');
    try {const result=await client.apply(action);setView(result);setNotice(`${message}${result.path?`：${result.path}`:''}`);setConfirmed(false);setCancelConfirmed(false);if(action.type==='cancel')setId(crypto.randomUUID());await onRefresh();}
    catch(reason){setError((reason as Error).message);try{setView(await client.view());await onRefresh();}catch(readError){setError(`${(reason as Error).message}；阶段重新读取失败：${(readError as Error).message}`);}}
    finally{setBusy(false);}
  }
  function prepare(event:FormEvent) {event.preventDefault();if(!directory){setError('请先选择交接资料目录');return;}void run({type:'prepare',id,target,directory:directory.token,folder:`FarFlow-备份-${id}`},'备份已保存，源空间已冻结');}
  function finish(event:FormEvent) {event.preventDefault();if(!file||!directory||!confirmed){setError('请选择目标回执、交接资料目录并明确确认');return;}void run({type:'complete',receipt:file.token,directory:directory.token,file:outputName||`FarFlow-完成凭据-${current}.json`},'源已完成交接，完成凭据已保存');}
  function activate(event:FormEvent) {event.preventDefault();if(!file||!confirmed){setError('请选择源完成凭据并明确确认');return;}void run({type:'activate',activation:file.token},'目标已激活，可以继续业务');}
  return <div className="page-content transfer-page"><div className="page-heading"><div><h1>空间迁移</h1><p>保留原空间、业务、成员、评论和附件，按阶段交接。</p></div><button disabled={busy} onClick={()=>setRetry(value=>value+1)}>重新读取阶段</button></div>
    <ol className="transfer-steps"><li>备份并冻结源</li><li>目标校验并暂存</li><li>源确认完成</li><li>目标激活续写</li></ol>
    {error&&<p role="alert" className="error">{error}</p>}{notice&&<p role="status" className="notice pack-path">{notice}</p>}
    <section aria-label="当前交接阶段"><h2>{status?phases[status.phase]:'尚无迁移计划'}</h2>{status?<><p>{modeNames[status.plan.source_mode]} → {modeNames[status.plan.target_mode]}</p><p className="mono pack-path">计划 {status.plan.id}</p><p className="hint">{snapshot.state.members[status.changed_by]?.name||'历史成员'} · {dateText(status.changed_at)}</p><p className="callout">{staged?'目标仍为只读。先把目标回执交回源端，再接收源完成凭据，激活后才能继续业务。':frozen?'源业务已停止写入。历史记录仍可查看，交接前可取消；完成后不能恢复源写入。':completed?'源永久只读。可用同一目标回执重新保存完成凭据，不能取消或恢复旧源写入。':'当前空间可以继续业务，下一次交接须使用新的迁移计划。'}</p></>:<p className="hint">源完成前可以取消；完成后源保持只读，需在目标激活继续工作。</p>}</section>
    {owner&&<>
      {(!status||['active','cancelled'].includes(status.phase)||frozen||completed)&&<section><h2>交接资料位置</h2><label>交接资料目录<div className="directory-field"><input aria-label="交接资料目录" readOnly value={directory?.path||''} placeholder="选择业务目录之外的位置"/><button disabled={busy} onClick={()=>choose('directory')}>选择资料目录</button></div></label><p className="hint">备份、交换包和完成凭据分别保存。该目录应独立于原空间；文件保存失败后可重新选择位置重试。</p></section>}
      {view&&(!status||['active','cancelled'].includes(status.phase))&&<form className="form-stack" onSubmit={prepare}><h2>发起迁移</h2><label>目标模式<select value={target} disabled={busy} onChange={event=>setTarget(event.target.value as Mode)}>{(['local','git','service'] as Mode[]).filter(mode=>mode!==snapshot.workspace.mode).map(mode=><option key={mode} value={mode}>{modeNames[mode]}</option>)}</select></label><p className="hint">{target==='service'?'目标服务需用真实账户完成身份映射和接收。导出交换包后交给该服务的迁移管理员，收到目标回执再继续。':'在目标设备初始化页选择“接收正式迁移”，核对原成员并暂存交换包，再把回执交回这里。'}</p><button className="primary" disabled={busy}>保存备份并冻结源</button></form>}
      {frozen&&<section><h2>导出交换包</h2><p className="hint">导出固定切点和附件，暂存目标在激活之前保持只读。导出只保存到本机，尚未交给其他设备。</p><button disabled={busy||!directory} onClick={()=>run({type:'export',directory:directory!.token,folder:`FarFlow-交换包-${current}`},'交换包已保存到本机')}>导出正式交换包</button></section>}
      {(frozen||completed)&&<form className="form-stack" onSubmit={finish}><h2>{completed?'重新保存完成凭据':'确认源完成交接'}</h2><label>目标回执<div className="directory-field"><input aria-label="目标回执" readOnly value={file?.path||''}/><button type="button" disabled={busy} onClick={()=>choose('file')}>选择目标回执</button></div></label><label>完成凭据文件名<input value={outputName} placeholder={`FarFlow-完成凭据-${current}.json`} onChange={event=>setOutputName(event.target.value)} disabled={busy}/></label><label className="check-row"><input type="checkbox" checked={confirmed} onChange={event=>setConfirmed(event.target.checked)} disabled={busy}/>已核对目标回执，确认源永久只读</label><button className="primary" disabled={busy}>确认源完成并保存凭据</button></form>}
      {staged&&<form className="form-stack" onSubmit={activate}><h2>激活目标</h2><label>源完成凭据<div className="directory-field"><input aria-label="源完成凭据" readOnly value={file?.path||''}/><button type="button" disabled={busy} onClick={()=>choose('file')}>选择源完成凭据</button></div></label><label className="check-row"><input type="checkbox" checked={confirmed} onChange={event=>setConfirmed(event.target.checked)} disabled={busy}/>已核对源空间完成交接，确认激活目标</label><button className="primary" disabled={busy}>核对凭据并激活目标</button></form>}
      {frozen&&<section><h2>取消本次迁移</h2><label className="check-row"><input type="checkbox" checked={cancelConfirmed} onChange={event=>setCancelConfirmed(event.target.checked)} disabled={busy}/>确认取消当前计划，恢复源空间写入</label><button disabled={busy||!cancelConfirmed} onClick={()=>run({type:'cancel',id:current},'迁移已取消，源空间恢复写入')}>取消当前迁移</button><p className="hint">已暂存的目标继续只读。后续迁移使用新计划，不用旧源覆盖另一处已经继续的业务。</p></section>}
    </>}
    {!!view?.plans.length&&<section aria-label="Git 成员停写确认"><h2>Git 成员停写确认</h2><p className="hint">需每位启用的可写成员分别确认本人全部副本已同步并停写。系统不能远程锁住未知离线设备。</p>{view.plans.map(plan=><article className="snapshot-row" key={plan.status.plan.id}><p className="mono pack-path">{plan.status.plan.id}</p><p>{phases[plan.status.phase]}</p>{plan.late_operations&&<p role="alert" className="error">出现切点之外的业务或并行计划，请先同步并核对，不能继续完成交接。</p>}<ul>{plan.required_members.map(member=><li key={member}>{snapshot.state.members[member]?.name||member}：{plan.confirmations[member]?.length?'已确认':'待本人确认'}</li>)}</ul>{actor.active&&!actor.roles.every(role=>role==='viewer')&&plan.status.phase==='frozen'&&plan.required_members.includes(snapshot.actor_id)&&!plan.confirmations[snapshot.actor_id]?.length&&<form className="form-stack" onSubmit={event=>{event.preventDefault();if(!acknowledged||!note.trim()){setError('请填写停写说明并确认本人全部副本已停写');return;}void run({type:'acknowledge',id:plan.status.plan.id,note:note.trim(),all_copies_stopped:acknowledged},'本人副本停写声明已保存');}}><label>停写说明<textarea value={note} onChange={event=>setNote(event.target.value)} maxLength={10000} disabled={busy}/></label><label className="check-row"><input type="checkbox" checked={acknowledged} onChange={event=>setAcknowledged(event.target.checked)} disabled={busy}/>本人全部 Git 副本已同步并停止写入</label><button disabled={busy}>提交本人停写声明</button></form>}</article>)}</section>}
    {!!view?.events.length&&<section aria-label="交接事件"><h2>交接事件</h2>{view.events.map((event,index)=><p key={`${event.plan.id}-${index}`}>{phases[event.phase]} · {snapshot.state.members[event.changed_by]?.name||'历史成员'} · {dateText(event.changed_at)}</p>)}</section>}
  </div>;
}
