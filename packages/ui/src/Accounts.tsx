import { useEffect, useState, type FormEvent } from 'react';
import type { AccountHistory, ServiceClient, User } from '@farflow/client';
import { Dialog } from './Dialog';
import { accountActionNames, dateText } from './catalog';

export function Accounts({ client }: { client: ServiceClient }) {
  const [users, setUsers] = useState<User[]>([]); const [editing, setEditing] = useState<User | 'new' | null>(null);
  const [reset, setReset] = useState<User | null>(null); const [historyUser, setHistoryUser] = useState<User | null>(null); const [history, setHistory] = useState<AccountHistory>({events:[],next:null});
  const [error, setError] = useState(''); const [busy,setBusy] = useState(false);
  async function load() { setError(''); try { setUsers(await client.users()); } catch (reason) { setError((reason as Error).message); } }
  useEffect(() => { load(); }, [client]);
  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault(); const data = new FormData(event.currentTarget); const get = (key: string) => String(data.get(key) || '').trim(); setError(''); setBusy(true);
    try {
      if (editing === 'new') await client.createUser({account:get('account'),name:get('name'),department:get('department'),password:String(data.get('password')),is_admin:data.get('is_admin') === 'on'});
      else await client.updateUser(editing!.id,{version:editing!.version,name:get('name'),department:get('department'),active:data.get('active') === 'on',is_admin:data.get('is_admin') === 'on'});
      setEditing(null); await load();
    } catch (reason) { setError((reason as Error).message); } finally { setBusy(false); }
  }
  async function showHistory(user: User, after = 0) {
    setError(''); try { const page = await client.userHistory(user.id,after); setHistory(previous => ({events:after ? [...previous.events,...page.events] : page.events,next:page.next})); setHistoryUser(user); } catch (reason) { setError((reason as Error).message); }
  }
  return <div className="page-content"><header className="page-heading"><div><h1>用户管理</h1><p>管理服务登录账号。新账号和重置密码后的账号须先修改密码。</p></div><button className="primary" onClick={() => { setError(''); setEditing('new'); }}>创建用户</button></header>
    {error && !editing && !reset && <p role="alert" className="error">{error}</p>}<div className="table-scroll"><table><thead><tr><th>用户</th><th>账号</th><th>部门</th><th>管理员</th><th>状态</th><th>操作</th></tr></thead><tbody>{users.map(user => <tr key={user.id}><td>{user.name}</td><td>{user.account}</td><td>{user.department || '—'}</td><td>{user.is_admin ? '是' : '否'}</td><td>{user.active ? '启用' : '停用'}{user.must_change_password && ' · 待改密'}</td><td className="row-actions"><button onClick={() => { setError(''); setEditing(user); }}>编辑</button><button onClick={() => { setError(''); setReset(user); }}>重置密码</button><button onClick={() => showHistory(user)}>动态</button></td></tr>)}</tbody></table></div>
    {editing && <Dialog title={editing === 'new' ? '创建用户' : `编辑 ${editing.name}`} description="停用账号会撤销会话；最后一位启用管理员不能移除。" onClose={() => setEditing(null)}><form className="form-stack" onSubmit={submit}>
      {editing === 'new' && <label>登录账号<input name="account" required maxLength={64}/></label>}<label>姓名<input name="name" required maxLength={120} defaultValue={editing === 'new' ? '' : editing.name}/></label><label>部门<input name="department" maxLength={120} defaultValue={editing === 'new' ? '' : editing.department}/></label>
      {editing === 'new' && <label>临时密码<input name="password" type="password" required minLength={12} maxLength={1024} autoComplete="new-password"/></label>}
      <label className="check-row"><input name="is_admin" type="checkbox" defaultChecked={editing !== 'new' && editing.is_admin}/>服务管理员</label>{editing !== 'new' && <label className="check-row"><input name="active" type="checkbox" defaultChecked={editing.active}/>启用账号</label>}
      {error && <p role="alert" className="error">{error}</p>}<footer className="form-actions"><button type="button" onClick={() => setEditing(null)}>取消</button><button className="primary" disabled={busy}>保存用户</button></footer>
    </form></Dialog>}
    {reset && <Dialog title={`重置 ${reset.name} 的密码`} description="此操作会撤销该账号全部会话。用户下次登录必须修改密码。" onClose={() => setReset(null)}><form className="form-stack" onSubmit={async e => { e.preventDefault(); const password = String(new FormData(e.currentTarget).get('password')); setError('');setBusy(true);try { await client.resetPassword(reset.id,password);setReset(null);await load(); } catch(reason){setError((reason as Error).message);}finally{setBusy(false);} }}><label>新临时密码<input name="password" type="password" autoComplete="new-password" minLength={12} required/></label>{error && <p role="alert" className="error">{error}</p>}<button className="primary" disabled={busy}>确认重置</button></form></Dialog>}
    {historyUser && <Dialog title={`${historyUser.name} 的账户动态`} description="账户操作审计" onClose={() => setHistoryUser(null)}><ol className="activity">{history.events.map(event => <li key={event.sequence}><strong>{accountActionNames[event.action] || event.action}</strong><time>{dateText(event.at)}</time></li>)}</ol>{history.next != null && <button onClick={() => showHistory(historyUser,history.next!)}>加载更多动态</button>}</Dialog>}
  </div>;
}
