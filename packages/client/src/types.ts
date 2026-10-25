export type Mode = 'local' | 'git' | 'service';
export type Role = 'owner' | 'product' | 'manager' | 'developer' | 'tester' | 'viewer';
export type Kind = 'product' | 'module' | 'project' | 'iteration' | 'inbox' | 'requirement' | 'plan' | 'task' | 'test_case' | 'test_plan' | 'test_run' | 'build' | 'bug' | 'release' | 'document';
export interface Member { id: string; account: string; name: string; department: string; roles: Role[]; active: boolean }
export interface User { id: string; account: string; name: string; department: string; active: boolean; is_admin: boolean; must_change_password: boolean; version: number }
export interface Session { token: string; expires_at: number; user: User }
export interface WorkspaceInfo { schema: number; id: string; name: string; mode: Mode }
export interface Link { target: string; relation: 'related' | 'source' | 'implements' | 'verifies' | 'contains' | 'known_issue' | 'depends_on'; content_version: number | null }
export interface Item {
  id: string; kind: Kind; title: string; description: string; acceptance: string; status: string;
  priority: number; severity: number; assigned_to: string | null; product_id: string | null;
  project_id: string | null; iteration_id: string | null; parent_id: string | null; links: Link[];
  attributes: Record<string, unknown>; version: number; content_version: number;
  created_by: string; created_at: number; updated_at: number; deleted: boolean;
}
export interface Comment { id: string; item_id: string; actor_id: string; body: string; parent_id: string | null; mentions: string[]; attachments: string[]; created_at: number; updated_at: number; version: number; deleted: boolean }
export interface Attachment { id: string; item_id: string; name: string; mime: string; sha256: string; size: number; actor_id: string; at: number; deleted: boolean }
export interface Audit { operation_id: string; actor_id: string; at: number; action: string; object_id: string; before: unknown; after: unknown; note: string }
export interface ContentVersion { item_id: string; version: number; title: string; description: string; acceptance: string; attributes: Record<string, unknown>; actor_id: string; at: number }
export interface DomainState {
  schema: number; id: string; name: string; revision: number; members: Record<string, Member>;
  items: Record<string, Item>; comments: Record<string, Comment>; attachments: Record<string, Attachment>;
  versions: ContentVersion[]; history: Audit[]; operations: Record<string, unknown>;
}
export interface TransferStatus { role: 'source' | 'target'; phase: 'frozen' | 'completed' | 'cancelled' | 'staged' | 'active'; plan: { id: string; source_mode: Mode; target_mode: Mode; prepared_by: string; prepared_at: number }; changed_by: string; changed_at: number }
export interface Snapshot { workspace: WorkspaceInfo; state: DomainState; actor_id: string; accounts: Array<{ id: string; account_id: string; account: string; name: string; department: string; active: boolean }>; transfer: TransferStatus | null; issues?: Array<{operation_id:string;kind:string;message:string;peers:string[]}> }
export interface Receipt { operation_id: string; revision: number; affected: string[]; warning?: string | null }
export interface Command { type: string; [key: string]: unknown }
export interface NewUser { account: string; name: string; department: string; password: string; is_admin: boolean }
export interface UserChange { version: number; name: string; department: string; active: boolean; is_admin: boolean }
export interface PeoplePage { accounts: Array<{ id: string; member_id: string | null; account: string; name: string; department: string; active: boolean }>; next: string | null }
export interface AccountHistory { events: Array<{ sequence: number; actor_id: string; action: string; at: number; data: unknown }>; next: number | null }

export interface WorkspaceClient {
  git?: GitClient;
  transfer?: TransferClient;
  snapshot(id: string): Promise<Snapshot>;
  command(workspace: string, operationId: string, command: Command, expectedRevision?: number): Promise<Receipt>;
  upload(workspace: string, itemId: string, operationId: string, attachmentId: string, file: File): Promise<Receipt>;
  download(workspace: string, attachmentId: string): Promise<Blob>;
}
export type GitRemoteAction = 'pull' | 'push' | 'sync';
export interface GitRemoteResult { push: {commit:string;pushed:boolean;failure:'non_fast_forward'|'authentication'|'transport'|'rejected'|null;warning:string|null} | null; pending_local:boolean|null }
export type GitJobStatus = {phase:'running'} | {phase:'completed';result:GitRemoteResult} | {phase:'failed';message:string} | {phase:'packed';result:GitPackResult};
export interface GitPackResult { action:'import'|'export';path:string;bytes:number|null;issues:number|null }
export interface PackFile { token:string;path:string }
export interface GitClient {
  selectPack():Promise<PackFile|null>;
  exportPack(job:string):Promise<boolean>;
  importPack(file:string,job:string):Promise<void>;
  start(action:GitRemoteAction,address:string):Promise<string>;
  status(job:string):Promise<GitJobStatus>;
  cancel(job:string):Promise<void>;
  review():Promise<GitReview>;
  resolve(input:GitResolutionInput):Promise<Receipt>;
  rebuild():Promise<Snapshot>;
}
export interface GitResolutionInput { id:string;token:string;targets:string[];command:Command|null;note:string;at:number }
export interface GitOperation { id:string;device_id:string;parents:string[];change:{type:'apply';request:{actor_id:string;at:number;command:Command}}|{type:'resolve';resolution:{actor_id:string;at:number;command:Command|null;targets:string[];note:string}} }
export interface GitDecisionReview { operation:GitOperation & {change:Extract<GitOperation['change'],{type:'resolve'}>};applied:boolean;issue:NonNullable<Snapshot['issues']>[number]|null;targets:Record<string,GitOperation> }
export interface GitReview { token:string;state:DomainState;issues:Record<string,NonNullable<Snapshot['issues']>[number]>;proposals:Record<string,GitOperation>;decisions:GitDecisionReview[] }
export interface ServiceClient extends WorkspaceClient {
  listWorkspaces(): Promise<WorkspaceInfo[]>;
  createWorkspace(id: string, name: string): Promise<WorkspaceInfo>;
  login(account: string, password: string): Promise<Session>;
  logout(): Promise<void>;
  me(): Promise<User>;
  changePassword(current: string, next: string): Promise<void>;
  users(): Promise<User[]>;
  createUser(input: NewUser): Promise<User>;
  updateUser(id: string, input: UserChange): Promise<User>;
  resetPassword(id: string, password: string): Promise<void>;
  userHistory(id: string, after?: number): Promise<AccountHistory>;
  people(workspace: string, text?: string, after?: string): Promise<PeoplePage>;
}

export interface DirectoryGrant { token:string;path:string }
export type TransferAction = {type:'prepare';id:string;target:Mode;directory:string;folder:string}|{type:'export';directory:string;folder:string}|{type:'complete';receipt:string;directory:string;file:string}|{type:'activate';activation:string}|{type:'cancel';id:string}|{type:'acknowledge';id:string;note:string;all_copies_stopped:boolean};
export interface TransferPlanView {status:TransferStatus;origin_device:string;required_members:string[];confirmations:Record<string,string[]>;late_operations:boolean}
export interface MigrationReport {package_id:string;source:WorkspaceInfo;target:WorkspaceInfo|null;revision:number;operation_count:number;item_count:number;comment_count:number;attachment_count:number;blob_count:number;members:Member[];identity_ids:string[];transfer?:TransferStatus['plan']}
export interface TransferResult {status:TransferStatus|null;events:TransferStatus[];plans:TransferPlanView[];report:MigrationReport|null;path:string|null}
export interface TransferStage {source:string;directory:string;folder:string;actor:string;receipt_directory:string;receipt_file:string}
export interface TransferClient {
  selectDirectory():Promise<DirectoryGrant|null>;
  selectFile():Promise<PackFile|null>;
  view():Promise<TransferResult>;
  apply(action:TransferAction):Promise<TransferResult>;
}
export interface OpenedWorkspace { handle:string;workspace:WorkspaceInfo;path:string;identities:Member[] }
export interface DesktopSession { opened:OpenedWorkspace;client:WorkspaceClient;snapshot:Snapshot }
export interface DesktopHost {
  migration: {
    inspect(directory:string):Promise<MigrationReport>;
    stage(input:TransferStage):Promise<{opened:OpenedWorkspace;report:MigrationReport;receipt_path:string}>;
  };
  clone: {
    start(directory:string,folder:string,address:string,job:string):Promise<void>;
    startPack(directory:string,folder:string,file:string,job:string):Promise<void>;
    status(directory:string,job:string):Promise<{phase:'running'}|{phase:'cloned'}|{phase:'failed';message:string}>;
    cancel(directory:string,job:string):Promise<void>;
    close(directory:string,job:string):Promise<void>;
    open(directory:string,job:string):Promise<OpenedWorkspace>;
  };
  selectDirectory():Promise<DirectoryGrant | null>;
  selectPack():Promise<PackFile|null>;
  create(directory:string,folder:string,mode:'local'|'git',name:string,profile:Pick<Member,'account'|'name'|'department'>):Promise<OpenedWorkspace>;
  inspect(directory:string,mode:'local'|'git'):Promise<OpenedWorkspace>;
  selectIdentity(opened:OpenedWorkspace,actor:string):Promise<DesktopSession>;
  close(handle:string):Promise<void>;
  service(address:string):ServiceClient;
}
