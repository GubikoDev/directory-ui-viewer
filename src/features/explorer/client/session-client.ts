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
  constructor(readonly code: string) {
    super(code);
  }
}
export function unwrap<T>(response: Response<T>, requestId: string): T | null {
  if (response.requestId !== requestId)
    throw new AdapterError('INVALID_RESPONSE');
  if (response.outcome === 'error') throw new AdapterError(response.error.code);
  return response.outcome === 'cancelled' ? null : response.data;
}

/** Owns control order, bounded cache recovery and one subscription per document. */
export class SessionClient {
  private epoch: string | null = null;
  private number = 0;
  private lifecycle = 0;
  private intent = 0;
  private picker?: Promise<boolean>;
  private controlTail: Promise<unknown> = Promise.resolve();
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
    const next = this.controlTail.then(fn, fn);
    this.controlTail = next.catch(() => undefined);
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
    this.store.reset(root);
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
    const { start, order } = await this.control(async () => {
      const request = { ...this.scoped(scope), directoryId, category };
      const order = this.number;
      const start = unwrap(
        await this.adapter.startListing(request),
        request.requestId,
      );
      return { start, order };
    });
    if (!start || !sameScope(this.store.getSnapshot().session, scope)) return;
    if (!this.store.registerListing(start, order)) return;
    this.lastEvent.set(start.work.taskId, this.now());
    await this.nextPage(start.work.taskId);
  }
  async scan() {
    const scope = this.store.getSnapshot().session;
    if (!scope) return;
    const work = await this.control(async () => {
      const request = this.scoped(scope);
      return unwrap(
        await this.adapter.startUsageScan(request),
        request.requestId,
      );
    });
    if (work && sameScope(this.store.getSnapshot().session, scope)) {
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
  private async readPage(taskId: string): Promise<boolean> {
    const state = this.store.getSnapshot();
    const task = state.tasks.get(taskId);
    if (!task || !state.session || state.resourceLimited) return false;
    const listing = [...state.listings.values()].find(
      (value) => value.taskId === taskId,
    );
    if (!listing?.snapshot || listing.loadState !== 'loading') return false;
    if (this.now() - (this.lastPage.get(taskId) ?? -Infinity) < 250)
      return false;
    this.lastPage.set(taskId, this.now());
    const request = {
      ...this.scoped(state.session),
      taskId,
      cursor: listing.snapshot.nextCursor,
      limit: 200,
    };
    const page = unwrap(
      await this.adapter.readListingPage(request),
      request.requestId,
    );
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
    const work = unwrap(
      await this.adapter.readTask(request),
      request.requestId,
    );
    if (!work || !sameScope(this.store.getSnapshot().session, scope)) return;
    this.store.applyTask(work);
    if (work.operation === 'scanUsage') await this.readUsage(scope);
    else if (this.pageDemand.has(taskId)) await this.nextPage(taskId);
  }
  async readUsage(scope: Scope | null = this.store.getSnapshot().session) {
    if (!scope) return;
    const ids = [...this.store.getSnapshot().entries.values()]
      .filter((e) => e.kind === 'directory')
      .map((e) => e.entryId);
    for (let offset = 0; offset < ids.length; offset += 1000) {
      if (!sameScope(this.store.getSnapshot().session, scope)) return;
      const request = {
        ...this.scoped(scope),
        directoryIds: ids.slice(offset, offset + 1000),
      };
      const usages = unwrap(
        await this.adapter.readUsage(request),
        request.requestId,
      );
      if (usages) this.store.applyUsage(scope, usages);
    }
  }
  async poll() {
    const state = this.store.getSnapshot();
    for (const task of state.tasks.values()) {
      const pendingListing = this.pageDemand.has(task.taskId);
      if (
        (!terminal(task.phase) || pendingListing) &&
        this.now() - (this.lastEvent.get(task.taskId) ?? 0) >= 1000
      ) {
        await this.synchronize(task.taskId);
      }
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
  async refresh() {
    const scope = this.store.getSnapshot().session;
    if (!scope) return;
    const lifecycle = this.lifecycle;
    const root = await this.control(async () => {
      const request = {
        ...this.request(),
        sessionId: scope.sessionId,
        expectedGeneration: scope.generation,
      };
      return unwrap(await this.adapter.refreshRoot(request), request.requestId);
    });
    if (
      root &&
      lifecycle === this.lifecycle &&
      sameScope(this.store.getSnapshot().session, scope)
    )
      await this.activate(root);
  }
  async close() {
    this.intent++;
    const scope = this.store.getSnapshot().session;
    if (!scope) return;
    await this.control(async () => {
      const request = { ...this.request(), sessionId: scope.sessionId };
      unwrap(await this.adapter.closeSession(request), request.requestId);
    });
    if (sameScope(this.store.getSnapshot().session, scope)) {
      this.store.reset(null);
      this.lastPage.clear();
      this.pageDemand.clear();
      this.lastEvent.clear();
    }
  }
}
