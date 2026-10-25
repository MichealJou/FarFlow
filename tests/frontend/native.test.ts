import { describe, expect, it, vi } from 'vitest';
import { invoke } from '@tauri-apps/api/core';
import { fetch as nativeFetch } from '@tauri-apps/plugin-http';
import { createDesktopHost } from '../../apps/desktop/src/host';
import { snapshot } from './fixtures';

vi.mock('@tauri-apps/api/core',()=>({invoke:vi.fn()}));
vi.mock('@tauri-apps/plugin-http',()=>({fetch:vi.fn()}));

describe('桌面 IPC 客户端',()=>{
  it('Git 会话提供开始、读取状态和取消入口，本地会话没有 Git 操作',async()=>{
    const data=snapshot();data.workspace.mode='git';const call=vi.fn().mockResolvedValue(data);const host=createDesktopHost(call);
    const opened={handle:'git-handle',workspace:data.workspace,path:'/本机仓库',identities:Object.values(data.state.members)};
    const session=await host.selectIdentity(opened,'owner');call.mockResolvedValue(undefined);
    const job=await session.client.git!.start('sync','/本机远端.git');expect(call).toHaveBeenLastCalledWith('desktop_git_start',{handle:'git-handle',job,action:'sync',address:'/本机远端.git'},undefined);
    call.mockResolvedValue({phase:'running'});expect(await session.client.git!.status(job)).toEqual({phase:'running'});expect(call).toHaveBeenLastCalledWith('desktop_git_status',{handle:'git-handle',job},undefined);
    await session.client.git!.cancel(job);expect(call).toHaveBeenLastCalledWith('desktop_git_cancel',{handle:'git-handle',job},undefined);
    await session.client.git!.review();expect(call).toHaveBeenLastCalledWith('desktop_git_review',{handle:'git-handle'},undefined);
    const input={id:'decision',token:'review-token',targets:['left','right'],command:null,note:'核对后保留原文',at:100};await session.client.git!.resolve(input);expect(call).toHaveBeenLastCalledWith('desktop_git_resolve',{handle:'git-handle',input},undefined);
    await session.client.git!.rebuild();expect(call).toHaveBeenLastCalledWith('desktop_git_rebuild',{handle:'git-handle'},undefined);
    await session.client.git!.selectPack();expect(call).toHaveBeenLastCalledWith('desktop_select_pack',undefined,undefined);await session.client.git!.exportPack('export-job');expect(call).toHaveBeenLastCalledWith('desktop_pack_export',{handle:'git-handle',job:'export-job'},undefined);await session.client.git!.importPack('file','import-job');expect(call).toHaveBeenLastCalledWith('desktop_pack_import',{handle:'git-handle',file:'file',job:'import-job'},undefined);
    data.workspace.mode='local';call.mockResolvedValue(data);expect((await host.selectIdentity(opened,'owner')).client.git).toBeUndefined();
  });
  it('目录授权、创建、检查和身份选择使用独立的原生入口',async()=>{
    const call=vi.fn().mockResolvedValue(null),host=createDesktopHost(call);await host.selectDirectory();expect(call).toHaveBeenLastCalledWith('desktop_select_directory',undefined,undefined);
    const profile={account:'owner',name:'本机成员',department:'研发'};await host.create('directory','数据','local','研发',profile);expect(call).toHaveBeenLastCalledWith('desktop_create',{directory:'directory',folder:'数据',mode:'local',name:'研发',profile},undefined);await host.inspect('directory','git');expect(call).toHaveBeenLastCalledWith('desktop_inspect',{directory:'directory',mode:'git'},undefined);await host.close('handle');expect(call).toHaveBeenLastCalledWith('desktop_close',{handle:'handle'},undefined);
    await host.clone.start('directory','副本','/远端.git','job');expect(call).toHaveBeenLastCalledWith('desktop_clone_start',{directory:'directory',folder:'副本',address:'/远端.git',job:'job'},undefined);
    await host.selectPack();expect(call).toHaveBeenLastCalledWith('desktop_select_pack',undefined,undefined);await host.clone.startPack('directory','副本','file','job');expect(call).toHaveBeenLastCalledWith('desktop_clone_pack_start',{directory:'directory',folder:'副本',file:'file',job:'job'},undefined);
    for(const [method,command] of [['status','desktop_clone_status'],['cancel','desktop_clone_cancel'],['close','desktop_clone_close'],['open','desktop_clone_open']] as const){await host.clone[method]('directory','job');expect(call).toHaveBeenLastCalledWith(command,{directory:'directory',job:'job'},undefined);}
    vi.mocked(invoke).mockResolvedValueOnce(null);await createDesktopHost().selectDirectory();expect(invoke).toHaveBeenCalled();
  });
  it('本地业务命令固定会话和修订，重复操作保留原时间，附件走原始二进制',async()=>{
    const data=snapshot();data.workspace.mode='local';data.state.attachments.file={id:'file',item_id:'requirement',name:'附件.txt',mime:'text/plain',sha256:'a'.repeat(64),size:4,actor_id:'owner',at:1,deleted:false};
    const call=vi.fn().mockResolvedValue(data);const host=createDesktopHost(call);const opened={handle:'handle',workspace:data.workspace,path:'/本机目录',identities:Object.values(data.state.members)};const session=await host.selectIdentity(opened,'owner');expect(call).toHaveBeenLastCalledWith('desktop_select_identity',{handle:'handle',actor:'owner'},undefined);
    expect(await session.client.snapshot('space')).toEqual(data);call.mockResolvedValue({operation_id:'operation',revision:3,affected:[],warning:'索引待刷新'});
    const clock=vi.spyOn(Date,'now').mockReturnValue(100000);const saved=await session.client.command('space','operation',{type:'archive'});expect(saved.warning).toBe('索引待刷新');expect(call.mock.calls.at(-1)![1]).toMatchObject({handle:'handle',operation_id:'operation',expected_revision:2,at:100});clock.mockReturnValue(200000);await session.client.command('space','operation',{type:'archive'},2);expect(call.mock.calls.at(-1)![1].at).toBe(100);clock.mockRestore();
    const bytes=new TextEncoder().encode('原始字节');const file=Object.assign(new File(['原始字节'],'验收 & 证据.txt',{type:'text/plain'}),{arrayBuffer:async()=>bytes.buffer});await session.client.upload('space','requirement','upload','file',file);
    const [command,body,options]=call.mock.calls.at(-1)!;expect(command).toBe('desktop_upload');expect([...body]).toEqual([...bytes]);const meta=JSON.parse(decodeURIComponent(options.headers['x-farflow-upload']));expect(meta).toMatchObject({handle:'handle',attachment:{operation_id:'upload',id:'file',item_id:'requirement',name:'验收 & 证据.txt',mime:'text/plain'}});
    const emptyType=Object.assign(new File(['字节'],'无类型'),{arrayBuffer:async()=>bytes.buffer});await session.client.upload('space','requirement','upload2','file2',emptyType);expect(JSON.parse(decodeURIComponent(call.mock.calls.at(-1)![2].headers['x-farflow-upload'])).attachment.mime).toBe('application/octet-stream');
    call.mockResolvedValue(new Uint8Array([1,2,3,4]).buffer);const blob=await session.client.download('space','file');expect(blob.size).toBe(4);expect(blob.type).toBe('text/plain');expect((await session.client.download('space','unknown')).type).toBe('application/octet-stream');
    const large=new File([new Uint8Array(10*1024*1024+1)],'too-large');await expect(session.client.upload('space','item','large','file',large)).rejects.toThrow('10 MiB');
    for(const run of [()=>session.client.snapshot('wrong'),()=>session.client.command('wrong','op',{type:'create'}),()=>session.client.upload('wrong','item','op','file',file),()=>session.client.download('wrong','file')])await expect(run()).rejects.toThrow('不一致');
    call.mockResolvedValue({...data,workspace:{...data.workspace,id:'other'}});await expect(session.client.snapshot('space')).rejects.toThrow('不同的工作空间');
  });
  it('正式迁移只传目录及文件授权标识，时间与会话固定在原生入口',async()=>{
    const data=snapshot();data.workspace.mode='local';const call=vi.fn().mockResolvedValue(data),host=createDesktopHost(call);const opened={handle:'source',workspace:data.workspace,path:'/空间',identities:Object.values(data.state.members)};
    const session=await host.selectIdentity(opened,'owner');const api=session.client.transfer!;
    await api.view();expect(call).toHaveBeenLastCalledWith('desktop_transfer_view',{handle:'source'},undefined);
    await api.selectDirectory();expect(call).toHaveBeenLastCalledWith('desktop_select_directory',undefined,undefined);
    await api.selectFile();expect(call).toHaveBeenLastCalledWith('desktop_select_transfer_file',undefined,undefined);
    const action={type:'prepare' as const,id:'transfer',target:'git' as const,directory:'directory',folder:'备份'};await api.apply(action);expect(call).toHaveBeenLastCalledWith('desktop_transfer',{handle:'source',action,at:expect.any(Number)},undefined);
    await host.migration.inspect('source-directory');expect(call).toHaveBeenLastCalledWith('desktop_inspect_transfer',{directory:'source-directory'},undefined);
    const input={source:'archive',directory:'destination',folder:'接续',actor:'actual-owner',receipt_directory:'receipts',receipt_file:'回执.json'};await host.migration.stage(input);expect(call).toHaveBeenLastCalledWith('desktop_stage_transfer',{input,at:expect.any(Number)},undefined);
  });
  it('原生错误统一为可显示的 Error，服务模式复用同一 HTTP 协议',async()=>{
    const call=vi.fn().mockRejectedValue('目录未授权'),host=createDesktopHost(call);await expect(host.selectDirectory()).rejects.toThrow('目录未授权');call.mockRejectedValueOnce(new Error('本机目录不存在'));await expect(host.selectDirectory()).rejects.toThrow('不存在');call.mockRejectedValueOnce({code:1});await expect(host.selectDirectory()).rejects.toThrow('本机操作失败');
    const fetcher=vi.fn<typeof fetch>().mockResolvedValue(new Response(JSON.stringify({token:'a'.repeat(64),expires_at:999999999,user:{id:'owner'}})));const api=createDesktopHost(call,fetcher).service('https://farflow.example');await api.login('owner','password');expect(fetcher.mock.calls[0][0]).toBe('https://farflow.example/api/auth/login');expect(JSON.parse(fetcher.mock.calls[0][1]!.body as string)).toEqual({account:'owner',password:'password'});
    vi.mocked(nativeFetch).mockResolvedValueOnce(new Response(JSON.stringify([])));expect(await createDesktopHost(call).service('https://farflow.example').listWorkspaces()).toEqual([]);
  });
});
