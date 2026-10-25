import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import { applyTheme, readTheme, themes } from '../../packages/ui/src/theme';
import { dateText, displayStatus, transitions } from '../../packages/ui/src/catalog';
import { Dialog } from '../../packages/ui/src/Dialog';
import { item } from './fixtures';

describe('主题和界面基础', () => {
  it('保存个人主题，校验无效值，在存储不可用时继续运行', () => {
    expect(readTheme()).toBe('cobalt');
    for(const theme of themes) {applyTheme(theme.id);expect(readTheme()).toBe(theme.id);expect(document.documentElement.dataset.theme).toBe(theme.id);}
    localStorage.setItem('farflow.theme','不存在');expect(readTheme()).toBe('cobalt');applyTheme('无效');expect(readTheme()).toBe('cobalt');
    const get = vi.spyOn(Storage.prototype,'getItem').mockImplementation(() => {throw new Error('隐私限制');});expect(readTheme()).toBe('cobalt');get.mockRestore();
    const set = vi.spyOn(Storage.prototype,'setItem').mockImplementation(() => {throw new Error('隐私限制');});expect(() => applyTheme('teal')).not.toThrow();set.mockRestore();
  });
  it('业务标签和动作映射覆盖全部对象及未知状态', () => {
    for(const [kind,status] of [['product','active'],['bug','active'],['task','doing'],['document','未知']] as const) expect(displayStatus({kind,status})).toBeTruthy();
    expect(displayStatus({kind:'bug',status:'active'})).toBe('待处理');expect(displayStatus({kind:'product',status:'active'})).toBe('启用中');expect(dateText(1792861949)).toMatch(/\d/);
    expect(transitions(item('bug','bug','new'))[0].label).toContain('确认');expect(transitions(item('task','task','doing'))).toHaveLength(3);expect(transitions(item('x','document','active'))).toEqual([]);expect(transitions(item('x','task','unknown'))).toEqual([]);
  });
  it('对话框有标题说明，按钮和 Escape 均可关闭', async () => {
    const close=vi.fn();render(<Dialog title="创建需求" description="输入业务字段" onClose={close}><input aria-label="标题"/></Dialog>);
    expect(screen.getByRole('dialog',{name:'创建需求'})).toBeInTheDocument();await userEvent.click(screen.getByRole('button',{name:'关闭对话框'}));expect(close).toHaveBeenCalled();await userEvent.keyboard('{Escape}');expect(close).toHaveBeenCalledTimes(2);
  });
});
