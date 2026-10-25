import { fireEvent, render, screen, waitFor, cleanup } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import type { DesktopHost } from '@farflow/client';
import { GitClone } from '../../packages/ui/src/GitClone';
import { DesktopSetup } from '../../packages/ui/src/DesktopSetup';
import { client, snapshot } from './fixtures';

function fixture(){
  const data=snapshot();data.workspace.mode='git';const opened={handle:'cloned-handle',workspace:data.workspace,path:'/保存位置/FarFlow',identities:Object.values(data.state.members)};
  const host:DesktopHost={migration:{inspect:vi.fn(),stage:vi.fn()},clone:{startPack:vi.fn().mockResolvedValue(undefined),start:vi.fn().mockResolvedValue(undefined),status:vi.fn().mockResolvedValue({phase:'running'}),cancel:vi.fn().mockResolvedValue(undefined),close:vi.fn().mockResolvedValue(undefined),open:vi.fn().mockResolvedValue(opened)},selectPack:vi.fn().mockResolvedValue(null),selectDirectory:vi.fn().mockResolvedValue({token:'parent',path:'/保存位置'}),create:vi.fn(),inspect:vi.fn().mockResolvedValue(opened),selectIdentity:vi.fn().mockResolvedValue({opened,snapshot:data,client:client()}),close:vi.fn().mockResolvedValue(undefined),service:vi.fn()};
  return {host,opened};
}
async function prepare(){fireEvent.change(screen.getByLabelText('远端仓库地址'),{target:{value:' /本机远端.git '}});await userEvent.click(screen.getByRole('button',{name:'选择目录'}));}
async function start(){await prepare();await userEvent.click(screen.getByRole('button',{name:'开始克隆'}));}

describe('从远端克隆初始化',()=>{
  it('从文件包创建沿用目录和成员选择，不自动选中身份，本机落位不提供网络取消按钮',async()=>{
    const {host}=fixture(),ready=vi.fn();render(<DesktopSetup mode="git" host={host} onReady={ready}/>);await userEvent.click(screen.getByRole('button',{name:'从文件包创建'}));expect(screen.getByRole('button',{name:'从文件包创建'})).toHaveAttribute('aria-pressed','true');expect(screen.queryByLabelText('远端仓库地址')).not.toBeInTheDocument();await userEvent.click(screen.getByRole('button',{name:'创建文件包副本'}));expect(screen.getByRole('alert')).toHaveTextContent('请先选择保存目录');await userEvent.click(screen.getByRole('button',{name:'选择目录'}));await userEvent.click(screen.getByRole('button',{name:'创建文件包副本'}));expect(screen.getByRole('alert')).toHaveTextContent('请先选择研流 Git 文件包');await userEvent.click(screen.getByRole('button',{name:'选择文件包'}));expect(screen.getByLabelText('研流 Git 文件包')).toHaveValue('');vi.mocked(host.selectPack).mockRejectedValueOnce(new Error('文件窗口不可用'));await userEvent.click(screen.getByRole('button',{name:'选择文件包'}));expect(await screen.findByRole('alert')).toHaveTextContent('文件窗口不可用');vi.mocked(host.selectPack).mockResolvedValueOnce({token:'pack',path:'/原始.farflowpack'});await userEvent.click(screen.getByRole('button',{name:'选择文件包'}));vi.mocked(host.clone.status).mockResolvedValueOnce({phase:'running'}).mockResolvedValueOnce({phase:'cloned'});await userEvent.click(screen.getByRole('button',{name:'创建文件包副本'}));expect(screen.queryByRole('button',{name:'取消克隆'})).not.toBeInTheDocument();expect(await screen.findByLabelText('当前成员')).toHaveValue('');expect(host.clone.startPack).toHaveBeenCalledWith('parent','FarFlow','pack',expect.any(String));expect(host.clone.start).not.toHaveBeenCalled();expect(host.selectIdentity).not.toHaveBeenCalled();expect(ready).not.toHaveBeenCalled();
  });
  it('文件包副本校验失败保留错误，不把无效包说成创建成功',async()=>{
    const {host}=fixture();vi.mocked(host.selectPack).mockResolvedValue({token:'pack',path:'/无效.farflowpack'});vi.mocked(host.clone.status).mockResolvedValueOnce({phase:'failed',message:'文件包历史无效'});render(<GitClone host={host} source="pack" onOpened={vi.fn()}/>);await userEvent.click(screen.getByRole('button',{name:'选择目录'}));await userEvent.click(screen.getByRole('button',{name:'选择文件包'}));await userEvent.click(screen.getByRole('button',{name:'创建文件包副本'}));expect(await screen.findByRole('alert')).toHaveTextContent('文件包历史无效');expect(host.clone.open).not.toHaveBeenCalled();expect(screen.getByRole('button',{name:'创建文件包副本'})).toBeEnabled();
  });
  it('Git 初始化可切换新建和克隆，完成后必须明确选择成员',async()=>{
    const {host,opened}=fixture(),ready=vi.fn();vi.mocked(host.clone.status).mockResolvedValueOnce({phase:'running'}).mockResolvedValueOnce({phase:'cloned'});
    render(<DesktopSetup mode="git" host={host} onReady={ready}/>);await userEvent.click(screen.getByRole('button',{name:'从远端克隆'}));await userEvent.click(screen.getByRole('button',{name:'新建空间'}));expect(screen.getByLabelText('空间名称')).toBeInTheDocument();await userEvent.click(screen.getByRole('button',{name:'从远端克隆'}));await start();
    expect(await screen.findByLabelText('当前成员')).toHaveValue('');expect(host.clone.start).toHaveBeenCalledWith('parent','FarFlow','/本机远端.git',expect.any(String));expect(host.selectIdentity).not.toHaveBeenCalled();expect(ready).not.toHaveBeenCalled();await userEvent.selectOptions(screen.getByLabelText('当前成员'),'owner');await userEvent.click(screen.getByRole('button',{name:'进入工作空间'}));expect(host.selectIdentity).toHaveBeenCalledWith(opened,'owner');expect(ready).toHaveBeenCalledOnce();
  });
  it('取消目录选择、授权失败及重复目录不伪造克隆完成，仍可打开已有空间',async()=>{
    const {host}=fixture();render(<DesktopSetup mode="git" host={host} onReady={vi.fn()}/>);await userEvent.click(screen.getByRole('button',{name:'从远端克隆'}));fireEvent.change(screen.getByLabelText('远端仓库地址'),{target:{value:'/远端.git'}});await userEvent.click(screen.getByRole('button',{name:'开始克隆'}));expect(screen.getByRole('alert')).toHaveTextContent('请先选择保存目录');
    vi.mocked(host.selectDirectory).mockResolvedValueOnce(null);await userEvent.click(screen.getByRole('button',{name:'选择目录'}));expect(screen.getByLabelText('保存位置')).toHaveValue('');vi.mocked(host.selectDirectory).mockRejectedValueOnce(new Error('目录对话框不可用'));await userEvent.click(screen.getByRole('button',{name:'选择目录'}));expect(await screen.findByRole('alert')).toHaveTextContent('对话框不可用');await prepare();vi.mocked(host.clone.start).mockRejectedValueOnce(new Error('目标目录已存在'));await userEvent.click(screen.getByRole('button',{name:'开始克隆'}));expect(await screen.findByRole('alert')).toHaveTextContent('目标目录已存在');expect(host.clone.open).not.toHaveBeenCalled();expect(host.clone.close).toHaveBeenCalled();
    vi.mocked(host.inspect).mockRejectedValueOnce(new Error('不能打开已有空间'));await userEvent.click(screen.getByRole('button',{name:'打开已有空间'}));expect(screen.getByText('不能打开已有空间')).toBeInTheDocument();await userEvent.click(screen.getByRole('button',{name:'打开已有空间'}));expect(await screen.findByLabelText('当前成员')).toHaveValue('');
  });
  it('状态读取失败可重读或取消，真实取消结果保留本机提示',async()=>{
    const {host}=fixture();vi.mocked(host.clone.status).mockRejectedValueOnce(new Error('状态暂不可读'));render(<GitClone host={host} onOpened={vi.fn()}/>);await start();expect(await screen.findByRole('alert')).toHaveTextContent('状态暂不可读');await userEvent.click(screen.getByRole('button',{name:'重新读取状态'}));await waitFor(()=>expect(host.clone.status).toHaveBeenCalledTimes(2));
    vi.mocked(host.clone.cancel).mockRejectedValueOnce(new Error('取消暂不可用'));await userEvent.click(screen.getByRole('button',{name:'取消克隆'}));expect(await screen.findByRole('alert')).toHaveTextContent('取消暂不可用');vi.mocked(host.clone.status).mockResolvedValueOnce({phase:'failed',message:'Git 操作已取消，本机已完成数据保留'});await userEvent.click(screen.getByRole('button',{name:'取消克隆'}));expect(await screen.findByRole('button',{name:'开始克隆'})).toBeEnabled();expect(screen.getByRole('alert')).toHaveTextContent('已取消');expect(screen.queryByLabelText('当前成员')).not.toBeInTheDocument();
  });
  it('完成后的读取失败可以重试，原生打开成功后才交给成员选择',async()=>{
    const {host,opened}=fixture(),ready=vi.fn();vi.mocked(host.clone.status).mockResolvedValue({phase:'cloned'});vi.mocked(host.clone.open).mockRejectedValueOnce(new Error('读取副本失败'));render(<GitClone host={host} onOpened={ready}/>);await start();expect(await screen.findByRole('alert')).toHaveTextContent('读取副本失败');expect(ready).not.toHaveBeenCalled();await userEvent.click(screen.getByRole('button',{name:'重新读取状态'}));await waitFor(()=>expect(ready).toHaveBeenCalledWith(opened));expect(host.clone.open).toHaveBeenCalledTimes(2);
  });
  it('离开初始化会取消任务，不处理晚到注册或状态；晚到的会话会关闭',async()=>{
    for(const stage of ['start','status','open'] as const){
      const {host,opened}=fixture(),ready=vi.fn();let finish!:(value:any)=>void;
      if(stage==='start')vi.mocked(host.clone.start).mockReturnValueOnce(new Promise(resolve=>{finish=resolve;}));
      if(stage==='status')vi.mocked(host.clone.status).mockReturnValueOnce(new Promise(resolve=>{finish=resolve;}));
      if(stage==='open'){vi.mocked(host.clone.status).mockResolvedValueOnce({phase:'cloned'});vi.mocked(host.clone.open).mockReturnValueOnce(new Promise(resolve=>{finish=resolve;}));}
      const view=render(<GitClone host={host} onOpened={ready}/>);await start();if(stage==='open')await waitFor(()=>expect(host.clone.open).toHaveBeenCalled());view.unmount();finish(stage==='status'?{phase:'cloned'}:stage==='open'?opened:undefined);await waitFor(()=>expect(host.clone.close).toHaveBeenCalled());if(stage==='open')await waitFor(()=>expect(host.close).toHaveBeenCalledWith('cloned-handle'));expect(ready).not.toHaveBeenCalled();cleanup();
    }
    const {host}=fixture();vi.mocked(host.clone.close).mockRejectedValue(new Error('正在退出'));const view=render(<GitClone host={host} onOpened={vi.fn()}/>);await start();await waitFor(()=>expect(host.clone.status).toHaveBeenCalledTimes(1));view.unmount();await new Promise(resolve=>setTimeout(resolve,300));expect(host.clone.status).toHaveBeenCalledTimes(1);
  });
});
