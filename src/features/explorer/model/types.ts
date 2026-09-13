/** Wire values are observations, never paths or filesystem capabilities. */
export type Bytes = `${bigint}`;
export type Category = 'directories' | 'files';
export type Phase = 'queued' | 'running' | 'completed' | 'failed' | 'cancelled';
export type Reason =
  | 'scanning'
  | 'readError'
  | 'cancelled'
  | 'unsupported'
  | 'changedDuringScan'
  | 'overflow'
  | 'resourceLimit'
  | 'excludedByPolicy';
export type Field<T> =
  | { state: 'known'; value: T }
  | { state: 'unknown' | 'unsupported' }
  | { state: 'error'; issue: FsIssue };
export type Measure =
  | { state: 'unknown' }
  | { state: 'complete'; bytes: Bytes }
  | { state: 'partial'; observedBytes: Bytes; reasons: Reason[] }
  | { state: 'unavailable'; reason: Reason };
export interface FsIssue {
  code: string;
  operation: string;
  scope: 'root' | 'entry' | 'subtree';
  entryId?: string;
  nativeCode?: number;
}
export interface Issues {
  counts: Record<string, Bytes>;
  samples: FsIssue[];
}
export interface Scope {
  sessionId: string;
  generation: number;
}
export interface RootSession extends Scope {
  rootEntryId: string;
  platform: 'macos' | 'linux' | 'fixture';
  capacity: {
    background: 'available' | 'limited';
    foreground: 'available' | 'limited';
  };
}
export interface Entry {
  entryId: string;
  parentId: string | null;
  displayName: string;
  kind: 'directory' | 'regularFile' | 'symlink' | 'other' | 'unknown';
  hidden: Field<boolean>;
  specialType: Field<
    'none' | 'macAlias' | 'macPackage' | 'linuxDesktopEntry' | 'other'
  >;
  modifiedAt: Field<string>;
  ownLogicalBytes: Field<Bytes>;
  ownAllocatedBytes: Field<Bytes>;
  observedAt: string;
  followPolicy: 'never';
}
export interface WorkRecord extends Scope {
  taskId: string;
  operation: 'listDirectories' | 'listFiles' | 'scanUsage';
  targetId: string;
  phase: Phase;
  waitReason?: 'draining' | 'slot' | 'pageDemand';
  sequence: number;
  processedEntries: Bytes;
  processedDirectories: Bytes;
  observedAt: string;
  issues: Issues;
}
export interface DirectoryUsage {
  directoryId: string;
  usageRevision: number;
  scanState:
    'notRequested' | 'queued' | 'running' | 'settled' | 'failed' | 'cancelled';
  logical: Measure;
  allocated: Measure;
  observedAt: string;
}
export interface ListingStart {
  work: WorkRecord;
  listingRevision: string;
  directoryId: string;
  category: Category;
}
export interface ListingPage extends ListingStart {
  entries: Entry[];
  /** Echoed requested cursor; null means the first page. */
  cursor: string | null;
  nextCursor: string | null;
  coverage: 'complete' | 'partial';
  issues: Issues;
}
export interface DirectoryListing {
  directoryId: string;
  category: Category;
  loadState: 'notRequested' | 'loading' | 'settled' | 'failed' | 'cancelled';
  taskId?: string;
  snapshot?: {
    listingRevision: string;
    entryIds: string[];
    nextCursor: string | null;
    coverage: 'complete' | 'partial';
    observedAt: string;
    issues: Issues;
  };
}
export interface WorkEvent extends Scope {
  protocolVersion: 1;
  taskId: string;
  sequence: number;
  kind: 'listingAvailable' | 'usageChanged' | 'progress' | 'terminal';
  observedAt: string;
}
export interface Request {
  protocolVersion: 1;
  requestId: string;
}
export type ScopedRequest = Request & Scope;
export type Response<T> =
  | { requestId: string; outcome: 'ok'; data: T }
  | { requestId: string; outcome: 'cancelled' }
  | { requestId: string; outcome: 'error'; error: FsIssue };
export interface RootData {
  session: RootSession;
  root: Entry;
}
export interface FilesystemAdapter {
  connectClient(input: {
    protocolVersion: 1;
    clientNonce: string;
  }): Promise<{ epoch: string }>;
  chooseRoot(input: Request): Promise<Response<RootData>>;
  startListing(
    input: ScopedRequest & { directoryId: string; category: Category },
  ): Promise<Response<ListingStart>>;
  readListingPage(
    input: ScopedRequest & {
      taskId: string;
      cursor: string | null;
      limit: number;
    },
  ): Promise<Response<ListingPage>>;
  startUsageScan(input: ScopedRequest): Promise<Response<WorkRecord>>;
  readUsage(
    input: ScopedRequest & { directoryIds: string[] },
  ): Promise<Response<DirectoryUsage[]>>;
  readTask(
    input: ScopedRequest & { taskId: string },
  ): Promise<Response<WorkRecord>>;
  cancelTask(
    input: ScopedRequest & { taskId: string },
  ): Promise<
    Response<{ status: 'accepted' | 'alreadyTerminal'; work: WorkRecord }>
  >;
  refreshRoot(
    input: Request & { sessionId: string; expectedGeneration: number },
  ): Promise<Response<RootData>>;
  closeSession(
    input: Request & { sessionId: string },
  ): Promise<Response<{ status: 'closed' | 'alreadyClosed' }>>;
  subscribe(listener: (event: WorkEvent) => void): Promise<() => void>;
}
export const terminal = (phase: Phase) =>
  phase === 'completed' || phase === 'failed' || phase === 'cancelled';
export const sameScope = (a: Scope | null, b: Scope) =>
  a?.sessionId === b.sessionId && a.generation === b.generation;
export const listingKey = (id: string, category: Category) =>
  JSON.stringify([id, category]);
export function parseBytes(value: string): bigint {
  if (!/^(0|[1-9][0-9]*)$/.test(value)) throw new Error('INVALID_BYTES');
  return BigInt(value);
}
