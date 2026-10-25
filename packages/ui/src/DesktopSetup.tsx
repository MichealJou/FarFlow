import { useState, type FormEvent } from 'react';
import type { DesktopHost, DesktopSession, DirectoryGrant, OpenedWorkspace } from '@farflow/client';
import { TransferReceive } from './TransferReceive';
import { GitClone } from './GitClone';

export function DesktopSetup({ mode, host, onReady }: {mode:'local'|'git';host:DesktopHost;onReady:(session:DesktopSession)=>void}) {
  const [directory,setDirectory]=useState<DirectoryGrant|null>(null);const [opened,setOpened]=useState<OpenedWorkspace|null>(null);
  const [error,setError]=useState('');const [busy,setBusy]=useState(false);
  const [receiving,setReceiving]=useState(false);
  const [cloning,setCloning]=useState<'new'|'remote'|'pack'>('new');
  async function choose(){setError('');setBusy(true);try{const chosen=await host.selectDirectory();if(chosen)setDirectory(chosen);}catch(reason){setError((reason as Error).message);}finally{setBusy(false);}}
  async function create(event:FormEvent<HTMLFormElement>){
    event.preventDefault();if(!directory){setError('请先选择保存目录');return;}const data=new FormData(event.currentTarget);setError('');setBusy(true);
    try{const created=await host.create(directory.token,String(data.get('folder')).trim(),mode,String(data.get('name')).trim(),{account:String(data.get('account')).trim(),name:String(data.get('member_name')).trim(),department:String(data.get('department')).trim()});setOpened(created);onReady(await host.selectIdentity(created,created.identities[0].id));}catch(reason){setError((reason as Error).message);}finally{setBusy(false);}
  }
  async function open(){setError('');setBusy(true);try{const chosen=await host.selectDirectory();if(chosen)setOpened(await host.inspect(chosen.token,mode));}catch(reason){setError((reason as Error).message);}finally{setBusy(false);}}
  async function select(event:FormEvent<HTMLFormElement>){event.preventDefault();const actor=String(new FormData(event.currentTarget).get('actor'));setError('');setBusy(true);try{onReady(await host.selectIdentity(opened!,actor));}catch(reason){setError((reason as Error).message);}finally{setBusy(false);}}
  async function back(){setError('');setBusy(true);try{await host.close(opened!.handle);setOpened(null);setReceiving(false);}catch(reason){setError((reason as Error).message);}finally{setBusy(false);}}
  if(receiving&&!opened)return <TransferReceive host={host} onOpened={setOpened} onBack={()=>setReceiving(false)}/>;
  return opened ? <div className="native-setup"><h2>选择成员身份</h2><p className="hint">{opened.workspace.name} · {opened.path}</p><p className="hint">本地身份依赖系统文件权限。请选择实际操作者，操作记录会保留该成员标识。</p><form className="form-stack" onSubmit={select}><label>当前成员<select name="actor" required defaultValue=""><option value="">请选择</option>{opened.identities.map(member=><option key={member.id} value={member.id} disabled={!member.active}>{member.name} · {member.account}{!member.active?'（已停用）':''}</option>)}</select></label>{error&&<p role="alert" className="error">{error}</p>}<button className="primary" disabled={busy}>进入工作空间</button><button type="button" disabled={busy} onClick={back}>返回初始化</button></form></div> : <div className="native-setup">{mode==='git'&&<div className="subnav setup-actions" role="group" aria-label="Git 初始化方式"><button type="button" className={cloning==='new'?'selected':''} aria-pressed={cloning==='new'} disabled={busy} onClick={()=>setCloning('new')}>新建空间</button><button type="button" className={cloning==='remote'?'selected':''} aria-pressed={cloning==='remote'} disabled={busy} onClick={()=>setCloning('remote')}>从远端克隆</button><button type="button" className={cloning==='pack'?'selected':''} aria-pressed={cloning==='pack'} disabled={busy} onClick={()=>setCloning('pack')}>从文件包创建</button></div>}{cloning!=='new'&&mode==='git'?<><GitClone key={cloning} source={cloning==='pack'?'pack':'remote'} host={host} onOpened={setOpened}/><button type="button" onClick={open}>打开已有空间</button>{error&&<p className="error" role="alert">{error}</p>}</>:<form className="form-stack" onSubmit={create}>
    <label>空间名称<input name="name" required maxLength={120} autoComplete="off"/></label>
    <label>保存位置<div className="directory-field"><input aria-label="保存位置" readOnly value={directory?.path||''} placeholder="选择本机保存目录"/><button type="button" disabled={busy} onClick={choose}>选择目录</button></div></label>
    <label>新文件夹名称<input name="folder" required maxLength={64} defaultValue="FarFlow" autoComplete="off"/></label>
    <div className="form-grid"><label>你的账号<input name="account" required maxLength={64} defaultValue="owner" autoComplete="off"/></label><label>你的姓名<input name="member_name" required maxLength={120} autoComplete="name"/></label></div><label>部门<input name="department" maxLength={120}/></label>
    <p className="hint">{mode==='local'?'SQLite 和附件保存在所选目录，无需另装数据库或启动服务。':'业务记录和附件保存在 Git 仓库，本机索引可重建；同步需要系统 Git。'}</p>
    {error&&<p role="alert" className="error">{error}</p>}<button className="primary" disabled={busy}>{busy?'处理中…':'创建工作空间'}</button><button type="button" disabled={busy} onClick={open}>打开已有空间</button>
  </form>}<button type="button" disabled={busy} onClick={()=>setReceiving(true)}>接收正式迁移</button></div>;
}
