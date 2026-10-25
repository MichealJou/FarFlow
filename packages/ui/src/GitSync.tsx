import { useEffect, useState, type FormEvent } from 'react';
import type { GitClient, GitJobStatus, GitRemoteAction, Snapshot } from '@farflow/client';
import { GitConflicts } from './GitConflicts';

export function GitSync({client,snapshot,onRefresh}:{client:GitClient;snapshot:Snapshot;onRefresh:()=>Promise<void>}) {
  const [address,setAddress]=useState('');const [job,setJob]=useState<string|null>(null);const [preparing,setPreparing]=useState(false);
  const [status,setStatus]=useState<GitJobStatus|null>(null);const [error,setError]=useState('');const [notice,setNotice]=useState('');const [retry,setRetry]=useState(0);
  const [source,setSource]=useState<'remote'|'import'|'export'>('remote');
  const owner=snapshot.state.members[snapshot.actor_id].active&&snapshot.state.members[snapshot.actor_id].roles.includes('owner');
  const viewer=snapshot.state.members[snapshot.actor_id].roles.every(role=>role==='viewer');
  const [choice,setChoice]=useState<GitRemoteAction>('sync');const action=viewer?'pull':choice;
  useEffect(()=>{
    if(!job)return;
    let disposed=false;let timer:ReturnType<typeof setTimeout>|undefined;
    async function poll(){
      try {
        const next=await client.status(job!);if(disposed)return;setError('');setStatus(next);
        if(next.phase==='running'){timer=setTimeout(poll,250);return;}
        setJob(null);setNotice('');await onRefresh();
      }catch(reason){if(!disposed)setError((reason as Error).message);}
    }
    void poll();
    return()=>{disposed=true;clearTimeout(timer);};
  },[client,job,retry,onRefresh]);
  async function start(event:FormEvent){
    event.preventDefault();setSource('remote');setPreparing(true);setError('');setNotice('');setStatus(null);
    try{setJob(await client.start(action,address.trim()));}catch(reason){setError((reason as Error).message);}finally{setPreparing(false);}
  }
  async function cancel(){
    try{await client.cancel(job!);setNotice('正在取消，等待 Git 传输结束后核对结果。');setRetry(value=>value+1);}catch(reason){setError((reason as Error).message);}
  }
  async function pack(kind:'import'|'export'){
    setSource(kind);setPreparing(true);setError('');setNotice('');setStatus(null);
    try{const id=crypto.randomUUID();if(kind==='export'){if(!await client.exportPack(id))return;}else{const file=await client.selectPack();if(!file)return;await client.importPack(file.token,id);}setJob(id);}catch(reason){setError((reason as Error).message);}finally{setPreparing(false);}
  }
  const busy=preparing||job!==null;
  return <><h1>Git 同步</h1><p className="hint">业务记录先保存在本机。远端访问使用系统 Git 凭据助手或 SSH 配置。</p>
    <form className="form-stack git-remote-form" onSubmit={start}><label>远端仓库地址<input required maxLength={4096} value={address} onChange={event=>setAddress(event.target.value)} placeholder="HTTPS、SSH 或本机仓库路径" disabled={busy}/></label><label>同步方式<select value={action} onChange={event=>setChoice(event.target.value as GitRemoteAction)} disabled={busy}><option value="pull">获取并合并远端记录</option>{!viewer&&<><option value="sync">获取、合并并上传</option><option value="push">仅上传当前提交</option></>}</select></label><p className="hint">网络等待共用 120 秒截止时间。取消或超时会保留本机记录；已经开始的上传需重试同步核对远端结果。</p><div className="row-actions"><button className="primary" disabled={busy}>{preparing?'正在准备…':job?source==='remote'?'正在同步…':'正在处理文件包…':'开始同步'}</button>{job&&source==='remote'&&<button type="button" onClick={cancel}>取消同步</button>}</div></form>
    <section aria-label="离线文件包"><h2>离线文件包</h2><p className="hint">使用 .farflowpack 交换完整历史和附件。导出只保存到本机，不会上传远端；导入按原有冲突规则合并，不改写原始操作。</p><div className="row-actions"><button type="button" disabled={busy} onClick={()=>pack('import')}>导入文件包</button>{owner&&<button type="button" disabled={busy} onClick={()=>pack('export')}>导出文件包</button>}</div>{job&&source!=='remote'&&<p className="callout" role="status">正在校验并处理本机文件包，退出应用会等候本次操作结束。已开始的落位不能通过关闭界面撤回。</p>}</section>
    {error&&<p className="error" role="alert">{error}</p>}{job&&error&&<button onClick={()=>setRetry(value=>value+1)}>重新读取状态</button>}{notice&&<p className="callout" role="status">{notice}</p>}
    {status?.phase==='failed'&&<p role="alert" className="error">{status.message}</p>}
    {status?.phase==='packed'&&<section aria-label="文件包结果"><h2>文件包结果</h2><p className="callout" role="status">{status.result.action==='export'?'完整文件包已保存到本机，尚未交给其他成员。':'文件包已导入并合并。'}</p><p className="pack-path">{status.result.path}</p>{status.result.bytes!==null&&<p>文件大小 {status.result.bytes.toLocaleString()} 字节</p>}{status.result.issues!==null&&<p>{status.result.issues?`本次导入返回 ${status.result.issues} 条冲突或受阻记录，请继续比较处理。`:'本次导入未发现待处理冲突。'}</p>}</section>}
    {status?.phase==='completed'&&<section aria-label="同步结果"><h2>本次结果</h2><p className="callout" role="status">{status.result.push ? status.result.push.pushed?'已上传固定提交':'本机提交已保留，未完成上传':'远端记录已获取并合并'}</p>{status.result.push&&<><p className="mono">提交 {status.result.push.commit}</p>{status.result.push.warning&&<p className="error" role="alert">{status.result.push.warning}</p>}</>}<p>{status.result.pending_local===true?'本机仍有后续变化，请再次同步。':status.result.pending_local===false?'本次同步完成时，未发现后续待上传变化。':'本次没有核对后续本机变化，可执行完整同步确认。'}</p></section>}
    {!!snapshot.issues?.length&&<section><h2>冲突与受阻操作</h2><p className="callout">远端记录已保留，当前空间暂停业务写入。上传成功不会自动消除业务冲突。</p>{snapshot.issues.map(issue=><article className="snapshot-row" key={issue.operation_id}><small className="mono">操作 {issue.operation_id}</small><p>{issue.message}</p></article>)}</section>}
    <GitConflicts client={client} snapshot={snapshot} onRefresh={onRefresh} disabled={busy}/>
  </>;
}
