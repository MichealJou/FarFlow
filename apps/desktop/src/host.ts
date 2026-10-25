import { invoke } from '@tauri-apps/api/core';
import { fetch as nativeFetch } from '@tauri-apps/plugin-http';
import { createServiceClient, type DesktopHost, type OpenedWorkspace, type Receipt, type Snapshot, type WorkspaceClient } from '@farflow/client';

export function createDesktopHost(call:typeof invoke=invoke, fetcher:typeof fetch=nativeFetch):DesktopHost {
  async function ipc<T>(command:string,args?:Parameters<typeof invoke>[1],options?:Parameters<typeof invoke>[2]):Promise<T>{
    try{return await call<T>(command,args,options);}catch(reason){throw new Error(reason instanceof Error?reason.message:typeof reason==='string'?reason:'本机操作失败，请重新打开工作空间');}
  }
  const selectPack:DesktopHost['selectPack']=()=>ipc('desktop_select_pack');
  const selectDirectory:DesktopHost['selectDirectory']=()=>ipc('desktop_select_directory');
  return {
    migration:{
      inspect:directory=>ipc('desktop_inspect_transfer',{directory}),
      stage:input=>ipc('desktop_stage_transfer',{input,at:Math.floor(Date.now()/1000)}),
    },
    selectPack,
    clone:{
      start:(directory,folder,address,job)=>ipc('desktop_clone_start',{directory,folder,address,job}),
      startPack:(directory,folder,file,job)=>ipc('desktop_clone_pack_start',{directory,folder,file,job}),
      status:(directory,job)=>ipc('desktop_clone_status',{directory,job}),
      cancel:(directory,job)=>ipc('desktop_clone_cancel',{directory,job}),
      close:(directory,job)=>ipc('desktop_clone_close',{directory,job}),
      open:(directory,job)=>ipc('desktop_clone_open',{directory,job}),
    },
    selectDirectory,
    create:(directory,folder,mode,name,profile)=>ipc('desktop_create',{directory,folder,mode,name,profile}),
    inspect:(directory,mode)=>ipc('desktop_inspect',{directory,mode}),
    async selectIdentity(opened:OpenedWorkspace,actor:string){
      let snapshot=await ipc<Snapshot>('desktop_select_identity',{handle:opened.handle,actor});
      const timestamps=new Map<string,number>();
      function at(id:string){if(!timestamps.has(id))timestamps.set(id,Math.floor(Date.now()/1000));return timestamps.get(id)!;}
      function workspace(id:string){if(id!==opened.workspace.id)throw new Error('请求的工作空间与当前本机会话不一致');}
      const client:WorkspaceClient={
        transfer:{
          selectDirectory,
          selectFile:()=>ipc('desktop_select_transfer_file'),
          view:()=>ipc('desktop_transfer_view',{handle:opened.handle}),
          apply:action=>ipc('desktop_transfer',{handle:opened.handle,action,at:Math.floor(Date.now()/1000)}),
        },
        async snapshot(id){workspace(id);const next=await ipc<Snapshot>('desktop_snapshot',{handle:opened.handle});if(next.workspace.id!==id)throw new Error('本机返回了不同的工作空间，请重新打开');snapshot=next;return snapshot;},
        async command(id,operation_id,command,expected_revision){workspace(id);return ipc<Receipt>('desktop_execute',{handle:opened.handle,operation_id,command,expected_revision:expected_revision ?? snapshot.state.revision,at:at(operation_id)});},
        async upload(id,item_id,operation_id,attachmentId,file){
          workspace(id);if(file.size>10*1024*1024)throw new Error('单个附件不能超过 10 MiB');
          const metadata={handle:opened.handle,at:at(operation_id),attachment:{operation_id,id:attachmentId,item_id,name:file.name,mime:file.type||'application/octet-stream'}};
          return ipc<Receipt>('desktop_upload',new Uint8Array(await file.arrayBuffer()),{headers:{'x-farflow-upload':encodeURIComponent(JSON.stringify(metadata))}});
        },
        async download(id,attachment){workspace(id);const bytes=await ipc<ArrayBuffer>('desktop_attachment',{handle:opened.handle,id:attachment});return new Blob([bytes],{type:snapshot.state.attachments[attachment]?.mime||'application/octet-stream'});},
      };
      if(opened.workspace.mode==='git')client.git={
        selectPack,
        exportPack:job=>ipc('desktop_pack_export',{handle:opened.handle,job}),
        importPack:(file,job)=>ipc('desktop_pack_import',{handle:opened.handle,file,job}),
        async start(action,address){const job=crypto.randomUUID();await ipc('desktop_git_start',{handle:opened.handle,job,action,address});return job;},
        status:job=>ipc('desktop_git_status',{handle:opened.handle,job}),
        cancel:job=>ipc('desktop_git_cancel',{handle:opened.handle,job}),
        review:()=>ipc('desktop_git_review',{handle:opened.handle}),
        resolve:input=>ipc('desktop_git_resolve',{handle:opened.handle,input}),
        rebuild:()=>ipc('desktop_git_rebuild',{handle:opened.handle}),
      };
      return {opened,snapshot,client};
    },
    close:handle=>ipc('desktop_close',{handle}),
    service:address=>createServiceClient(address,fetcher),
  };
}
