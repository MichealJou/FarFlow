import { render, screen, waitFor, fireEvent } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import type { GitClient, GitJobStatus } from '@farflow/client';
import { GitSync } from '../../packages/ui/src/GitSync';
import { Workspace } from '../../packages/ui/src/Workspace';
import { client, snapshot, user } from './fixtures';

function git():GitClient{return {selectPack:vi.fn(),exportPack:vi.fn(),importPack:vi.fn(),start:vi.fn().mockResolvedValue('job'),status:vi.fn().mockResolvedValue({phase:'running'}),cancel:vi.fn().mockResolvedValue(undefined),review:vi.fn(),resolve:vi.fn(),rebuild:vi.fn()};}
const completed:GitJobStatus={phase:'completed',result:{push:{commit:'a'.repeat(40),pushed:true,failure:null,warning:null},pending_local:false}};

describe('Git 同步中心',()=>{
  it('导出文件包先等待本机保存窗口，取消不报成功，实际完成只表示已保存到本机',async()=>{
    const api=git(),refresh=vi.fn().mockResolvedValue(undefined);vi.mocked(api.exportPack).mockResolvedValueOnce(false).mockResolvedValueOnce(true);render(<GitSync client={api} snapshot={snapshot()} onRefresh={refresh}/>);await userEvent.click(screen.getByRole('button',{name:'导出文件包'}));expect(api.status).not.toHaveBeenCalled();expect(screen.queryByLabelText('文件包结果')).not.toBeInTheDocument();vi.mocked(api.status).mockResolvedValueOnce({phase:'running'}).mockResolvedValueOnce({phase:'packed',result:{action:'export',path:'/已保存.farflowpack',bytes:1024,issues:null}});await userEvent.click(screen.getByRole('button',{name:'导出文件包'}));expect(screen.getByRole('status')).toHaveTextContent('正在校验并处理本机文件包');expect(screen.queryByRole('button',{name:'取消同步'})).not.toBeInTheDocument();expect(await screen.findByText('完整文件包已保存到本机，尚未交给其他成员。')).toBeInTheDocument();expect(screen.getByText('/已保存.farflowpack')).toBeInTheDocument();expect(screen.getByText('文件大小 1,024 字节')).toBeInTheDocument();expect(refresh).toHaveBeenCalledOnce();
  });
  it('导入选择取消不启动任务，实际导入显示冲突并刷新；部分导入错误仍刷新核对',async()=>{
    const api=git(),refresh=vi.fn().mockResolvedValue(undefined);vi.mocked(api.selectPack).mockResolvedValueOnce(null).mockRejectedValueOnce(new Error('文件选择失败')).mockResolvedValue({token:'pack',path:'/回传.farflowpack'});render(<GitSync client={api} snapshot={snapshot()} onRefresh={refresh}/>);await userEvent.click(screen.getByRole('button',{name:'导入文件包'}));expect(api.importPack).not.toHaveBeenCalled();await userEvent.click(screen.getByRole('button',{name:'导入文件包'}));expect(await screen.findByRole('alert')).toHaveTextContent('文件选择失败');vi.mocked(api.status).mockResolvedValueOnce({phase:'packed',result:{action:'import',path:'/回传.farflowpack',bytes:null,issues:2}});await userEvent.click(screen.getByRole('button',{name:'导入文件包'}));expect(await screen.findByText('本次导入返回 2 条冲突或受阻记录，请继续比较处理。')).toBeInTheDocument();expect(api.importPack).toHaveBeenCalledWith('pack',expect.any(String));vi.mocked(api.status).mockResolvedValueOnce({phase:'packed',result:{action:'import',path:'/回传.farflowpack',bytes:null,issues:0}});await userEvent.click(screen.getByRole('button',{name:'导入文件包'}));expect(await screen.findByText('本次导入未发现待处理冲突。')).toBeInTheDocument();vi.mocked(api.status).mockResolvedValueOnce({phase:'failed',message:'业务数据已导入，Git 关联待恢复'});await userEvent.click(screen.getByRole('button',{name:'导入文件包'}));expect(await screen.findByRole('alert')).toHaveTextContent('业务数据已导入');expect(refresh).toHaveBeenCalledTimes(3);
  });
  it('完整文件包导出只显示给启用所有者，只读成员仍可导入已有记录',async()=>{
    const api=git(),data=snapshot();data.state.members.owner.roles=['viewer'];render(<GitSync client={api} snapshot={data} onRefresh={vi.fn()}/>);expect(screen.queryByRole('button',{name:'导出文件包'})).not.toBeInTheDocument();expect(screen.getByRole('button',{name:'导入文件包'})).toBeEnabled();
  });
  it('完整同步展示真实固定提交和后续变化，非快进保留本机结果',async()=>{
    const api=git(),refresh=vi.fn().mockResolvedValue(undefined);vi.mocked(api.status).mockResolvedValueOnce({phase:'running'}).mockResolvedValueOnce(completed);
    render(<GitSync client={api} snapshot={snapshot()} onRefresh={refresh}/>);
    await userEvent.type(screen.getByLabelText('远端仓库地址'),'  /远端.git  ');await userEvent.click(screen.getByRole('button',{name:'开始同步'}));expect(api.start).toHaveBeenCalledWith('sync','/远端.git');expect(await screen.findByText('已上传固定提交')).toBeInTheDocument();expect(screen.getByText('本次同步完成时，未发现后续待上传变化。')).toBeInTheDocument();expect(refresh).toHaveBeenCalledTimes(1);
    await userEvent.selectOptions(screen.getByLabelText('同步方式'),'push');vi.mocked(api.status).mockResolvedValueOnce({phase:'completed',result:{push:{commit:'b'.repeat(40),pushed:false,failure:'non_fast_forward',warning:'远端已变化，请重新同步'},pending_local:null}});
    await userEvent.click(screen.getByRole('button',{name:'开始同步'}));expect(await screen.findByText('本机提交已保留，未完成上传')).toBeInTheDocument();expect(screen.getByRole('alert')).toHaveTextContent('远端已变化');expect(screen.getByText('本次没有核对后续本机变化，可执行完整同步确认。')).toBeInTheDocument();
    vi.mocked(api.status).mockResolvedValueOnce({phase:'completed',result:{push:completed.result.push,pending_local:true}});await userEvent.click(screen.getByRole('button',{name:'开始同步'}));expect(await screen.findByText('本机仍有后续变化，请再次同步。')).toBeInTheDocument();
  });
  it('状态读取失败保留取消入口，可重读状态和取消；超时与取消不伪装上传成功',async()=>{
    const api=git();vi.mocked(api.status).mockRejectedValueOnce(new Error('状态暂不可读'));render(<GitSync client={api} snapshot={snapshot()} onRefresh={vi.fn().mockResolvedValue(undefined)}/>);
    fireEvent.change(screen.getByLabelText('远端仓库地址'),{target:{value:'https://farflow.example/repo.git'}});await userEvent.click(screen.getByRole('button',{name:'开始同步'}));expect(await screen.findByRole('alert')).toHaveTextContent('状态暂不可读');expect(screen.getByRole('button',{name:'取消同步'})).toBeInTheDocument();await userEvent.click(screen.getByRole('button',{name:'重新读取状态'}));await waitFor(()=>expect(api.status).toHaveBeenCalledTimes(2));
    vi.mocked(api.cancel).mockRejectedValueOnce(new Error('取消请求暂不可用'));await userEvent.click(screen.getByRole('button',{name:'取消同步'}));expect(await screen.findByRole('alert')).toHaveTextContent('取消请求暂不可用');
    vi.mocked(api.status).mockResolvedValueOnce({phase:'failed',message:'Git 同步已取消，本机记录已保留'});await userEvent.click(screen.getByRole('button',{name:'取消同步'}));await waitFor(()=>expect(screen.getByRole('alert')).toHaveTextContent('已取消'));expect(screen.queryByText('已上传固定提交')).not.toBeInTheDocument();expect(api.cancel).toHaveBeenCalledWith('job');
    vi.mocked(api.start).mockRejectedValueOnce(new Error('当前空间已有同步'));await userEvent.click(screen.getByRole('button',{name:'开始同步'}));expect(await screen.findByRole('alert')).toHaveTextContent('当前空间已有同步');expect(screen.getByRole('button',{name:'开始同步'})).toBeEnabled();
  });
  it('只读成员只可获取，冲突列表显示真实操作，未核对变化保持未知',async()=>{
    const api=git(),data=snapshot();data.state.members.owner.roles=['viewer'];data.issues=[{operation_id:'conflict',kind:'concurrent',message:'两位成员同时修改正文',peers:['peer']}];vi.mocked(api.status).mockResolvedValueOnce({phase:'completed',result:{push:null,pending_local:null}});
    render(<GitSync client={api} snapshot={data} onRefresh={vi.fn().mockResolvedValue(undefined)}/>);expect(screen.queryByRole('option',{name:'获取、合并并上传'})).not.toBeInTheDocument();expect(screen.getByText('两位成员同时修改正文')).toBeInTheDocument();fireEvent.change(screen.getByLabelText('远端仓库地址'),{target:{value:'/远端.git'}});await userEvent.click(screen.getByRole('button',{name:'开始同步'}));expect(api.start).toHaveBeenCalledWith('pull','/远端.git');expect(await screen.findByText('远端记录已获取并合并')).toBeInTheDocument();
  });
  it('卸载后不处理晚到状态，也不留下轮询定时器',async()=>{
    const api=git(),refresh=vi.fn();let finish!:(result:GitJobStatus)=>void;vi.mocked(api.status).mockReturnValueOnce(new Promise(resolve=>{finish=resolve;}));
    const view=render(<GitSync client={api} snapshot={snapshot()} onRefresh={refresh}/>);fireEvent.change(screen.getByLabelText('远端仓库地址'),{target:{value:'/远端.git'}});await userEvent.click(screen.getByRole('button',{name:'开始同步'}));await waitFor(()=>expect(api.status).toHaveBeenCalled());view.unmount();finish(completed);await Promise.resolve();expect(refresh).not.toHaveBeenCalled();
    const other=git();const next=render(<GitSync client={other} snapshot={snapshot()} onRefresh={refresh}/>);fireEvent.change(screen.getByLabelText('远端仓库地址'),{target:{value:'/远端.git'}});await userEvent.click(screen.getByRole('button',{name:'开始同步'}));await waitFor(()=>expect(other.status).toHaveBeenCalledTimes(1));next.unmount();await new Promise(resolve=>setTimeout(resolve,300));expect(other.status).toHaveBeenCalledTimes(1);
  });
  it('切换业务页面仍保留后台任务，返回同步中心可看到完成结果',async()=>{
    const api=client(),remote=git(),data=snapshot();data.workspace.mode='git';api.git=remote;vi.mocked(api.snapshot).mockResolvedValue(data);
    render(<Workspace initial={data} client={api} user={user} onLeave={vi.fn()} onLogout={vi.fn()} theme="cobalt" onTheme={vi.fn()}/>);await userEvent.click(screen.getByRole('button',{name:'Git 同步'}));fireEvent.change(screen.getByLabelText('远端仓库地址'),{target:{value:'/远端.git'}});await userEvent.click(screen.getByRole('button',{name:'开始同步'}));await userEvent.click(screen.getByRole('button',{name:'文档'}));vi.mocked(remote.status).mockResolvedValueOnce(completed);await waitFor(()=>expect(api.snapshot).toHaveBeenCalled());await userEvent.click(screen.getByRole('button',{name:'Git 同步'}));expect(screen.getByText('已上传固定提交')).toBeVisible();expect(remote.start).toHaveBeenCalledTimes(1);
  });
});
