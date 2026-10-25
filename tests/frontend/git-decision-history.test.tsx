import { render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it } from 'vitest';
import type { GitDecisionReview } from '@farflow/client';
import { GitDecisionHistory } from '../../packages/ui/src/GitDecisionHistory';
import { snapshot } from './fixtures';

function decision(id:string,at:number,applied=true):GitDecisionReview {
  return {operation:{id,device_id:`device-${id}`,parents:['parent-record'],change:{type:'resolve',resolution:{actor_id:'owner',at,command:null,targets:['left'],note:`说明 ${id}`}}},applied,issue:null,targets:{left:{id:'left',device_id:'original-device',parents:[],change:{type:'apply',request:{actor_id:'owner',at:80,command:{type:'update',id:'requirement',patch:{description:'原始甲正文'}}}}}}};
}

describe('完整决策历史浏览',()=>{
  it('显示实际成员、状态、完整原文和因果关系；历史查看不提供编辑入口',async()=>{
    const first=decision('applied-id',300);first.operation.change.resolution.command={type:'update',id:'requirement',patch:{description:'最终合并正文'}};
    const pending=decision('pending-id',200,false);pending.issue={operation_id:'pending-id',kind:'concurrent',message:'两条决策并发，需再次核对',peers:['other']};pending.operation.change.resolution.actor_id='历史成员编号';pending.operation.parents=[];
    const handled=decision('handled-id',100,false);handled.operation.change.resolution.command={type:'future_action',value:'保留未知命令原文'};handled.targets={old:{id:'old',device_id:'old-device',parents:[],change:{type:'resolve',resolution:{actor_id:'已停用账号',at:50,command:null,targets:['ancestor'],note:'先前决策说明'}}}};
    render(<GitDecisionHistory decisions={[handled,first,pending]} members={snapshot().state.members}/>);
    expect(screen.getByText('共 3 条决策，符合条件 3 条，第 1 / 1 页')).toBeInTheDocument();
    const summaries=screen.getAllByText(/ · 当前重放已生效 · | · 等待处理 · | · 已被后续决策处理 · /);
    expect(summaries.map(x=>x.textContent)).toEqual([expect.stringContaining('applied-'),expect.stringContaining('pending-'),expect.stringContaining('handled-')]);
    await userEvent.click(summaries[0]);const applied=summaries[0].closest('details')!;
    expect(within(applied).getByText('说明 applied-id')).toBeVisible();expect(within(applied).getByText(/父级记录 parent-record/)).toBeVisible();expect(within(applied).getAllByText(/最终合并正文/)[0]).toBeVisible();
    await userEvent.click(within(applied).getByText('目标 left · 林默'));expect(within(applied).getByText(/原始甲正文/)).toBeVisible();await userEvent.click(within(applied).getByText('完整原始决策'));expect(within(applied).getAllByText(/"device_id": "device-applied-id"/)[0]).toBeVisible();
    await userEvent.click(summaries[1]);expect(screen.getByText('两条决策并发，需再次核对')).toBeVisible();expect(screen.getByText('不采纳目标操作，保留当时已生效内容。')).toBeVisible();expect(screen.getByText(/父级记录 无/)).toBeVisible();
    await userEvent.click(summaries[2]);expect(screen.getByText('future_action')).toBeVisible();await userEvent.click(screen.getByText('目标 old · 已停用账号'));expect(screen.getByText(/先前决策说明/)).toBeVisible();
    expect(screen.queryByRole('button',{name:/保存|编辑|删除/})).not.toBeInTheDocument();
  });
  it('按状态和原始目标搜索，分页可往返，改变筛选会回到第一页',async()=>{
    const entries=Array.from({length:22},(_,index)=>decision(`entry-${String(index).padStart(2,'0')}`,index));entries[0].applied=false;entries[1].applied=false;entries[1].issue={operation_id:entries[1].operation.id,kind:'blocked',message:'等待前置决策',peers:[]};
    entries[2].operation.change.resolution.at=entries[3].operation.change.resolution.at;
    render(<GitDecisionHistory decisions={entries} members={snapshot().state.members}/>);
    expect(screen.getByText('共 22 条决策，符合条件 22 条，第 1 / 2 页')).toBeInTheDocument();expect(screen.getByRole('button',{name:'上一页'})).toBeDisabled();await userEvent.click(screen.getByRole('button',{name:'下一页'}));expect(screen.getByText('共 22 条决策，符合条件 22 条，第 2 / 2 页')).toBeInTheDocument();expect(screen.getByRole('button',{name:'下一页'})).toBeDisabled();await userEvent.click(screen.getByRole('button',{name:'上一页'}));
    await userEvent.selectOptions(screen.getByLabelText('决策状态'),'pending');expect(screen.getByText('共 22 条决策，符合条件 1 条，第 1 / 1 页')).toBeInTheDocument();await userEvent.selectOptions(screen.getByLabelText('决策状态'),'handled');expect(screen.getByText(/ · 已被后续决策处理 · entry-00/)).toBeInTheDocument();await userEvent.selectOptions(screen.getByLabelText('决策状态'),'applied');expect(screen.getByText('共 22 条决策，符合条件 20 条，第 1 / 1 页')).toBeInTheDocument();
    await userEvent.selectOptions(screen.getByLabelText('决策状态'),'all');await userEvent.click(screen.getByRole('button',{name:'下一页'}));await userEvent.type(screen.getByLabelText('搜索决策'),'  ENTRY-21 ');expect(screen.getByText('共 22 条决策，符合条件 1 条，第 1 / 1 页')).toBeInTheDocument();await userEvent.clear(screen.getByLabelText('搜索决策'));await userEvent.type(screen.getByLabelText('搜索决策'),'原始甲正文');expect(screen.getByText('共 22 条决策，符合条件 22 条，第 1 / 2 页')).toBeInTheDocument();await userEvent.clear(screen.getByLabelText('搜索决策'));await userEvent.type(screen.getByLabelText('搜索决策'),'找不到的编号');expect(screen.getByText('没有符合条件的决策。')).toBeInTheDocument();
  });
  it('空历史有明确提示',()=>{
    render(<GitDecisionHistory decisions={[]} members={{}}/>);expect(screen.getByText('当前还没有冲突决策。')).toBeInTheDocument();expect(screen.queryByLabelText('决策历史分页')).not.toBeInTheDocument();
  });
});
