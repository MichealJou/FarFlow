import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { createServiceClient } from '@farflow/client';
import { App } from '../../packages/ui/src/App';
import { client, user } from './fixtures';

vi.mock('@farflow/client',async original=>({...await original<typeof import('@farflow/client')>(),createServiceClient:vi.fn()}));
let api=client();
beforeEach(()=>{api=client();vi.mocked(createServiceClient).mockReturnValue(api);});
async function login(){await userEvent.click(screen.getByRole('button',{name:'使用服务模式'}));fireEvent.change(screen.getByLabelText('登录账号'),{target:{value:'owner'}});fireEvent.change(screen.getByLabelText('密码',{exact:true}),{target:{value:'FarFlow-Test-Password'}});await userEvent.click(screen.getByRole('button',{name:'连接并登录'}));}
describe('应用入口',()=>{
  it('模式入口和主题切换有效；登录、创建空间、打开、返回和退出连接真实接口',async()=>{
    render(<App/>);await userEvent.click(screen.getByRole('link',{name:'FarFlow 研流'}));expect(screen.getByText('本地工作空间')).toBeInTheDocument();await userEvent.click(screen.getByRole('button',{name:/^Git 基于/}));expect(screen.getByText('Git 工作空间')).toBeInTheDocument();await userEvent.click(screen.getByRole('button',{name:/^本地 离线/}));await userEvent.selectOptions(screen.getByLabelText('主题配色'),'forest');expect(document.documentElement.dataset.theme).toBe('forest');
    await login();await screen.findByText('首版研发');expect(api.login).toHaveBeenCalledWith('owner','FarFlow-Test-Password');await userEvent.click(screen.getByRole('button',{name:'创建工作空间'}));fireEvent.change(screen.getByLabelText('空间名称'),{target:{value:'我的团队'}});await userEvent.click(screen.getByRole('button',{name:'创建工作空间'}));await screen.findByRole('navigation',{name:'主导航'});expect(api.createWorkspace).toHaveBeenCalledWith(expect.any(String),'我的团队');
    await userEvent.click(screen.getByRole('button',{name:'切换工作空间：首版研发'}));await screen.findByRole('heading',{name:'工作空间'});await userEvent.click(screen.getByRole('button',{name:/首版研发 服务工作空间/}));await screen.findByRole('navigation',{name:'主导航'});await userEvent.click(screen.getByRole('button',{name:'设置'}));await userEvent.click(screen.getByText('修改密码'));await userEvent.click(screen.getByRole('button',{name:'关闭对话框'}));await userEvent.click(screen.getByRole('button',{name:'退出登录：林默'}));await screen.findByRole('button',{name:'连接并登录'});expect(api.logout).toHaveBeenCalled();
  });
  it('首次登录先改密；确认失败不请求，成功后回到登录并清除会话',async()=>{
    vi.mocked(api.login).mockResolvedValueOnce({token:'a'.repeat(64),expires_at:9999999999,user:{...user,must_change_password:true}});render(<App/>);await login();await screen.findByText('设置你的新密码');expect(api.listWorkspaces).not.toHaveBeenCalled();
    for(const [name,value] of [['当前密码','FarFlow-Old-Password'],['新密码','FarFlow-New-Password'],['确认新密码','FarFlow-Other-Password']])fireEvent.change(screen.getByLabelText(name,{exact:true}),{target:{value}});await userEvent.click(screen.getByText('更新密码并重新登录'));expect(screen.getByRole('alert')).toHaveTextContent('不一致');expect(api.changePassword).not.toHaveBeenCalled();
    fireEvent.change(screen.getByLabelText('确认新密码'),{target:{value:'FarFlow-New-Password'}});vi.mocked(api.changePassword).mockRejectedValueOnce(new Error('旧密码错误'));await userEvent.click(screen.getByText('更新密码并重新登录'));expect(await screen.findByRole('alert')).toHaveTextContent('旧密码错误');await userEvent.click(screen.getByText('更新密码并重新登录'));await screen.findByText('密码已更新，请使用新密码登录。');expect(api.changePassword).toHaveBeenLastCalledWith('FarFlow-Old-Password','FarFlow-New-Password');
  });
  it('服务错误、打开失败、创建失败和取消有反馈，退出失败仍清理本机状态',async()=>{
    render(<App/>);vi.mocked(api.login).mockRejectedValueOnce(new Error('无法连接'));await login();expect(await screen.findByRole('alert')).toHaveTextContent('无法连接');await userEvent.click(screen.getByRole('button',{name:'连接并登录'}));await screen.findByText('首版研发');vi.mocked(api.snapshot).mockRejectedValueOnce(new Error('权限不足'));await userEvent.click(screen.getByRole('button',{name:/首版研发 服务工作空间/}));expect(await screen.findByRole('alert')).toHaveTextContent('权限不足');
    await userEvent.click(screen.getByRole('button',{name:'创建工作空间'}));fireEvent.change(screen.getByLabelText('空间名称'),{target:{value:'团队'}});vi.mocked(api.createWorkspace).mockRejectedValueOnce(new Error('创建失败'));await userEvent.click(screen.getByRole('button',{name:'创建工作空间'}));expect(screen.getAllByRole('alert').some(a=>a.textContent==='创建失败')).toBe(true);await userEvent.click(screen.getByRole('button',{name:'关闭对话框'}));vi.mocked(api.logout).mockRejectedValueOnce(new Error('退出时断网'));await userEvent.click(screen.getByRole('button',{name:'退出登录'}));await waitFor(()=>expect(screen.getByRole('status')).toHaveTextContent('退出时断网'));expect(screen.getByRole('button',{name:'连接并登录'})).toBeInTheDocument();
  });
  it('空空间有创建入口；强制改密页可退出',async()=>{
    vi.mocked(api.listWorkspaces).mockResolvedValue([]);render(<App/>);await login();await screen.findByText('还没有工作空间。创建一个，或请所有者添加你的账号。');await userEvent.click(screen.getByRole('button',{name:'退出登录'}));fireEvent.change(screen.getByLabelText('登录账号'),{target:{value:'owner'}});fireEvent.change(screen.getByLabelText('密码',{exact:true}),{target:{value:'FarFlow-Test-Password'}});vi.mocked(api.login).mockResolvedValueOnce({token:'a'.repeat(64),expires_at:9999999999,user:{...user,must_change_password:true}});await userEvent.click(screen.getByRole('button',{name:'连接并登录'}));await screen.findByText('设置你的新密码');await userEvent.click(screen.getByText('退出登录'));await screen.findByRole('button',{name:'连接并登录'});
  });
});
