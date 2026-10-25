import { useEffect, useRef, useState, type FormEvent } from 'react';
import type { DesktopHost, DirectoryGrant, OpenedWorkspace, PackFile } from '@farflow/client';

export function GitClone({host,onOpened,source='remote'}:{host:DesktopHost;onOpened:(opened:OpenedWorkspace)=>void;source?:'remote'|'pack'}){
  const [directory,setDirectory]=useState<DirectoryGrant|null>(null);const [busy,setBusy]=useState(false);const [phase,setPhase]=useState('');
  const fromPack=source==='pack';const [pack,setPack]=useState<PackFile|null>(null);
  const [error,setError]=useState('');const [notice,setNotice]=useState('');
  const active=useRef<{directory:string;job:string}|null>(null);const disposed=useRef(false);const reading=useRef(false);const timer=useRef<ReturnType<typeof setTimeout>|undefined>(undefined);
  useEffect(()=>{disposed.current=false;return()=>{disposed.current=true;clearTimeout(timer.current);const task=active.current;active.current=null;if(task)void host.clone.close(task.directory,task.job).catch(()=>{});};},[host]);
  async function choose(){setError('');try{const chosen=await host.selectDirectory();if(chosen)setDirectory(chosen);}catch(reason){setError((reason as Error).message);}}
  async function choosePack(){setError('');try{const chosen=await host.selectPack();if(chosen)setPack(chosen);}catch(reason){setError((reason as Error).message);}}
  async function read(){
    const task=active.current;if(!task||disposed.current||reading.current)return;reading.current=true;clearTimeout(timer.current);
    try{
      const status=await host.clone.status(task.directory,task.job);if(disposed.current||active.current!==task)return;
      setError('');
      if(status.phase==='running'){setPhase(fromPack?'正在校验并创建副本…':'正在克隆…');timer.current=setTimeout(read,250);return;}
      if(status.phase==='failed'){setError(status.message);setBusy(false);setPhase('');active.current=null;await host.clone.close(task.directory,task.job);return;}
      setPhase('正在读取工作空间…');const opened=await host.clone.open(task.directory,task.job);
      if(disposed.current||active.current!==task){await host.close(opened.handle);return;}
      active.current=null;setBusy(false);onOpened(opened);
    }catch(reason){if(!disposed.current)setError((reason as Error).message);}finally{reading.current=false;}
  }
  async function start(event:FormEvent<HTMLFormElement>){
    event.preventDefault();if(!directory){setError('请先选择保存目录');return;}
    if(fromPack&&!pack){setError('请先选择研流 Git 文件包');return;}
    const data=new FormData(event.currentTarget),task={directory:directory.token,job:crypto.randomUUID()};active.current=task;setBusy(true);setPhase('正在准备…');setError('');setNotice('');
    try{
      if(fromPack)await host.clone.startPack(task.directory,String(data.get('folder')).trim(),pack!.token,task.job);else await host.clone.start(task.directory,String(data.get('folder')).trim(),String(data.get('address')).trim(),task.job);
      if(disposed.current||active.current!==task){await host.clone.close(task.directory,task.job);return;}setPhase(fromPack?'正在校验并创建副本…':'正在克隆…');void read();
    }catch(reason){await host.clone.close(task.directory,task.job).catch(()=>{});if(!disposed.current){active.current=null;setBusy(false);setPhase('');setError((reason as Error).message);}}
  }
  async function cancel(){const task=active.current;if(!task)return;try{await host.clone.cancel(task.directory,task.job);setNotice('正在取消，等待 Git 传输结束。已完成的副本会保留在本机。');void read();}catch(reason){setError((reason as Error).message);}}
  return <form className="form-stack git-clone" onSubmit={start}><h2>{fromPack?'从文件包创建':'从远端克隆'}</h2><p className="hint">{fromPack?'从 .farflowpack 恢复完整历史和附件，创建独立副本，完成后选择原有实际成员。':'下载已有研流空间的完整记录和附件。使用系统 Git 凭据助手或 SSH 配置，完成后选择实际成员。'}</p>
    {fromPack?<label>研流 Git 文件包<div className="directory-field"><input aria-label="研流 Git 文件包" readOnly value={pack?.path||''} placeholder="选择 .farflowpack 文件"/><button type="button" onClick={choosePack} disabled={busy}>选择文件包</button></div></label>:<label>远端仓库地址<input name="address" required maxLength={4096} placeholder="HTTPS、SSH 或本机仓库路径" disabled={busy}/></label>}
    <label>保存位置<div className="directory-field"><input aria-label="保存位置" readOnly value={directory?.path||''} placeholder="选择本机保存目录"/><button type="button" onClick={choose} disabled={busy}>选择目录</button></div></label>
    <label>新文件夹名称<input name="folder" required maxLength={64} defaultValue="FarFlow" disabled={busy}/></label><p className="hint">{fromPack?'已有目录不会覆盖。文件包校验、落位期间请等待；离开后已开始的本机操作会继续完成，副本仍可重开。':'已有目录不会覆盖。网络等待共用 120 秒截止时间；关闭此入口会取消传输，已完成的副本继续保留。'}</p>
    {error&&<p role="alert" className="error">{error}</p>}{notice&&<p role="status" className="callout">{notice}</p>}
    <div className="row-actions"><button className="primary" disabled={busy}>{busy?phase:fromPack?'创建文件包副本':'开始克隆'}</button>{busy&&!fromPack&&phase!=='正在准备…'&&<button type="button" onClick={cancel}>取消克隆</button>}{busy&&error&&<button type="button" onClick={read}>重新读取状态</button>}</div>
    {busy&&error&&<p className="hint">读取失败不代表副本未创建；可重新读取，或稍后通过“打开已有空间”核对所选目录。</p>}
  </form>;
}
