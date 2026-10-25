import { useState, type FormEvent } from 'react';
import type { Command, Member, PeoplePage, Role, ServiceClient, Snapshot, WorkspaceClient } from '@farflow/client';
import { roleNames } from './catalog';
import { Dialog } from './Dialog';

export function Members({ snapshot, client, onSave }: { snapshot: Snapshot; client: WorkspaceClient; onSave: (command: Command) => Promise<void> }) {
  const [editing, setEditing] = useState<Member | 'new' | null>(null); const [people, setPeople] = useState<PeoplePage>({accounts:[],next:null});
  const [query, setQuery] = useState(''); const [error, setError] = useState(''); const [busy, setBusy] = useState(false);
  const service='people' in client ? client as ServiceClient : null;
  const owner = snapshot.state.members[snapshot.actor_id].roles.includes('owner') && (!snapshot.transfer || ['active','cancelled'].includes(snapshot.transfer.phase));
  async function search(after?: string) {
    if(!service)return;
    setError(''); setBusy(true); try { const page = await service.people(snapshot.workspace.id,query,after); setPeople(previous => ({accounts:after ? [...previous.accounts,...page.accounts] : page.accounts,next:page.next})); } catch (reason) { setError((reason as Error).message); } finally { setBusy(false); }
  }
  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault(); const data = new FormData(event.currentTarget); setError(''); setBusy(true);
    try {
      const selected = people.accounts.find(a => a.id === data.get('account'));
      const current = editing !== 'new' ? editing! : null;
      const identity = service ? current || {id:selected!.member_id || selected!.id,account:selected!.account,name:selected!.name,department:selected!.department} : {id:current?.id || crypto.randomUUID(),account:current?.account || String(data.get('local_account')).trim(),name:String(data.get('local_name')).trim(),department:String(data.get('local_department')).trim()};
      const member: Member = { ...identity, active:data.get('active') === 'on', roles:data.getAll('roles') as Role[] };
      await onSave({type:current ? 'update_member' : 'add_member',member}); setEditing(null);
    } catch (reason) { setError((reason as Error).message); } finally { setBusy(false); }
  }
  return <div className="page-content"><header className="page-heading"><div><h1>成员</h1><p>{service?'账户负责登录，成员角色决定当前工作空间内的业务权限。':'成员身份依赖系统文件权限；角色用于当前空间内的业务协作。'}</p></div>{owner && <button className="primary" onClick={() => { setError(''); setEditing('new'); search(); }}>添加成员</button>}</header>
    {error && !editing && <p role="alert" className="error">{error}</p>}<div className="table-scroll"><table><thead><tr><th>成员</th><th>账号</th><th>部门</th><th>角色</th><th>状态</th>{owner && <th>操作</th>}</tr></thead><tbody>{Object.values(snapshot.state.members).map(m => <tr key={m.id}><td>{m.name}{m.id === snapshot.actor_id && <small> · 我</small>}</td><td>{m.account}</td><td>{m.department || '—'}</td><td>{m.roles.map(r => roleNames[r]).join('、')}</td><td>{m.active ? '启用' : '停用'}</td>{owner && <td><button onClick={() => { setError(''); setEditing(m); }}>管理角色</button></td>}</tr>)}</tbody></table></div>
    {editing && <Dialog title={editing === 'new' ? '添加成员' : `管理 ${editing.name}`} description={service?'服务账号名称和部门由服务读取。至少保留一位启用的所有者。':'本地成员身份依赖系统文件权限；角色用于业务协作。至少保留一位启用的所有者。'} onClose={() => setEditing(null)}>{service && editing === 'new' && <form className="search-inline" onSubmit={e => { e.preventDefault(); search(); }}><label>查找服务账号<input value={query} onChange={e => setQuery(e.target.value)}/></label><button disabled={busy}>搜索</button></form>}<form className="form-stack" onSubmit={submit}>{service && editing === 'new' && <><label>服务账号<select name="account" required><option value="">请选择</option>{people.accounts.filter(p => p.active && !snapshot.state.members[p.member_id || p.id]).map(p => <option key={p.id} value={p.id}>{p.name} · {p.account}</option>)}</select></label>{people.next && <button type="button" onClick={() => search(people.next!)}>继续加载账号</button>}</>}
      {!service && <>{editing==='new' && <label>成员账号<input name="local_account" required maxLength={64}/></label>}<label>成员姓名<input name="local_name" required maxLength={120} defaultValue={editing==='new'?'':editing.name}/></label><label>部门<input name="local_department" maxLength={120} defaultValue={editing==='new'?'':editing.department}/></label></>}
      <fieldset><legend>工作空间角色</legend>{(Object.keys(roleNames) as Role[]).map(role => <label className="check-row" key={role}><input type="checkbox" name="roles" value={role} defaultChecked={editing !== 'new' && editing.roles.includes(role)}/>{roleNames[role]}</label>)}</fieldset><label className="check-row"><input name="active" type="checkbox" defaultChecked={editing === 'new' || editing.active}/>启用成员</label>{error && <p role="alert" className="error">{error}</p>}<footer className="form-actions"><button type="button" onClick={() => setEditing(null)}>取消</button><button className="primary" disabled={busy}>保存成员</button></footer></form></Dialog>}
  </div>;
}
