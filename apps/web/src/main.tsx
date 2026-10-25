import React from 'react';
import { createRoot } from 'react-dom/client';
import { App } from '@farflow/ui';
import { isTauri } from '@tauri-apps/api/core';
import '../../../packages/ui/src/styles.css';

const root=createRoot(document.getElementById('root')!);
if(isTauri()){
  import('@farflow/desktop').then(({createDesktopHost})=>root.render(<React.StrictMode><App desktop={createDesktopHost()}/></React.StrictMode>)).catch(()=>root.render(<main className="setup-card"><h1>桌面组件加载失败</h1><p>请重新打开 FarFlow，或检查应用安装是否完整。</p></main>));
}else root.render(<React.StrictMode><App /></React.StrictMode>);
