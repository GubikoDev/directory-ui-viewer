import { ExplorerStore } from '../model/store';
import {
  sameScope,
  terminal,
  type Category,
  type FilesystemAdapter,
  type Request,
  type Response,
  type RootData,
  type Scope,
  type ScopedRequest,
  type WorkRecord,
} from '../model/types';

export class AdapterError extends Error {
  constructor(
    readonly code: string,
    readonly currentScope?: Scope,
  ) {
    super(code);
  }
}
export function unwrap<T>(response: Response<T>, requestId: string): T | null {
  if (response.requestId !== requestId)
    throw new AdapterError('INVALID_RESPONSE');
  if (response.outcome === 'error')
    throw new AdapterError(response.error.code, response.currentScope);
  return response.outcome === 'cancelled' ? null : response.data;
}

/** Owns control order, bounded cache recovery and one subscription per document. */
export class SessionClient {
  private epoch: string | null = null;
  private number = 0;
  private lifecycle = 0;
  private intent = 0;
  private currentScope: Scope | null = null;
  private picker?: Promise<boolean>;
  private refreshing?: Promise<void>;
  private controlTail: Promise<void> = Promise.resolve();
  private queuedControls = 0;
  private polling?: Promise<void>;
  private subscription?: () => void;
  private timer?: ReturnType<typeof setInterval>;
  private connection?: Promise<void>;
  private recovery = new Map<string, Promise<void>>();
  private pageReads = new Map<string, Promise<boolean>>();
  private lastPage = new Map<string, number>();
  private pageDemand = new Set<string>();
  private lastEvent = new Map<string, number>();
  private deferredErrors: Error[] = [];
  constructor(
    readonly adapter: FilesystemAdapter,
    readonly store = new ExplorerStore(),
    private readonly nonce: string = crypto.randomUUID(),
    private readonly now = () => Date.now(),
  ) {}
  get errors(): readonly Error[] {
    return this.deferredErrors;
  }
  get hasSession(): boolean {
    return this.rootScope() !== null;
  }
  private report(error: unknown) {
    this.deferredErrors = [
      ...this.deferredErrors.slice(-31),
      error instanceof Error ? error : new Error('INTERNAL'),
    ];
  }
  connect(): Promise<void> {
    if (this.connection) return this.connection;
    const lifecycle = this.lifecycle;
    this.connection = (async () => {
      const { epoch } = await this.adapter.connectClient({
        protocolVersion: 1,
        clientNonce: this.nonce,
      });
      if (lifecycle !== this.lifecycle) return;
      this.epoch = epoch;
      const unsubscribe = await this.adapter.subscribe((event) => {
        if (!this.store.acceptEvent(event)) return;
        this.lastEvent.set(event.taskId, this.now());
        void this.synchronize(event.taskId).catch((error) =>
          this.report(error),
        );
      });
      if (lifecycle !== this.lifecycle) {
        unsubscribe();
        return;
      }
      this.subscription = unsubscribe;
      this.timer = setInterval(() => {
        void this.poll().catch((error) => this.report(error));
      }, 1000);
    })().catch((error) => {
      this.connection = undefined;
      throw error;
    });
    return this.connection;
  }
  dispose() {
    this.lifecycle++;
    this.subscription?.();
    this.subscription = undefined;
    clearInterval(this.timer);
    this.connection = undefined;
    this.epoch = null;
    this.currentScope = null;
    this.store.reset(null);
    this.lastPage.clear();
    this.pageDemand.clear();
    this.lastEvent.clear();
  }
  private request(): Request {
    if (!this.epoch) throw new AdapterError('CLIENT_EXPIRED');
    if (!Number.isSafeInteger(this.number + 1))
      throw new AdapterError('RESOURCE_LIMIT');
    return { protocolVersion: 1, requestId: `${this.epoch}:${++this.number}` };
  }
  private scoped(
    scope: Scope | null = this.store.getSnapshot().session,
  ): ScopedRequest {
    if (!scope) throw new AdapterError('SESSION_CLOSED');
    return {
      ...this.request(),
      sessionId: scope.sessionId,
      generation: scope.generation,
    };
  }
  private control<T>(fn: () => Promise<T>): Promise<T> {
    // Bound dispatch closures even if callers enqueue before the next microtask.
    if (this.queuedControls >= 64)
      return Promise.reject(new AdapterError('RESOURCE_LIMIT'));
    this.queuedControls++;
    const lifecycle = this.lifecycle;
    const dispatch = () => {
      this.queuedControls--;
      if (lifecycle !== this.lifecycle)
        throw new AdapterError('CLIENT_EXPIRED');
      return fn();
    };
    const next = this.controlTail.then(dispatch);
    // The tail must not retain the previous result or its pending response.
    this.controlTail = next.then(
      () => undefined,
      () => undefined,
    );
    return next;
  }
  private register(task: WorkRecord) {
    this.store.applyTask(task);
    this.lastEvent.set(task.taskId, this.now());
  }
  chooseRoot(): Promise<boolean> {
    if (this.picker) return this.picker;
    this.picker = this.choose().finally(() => {
      this.picker = undefined;
    });
    return this.picker;
  }
  private async choose(): Promise<boolean> {
    await this.connect();
    const lifecycle = this.lifecycle;
    const intent = ++this.intent;
    // Serialize dispatch only: an open native picker must not block close/cancel.
    const { request, pending } = await this.control(async () => {
      const request = this.request();
      return { request, pending: this.adapter.chooseRoot(request) };
    });
    const root = unwrap(await pending, request.requestId);
    if (!root || lifecycle !== this.lifecycle || intent !== this.intent)
      return false;
    await this.activate(root);
    return true;
  }
  private async activate(root: RootData) {
    this.currentScope = root.session;
    if (!this.store.reset(root)) {
      // Native has already accepted this scope. Keep its control identity even
      // if the view cannot admit its observations; never operate on the old one.
      this.store.reset(null);
      throw new AdapterError('RESOURCE_LIMIT');
    }
    this.lastPage.clear();
    this.pageDemand.clear();
    this.lastEvent.clear();
    await Promise.all([
      this.list(root.root.entryId, 'directories'),
      this.scan(),
    ]);
  }
  async list(directoryId: string, category: Category) {
    const scope = this.store.getSnapshot().session;
    if (!scope) throw new AdapterError('SESSION_CLOSED');
    const { request, pending, order } = await this.control(async () => {
      const request = { ...this.scoped(scope), directoryId, category };
      const order = this.number;
      return { request, pending: this.adapter.startListing(request), order };
    });
    const response = await pending;
    if (!sameScope(this.store.getSnapshot().session, scope)) return;
    const start = unwrap(response, request.requestId);
    if (!start) return;
    if (!this.store.registerListing(start, order)) return;
    this.lastEvent.set(start.work.taskId, this.now());
    await this.nextPage(start.work.taskId);
  }
  async scan() {
    const scope = this.store.getSnapshot().session;
    if (!scope) return;
    const { request, pending } = await this.control(async () => {
      const request = this.scoped(scope);
      return { request, pending: this.adapter.startUsageScan(request) };
    });
    const response = await pending;
    if (!sameScope(this.store.getSnapshot().session, scope)) return;
    const work = unwrap(response, request.requestId);
    if (work) {
      this.register(work);
      await this.synchronize(work.taskId);
    }
  }
  nextPage(taskId: string): Promise<boolean> {
    this.pageDemand.add(taskId);
    const existing = this.pageReads.get(taskId);
    if (existing) return existing;
    const promise = this.readPage(taskId).finally(() =>
      this.pageReads.delete(taskId),
    );
    this.pageReads.set(taskId, promise);
    return promise;
  }
  private pageRequest(taskId: string) {
    const state = this.store.getSnapshot();
    if (!state.tasks.has(taskId) || !state.session || state.resourceLimited)
      return null;
    let listing;
    for (const candidate of state.listings.values()) {
      if (candidate.taskId === taskId) {
        listing = candidate;
        break;
      }
    }
    if (!listing?.snapshot || listing.loadState !== 'loading') return null;
    if (this.now() - (this.lastPage.get(taskId) ?? -Infinity) < 250)
      return null;
    this.lastPage.set(taskId, this.now());
    return {
      ...this.scoped(state.session),
      taskId,
      cursor: listing.snapshot.nextCursor,
      limit: 200,
    };
  }
  private async readPage(taskId: string): Promise<boolean> {
    // Do not retain the entire previous state across an adapter wait.
    const request = this.pageRequest(taskId);
    if (!request) return false;
    const response = await this.adapter.readListingPage(request);
    if (!sameScope(this.store.getSnapshot().session, request)) return false;
    const page = unwrap(response, request.requestId);
    if (!page) return false;
    const applied = this.store.applyPage(page);
    if (page.entries.length > 0 || page.nextCursor === null || !applied)
      this.pageDemand.delete(taskId);
    return applied;
  }
  private synchronize(taskId: string): Promise<void> {
    const existing = this.recovery.get(taskId);
    if (existing) return existing;
    const work = this.sync(taskId).finally(() => this.recovery.delete(taskId));
    this.recovery.set(taskId, work);
    return work;
  }
  private async sync(taskId: string) {
    const scope = this.store.getSnapshot().session;
    if (!scope) return;
    const request = { ...this.scoped(scope), taskId };
    const response = await this.adapter.readTask(request);
    if (!sameScope(this.store.getSnapshot().session, scope)) return;
    const work = unwrap(response, request.requestId);
    if (!work) return;
    this.store.applyTask(work);
    if (work.operation === 'scanUsage') await this.readUsage(scope);
    else if (this.pageDemand.has(taskId)) await this.nextPage(taskId);
  }
  async readUsage(scope: Scope | null = this.store.getSnapshot().session) {
    if (!scope) return;
    const ids = this.observationIds('directories');
    for (let offset = 0; offset < ids.length; offset += 1000) {
      if (!sameScope(this.store.getSnapshot().session, scope)) return;
      const request = {
        ...this.scoped(scope),
        directoryIds: this.store.budget.allocate(
          this.idSliceBytes(ids, offset, 1000),
          () => ids.slice(offset, offset + 1000),
        ),
      };
      const response = await this.adapter.readUsage(request);
      if (!sameScope(this.store.getSnapshot().session, scope)) return;
      const usages = unwrap(response, request.requestId);
      if (usages) this.store.applyUsage(scope, usages);
    }
  }
  private idSliceBytes(ids: readonly string[], offset: number, count: number) {
    let bytes = 512;
    for (let i = offset; i < Math.min(ids.length, offset + count); i++)
      bytes += 128 + ids[i].length * 2;
    return bytes;
  }
  private observationIds(
    kind: 'directories' | 'pendingTasks',
  ): readonly string[] {
    // Both passes are synchronous. Only the charged IDs survive an async wait.
    const state = this.store.getSnapshot();
    const observedAt = this.now();
    const visit = (accept: (id: string) => void) => {
      if (kind === 'directories') {
        for (const entry of state.entries.values())
          if (entry.kind === 'directory') accept(entry.entryId);
      } else {
        for (const task of state.tasks.values())
          if (
            (!terminal(task.phase) || this.pageDemand.has(task.taskId)) &&
            observedAt - (this.lastEvent.get(task.taskId) ?? 0) >= 1000
          )
            accept(task.taskId);
      }
    };
    let bytes = 512;
    visit((id) => {
      bytes += 128 + id.length * 2;
    });
    return this.store.budget.allocate(bytes, () => {
      const ids: string[] = [];
      visit((id) => ids.push(id));
      return ids;
    });
  }
  poll(): Promise<void> {
    if (this.polling) return this.polling;
    const pending = this.pollOnce().finally(() => {
      if (this.polling === pending) this.polling = undefined;
    });
    this.polling = pending;
    return pending;
  }
  private async pollOnce() {
    const scope = this.store.getSnapshot().session;
    const lifecycle = this.lifecycle;
    if (!scope) return;
    const ids = this.observationIds('pendingTasks');
    for (const taskId of ids) {
      if (
        lifecycle !== this.lifecycle ||
        !sameScope(this.store.getSnapshot().session, scope)
      )
        return;
      await this.synchronize(taskId);
    }
  }
  async cancel(taskId: string) {
    const scope = this.store.getSnapshot().session;
    if (!scope) return;
    this.store.markCancelPending(taskId);
    const result = await this.control(async () => {
      const request = { ...this.scoped(scope), taskId };
      return unwrap(await this.adapter.cancelTask(request), request.requestId);
    });
    if (result) this.store.applyTask(result.work);
    await this.synchronize(taskId);
  }
  private rootScope(): Scope | null {
    const state = this.store.getSnapshot();
    return (
      this.currentScope ?? state.session ?? state.failedRoot?.scope ?? null
    );
  }
  refresh(): Promise<void> {
    if (this.refreshing) return this.refreshing;
    const pending = this.refreshOnce().finally(() => {
      if (this.refreshing === pending) this.refreshing = undefined;
    });
    this.refreshing = pending;
    return pending;
  }
  private async refreshOnce() {
    const scope = this.rootScope();
    if (!scope) return;
    const lifecycle = this.lifecycle;
    const intent = ++this.intent;
    // Dispatch order is serialized; a disk wait must not block close or cancel.
    const { request, pending } = await this.control(async () => {
      const request = {
        ...this.request(),
        sessionId: scope.sessionId,
        expectedGeneration: scope.generation,
      };
      return { request, pending: this.adapter.refreshRoot(request) };
    });
    const stillCurrent = () =>
      lifecycle === this.lifecycle &&
      intent === this.intent &&
      sameScope(this.rootScope(), scope);
    try {
      const root = unwrap(await pending, request.requestId);
      if (root && stillCurrent()) await this.activate(root);
    } catch (error) {
      if (
        error instanceof AdapterError &&
        error.currentScope &&
        stillCurrent()
      ) {
        const advanced = error.currentScope;
        if (
          advanced.sessionId !== scope.sessionId ||
          !Number.isSafeInteger(advanced.generation) ||
          advanced.generation !== scope.generation + 1
        )
          throw new AdapterError('INVALID_RESPONSE');
        this.currentScope = advanced;
        const admitted = this.store.failRoot(advanced, {
          code: error.code,
          operation: 'refreshRoot',
          scope: 'root',
        });
        if (!admitted) this.store.reset(null);
        this.lastPage.clear();
        this.pageDemand.clear();
        this.lastEvent.clear();
      }
      throw error;
    }
  }
  async close() {
    this.intent++;
    const scope = this.rootScope();
    if (!scope) return;
    await this.control(async () => {
      const request = { ...this.request(), sessionId: scope.sessionId };
      unwrap(await this.adapter.closeSession(request), request.requestId);
    });
    if (sameScope(this.rootScope(), scope)) {
      this.currentScope = null;
      this.store.reset(null);
      this.lastPage.clear();
      this.pageDemand.clear();
      this.lastEvent.clear();
    }
  }
}
