import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import { Members } from '../../packages/ui/src/Members';
import { Accounts } from '../../packages/ui/src/Accounts';
import { client, snapshot, user } from './fixtures';

describe('成员角色', () => {
  it('从服务搜索真实账号，添加成员，修改角色与启用状态', async () => {
    const api=client(),save=vi.fn().mockResolvedValue(undefined);vi.mocked(api.people).mockResolvedValueOnce({accounts:[{id:'dev',member_id:'historic',account:'dev',name:'开发者',department:'研发',active:true}],next:'next'}).mockResolvedValueOnce({accounts:[{id:'test',member_id:null,account:'test',name:'测试者',department:'测试',active:true}],next:null});
    render(<Members snapshot={snapshot()} client={api} onSave={save}/>);await userEvent.click(screen.getByText('添加成员'));await screen.findByText('开发者 · dev');await userEvent.click(screen.getByText('继续加载账号'));await screen.findByText('测试者 · test');expect(api.people).toHaveBeenLastCalledWith('space','','next');
    await userEvent.selectOptions(screen.getByLabelText('服务账号'),'dev');await userEvent.click(screen.getByLabelText('开发'));await userEvent.click(screen.getByText('保存成员'));await waitFor(()=>expect(save).toHaveBeenCalledWith({type:'add_member',member:{id:'historic',account:'dev',name:'开发者',department:'研发',active:true,roles:['developer']}}));
    await userEvent.click(screen.getByText('管理角色'));await userEvent.click(screen.getByLabelText('启用成员'));await userEvent.click(screen.getByText('保存成员'));expect(save).toHaveBeenLastCalledWith(expect.objectContaining({type:'update_member',member:expect.objectContaining({id:'owner',active:false})}));
  });
  it('搜索、失败和取消有真实反馈，只读成员没有管理入口', async () => {
    const api=client(),save=vi.fn().mockRejectedValue(new Error('最后一位所有者'));render(<Members snapshot={snapshot()} client={api} onSave={save}/>);
    await userEvent.click(screen.getByText('管理角色'));await userEvent.click(screen.getByText('保存成员'));expect(await screen.findByRole('alert')).toHaveTextContent('最后一位所有者');await userEvent.click(screen.getByText('取消'));
    await userEvent.click(screen.getByText('添加成员'));fireEvent.change(screen.getByLabelText('查找服务账号'),{target:{value:'开发'}});vi.mocked(api.people).mockRejectedValueOnce(new Error('目录不可用'));await userEvent.click(screen.getByRole('button',{name:'搜索'}));expect(await screen.findByRole('alert')).toHaveTextContent('目录不可用');await userEvent.click(screen.getByRole('button',{name:'关闭对话框'}));
  });
  it('没有所有者角色不能管理成员', () => {const data=snapshot();data.state.members.owner.roles=['viewer'];data.state.members.owner.department='';data.state.members.owner.active=false;render(<Members snapshot={data} client={client()} onSave={vi.fn()}/>);expect(screen.queryByText('添加成员')).not.toBeInTheDocument();expect(screen.queryByText('管理角色')).not.toBeInTheDocument();expect(screen.getByText('停用')).toBeInTheDocument();});
});

describe('服务用户管理', () => {
  it('创建账号、更新账号、重置密码和查看分页审计', async () => {
    const api=client();vi.mocked(api.users).mockResolvedValue([{...user,department:'',must_change_password:true}, {...user,id:'second',account:'second',name:'第二位',is_admin:false,active:false}]);
    render(<Accounts client={api}/>);await screen.findByText('林默');await userEvent.click(screen.getByText('创建用户'));
    for(const [name,value] of [['登录账号','dev'],['姓名','开发者'],['部门','研发'],['临时密码','FarFlow-Test-Password']])fireEvent.change(screen.getByLabelText(name),{target:{value}});
    await userEvent.click(screen.getByLabelText('服务管理员'));await userEvent.click(screen.getByText('保存用户'));await waitFor(()=>expect(api.createUser).toHaveBeenCalledWith({account:'dev',name:'开发者',department:'研发',password:'FarFlow-Test-Password',is_admin:true}));
    await userEvent.click(screen.getAllByRole('button',{name:'编辑'})[0]);fireEvent.change(screen.getByLabelText('姓名'),{target:{value:'新姓名'}});await userEvent.click(screen.getByLabelText('启用账号'));await userEvent.click(screen.getByText('保存用户'));await waitFor(()=>expect(api.updateUser).toHaveBeenCalledWith('owner',expect.objectContaining({name:'新姓名',active:false,version:1})));
    await userEvent.click(screen.getAllByText('重置密码')[0]);fireEvent.change(screen.getByLabelText('新临时密码'),{target:{value:'New-FarFlow-Password'}});await userEvent.click(screen.getByText('确认重置'));await waitFor(()=>expect(api.resetPassword).toHaveBeenCalledWith('owner','New-FarFlow-Password'));
    vi.mocked(api.userHistory).mockResolvedValueOnce({events:[{sequence:1,action:'账户创建',at:1792861949,actor_id:'owner',data:{}}],next:1}).mockResolvedValueOnce({events:[{sequence:2,action:'密码重置',at:1792861949,actor_id:'owner',data:{}}],next:null});
    await userEvent.click(screen.getAllByText('动态')[0]);await screen.findByText('账户创建');await userEvent.click(screen.getByText('加载更多动态'));await screen.findByText('密码重置');expect(api.userHistory).toHaveBeenLastCalledWith('owner',1);await userEvent.click(screen.getByRole('button',{name:'关闭对话框'}));
  });
  it('失败保留表单，加载和审计错误明确显示，关闭入口可用', async () => {
    const api=client();vi.mocked(api.users).mockRejectedValueOnce(new Error('账户目录失败'));render(<Accounts client={api}/>);expect(await screen.findByRole('alert')).toHaveTextContent('账户目录失败');
    await userEvent.click(screen.getByText('创建用户'));let dialog=screen.getByRole('dialog');for(const [name,value] of [['登录账号','dev'],['姓名','开发者'],['临时密码','FarFlow-Test-Password']])fireEvent.change(within(dialog).getByLabelText(name),{target:{value}});vi.mocked(api.createUser).mockRejectedValueOnce(new Error('账号重复'));await userEvent.click(screen.getByText('保存用户'));expect(await screen.findByRole('alert')).toHaveTextContent('账号重复');await userEvent.click(screen.getByRole('button',{name:'取消'}));
    await userEvent.click(screen.getByText('创建用户'));dialog=screen.getByRole('dialog');for(const [name,value] of [['登录账号','dev'],['姓名','开发者'],['临时密码','FarFlow-Test-Password']])fireEvent.change(within(dialog).getByLabelText(name),{target:{value}});await userEvent.click(screen.getByText('保存用户'));await screen.findByText('林默');
    vi.mocked(api.userHistory).mockRejectedValueOnce(new Error('审计失败'));await userEvent.click(screen.getByRole('button',{name:'动态'}));expect(await screen.findByRole('alert')).toHaveTextContent('审计失败');
    await userEvent.click(screen.getByText('重置密码'));fireEvent.change(screen.getByLabelText('新临时密码'),{target:{value:'FarFlow-Test-Password'}});vi.mocked(api.resetPassword).mockRejectedValueOnce(new Error('无权限'));await userEvent.click(screen.getByText('确认重置'));expect(await screen.findByRole('alert')).toHaveTextContent('无权限');await userEvent.click(screen.getByRole('button',{name:'关闭对话框'}));
    await userEvent.click(screen.getByRole('button',{name:'编辑'}));await userEvent.click(screen.getByRole('button',{name:'关闭对话框'}));await userEvent.click(screen.getByText('创建用户'));await userEvent.click(screen.getByRole('button',{name:'关闭对话框'}));
  });
});
