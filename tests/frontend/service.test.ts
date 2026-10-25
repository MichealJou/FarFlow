import { describe, expect, it, vi } from 'vitest';
import { createServiceClient } from '@farflow/client';

const account = { id: 'account', account: 'owner', name: '所有者', department: '研发', active: true, is_admin: true, must_change_password: false, version: 1 };
const login = { token: 'a'.repeat(64), expires_at: 123456789, user: account };
const ok = (data: unknown = {}) => new Response(JSON.stringify(data), { status: 200, headers: { 'Content-Type': 'application/json' } });

describe('共享服务客户端', () => {
  it('登录后只把会话放在请求头中，所有业务和账户接口使用对应 HTTP 方法', async () => {
    const fetcher = vi.fn<typeof fetch>().mockImplementation(async () => ok());
    fetcher.mockResolvedValueOnce(ok(login));
    const api = createServiceClient('https://farflow.example/team/', fetcher);
    expect(await api.login('owner', '密码')).toEqual(login);
    expect(JSON.parse(fetcher.mock.calls[0][1]!.body as string)).toEqual({ account: 'owner', password: '密码' });
    const calls: Array<[string, string, () => Promise<unknown>]> = [
      ['/api/auth/me', 'GET', () => api.me()],
      ['/api/workspaces', 'GET', () => api.listWorkspaces()],
      ['/api/workspaces', 'POST', () => api.createWorkspace('id', '研发')],
      ['/api/workspaces/space%2Fid', 'GET', () => api.snapshot('space/id')],
      ['/api/workspaces/space/commands', 'POST', () => api.command('space', 'operation', { type: 'create', item: {} }, 3)],
      ['/api/users', 'GET', () => api.users()],
      ['/api/users', 'POST', () => api.createUser({ account: 'dev', name: '开发', department: '研发', password: '临时密码', is_admin: false })],
      ['/api/users/user%2Fid', 'PATCH', () => api.updateUser('user/id', { version: 1, name: '测试', department: '', active: true, is_admin: false })],
      ['/api/users/user/password', 'POST', () => api.resetPassword('user', '新密码')],
      ['/api/users/user/history?after=0', 'GET', () => api.userHistory('user')],
      ['/api/users/user/history?after=101', 'GET', () => api.userHistory('user', 101)],
      ['/api/workspaces/space/people?text=', 'GET', () => api.people('space')],
      ['/api/workspaces/space/people?text=%25_%5C&after=cursor', 'GET', () => api.people('space', '%_\\', 'cursor')],
    ];
    for (const [path, method, run] of calls) {
      await run();
      const [url, options] = fetcher.mock.calls.at(-1)!;
      expect(url).toBe(`https://farflow.example/team${path}`);
      expect(options?.method || 'GET').toBe(method);
      expect(new Headers(options?.headers).get('Authorization')).toBe(`Bearer ${login.token}`);
      expect(options?.credentials).toBe('omit');
    }
    const data = JSON.parse(fetcher.mock.calls[5][1]!.body as string);
    expect(data).toMatchObject({ id: 'operation', expected_revision: 3 });
    await api.command('space', 'without-version', { type: 'publish' });
    expect(JSON.parse(fetcher.mock.calls.at(-1)![1]!.body as string)).not.toHaveProperty('expected_revision');
    expect(localStorage.length).toBe(0);
  });

  it('上传保留原字节与编码后的文件名，超限时不发请求，下载返回实际 Blob', async () => {
    const fetcher = vi.fn<typeof fetch>().mockImplementation(async () => ok({ revision: 1 }));
    const api = createServiceClient('/', fetcher);
    const file = new File(['实际文件'], '需求 & 验收.txt', { type: 'text/plain' });
    await api.upload('space', 'item', 'operation', 'file', file);
    const [path, options] = fetcher.mock.calls[0];
    const url = new URL(path as string);
    expect(url.searchParams.get('name')).toBe('需求 & 验收.txt');
    expect(url.searchParams.get('mime')).toBe('text/plain');
    expect(url.searchParams.get('operation_id')).toBe('operation');
    expect(options?.body).toBe(file);
    await api.upload('space', 'item', 'op2', 'file2', new File(['空类型'], '证据'));
    expect(new URL(fetcher.mock.calls[1][0] as string).searchParams.get('mime')).toBe('application/octet-stream');
    const large = new File([new Uint8Array(10 * 1024 * 1024 + 1)], 'too-large');
    await expect(api.upload('space', 'item', 'large', 'file', large)).rejects.toThrow('10 MiB');
    expect(fetcher).toHaveBeenCalledTimes(2);
    fetcher.mockResolvedValueOnce(new Response('原字节', { headers: { 'Content-Type': 'text/plain' } }));
    const blob = await api.download('space', 'file/id');
    expect(await blob.text()).toBe('原字节');
    expect(fetcher.mock.calls.at(-1)![0]).toContain('/attachments/file%2Fid');
  });

  it('改密和退出后清除会话，退出失败也清除本机令牌', async () => {
    const fetcher = vi.fn<typeof fetch>().mockImplementation(async () => ok());
    const api = createServiceClient('', fetcher);
    fetcher.mockResolvedValueOnce(ok(login)); await api.login('owner', '密码');
    fetcher.mockResolvedValueOnce(new Response(null, { status: 204 })); await api.changePassword('旧', '新');
    expect(JSON.parse(fetcher.mock.calls.at(-1)![1]!.body as string)).toEqual({ current_password: '旧', new_password: '新' });
    await api.me(); expect(new Headers(fetcher.mock.calls.at(-1)![1]?.headers).has('Authorization')).toBe(false);
    fetcher.mockResolvedValueOnce(ok(login)); await api.login('owner', '密码');
    fetcher.mockResolvedValueOnce(new Response(null, { status: 204 })); await api.logout();
    await api.me(); expect(new Headers(fetcher.mock.calls.at(-1)![1]?.headers).has('Authorization')).toBe(false);
    fetcher.mockResolvedValueOnce(ok(login)); await api.login('owner', '密码');
    fetcher.mockRejectedValueOnce(new Error('断网'));
    await expect(api.logout()).rejects.toThrow('无法连接');
    await api.me(); expect(new Headers(fetcher.mock.calls.at(-1)![1]?.headers).has('Authorization')).toBe(false);
  });

  it('处理失效会话、业务错误、非 JSON 响应和网络错误，不把失败当成功', async () => {
    const fetcher = vi.fn<typeof fetch>().mockImplementation(async () => ok());
    const api = createServiceClient('/', fetcher);
    fetcher.mockResolvedValueOnce(ok(login)); await api.login('owner', '密码');
    fetcher.mockResolvedValueOnce(new Response(JSON.stringify({ message: '登录已失效' }), { status: 401 }));
    await expect(api.me()).rejects.toMatchObject({ message: '登录已失效', status: 401 });
    await api.me(); expect(new Headers(fetcher.mock.calls.at(-1)![1]?.headers).has('Authorization')).toBe(false);
    fetcher.mockResolvedValueOnce(new Response('失败', { status: 502 }));
    await expect(api.me()).rejects.toThrow('502');
    fetcher.mockResolvedValueOnce(new Response('null', { status: 502 }));
    await expect(api.me()).rejects.toThrow('502');
    fetcher.mockResolvedValueOnce(new Response('不是 JSON', { status: 200 }));
    await expect(api.me()).rejects.toThrow('格式');
    fetcher.mockRejectedValueOnce(new Error('含内部地址的原始异常'));
    await expect(api.me()).rejects.toThrow('无法连接服务');
    fetcher.mockResolvedValueOnce(ok({ token: 'short', user: account }));
    await expect(api.login('x', 'y')).rejects.toThrow('登录信息不完整');
    fetcher.mockResolvedValueOnce(ok({ token: login.token }));
    await expect(api.login('x', 'y')).rejects.toThrow('登录信息不完整');
  });

  it('拒绝带凭据、查询参数或非 HTTP 的服务地址', () => {
    expect(() => createServiceClient('https://', vi.fn())).toThrow('有效网址');
    for (const address of ['file:///tmp/a', 'javascript:alert(1)', 'https://u:p@example.test', 'https://example.test?token=a', 'https://example.test#token']) {
      expect(() => createServiceClient(address, vi.fn())).toThrow('服务地址');
    }
  });
});
