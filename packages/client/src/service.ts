import type { AccountHistory, Command, NewUser, PeoplePage, Receipt, ServiceClient, Session, Snapshot, User, UserChange, WorkspaceInfo } from './types';

export function createServiceClient(address = '/', fetcher: typeof fetch = globalThis.fetch): ServiceClient {
  let url: URL;
  try { url = new URL(address.trim() || '/', globalThis.location.href); } catch { throw new Error('服务地址不是有效网址'); }
  if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password || url.search || url.hash) {
    throw new Error('服务地址必须是 HTTP 或 HTTPS 地址，不能携带账号、密码或查询参数');
  }
  const base = url.href.replace(/\/+$/, '');
  let token: string | null = null;

  async function request<T>(path: string, init: RequestInit = {}, binary = false): Promise<T> {
    const headers = new Headers(init.headers);
    headers.set('Accept', binary ? 'application/octet-stream' : 'application/json');
    if (token) headers.set('Authorization', `Bearer ${token}`);
    let response: Response;
    try {
      response = await fetcher(`${base}${path}`, { ...init, headers, credentials: 'omit', signal: AbortSignal.timeout(30_000) });
    } catch {
      throw new Error('无法连接服务，请检查服务地址和网络后重试');
    }
    if (response.status === 401) token = null;
    if (!response.ok) {
      const body = await response.json().catch(() => null);
      throw Object.assign(new Error(body && typeof body.message === 'string' ? body.message : `服务请求失败（${response.status}）`), { status: response.status });
    }
    if (response.status === 204) return undefined as T;
    if (binary) return await response.blob() as T;
    return await response.json().catch(() => { throw new Error('服务返回的数据格式不正确'); }) as T;
  }
  function json<T>(path: string, method: string, body: unknown): Promise<T> {
    return request<T>(path, { method, headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) });
  }
  const route = (id: string) => `/api/workspaces/${encodeURIComponent(id)}`;

  return {
    async login(account: string, password: string) {
      const session = await json<Session>('/api/auth/login', 'POST', { account, password });
      if (typeof session.token !== 'string' || session.token.length !== 64 || !session.user?.id) throw new Error('服务返回的登录信息不完整');
      token = session.token;
      return session;
    },
    async logout() { try { await json<void>('/api/auth/logout', 'POST', {}); } finally { token = null; } },
    me: () => request<User>('/api/auth/me'),
    async changePassword(current: string, next: string) {
      await json<void>('/api/auth/password', 'POST', { current_password: current, new_password: next });
      token = null;
    },
    listWorkspaces: () => request<WorkspaceInfo[]>('/api/workspaces'),
    createWorkspace: (id: string, name: string) => json<WorkspaceInfo>('/api/workspaces', 'POST', { id, name }),
    snapshot: (id: string) => request<Snapshot>(route(id)),
    command: (workspace: string, id: string, command: Command, expected_revision?: number) => json<Receipt>(`${route(workspace)}/commands`, 'POST', { id, command, expected_revision }),
    async upload(workspace: string, itemId: string, operationId: string, attachmentId: string, file: File) {
      if (file.size > 10 * 1024 * 1024) throw new Error('单个附件不能超过 10 MiB');
      const query = new URLSearchParams({ operation_id: operationId, id: attachmentId, item_id: itemId, name: file.name, mime: file.type || 'application/octet-stream' });
      return request<Receipt>(`${route(workspace)}/attachments?${query}`, { method: 'POST', headers: { 'Content-Type': 'application/octet-stream' }, body: file });
    },
    download: (workspace: string, attachment: string) => request<Blob>(`${route(workspace)}/attachments/${encodeURIComponent(attachment)}`, {}, true),
    users: () => request<User[]>('/api/users'),
    createUser: (input: NewUser) => json<User>('/api/users', 'POST', input),
    updateUser: (id: string, input: UserChange) => json<User>(`/api/users/${encodeURIComponent(id)}`, 'PATCH', input),
    resetPassword: (id: string, password: string) => json<void>(`/api/users/${encodeURIComponent(id)}/password`, 'POST', { password }),
    userHistory: (id: string, after = 0) => request<AccountHistory>(`/api/users/${encodeURIComponent(id)}/history?after=${after}`),
    people: (workspace: string, text = '', after = '') => request<PeoplePage>(`${route(workspace)}/people?${new URLSearchParams(after ? { text, after } : { text })}`),
  };
}
