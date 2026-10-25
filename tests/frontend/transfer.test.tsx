import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import type { DesktopHost, MigrationReport, TransferClient, TransferResult, TransferStatus } from '@farflow/client';
import { TransferPanel } from '../../packages/ui/src/TransferPanel';
import { TransferReceive } from '../../packages/ui/src/TransferReceive';
import { DesktopSetup } from '../../packages/ui/src/DesktopSetup';
import { Workspace } from '../../packages/ui/src/Workspace';
import { client, snapshot, user } from './fixtures';

function status(phase:TransferStatus['phase']='frozen',mode:'local'|'git'='local'):TransferStatus {
  return {role:['staged','active'].includes(phase)?'target':'source',phase,plan:{id:'transfer',source_mode:mode,target_mode:mode==='local'?'git':'local',prepared_by:'owner',prepared_at:1792861949},changed_by:'owner',changed_at:1792861950};
}
function view(control:TransferStatus|null=null):TransferResult {return {status:control,events:control?[control]:[],plans:[],report:null,path:null};}
function environment(control:TransferStatus|null=null) {
  const data=snapshot();data.workspace.mode='local';data.transfer=control;
  const api:TransferClient={selectDirectory:vi.fn().mockResolvedValue({token:'directory',path:'/交接目录'}),selectFile:vi.fn().mockResolvedValue({token:'file',path:'/回执.json'}),view:vi.fn().mockResolvedValue(view(control)),apply:vi.fn().mockResolvedValue(view(control))};
  const refresh=vi.fn().mockResolvedValue(undefined);return {data,api,refresh};
}
function report(mode:'local'|'git'|'service'='git'):MigrationReport {return {package_id:'transfer',source:{id:'space',name:'原工作空间',schema:1,mode:'local'},target:null,revision:2,operation_count:2,item_count:1,comment_count:1,attachment_count:1,blob_count:1,members:Object.values(snapshot().state.members),identity_ids:['owner'],transfer:{...status().plan,target_mode:mode}};}
function receiving() {
  const data=snapshot();data.workspace.mode='git';const opened={handle:'target',workspace:data.workspace,path:'/目标空间',identities:Object.values(data.state.members)};
  const host:DesktopHost={migration:{inspect:vi.fn().mockResolvedValue(report()),stage:vi.fn().mockResolvedValue({opened,report:report(),receipt_path:'/回执目录/回执.json'})},selectDirectory:vi.fn().mockResolvedValue({token:'directory',path:'/所选目录'}),selectPack:vi.fn(),clone:{start:vi.fn(),startPack:vi.fn(),status:vi.fn(),cancel:vi.fn(),close:vi.fn(),open:vi.fn()},create:vi.fn(),inspect:vi.fn(),selectIdentity:vi.fn(),close:vi.fn(),service:vi.fn()};
  return {host,opened};
}

describe('空间迁移源与目标阶段',()=>{
  it('先明确资料目录和方向，备份冻结后可导出，取消后使用新计划',async()=>{
    const {data,api,refresh}=environment();render(<TransferPanel client={api} snapshot={data} onRefresh={refresh}/>);
    await screen.findByText('发起迁移');await userEvent.click(screen.getByText('保存备份并冻结源'));expect(screen.getByRole('alert')).toHaveTextContent('选择交接资料目录');
    vi.mocked(api.selectDirectory).mockResolvedValueOnce(null);await userEvent.click(screen.getByText('选择资料目录'));expect(screen.getByLabelText('交接资料目录')).toHaveValue('');
    vi.mocked(api.selectDirectory).mockRejectedValueOnce(new Error('目录选择失败'));await userEvent.click(screen.getByText('选择资料目录'));expect(screen.getByRole('alert')).toHaveTextContent('目录选择失败');
    await userEvent.click(screen.getByText('选择资料目录'));await userEvent.selectOptions(screen.getByLabelText('目标模式'),'service');expect(screen.getByText(/真实账户完成身份映射/)).toBeInTheDocument();await userEvent.selectOptions(screen.getByLabelText('目标模式'),'git');
    vi.mocked(api.apply).mockResolvedValueOnce({...view(status()),path:'/交接目录/备份'});await userEvent.click(screen.getByText('保存备份并冻结源'));expect(api.apply).toHaveBeenLastCalledWith({type:'prepare',id:expect.any(String),target:'git',directory:'directory',folder:expect.stringContaining('FarFlow-备份-')});await screen.findByText('导出正式交换包');expect(refresh).toHaveBeenCalled();
    const first=vi.mocked(api.apply).mock.calls[0][0];vi.mocked(api.apply).mockResolvedValueOnce({...view(status()),path:'/交接目录/交换包'});await userEvent.click(screen.getByText('导出正式交换包'));expect(api.apply).toHaveBeenLastCalledWith({type:'export',directory:'directory',folder:'FarFlow-交换包-transfer'});expect(screen.getByRole('status')).toHaveTextContent('/交接目录/交换包');
    expect(screen.getByText('取消当前迁移')).toBeDisabled();await userEvent.click(screen.getByLabelText('确认取消当前计划，恢复源空间写入'));vi.mocked(api.apply).mockResolvedValueOnce(view(status('cancelled')));await userEvent.click(screen.getByText('取消当前迁移'));await screen.findByText('发起迁移');vi.mocked(api.apply).mockResolvedValueOnce(view(status()));await userEvent.click(screen.getByText('保存备份并冻结源'));const next=vi.mocked(api.apply).mock.calls.at(-1)![0];expect(next.type==='prepare'&&first.type==='prepare'&&next.id!==first.id).toBe(true);
  });
  it('源完成需回执、保存目录与明确确认，完成后只能重取凭据',async()=>{
    const {data,api,refresh}=environment(status());render(<TransferPanel client={api} snapshot={data} onRefresh={refresh}/>);await screen.findByText('确认源完成交接');await userEvent.click(screen.getByText('确认源完成并保存凭据'));expect(api.apply).not.toHaveBeenCalled();
    vi.mocked(api.selectFile).mockResolvedValueOnce(null);await userEvent.click(screen.getByText('选择目标回执'));expect(screen.getByLabelText('目标回执')).toHaveValue('');vi.mocked(api.selectFile).mockRejectedValueOnce(new Error('文件选择失败'));await userEvent.click(screen.getByText('选择目标回执'));expect(screen.getByRole('alert')).toHaveTextContent('文件选择失败');
    await userEvent.click(screen.getByText('选择目标回执'));await userEvent.click(screen.getByText('选择资料目录'));await userEvent.click(screen.getByLabelText('已核对目标回执，确认源永久只读'));fireEvent.change(screen.getByLabelText('完成凭据文件名'),{target:{value:'明确凭据.json'}});
    vi.mocked(api.apply).mockResolvedValueOnce({...view(status('completed')),path:'/交接目录/明确凭据.json'});await userEvent.click(screen.getByText('确认源完成并保存凭据'));expect(api.apply).toHaveBeenLastCalledWith({type:'complete',receipt:'file',directory:'directory',file:'明确凭据.json'});await screen.findByText('重新保存完成凭据');expect(screen.queryByText('取消当前迁移')).not.toBeInTheDocument();
    fireEvent.change(screen.getByLabelText('完成凭据文件名'),{target:{value:''}});await userEvent.click(screen.getByLabelText('已核对目标回执，确认源永久只读'));await userEvent.click(screen.getByText('确认源完成并保存凭据'));expect(api.apply).toHaveBeenLastCalledWith(expect.objectContaining({file:'FarFlow-完成凭据-transfer.json'}));
  });
  it('目标激活前保持只读，凭据错误后读取真实阶段，不自动恢复源写入',async()=>{
    const {data,api,refresh}=environment(status('staged'));render(<TransferPanel client={api} snapshot={data} onRefresh={refresh}/>);await screen.findByText('激活目标');await userEvent.click(screen.getByText('核对凭据并激活目标'));expect(api.apply).not.toHaveBeenCalled();await userEvent.click(screen.getByText('选择源完成凭据'));await userEvent.click(screen.getByLabelText('已核对源空间完成交接，确认激活目标'));
    vi.mocked(api.apply).mockRejectedValueOnce(new Error('凭据不属于当前计划'));await userEvent.click(screen.getByText('核对凭据并激活目标'));expect(screen.getByRole('alert')).toHaveTextContent('凭据不属于当前计划');expect(api.view).toHaveBeenCalledTimes(2);
    vi.mocked(api.apply).mockResolvedValueOnce(view(status('active')));await userEvent.click(screen.getByText('核对凭据并激活目标'));await screen.findByText('发起迁移');expect(screen.getByRole('status')).toHaveTextContent('目标已激活');expect(refresh).toHaveBeenCalled();
  });
  it('阶段读取失败可重试，提交后再读失败明确显示两项错误',async()=>{
    const {data,api,refresh}=environment(status());vi.mocked(api.view).mockRejectedValueOnce(new Error('阶段不可读'));render(<TransferPanel client={api} snapshot={data} onRefresh={refresh}/>);expect(await screen.findByRole('alert')).toHaveTextContent('阶段不可读');await userEvent.click(screen.getByText('重新读取阶段'));await waitFor(()=>expect(api.view).toHaveBeenCalledTimes(2));
    await userEvent.click(screen.getByText('选择资料目录'));vi.mocked(api.apply).mockRejectedValueOnce(new Error('导出失败'));vi.mocked(api.view).mockRejectedValueOnce(new Error('目录不可读'));await userEvent.click(screen.getByText('导出正式交换包'));expect(screen.getByRole('alert')).toHaveTextContent('导出失败；阶段重新读取失败：目录不可读');
  });
  it('Git 按实际成员分别确认，完整计划和迟到记录可查，查看者无迁移写入口',async()=>{
    const {data,api,refresh}=environment(status('frozen','git'));data.workspace.mode='git';data.state.members.owner.roles=['developer'];
    const control=status('frozen','git');control.changed_by='historical';const plans=[{status:control,origin_device:'device',required_members:['owner','historical'],confirmations:{historical:['ack']},late_operations:true}];vi.mocked(api.view).mockResolvedValue({...view(control),events:[],plans});
    render(<TransferPanel client={api} snapshot={data} onRefresh={refresh}/>);await screen.findByText('Git 成员停写确认');expect(screen.queryByText('保存备份并冻结源')).not.toBeInTheDocument();expect(screen.getByText('historical：已确认')).toBeInTheDocument();await userEvent.click(screen.getByText('提交本人停写声明'));expect(screen.getByText('请填写停写说明并确认本人全部副本已停写')).toHaveAttribute('role','alert');fireEvent.change(screen.getByLabelText('停写说明'),{target:{value:'本人两个副本均已同步并停写'}});await userEvent.click(screen.getByLabelText('本人全部 Git 副本已同步并停止写入'));
    vi.mocked(api.apply).mockResolvedValueOnce({...view(control),plans:[{...plans[0],confirmations:{owner:['saved'],historical:['ack']}}]});await userEvent.click(screen.getByText('提交本人停写声明'));expect(api.apply).toHaveBeenLastCalledWith({type:'acknowledge',id:'transfer',note:'本人两个副本均已同步并停写',all_copies_stopped:true});await waitFor(()=>expect(screen.queryByText('提交本人停写声明')).not.toBeInTheDocument());cleanup();
    data.state.members.owner.roles=['viewer'];render(<TransferPanel client={api} snapshot={data} onRefresh={refresh}/>);await screen.findByText('Git 成员停写确认');expect(screen.queryByText('取消当前迁移')).not.toBeInTheDocument();
  });
  it('工作空间设置进入真实迁移页，冻结后的业务新建入口消失',async()=>{
    const {data,api}=environment();const business=client();business.transfer=api;vi.mocked(business.snapshot).mockResolvedValue({...data,transfer:status()});vi.mocked(api.apply).mockResolvedValue(view(status()));render(<Workspace initial={data} client={business} user={user} onLeave={vi.fn()} onLogout={vi.fn()} theme="cobalt" onTheme={vi.fn()}/>);await userEvent.click(screen.getByRole('button',{name:'设置'}));await userEvent.click(screen.getByRole('button',{name:'空间迁移'}));await screen.findByText('发起迁移');await userEvent.click(screen.getByText('选择资料目录'));await userEvent.click(screen.getByText('保存备份并冻结源'));expect(await screen.findByText(/此空间正在交接/)).toBeInTheDocument();await userEvent.click(screen.getByRole('button',{name:/^需求$/}));expect(screen.queryByText('新建需求')).not.toBeInTheDocument();
  });
});

describe('初始化接收正式迁移',()=>{
  it('核对方向、原成员与数量，暂存后先展示回执位置，再明确打开目标',async()=>{
    const {host,opened}=receiving(),onOpened=vi.fn();render(<TransferReceive host={host} onOpened={onOpened} onBack={vi.fn()}/>);vi.mocked(host.selectDirectory).mockResolvedValueOnce(null);await userEvent.click(screen.getByText('选择正式交换包目录'));expect(host.migration.inspect).not.toHaveBeenCalled();await userEvent.click(screen.getByText('选择正式交换包目录'));await screen.findByText('原工作空间');expect(screen.getByText('工作项 1 · 评论 1 · 附件 1')).toBeInTheDocument();await userEvent.click(screen.getByText('校验并暂存目标'));expect(host.migration.stage).not.toHaveBeenCalled();
    await userEvent.selectOptions(screen.getByLabelText('原所有者身份'),'owner');await userEvent.click(screen.getByText('选择目标父目录'));await userEvent.click(screen.getByText('选择回执目录'));fireEvent.change(screen.getByLabelText('目标文件夹名称'),{target:{value:'实际接续目录'}});fireEvent.change(screen.getByLabelText('目标回执文件名'),{target:{value:'实际回执.json'}});await userEvent.click(screen.getByLabelText('已核对原空间、实际所有者和迁移方向，确认只读暂存'));vi.mocked(host.migration.stage).mockRejectedValueOnce(new Error('回执输出失败，目标仍只读'));await userEvent.click(screen.getByText('校验并暂存目标'));expect(screen.getByRole('alert')).toHaveTextContent('目标仍只读');await userEvent.click(screen.getByText('校验并暂存目标'));expect(host.migration.stage).toHaveBeenLastCalledWith({source:'directory',directory:'directory',folder:'实际接续目录',actor:'owner',receipt_directory:'directory',receipt_file:'实际回执.json'});expect(onOpened).not.toHaveBeenCalled();expect(screen.getByRole('status')).toHaveTextContent('/回执目录/回执.json');await userEvent.click(screen.getByText('打开暂存空间'));expect(onOpened).toHaveBeenCalledWith(opened);
  });
  it('服务目标不在本机暂存，坏包不保留上一个报告，选择失败可见',async()=>{
    const {host}=receiving(),back=vi.fn();render(<TransferReceive host={host} onOpened={vi.fn()} onBack={back}/>);vi.mocked(host.migration.inspect).mockResolvedValueOnce(report('service'));await userEvent.click(screen.getByText('选择正式交换包目录'));expect(await screen.findByRole('alert')).toHaveTextContent('团队服务');expect(screen.queryByText('校验并暂存目标')).not.toBeInTheDocument();vi.mocked(host.migration.inspect).mockRejectedValueOnce(new Error('交换包附件损坏'));await userEvent.click(screen.getByText('选择正式交换包目录'));expect(screen.getByRole('alert')).toHaveTextContent('附件损坏');expect(screen.queryByText('原工作空间')).not.toBeInTheDocument();vi.mocked(host.selectDirectory).mockRejectedValueOnce(new Error('目录窗口失败'));await userEvent.click(screen.getByText('选择正式交换包目录'));expect(screen.getByRole('alert')).toHaveTextContent('目录窗口失败');await userEvent.click(screen.getByText('返回初始化'));expect(back).toHaveBeenCalled();
  });
  it('本地初始化提供接收入口，暂存后仍需明确选择实际成员',async()=>{
    const {host}=receiving();render(<DesktopSetup mode="local" host={host} onReady={vi.fn()}/>);await userEvent.click(screen.getByText('接收正式迁移'));await userEvent.click(screen.getByText('返回初始化'));expect(screen.getByLabelText('空间名称')).toBeInTheDocument();
  });
});
