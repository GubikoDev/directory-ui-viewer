import {
  terminal,
  type Category,
  type DirectoryUsage,
  type Entry,
  type FilesystemAdapter,
  type ListingPage,
  type ListingStart,
  type Request,
  type Response,
  type RootData,
  type ScopedRequest,
  type WorkEvent,
  type WorkRecord,
} from '../model/types';

const stamp = '2026-09-13T00:00:00.000Z';
const issues = () => ({ counts: {}, samples: [] });
export function fixtureEntry(
  entryId: string,
  parentId: string | null,
  kind: Entry['kind'] = 'directory',
): Entry {
  return {
    entryId,
    parentId,
    displayName: entryId,
    kind,
    hidden: { state: 'known', value: false },
    specialType: { state: 'unsupported' },
    modifiedAt: { state: 'known', value: stamp },
    ownLogicalBytes: { state: 'known', value: '0' },
    ownAllocatedBytes: { state: 'unknown' },
    observedAt: stamp,
    followPolicy: 'never',
  };
}
export interface FixtureTree {
  entries: Entry[];
  usages?: DirectoryUsage[];
  deniedIds?: string[];
}
/** Deterministic in-memory test double. Does not emulate or claim native security. */
export class FixtureAdapter implements FilesystemAdapter {
  private epoch = 'fixture-epoch';
  private nonce?: string;
  private root: RootData | null = null;
  private sessionNumber = 0;
  private taskNumber = 0;
  private listeners = new Set<(event: WorkEvent) => void>();
  private tasks = new Map<string, WorkRecord>();
  private listings = new Map<string, ListingStart>();
  private pages = new Map<string, ListingPage>();
  private pageLimits = new Map<string, number>();
  private records = new Map<string, Response<unknown>>();
  private highWater = 0;
  readonly calls: Record<string, number> = {};
  cancelNextChoice = false;
  dropEvents = false;
  deferTasks = false;
  constructor(
    readonly tree: FixtureTree = {
      entries: [
        fixtureEntry('A', null),
        fixtureEntry('B', 'A'),
        fixtureEntry('C', 'B'),
        fixtureEntry('file', 'B', 'regularFile'),
      ],
    },
  ) {}
  get subscriberCount() {
    return this.listeners.size;
  }
  async connectClient(input: { protocolVersion: 1; clientNonce: string }) {
    if (input.protocolVersion !== 1) throw new Error('VERSION_UNSUPPORTED');
    if (this.nonce && this.nonce !== input.clientNonce)
      throw new Error('CLIENT_EXPIRED');
    this.nonce = input.clientNonce;
    return { epoch: this.epoch };
  }
  async subscribe(listener: (event: WorkEvent) => void) {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  }
  private ok<T>(input: Request, data: T): Response<T> {
    return { requestId: input.requestId, outcome: 'ok', data };
  }
  private error<T>(input: Request, code: string): Response<T> {
    return {
      requestId: input.requestId,
      outcome: 'error',
      error: { code, operation: 'fixture', scope: 'root' },
    };
  }
  private invoke<T>(
    name: string,
    input: Request,
    operation: () => Response<T>,
  ): Response<T> {
    this.calls[name] = (this.calls[name] ?? 0) + 1;
    if (input.protocolVersion !== 1)
      return this.error(input, 'VERSION_UNSUPPORTED');
    const [epoch, number] = input.requestId.split(':');
    const sequence = Number(number);
    if (!this.nonce || epoch !== this.epoch)
      return this.error(input, 'CLIENT_EXPIRED');
    if (!Number.isSafeInteger(sequence) || sequence < 1)
      return this.error(input, 'INVALID_ARGUMENT');
    const previous = this.records.get(input.requestId);
    if (previous) return previous as Response<T>;
    if (sequence <= this.highWater) return this.error(input, 'REQUEST_EXPIRED');
    this.highWater = sequence;
    const response = operation();
    this.records.set(input.requestId, response);
    if (this.records.size > 256)
      this.records.delete(this.records.keys().next().value!);
    return response;
  }
  private scoped<T>(
    name: string,
    input: ScopedRequest,
    operation: () => Response<T>,
  ) {
    return this.invoke(name, input, () => {
      if (!this.root || this.root.session.sessionId !== input.sessionId)
        return this.error<T>(input, 'SESSION_CLOSED');
      if (this.root.session.generation !== input.generation)
        return this.error<T>(input, 'STALE_GENERATION');
      return operation();
    });
  }
  private newRoot(generation = 1): RootData {
    const root = this.tree.entries.find((entry) => entry.parentId === null)!;
    return {
      root,
      session: {
        sessionId: `session-${this.sessionNumber}`,
        generation,
        rootEntryId: root.entryId,
        platform: 'fixture',
        capacity: { background: 'available', foreground: 'available' },
      },
    };
  }
  async chooseRoot(input: Request) {
    return this.invoke<RootData>('chooseRoot', input, () => {
      if (this.cancelNextChoice) {
        this.cancelNextChoice = false;
        return { requestId: input.requestId, outcome: 'cancelled' };
      }
      this.sessionNumber++;
      this.tasks.clear();
      this.listings.clear();
      this.pages.clear();
      this.pageLimits.clear();
      this.root = this.newRoot();
      return this.ok(input, this.root);
    });
  }
  private work(
    targetId: string,
    operation: WorkRecord['operation'],
  ): WorkRecord {
    return {
      ...this.root!.session,
      taskId: `task-${++this.taskNumber}`,
      targetId,
      operation,
      phase: this.deferTasks ? 'running' : 'completed',
      sequence: 0,
      processedEntries: '0',
      processedDirectories: '0',
      observedAt: stamp,
      issues: issues(),
    };
  }
  async startListing(
    input: ScopedRequest & { directoryId: string; category: Category },
  ) {
    return this.scoped('startListing', input, () => {
      if (
        this.tree.entries.find((e) => e.entryId === input.directoryId)?.kind !==
        'directory'
      )
        return this.error<ListingStart>(input, 'NOT_DIRECTORY');
      const existing = [...this.listings.values()].find(
        (l) =>
          l.directoryId === input.directoryId && l.category === input.category,
      );
      if (existing) return this.ok(input, existing);
      const work = this.work(
        input.directoryId,
        input.category === 'directories' ? 'listDirectories' : 'listFiles',
      );
      if (this.tree.deniedIds?.includes(input.directoryId)) {
        work.phase = 'failed';
        work.issues = {
          counts: { PERMISSION_DENIED: '1' },
          samples: [
            {
              code: 'PERMISSION_DENIED',
              operation: 'list',
              scope: 'entry',
              entryId: input.directoryId,
              nativeCode: 13,
            },
          ],
        };
      }
      const start: ListingStart = {
        work,
        listingRevision: `revision-${this.taskNumber}`,
        directoryId: input.directoryId,
        category: input.category,
      };
      this.tasks.set(work.taskId, work);
      this.listings.set(work.taskId, start);
      return this.ok(input, start);
    });
  }
  async readListingPage(
    input: ScopedRequest & {
      taskId: string;
      cursor: string | null;
      limit: number;
    },
  ) {
    return this.scoped('readListingPage', input, () => {
      const start = this.listings.get(input.taskId);
      if (!start)
        return this.error<ListingPage>(
          input,
          input.cursor ? 'CURSOR_EXPIRED' : 'TASK_EXPIRED',
        );
      if (
        !Number.isInteger(input.limit) ||
        input.limit < 1 ||
        input.limit > 1000
      )
        return this.error<ListingPage>(input, 'INVALID_ARGUMENT');
      const existingLimit = this.pageLimits.get(input.taskId);
      if (existingLimit !== undefined && existingLimit !== input.limit)
        return this.error<ListingPage>(input, 'INVALID_ARGUMENT');
      this.pageLimits.set(input.taskId, input.limit);
      const work = this.tasks.get(input.taskId)!;
      let offset = 0;
      if (input.cursor) {
        const expected = `${input.taskId}/${start.listingRevision}/${input.limit}/`;
        if (!input.cursor.startsWith(expected))
          return this.error<ListingPage>(input, 'CURSOR_EXPIRED');
        offset = Number(input.cursor.slice(expected.length));
        if (!Number.isSafeInteger(offset) || offset < 0)
          return this.error<ListingPage>(input, 'INVALID_ARGUMENT');
      }
      const pageKey = JSON.stringify([input.taskId, input.cursor, input.limit]);
      const cached = this.pages.get(pageKey);
      if (cached) return this.ok(input, { ...cached, work });
      const all =
        work.phase === 'failed' || work.phase === 'cancelled'
          ? []
          : this.tree.entries.filter(
              (entry) =>
                entry.parentId === start.directoryId &&
                (entry.kind === 'directory') ===
                  (start.category === 'directories'),
            );
      const entries = terminal(work.phase)
        ? all.slice(offset, offset + input.limit)
        : [];
      const end = offset + entries.length;
      const page: ListingPage = {
        ...start,
        work,
        entries,
        cursor: input.cursor,
        nextCursor:
          terminal(work.phase) && end >= all.length
            ? null
            : `${input.taskId}/${start.listingRevision}/${input.limit}/${end}`,
        coverage: work.phase === 'completed' ? 'complete' : 'partial',
        issues: work.issues,
      };
      if (entries.length || terminal(work.phase)) this.pages.set(pageKey, page);
      return this.ok(input, page);
    });
  }
  async startUsageScan(input: ScopedRequest) {
    return this.scoped('startUsageScan', input, () => {
      const work =
        [...this.tasks.values()].find((w) => w.operation === 'scanUsage') ??
        this.work(this.root!.root.entryId, 'scanUsage');
      this.tasks.set(work.taskId, work);
      return this.ok(input, work);
    });
  }
  async readTask(input: ScopedRequest & { taskId: string }) {
    return this.scoped('readTask', input, () => {
      const work = this.tasks.get(input.taskId);
      return work
        ? this.ok(input, work)
        : this.error<WorkRecord>(input, 'TASK_EXPIRED');
    });
  }
  async readUsage(input: ScopedRequest & { directoryIds: string[] }) {
    return this.scoped<DirectoryUsage[]>('readUsage', input, () => {
      if (
        input.directoryIds.length > 1000 ||
        input.directoryIds.some(
          (id) =>
            this.tree.entries.find((e) => e.entryId === id)?.kind !==
            'directory',
        )
      )
        return this.error<DirectoryUsage[]>(input, 'INVALID_ARGUMENT');
      return this.ok(
        input,
        input.directoryIds.map(
          (directoryId): DirectoryUsage =>
            this.tree.usages?.find((u) => u.directoryId === directoryId) ?? {
              directoryId,
              usageRevision: 0,
              scanState: 'notRequested',
              logical: { state: 'unknown' },
              allocated: { state: 'unknown' },
              observedAt: stamp,
            },
        ),
      );
    });
  }
  async cancelTask(input: ScopedRequest & { taskId: string }) {
    return this.scoped('cancelTask', input, () => {
      const previous = this.tasks.get(input.taskId);
      if (!previous)
        return this.error<{
          status: 'accepted' | 'alreadyTerminal';
          work: WorkRecord;
        }>(input, 'TASK_EXPIRED');
      const status = terminal(previous.phase)
        ? ('alreadyTerminal' as const)
        : ('accepted' as const);
      const work =
        status === 'accepted'
          ? {
              ...previous,
              phase: 'cancelled' as const,
              sequence: previous.sequence + 1,
            }
          : previous;
      this.tasks.set(input.taskId, work);
      return this.ok(input, { status, work });
    });
  }
  async refreshRoot(
    input: Request & { sessionId: string; expectedGeneration: number },
  ) {
    return this.scoped(
      'refreshRoot',
      { ...input, generation: input.expectedGeneration },
      () => {
        this.root = this.newRoot(input.expectedGeneration + 1);
        this.tasks.clear();
        this.listings.clear();
        this.pages.clear();
        this.pageLimits.clear();
        return this.ok(input, this.root);
      },
    );
  }
  async closeSession(input: Request & { sessionId: string }) {
    return this.invoke('closeSession', input, () => {
      if (this.root && input.sessionId !== this.root.session.sessionId)
        return this.error<{ status: 'closed' | 'alreadyClosed' }>(
          input,
          'SESSION_CLOSED',
        );
      const status = this.root
        ? ('closed' as const)
        : ('alreadyClosed' as const);
      this.root = null;
      this.tasks.clear();
      this.listings.clear();
      this.pages.clear();
      this.pageLimits.clear();
      return this.ok(input, { status });
    });
  }
  finish(taskId: string) {
    const previous = this.tasks.get(taskId);
    if (!previous || terminal(previous.phase)) return;
    const work = {
      ...previous,
      phase: 'completed' as const,
      sequence: previous.sequence + 1,
    };
    this.tasks.set(taskId, work);
    if (!this.dropEvents)
      this.listeners.forEach((listener) =>
        listener({ protocolVersion: 1, ...work, kind: 'terminal' }),
      );
  }
}
