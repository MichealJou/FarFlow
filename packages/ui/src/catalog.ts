import type { Item, Kind, Role } from '@farflow/client';

export const kindNames: Record<Kind, string> = { product: '产品', module: '模块', project: '项目', iteration: '迭代', inbox: '收集箱', requirement: '需求', plan: '计划', task: '任务', test_case: '测试用例', test_plan: '测试单', test_run: '执行记录', build: '构建', bug: 'Bug', release: '发布', document: '文档' };
export const roleNames: Record<Role, string> = { owner: '所有者', product: '产品经理', manager: '项目经理', developer: '开发', tester: '测试', viewer: '只读成员' };
export const statusNames: Record<string, string> = { new: '待确认', clarifying: '待澄清', accepted: '已接纳', rejected: '已拒绝', converted: '已转化', draft: '草稿', reviewing: '待评审', active: '已批准', changing: '变更中', wait: '未开始', doing: '进行中', verify: '待验收', pause: '已暂停', suspended: '已挂起', cancel: '已取消', closed: '已关闭', resolved: '待复测', ready: '已就绪', done: '已完成', normal: '已发布', terminate: '停止维护', pass: '通过', fail: '失败', blocked: '阻塞', skipped: '跳过' };
export const modeNames = { local: '本地', git: 'Git', service: '服务' };
export const actionNames: Record<string,string> = { create:'创建工作项',update:'更新工作项',review:'提交评审',transition:'状态流转',accept_requirement:'产品验收',convert_inbox:'转化收集项',run_test:'执行测试',bug_from_test:'由测试提交 Bug',resolve_bug:'提交解决方案',retest_bug:'独立复测',publish:'发布',archive:'归档',restore:'恢复',add_comment:'发表评论',edit_comment:'编辑评论',delete_comment:'删除评论',add_attachment:'上传附件',remove_attachment:'删除附件',add_member:'添加成员',update_member:'修改成员',batch:'批量操作',bootstrap:'初始化管理员',password:'修改密码',reset_password:'重置密码' };
export const accountActionNames: Record<string,string> = { create:'创建账号',update:'更新账号',bootstrap:'初始化管理员',change_password:'修改密码',reset_password:'管理员重置密码' };
export const resolutionNames: Record<string,string> = { fixed:'已修复',duplicate:'重复 Bug',by_design:'设计如此',not_reproducible:'无法复现',external:'外部原因',postponed:'延期处理',will_not_fix:'不予修复',to_requirement:'转为需求' };

const routes: Partial<Record<Kind, Record<string, string[]>>> = {
  inbox: { new: ['clarifying', 'accepted', 'rejected'], clarifying: ['accepted', 'rejected'], accepted: ['clarifying'], rejected: ['new'] },
  requirement: { draft: ['reviewing'], changing: ['reviewing'], active: ['changing', 'closed'], closed: ['active'] },
  task: { wait: ['doing', 'cancel'], doing: ['verify', 'pause', 'cancel'], pause: ['doing', 'cancel'], verify: ['closed', 'doing'], closed: ['doing'], cancel: ['doing'] },
  bug: { new: ['active'], active: ['doing'], doing: ['active'], resolved: ['active'], closed: ['active'] },
  test_case: { draft: ['reviewing'], active: ['draft'] },
  test_plan: { wait: ['doing'], doing: ['done'], done: ['doing'] },
  project: { wait: ['doing'], doing: ['suspended', 'closed'], suspended: ['doing', 'closed'], closed: ['doing'] },
  iteration: { wait: ['doing'], doing: ['suspended', 'closed'], suspended: ['doing', 'closed'], closed: ['doing'] },
  plan: { wait: ['doing'], doing: ['done'], done: ['doing'] },
  build: { draft: ['ready'] }, release: { draft: ['wait'], wait: ['draft'], normal: ['terminate'] },
};

export function transitions(item: Item): Array<{ value: string; label: string }> {
  return (routes[item.kind]?.[item.status] || []).map(value => ({ value, label: item.kind === 'bug' && value === 'active' ? '确认 / 重开' : `转为${displayStatus({kind:item.kind,status:value})}` }));
}

export function displayStatus(item: Pick<Item, 'kind' | 'status'>): string {
  if (item.status === 'active' && ['product', 'module', 'document'].includes(item.kind)) return '启用中';
  if (item.kind === 'bug' && item.status === 'active') return '待处理';
  if (item.kind === 'release' && item.status === 'wait') return '待发布';
  return statusNames[item.status] || item.status;
}

export function dateText(at: number): string {
  return new Intl.DateTimeFormat('zh-CN', { month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', hour12: false }).format(new Date(at * 1000));
}
