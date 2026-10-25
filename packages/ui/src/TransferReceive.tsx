import { useState, type FormEvent } from 'react';
import type { DesktopHost, DirectoryGrant, MigrationReport, OpenedWorkspace } from '@farflow/client';
import { modeNames } from './catalog';

export function TransferReceive({host,onOpened,onBack}:{host:DesktopHost;onOpened:(opened:OpenedWorkspace)=>void;onBack:()=>void}) {
  const [source,setSource]=useState<DirectoryGrant|null>(null);const [destination,setDestination]=useState<DirectoryGrant|null>(null);const [receipts,setReceipts]=useState<DirectoryGrant|null>(null);
  const [report,setReport]=useState<MigrationReport|null>(null);const [actor,setActor]=useState('');const [folder,setFolder]=useState('FarFlow-接续');const [receiptName,setReceiptName]=useState('FarFlow-目标回执.json');
  const [opened,setOpened]=useState<OpenedWorkspace|null>(null);
  const [busy,setBusy]=useState(false);const [error,setError]=useState('');const [notice,setNotice]=useState('');const [confirmed,setConfirmed]=useState(false);
  async function choose(kind:'source'|'destination'|'receipt') {
    setBusy(true);setError('');
    try {const directory=await host.selectDirectory();if(!directory)return;if(kind==='source'){setReport(null);setActor('');setConfirmed(false);setSource(directory);setReport(await host.migration.inspect(directory.token));}else if(kind==='destination')setDestination(directory);else setReceipts(directory);}
    catch(reason){setError((reason as Error).message);}finally{setBusy(false);}
  }
  async function stage(event:FormEvent) {
    event.preventDefault();if(!source||!report||!destination||!receipts||!actor||!confirmed){setError('请核对交换包、选择原所有者和两个保存位置，并明确确认');return;}
    setBusy(true);setError('');setNotice('');
    try {const result=await host.migration.stage({source:source.token,directory:destination.token,folder,actor,receipt_directory:receipts.token,receipt_file:receiptName});setNotice(`目标已暂存，仍为只读。请把回执交回源端：${result.receipt_path}`);setOpened(result.opened);}
    catch(reason){setError((reason as Error).message);}finally{setBusy(false);}
  }
  const service=report?.transfer?.target_mode==='service';
  if(opened)return <section className="transfer-receive"><h2>目标已暂存</h2><p className="callout">目标继续只读。请把目标回执交回源端，取得源完成凭据后，在空间迁移页激活。</p><p role="status" className="notice pack-path">{notice}</p><button className="primary" onClick={()=>onOpened(opened)}>打开暂存空间</button></section>;
  return <section className="transfer-receive"><h2>接收正式迁移</h2><p className="hint">保留原空间 ID 和全部历史。此处只暂存目标，不会自动开放写入；回执交回源端后，用源完成凭据激活。</p><button disabled={busy} onClick={()=>choose('source')}>选择正式交换包目录</button>{source&&<p className="pack-path">{source.path}</p>}
    {report&&<><div className="callout"><strong>{report.source.name}</strong><p>{modeNames[report.source.mode]} → {modeNames[report.transfer!.target_mode]}</p><p>工作项 {report.item_count} · 评论 {report.comment_count} · 附件 {report.attachment_count}</p><p className="mono pack-path">计划 {report.transfer!.id}</p></div>{service?<p className="error" role="alert">此交换包的目标是团队服务，请交给对应服务的迁移管理员接收。</p>:<form className="form-stack" onSubmit={stage}><label>原所有者身份<select value={actor} onChange={event=>setActor(event.target.value)} disabled={busy}><option value="">请选择实际操作者</option>{report.members.filter(member=>member.active&&member.roles.includes('owner')).map(member=><option key={member.id} value={member.id}>{member.name} · {member.account}</option>)}</select></label><label>目标父目录<div className="directory-field"><input aria-label="目标父目录" readOnly value={destination?.path||''}/><button type="button" disabled={busy} onClick={()=>choose('destination')}>选择目标父目录</button></div></label><label>目标文件夹名称<input value={folder} onChange={event=>setFolder(event.target.value)} required maxLength={200} disabled={busy}/></label><label>回执保存目录<div className="directory-field"><input aria-label="回执保存目录" readOnly value={receipts?.path||''}/><button type="button" disabled={busy} onClick={()=>choose('receipt')}>选择回执目录</button></div></label><label>目标回执文件名<input value={receiptName} onChange={event=>setReceiptName(event.target.value)} required maxLength={200} disabled={busy}/></label><label className="check-row"><input type="checkbox" checked={confirmed} onChange={event=>setConfirmed(event.target.checked)} disabled={busy}/>已核对原空间、实际所有者和迁移方向，确认只读暂存</label><button className="primary" disabled={busy}>校验并暂存目标</button></form>}</>}
    {error&&<p role="alert" className="error">{error}</p>}{notice&&<p role="status" className="notice pack-path">{notice}</p>}<button type="button" disabled={busy} onClick={onBack}>返回初始化</button>
  </section>;
}
