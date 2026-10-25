import { fireEvent, render, screen, within, cleanup } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import type { Kind } from '@farflow/client';
import { ItemForm } from '../../packages/ui/src/ItemForm';
import { ActionForm, actionChoices } from '../../packages/ui/src/ActionForm';
import { item, snapshot } from './fixtures';

describe('业务表单', () => {
  it('所有对象使用正常字段提交，范围和关联内容版本来自当前空间', async () => {
    for(const kind of ['product','module','project','iteration','inbox','requirement','plan','task','test_case','build','bug','test_plan','release','document'] as Kind[]) {
      const save=vi.fn().mockResolvedValue(undefined), close=vi.fn();render(<ItemForm kind={kind} snapshot={snapshot()} onSave={save} onClose={close}/>);
      const dialog=screen.getByRole('dialog');fireEvent.change(within(dialog).getByLabelText('标题'),{target:{value:'实际业务标题'}});
      if(!['product','project'].includes(kind)){fireEvent.change(within(dialog).getByLabelText('所属产品'),{target:{value:'product'}});fireEvent.change(within(dialog).getByLabelText('所属项目'),{target:{value:'project'}});
      fireEvent.change(within(dialog).getByLabelText('所属迭代'),{target:{value:'iteration'}});}
      fireEvent.change(within(dialog).getByLabelText('描述'),{target:{value:'实际描述'}});
      if(kind === 'requirement') fireEvent.change(within(dialog).getByLabelText('验收标准'),{target:{value:'实际验收'}});
      if(kind === 'project') await userEvent.selectOptions(within(dialog).getByLabelText('关联产品（可多选）'),'product');
      if(kind === 'test_case') {await userEvent.click(within(dialog).getByText('添加步骤'));fireEvent.change(within(dialog).getByLabelText('步骤 1'),{target:{value:'打开'}});fireEvent.change(within(dialog).getByLabelText('预期 1'),{target:{value:'显示'}});fireEvent.change(within(dialog).getByLabelText('步骤 2'),{target:{value:'保存'}});fireEvent.change(within(dialog).getByLabelText('预期 2'),{target:{value:'成功'}});}
      if(kind === 'bug') for(const label of ['复现步骤','预期结果','实际结果','环境信息'])fireEvent.change(within(dialog).getByLabelText(label),{target:{value:label}});
      if(kind === 'build' || kind === 'release')fireEvent.change(within(dialog).getByLabelText('版本名称'),{target:{value:'1.0.0'}});
      if(kind === 'build')fireEvent.change(within(dialog).getByLabelText('代码版本 / Commit'),{target:{value:'abc'}});
      if(kind === 'test_plan') {await userEvent.selectOptions(within(dialog).getByLabelText('测试构建'),'build');await userEvent.selectOptions(within(dialog).getByLabelText('测试用例（可多选）'),'case');}
      if(kind === 'release') {await userEvent.selectOptions(within(dialog).getByLabelText('构建（可多选）'),'build');await userEvent.selectOptions(within(dialog).getByLabelText('测试单（可多选）'),'plan');}
      if(['task','test_case','release'].includes(kind))await userEvent.selectOptions(within(dialog).getByLabelText('关联需求（可多选）'),'requirement');
      await userEvent.click(within(dialog).getByRole('button',{name:'保存'}));expect(save).toHaveBeenCalledTimes(1);expect(close).toHaveBeenCalled();const command=save.mock.calls[0][0];expect(command.type).toBe('create');expect(command.item.title).toBe('实际业务标题');expect(command.item.project_id).toBe(['product','project'].includes(kind)?null:'project');if(kind==='test_case')expect(command.item.attributes.steps).toHaveLength(2);if(kind==='task')expect(command.item.links[0]).toEqual({target:'requirement',relation:'implements',content_version:1});cleanup();
    }
  });
  it('编辑保留标识和版本、支持取消，并显示失败而不关闭', async () => {
    const current=snapshot().state.items.bug, save=vi.fn().mockRejectedValue(new Error('版本冲突')),close=vi.fn();render(<ItemForm kind="bug" item={current} snapshot={snapshot()} onSave={save} onClose={close}/>);
    await userEvent.click(screen.getByRole('button',{name:'保存'}));expect(await screen.findByRole('alert')).toHaveTextContent('版本冲突');expect(close).not.toHaveBeenCalled();expect(save.mock.calls[0][0]).toMatchObject({type:'update',id:'bug',version:1,patch:{clear_assignee:false}});await userEvent.click(screen.getByRole('button',{name:'取消'}));expect(close).toHaveBeenCalled();
    cleanup();const testcase=snapshot().state.items.case;render(<ItemForm kind="test_case" item={testcase} snapshot={snapshot()} onSave={vi.fn()} onClose={vi.fn()}/>);expect(screen.getByLabelText('步骤 1')).toHaveValue('打开页面');
  });
  it('发布同时保存已复测修复清单与有明确说明的遗留问题', async () => {
    const data=snapshot();data.state.items.fixed=item('fixed','bug','closed',{resolution:'fixed'});const save=vi.fn().mockResolvedValue(undefined);render(<ItemForm kind="release" item={data.state.items.release} snapshot={data} onSave={save} onClose={vi.fn()}/>);
    await userEvent.selectOptions(screen.getByLabelText('修复 Bug（可多选）'),'fixed');await userEvent.selectOptions(screen.getByLabelText('遗留 Bug（可多选）'),'bug');fireEvent.change(screen.getByLabelText('遗留说明：bug标题'),{target:{value:'低风险，后续版本处理'}});await userEvent.click(screen.getByRole('button',{name:'保存'}));expect(save.mock.calls[0][0]).toMatchObject({type:'update',patch:{attributes:{known_issue_notes:{bug:'低风险，后续版本处理'}},links:expect.arrayContaining([{target:'fixed',relation:'contains',content_version:1},{target:'bug',relation:'known_issue',content_version:1}])}});
  });
  it('测试开始后可修改说明，但不会重新发送已固定的范围', async () => {
    const data=snapshot(),save=vi.fn().mockResolvedValue(undefined);render(<ItemForm kind="test_plan" item={data.state.items.plan} snapshot={data} onSave={save} onClose={vi.fn()}/>);expect(screen.queryByLabelText('测试构建')).not.toBeInTheDocument();fireEvent.change(screen.getByLabelText('描述'),{target:{value:'补充交接说明'}});await userEvent.click(screen.getByRole('button',{name:'保存'}));expect(save.mock.calls[0][0].patch).toMatchObject({description:'补充交接说明',attributes:{}});
  });
});

describe('独立业务动作', () => {
  it('只有匹配状态的动作可以进入，执行记录不能被归档或编辑', () => {
    const data=snapshot();const cases=[['requirement','reviewing','review'],['requirement','active','accept_requirement'],['inbox','accepted','convert_inbox'],['plan','doing','run_test'],['run','fail','bug_from_test'],['bug','active','resolve_bug'],['bug','resolved','retest_bug'],['release','wait','publish']] as const;
    for(const [id,status,choice] of cases)expect(actionChoices({...data.state.items[id],status}).map(a=>a.value)).toContain(choice);
    expect(actionChoices({...data.state.items.bug,deleted:true})).toEqual([{value:'restore',label:'恢复归档'}]);expect(actionChoices(item('run','test_run','pass'))).toEqual([]);
  });
  it('所有流程动作提交对应命令，测试执行固定用例版本，复测保留独立证据', async () => {
    const data=snapshot();const cases=[['requirement','draft','transition:reviewing'],['requirement','reviewing','review'],['requirement','active','accept_requirement'],['inbox','accepted','convert_inbox'],['plan','doing','run_test'],['run','fail','bug_from_test'],['bug','active','resolve_bug'],['bug','resolved','retest_bug'],['release','wait','publish'],['doc','active','archive'],['doc','active','restore']] as const;
    for(const [id,status,action] of cases){const current={...data.state.items[id],status,deleted:action==='restore'};if(action==='retest_bug')current.attributes={resolution:'fixed',resolved_build:'build'};const save=vi.fn().mockResolvedValue(undefined),close=vi.fn();render(<ActionForm action={action} item={current} snapshot={data} onSave={save} onClose={close}/>);
      if(action!=='restore')fireEvent.change(screen.getByRole('textbox',{name:action==='run_test'?'实际执行记录':'交接说明'}),{target:{value:'真实验证说明'}});
      if(action==='bug_from_test')fireEvent.change(screen.getByLabelText('环境信息'),{target:{value:'桌面'}});
      if(action==='convert_inbox')fireEvent.change(screen.getByLabelText('验收标准'),{target:{value:'验收'}});
      if(['resolve_bug','retest_bug'].includes(action))await userEvent.selectOptions(screen.getByLabelText('修复构建'),'build');
      if(action==='retest_bug')await userEvent.selectOptions(screen.getByLabelText('独立执行记录'),'run');
      await userEvent.click(screen.getByRole('button',{name:'确认提交'}));expect(save).toHaveBeenCalledTimes(1);expect(close).toHaveBeenCalled();const command=save.mock.calls[0][0];if(action==='run_test')expect(command).toMatchObject({plan_id:'plan',case_id:'case',case_version:1,actual:'真实验证说明'});if(action==='retest_bug')expect(command).toMatchObject({build_id:'build',run_id:'run',result:'pass'});cleanup();
    }
  });
  it('重复 Bug 和非修复结论不伪造构建，失败保留对话框，取消可用', async () => {
    const save=vi.fn().mockRejectedValue(new Error('无权操作')),close=vi.fn();render(<ActionForm action="resolve_bug" item={snapshot().state.items.bug} snapshot={snapshot()} onSave={save} onClose={close}/>);
    await userEvent.selectOptions(screen.getByLabelText('解决方案'),'duplicate');await userEvent.selectOptions(screen.getByLabelText('原 Bug'),'bug');fireEvent.change(screen.getByLabelText('交接说明'),{target:{value:'相同原因'}});await userEvent.click(screen.getByRole('button',{name:'确认提交'}));expect(await screen.findByRole('alert')).toHaveTextContent('无权操作');expect(save.mock.calls[0][0]).toMatchObject({resolution:'duplicate',build_id:null,duplicate_id:'bug'});
    await userEvent.selectOptions(screen.getByLabelText('解决方案'),'external');await userEvent.click(screen.getByRole('button',{name:'确认提交'}));expect(save.mock.calls[1][0]).toMatchObject({resolution:'external',build_id:null,duplicate_id:null});await userEvent.click(screen.getByRole('button',{name:'取消'}));expect(close).toHaveBeenCalled();
  });
  it('执行时显示测试单固定的历史步骤；缺失固定内容时阻止提交', async () => {
    const data=snapshot();data.state.versions=[{item_id:'case',version:1,title:'原版用例',description:'原版前提',acceptance:'',attributes:{steps:[{action:'原版步骤',expected:'原版结果'}]},actor_id:'owner',at:1792861949}];data.state.items.case.content_version=2;
    render(<ActionForm action="run_test" item={data.state.items.plan} snapshot={data} onSave={vi.fn()} onClose={vi.fn()}/>);expect(screen.getByText('原版步骤')).toBeInTheDocument();expect(screen.getByText('预期：原版结果')).toBeInTheDocument();await userEvent.selectOptions(screen.getByLabelText('测试用例（固定版本）'),'case');cleanup();
    data.state.versions=[];render(<ActionForm action="run_test" item={data.state.items.plan} snapshot={data} onSave={vi.fn()} onClose={vi.fn()}/>);expect(screen.getByRole('alert')).toHaveTextContent('固定的用例内容');expect(screen.getByRole('button',{name:'确认提交'})).toBeDisabled();
  });
});
