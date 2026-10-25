import { render, screen, waitFor, fireEvent } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import type { GitClient, GitReview } from '@farflow/client';
import { GitConflicts } from '../../packages/ui/src/GitConflicts';
import { snapshot } from './fixtures';

function fixture(){
  const data=snapshot();const review:GitReview={token:'review-token',state:data.state,issues:{},proposals:{},decisions:[]};
  for(const [id,text] of [['left','甲正文'],['right','乙正文']]){
    review.issues[id]={operation_id:id,kind:'concurrent',message:'同时修改正文，需要决策',peers:[id==='left'?'right':'left']};
    review.proposals[id]={id,device_id:`device-${id}`,parents:[],change:{type:'apply',request:{actor_id:'owner',at:100,command:{type:'update',id:'requirement',version:1,patch:{description:text}}}}};
  }
  const api:GitClient={selectPack:vi.fn(),exportPack:vi.fn(),importPack:vi.fn(),start:vi.fn(),status:vi.fn(),cancel:vi.fn(),review:vi.fn().mockResolvedValue(review),resolve:vi.fn().mockResolvedValue({operation_id:'decision',revision:3,affected:[]}),rebuild:vi.fn().mockResolvedValue(data)};
  const refresh=vi.fn().mockResolvedValue(undefined);
  return {data,review,api,refresh};
}

async function open(){await userEvent.click(screen.getByRole('button',{name:'查看冲突与决策历史'}));await screen.findByText('当前共有 2 条待处理记录。重新读取会清空未提交的选择。');}
async function choose(){await userEvent.click(screen.getByLabelText('纳入决策 left'));await userEvent.click(screen.getByLabelText('纳入决策 right'));}
async function confirm(){await userEvent.type(screen.getByLabelText('处理说明'),'已比较两边正文');await userEvent.click(screen.getByLabelText('已比较所选操作，确认记录以上决策'));}

describe('Git 冲突比较和明确决策',()=>{
  it('比较双方原文，采用一方原始操作，保存后刷新并保留真实警告',async()=>{
    const {data,api,refresh}=fixture();vi.mocked(api.resolve).mockResolvedValueOnce({operation_id:'decision',revision:3,affected:[],warning:'已保存，索引待重建'});
    render(<GitConflicts client={api} snapshot={data} onRefresh={refresh} disabled={false}/>);await open();expect(screen.getByText('甲正文')).toBeInTheDocument();expect(screen.getByText('乙正文')).toBeInTheDocument();expect(screen.getAllByText('当前已生效内容')).toHaveLength(2);
    expect(screen.getByRole('button',{name:'保存冲突决策'})).toBeDisabled();await choose();await userEvent.selectOptions(screen.getByLabelText('处理方式'),'right');await confirm();await userEvent.click(screen.getByRole('button',{name:'保存冲突决策'}));
    expect(await screen.findByRole('status')).toHaveTextContent('已保存，索引待重建');expect(api.resolve).toHaveBeenCalledWith(expect.objectContaining({token:'review-token',targets:['left','right'],command:{type:'update',id:'requirement',version:1,patch:{description:'乙正文'}},note:'已比较两边正文',at:expect.any(Number),id:expect.any(String)}));expect(refresh).toHaveBeenCalledOnce();expect(screen.queryByText('甲正文')).not.toBeInTheDocument();
  });
  it('重新读取失败保留比较，保存失败可原请求重试；新选择重新生成决策',async()=>{
    const {data,api,refresh}=fixture();render(<GitConflicts client={api} snapshot={data} onRefresh={refresh} disabled={false}/>);
    vi.mocked(api.review).mockRejectedValueOnce(new Error('原文暂不可读'));await userEvent.click(screen.getByRole('button',{name:'查看冲突与决策历史'}));expect(await screen.findByRole('alert')).toHaveTextContent('原文暂不可读');await open();await choose();
    fireEvent.submit(screen.getByRole('group',{name:'比较与决策'}).closest('form')!);expect(await screen.findByRole('alert')).toHaveTextContent('请选择待处理操作');
    await userEvent.selectOptions(screen.getByLabelText('处理方式'),'keep');await confirm();vi.mocked(api.resolve).mockRejectedValueOnce(new Error('记录已更新，请重新比较')).mockRejectedValueOnce(new Error('写入中断'));await userEvent.click(screen.getByRole('button',{name:'保存冲突决策'}));expect(await screen.findByRole('alert')).toHaveTextContent('重新比较');const first=vi.mocked(api.resolve).mock.calls[0][0];
    await userEvent.click(screen.getByRole('button',{name:'保存冲突决策'}));expect(api.resolve).toHaveBeenLastCalledWith(first);expect(first.command).toBeNull();
    await userEvent.click(screen.getByRole('button',{name:'重新读取并比较'}));expect(screen.getByLabelText('处理说明')).toHaveValue('');await userEvent.click(screen.getByLabelText('纳入决策 left'));await userEvent.click(screen.getByLabelText('纳入决策 left'));expect(screen.queryByRole('option',{name:'采用原始操作 left'})).not.toBeInTheDocument();
    await choose();await userEvent.selectOptions(screen.getByLabelText('处理方式'),'keep');await confirm();await userEvent.click(screen.getByRole('button',{name:'保存冲突决策'}));expect(await screen.findByRole('status')).toHaveTextContent('决策已保存在本机');expect(vi.mocked(api.resolve).mock.calls[2][0].id).not.toBe(first.id);
  });
  it('合并复用工作项表单，准备内容时不写入，最终确认才提交',async()=>{
    const {data,api,refresh}=fixture();render(<GitConflicts client={api} snapshot={data} onRefresh={refresh} disabled={false}/>);await open();await choose();await userEvent.selectOptions(screen.getByLabelText('处理方式'),'merge');
    const edit=screen.getByRole('button',{name:/编辑合并：/});await userEvent.click(edit);expect(await screen.findByRole('dialog',{name:'合并需求'})).toBeInTheDocument();await userEvent.clear(screen.getByLabelText('描述'));await userEvent.type(screen.getByLabelText('描述'),'合并双方后的完整正文');await userEvent.click(screen.getByRole('button',{name:'采用合并内容'}));expect(screen.queryByRole('dialog')).not.toBeInTheDocument();expect(api.resolve).not.toHaveBeenCalled();expect(screen.getByText('待确认的合并内容')).toBeInTheDocument();expect(screen.getByText('合并双方后的完整正文')).toBeVisible();expect(screen.getByText('查看完整合并字段').closest('details')).not.toHaveAttribute('open');
    await confirm();await userEvent.click(screen.getByRole('button',{name:'保存冲突决策'}));await waitFor(()=>expect(api.resolve).toHaveBeenCalled());expect(vi.mocked(api.resolve).mock.calls[0][0].command).toMatchObject({type:'update',id:'requirement',patch:{description:'合并双方后的完整正文'}});
  });
  it('只读成员、交接空间与后台同步不提供决策写入；索引可重新建立',async()=>{
    const {data,review,api,refresh}=fixture();data.state.members.owner.roles=['viewer'];review.proposals.left.change={type:'resolve',resolution:{actor_id:'历史账号',at:200,command:null,targets:['old'],note:'旧的决策'}};review.issues.left.kind='blocked';review.issues.right.kind='constraint';review.issues.right.peers=[];
    const view=render(<GitConflicts client={api} snapshot={data} onRefresh={refresh} disabled={true}/>);expect(screen.getByRole('button',{name:'查看冲突与决策历史'})).toBeDisabled();view.rerender(<GitConflicts client={api} snapshot={data} onRefresh={refresh} disabled={false}/>);await open();expect(screen.getByText('原决策说明：旧的决策')).toBeInTheDocument();expect(screen.getByText('当前成员只能查看，需要所有者或项目经理处理冲突。')).toBeInTheDocument();expect(screen.getByLabelText('纳入决策 left')).toBeDisabled();
    data.state.members.owner.roles=['owner'];data.transfer={role:'source',phase:'frozen',plan:{id:'transfer',source_mode:'git',target_mode:'service',prepared_by:'owner',prepared_at:1},changed_by:'owner',changed_at:1};view.rerender(<GitConflicts client={api} snapshot={data} onRefresh={refresh} disabled={false}/>);expect(screen.getByText('空间正在交接，当前不能保存冲突决策。')).toBeInTheDocument();
    vi.mocked(api.rebuild).mockRejectedValueOnce(new Error('索引目录不可写'));await userEvent.click(screen.getByRole('button',{name:'重建本机索引'}));expect(await screen.findByRole('alert')).toHaveTextContent('索引目录不可写');await userEvent.click(screen.getByRole('button',{name:'重建本机索引'}));expect(await screen.findByRole('status')).toHaveTextContent('本机索引已从完整记录重建');expect(refresh).toHaveBeenCalledOnce();
    vi.mocked(api.review).mockResolvedValueOnce({...review,issues:{},proposals:{}});await userEvent.click(screen.getByRole('button',{name:'重新读取并比较'}));expect(await screen.findByText('当前没有待处理冲突。')).toBeInTheDocument();
  });
});
