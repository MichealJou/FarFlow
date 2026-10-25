import * as DialogPrimitive from '@radix-ui/react-dialog';
import { IconX } from '@tabler/icons-react';
import type { ReactNode } from 'react';

export function Dialog({ title, description, children, onClose }: { title: string; description: string; children: ReactNode; onClose: () => void }) {
  return <DialogPrimitive.Root open onOpenChange={onClose}><DialogPrimitive.Portal><DialogPrimitive.Overlay className="dialog-shade" /><DialogPrimitive.Content className="dialog-card"><header><div><DialogPrimitive.Title>{title}</DialogPrimitive.Title><DialogPrimitive.Description>{description}</DialogPrimitive.Description></div><DialogPrimitive.Close className="icon-button" aria-label="关闭对话框"><IconX size={19}/></DialogPrimitive.Close></header>{children}</DialogPrimitive.Content></DialogPrimitive.Portal></DialogPrimitive.Root>;
}
