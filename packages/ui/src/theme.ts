export const themes = [
  { id: 'cobalt', name: '浅灰钴蓝' }, { id: 'indigo', name: '冷白靛蓝' }, { id: 'forest', name: '暖白墨绿' },
  { id: 'graphite', name: '石墨留白' }, { id: 'teal', name: '纸白深青' }, { id: 'slate', name: '墨岩工作台' },
  { id: 'electric', name: '云白电蓝' }, { id: 'iris', name: '月白鸢尾' }, { id: 'amber', name: '深空琥珀' },
  { id: 'mono', name: '曜石银白' }, { id: 'cyan', name: '雾白湖青' }, { id: 'night', name: '炭灰晴蓝' },
] as const;

export function readTheme(): string {
  try { const saved = localStorage.getItem('farflow.theme'); return themes.some(theme => theme.id === saved) ? saved! : 'cobalt'; }
  catch { return 'cobalt'; }
}

export function applyTheme(id: string): void {
  const value = themes.some(theme => theme.id === id) ? id : 'cobalt';
  document.documentElement.dataset.theme = value;
  try { localStorage.setItem('farflow.theme', value); } catch { /* 私人浏览限制不能阻止业务界面使用。 */ }
}
