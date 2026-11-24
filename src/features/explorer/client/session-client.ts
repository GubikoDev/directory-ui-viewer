import { ExplorerStore } from '../model/store';
import { TaskMap } from '../runtime/task-map';
import { IssueLog, type ClientIssue } from '../runtime/issue-log';
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
  type ListingStart,
} from '../model/types';

type PendingStart =
  | {
      kind: 'listing';
      request: ScopedRequest & { directoryId: string; category: Category };
      order: number;
    }
  | { kind: 'usage'; request: ScopedRequest };

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
  private usageViewport?: {
    scope: Scope;
    ids: readonly string[];
    wanted: ReadonlySet<string>;
  };
  private usageRead?: {
    scope: Scope;
    pending: Promise<void>;
    repeat: () => void;
  };
  private currentScope: Scope | null = null;
  private picker?: Promise<boolean>;
  private refreshing?: Promise<void>;
  private controlTail: Promise<void> = Promise.resolve();
  private queuedControls = 0;
  private polling?: Promise<void>;
  private subscription?: () => void;
  private stateSubscription?: () => void;
  private timer?: ReturnType<typeof setInterval>;
  private connection?: Promise<void>;
  private recovery: TaskMap<Promise<void>>;
  private pageReads: TaskMap<Promise<boolean>>;
  private lastPage: TaskMap<number>;
  private pageDemand: TaskMap<boolean>;
  private lastEvent: TaskMap<number>;
  private pendingStarts: TaskMap<PendingStart>;
  private readonly errorLog: IssueLog;
  constructor(
    readonly adapter: FilesystemAdapter,
    readonly store = new ExplorerStore(),
    private readonly nonce: string = crypto.randomUUID(),
    private readonly now = () => Date.now(),
  ) {
    store.budget.retainLifetime(this, 8192 + nonce.length * 2);
    this.errorLog = new IssueLog(store.budget);
    this.recovery = new TaskMap(store.budget);
    this.pageReads = new TaskMap(store.budget);
    this.lastPage = new TaskMap(store.budget);
    this.pageDemand = new TaskMap(store.budget);
    this.lastEvent = new TaskMap(store.budget);
    this.pendingStarts = new TaskMap(store.budget);
    this.stateSubscription = store.subscribe(() => {
      const tasks = store.getSnapshot().tasks;
      const keep = (id: string) => tasks.has(id);
      this.lastPage.prune(keep);
      this.pageDemand.prune(keep);
      this.lastEvent.prune(keep);
      this.pendingStarts.prune((id) => {
        const start = this.pendingStarts.get(id)!;
        return (
          sameScope(store.getSnapshot().session, start.request) &&
          (start.kind === 'usage' ||
            store.isListingRequestCurrent(
              start.request.directoryId,
              start.request.category,
              start.order,
            ))
        );
      });
    });
  }
  get errors(): readonly ClientIssue[] {
    return this.errorLog.snapshot;
  }
  get hasSession(): boolean {
    return this.rootScope() !== null;
  }
  private report(error: unknown) {
    this.errorLog.append(error);
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
        try {
          if (!this.store.acceptEvent(event)) return;
          this.lastEvent.set(event.taskId, this.now());
          void this.synchronize(event.taskId).catch((error) =>
            this.report(error),
          );
        } catch (error) {
          // Admission can fail synchronously before a Promise exists. Hints
          // must not throw through the adapter's event producer.
          this.report(error);
        }
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
    this.usageViewport = undefined;
    this.currentScope = null;
    this.recovery.clear();
    this.pageReads.clear();
    this.pendingStarts.clear();
    this.store.reset(null);
    this.stateSubscription?.();
    this.stateSubscription = undefined;
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
  private control<T>(fn: () => Promise<T>, priority = false): Promise<T> {
    // Bound dispatch closures even if callers enqueue before the next microtask.
    if (this.queuedControls >= (priority ? 64 : 62))
      return Promise.reject(new AdapterError('RESOURCE_LIMIT'));
    // Admit both Promise owners before enqueueing an effect. Their request
    // wrapper/callback metadata remains charged until the actual owners' GC.
    const memory = this.store.budget.reserve(12 * 1024, priority);
    this.queuedControls++;
    const lifecycle = this.lifecycle;
    const dispatch = () => {
      this.queuedControls--;
      if (lifecycle !== this.lifecycle)
        throw new AdapterError('CLIENT_EXPIRED');
      return fn();
    };
    try {
      const next = this.controlTail.then(dispatch);
      this.store.budget.retainLifetimeFrom(next, memory, 8 * 1024);
      // The tail must not retain the previous result or its pending response.
      const tail = next.then(
        () => undefined,
        () => undefined,
      );
      this.store.budget.retainLifetimeFrom(tail, memory, 4 * 1024);
      this.controlTail = tail;
      return next;
    } finally {
      memory.close();
    }
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
    this.errorLog.clear();
    this.usageViewport = undefined;
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
    const { start, pending } = await this.control(async () => {
      if (!sameScope(this.store.getSnapshot().session, scope))
        throw new AdapterError('STALE_GENERATION');
      const request = { ...this.scoped(scope), directoryId, category };
      const order = this.number;
      if (!this.store.beginListing(directoryId, category, order))
        throw new AdapterError('RESOURCE_LIMIT');
      return this.prepareStart({ kind: 'listing', request, order });
    });
    await this.consumeStart(start, pending);
  }
  async scan() {
    const scope = this.store.getSnapshot().session;
    if (!scope) return;
    const { start, pending } = await this.control(async () => {
      const request = this.scoped(scope);
      return this.prepareStart({ kind: 'usage', request });
    });
    await this.consumeStart(start, pending);
  }
  private invokeStart(
    start: PendingStart,
  ): Promise<Response<ListingStart | WorkRecord>> {
    return start.kind === 'listing'
      ? this.adapter.startListing(start.request)
      : this.adapter.startUsageScan(start.request);
  }
  private prepareStart(input: PendingStart) {
    if (this.pendingStarts.size >= 64) throw new AdapterError('RESOURCE_LIMIT');
    // Preserve only the small original request before effects. A transient
    // registration failure is recovered with the SAME requestId, so native
    // replay can never silently execute a fresh scan after cache expiry.
    const start = this.store.budget.adopt(input);
    this.pendingStarts.set(start.request.requestId, start);
    try {
      return { start, pending: this.invokeStart(start) };
    } catch (error) {
      this.pendingStarts.delete(start.request.requestId);
      throw error;
    }
  }
  private async consumeStart(
    start: PendingStart,
    pending: Promise<Response<ListingStart | WorkRecord>>,
  ) {
    const id = start.request.requestId;
    try {
      const response = await pending;
      if (
        this.pendingStarts.get(id) !== start ||
        !sameScope(this.store.getSnapshot().session, start.request)
      )
        return;
      // A definitive native error is itself replayable. Keeping its request
      // would retry the same cached failure forever; only an unobserved result
      // or a rejected local registration needs pending-start recovery.
      if (response.requestId === id && response.outcome === 'error') {
        this.pendingStarts.delete(id);
        if (start.kind === 'listing')
          this.store.failListingStart(
            start.request.directoryId,
            start.request.category,
            start.order,
          );
      }
      const data = unwrap(response, id);
      if (!data) {
        this.pendingStarts.delete(id);
        return;
      }
      let task: WorkRecord;
      let admitted: boolean;
      if (start.kind === 'listing') {
        if (!('work' in data)) throw new AdapterError('INVALID_RESPONSE');
        task = data.work;
        admitted = this.store.registerListing(data, start.order);
      } else {
        if ('work' in data) throw new AdapterError('INVALID_RESPONSE');
        task = data;
        admitted = this.store.applyTask(task);
      }
      if (
        !admitted &&
        this.store.getSnapshot().resourceLimited &&
        !this.store.getSnapshot().tasks.has(task.taskId)
      )
        return;
      this.pendingStarts.delete(id);
      if (!admitted) return;
      this.lastEvent.set(task.taskId, this.now());
      if (start.kind === 'listing') await this.nextPage(task.taskId);
      else await this.synchronize(task.taskId);
    } catch (error) {
      if (
        this.pendingStarts.get(id) === start &&
        error instanceof Error &&
        error.message === 'RESOURCE_LIMIT'
      )
        this.store.suspendObservations();
      else this.pendingStarts.delete(id);
      throw error;
    }
  }
  async startTreeListing(scope: Scope, directoryId: string) {
    if (!sameScope(this.store.getSnapshot().session, scope))
      throw new AdapterError('STALE_GENERATION');
    const request = {
      ...this.scoped(scope),
      directoryId,
      category: 'directories' as const,
    };
    const start = unwrap(
      await this.adapter.startListing(request),
      request.requestId,
    );
    if (
      !start ||
      !sameScope(scope, start.work) ||
      start.category !== 'directories' ||
      start.directoryId !== directoryId
    )
      throw new AdapterError('INVALID_RESPONSE');
    return start;
  }
  async readTreePage(start: ListingStart, cursor: string | null) {
    if (!sameScope(this.store.getSnapshot().session, start.work))
      throw new AdapterError('STALE_GENERATION');
    const request = {
      ...this.scoped(start.work),
      taskId: start.work.taskId,
      cursor,
      limit: 200,
    };
    const page = unwrap(
      await this.adapter.readListingPage(request),
      request.requestId,
    );
    if (
      !page ||
      !sameScope(start.work, page.work) ||
      page.work.taskId !== start.work.taskId ||
      page.listingRevision !== start.listingRevision ||
      page.directoryId !== start.directoryId ||
      page.category !== start.category ||
      page.cursor !== cursor
    )
      throw new AdapterError('INVALID_RESPONSE');
    return page;
  }
  hasFileDemand() {
    let starting = false;
    this.pendingStarts.forEach((start) => {
      if (
        start.kind === 'listing' &&
        start.request.category === 'files' &&
        sameScope(this.store.getSnapshot().session, start.request) &&
        this.store.isListingRequestCurrent(
          start.request.directoryId,
          'files',
          start.order,
        )
      )
        starting = true;
    });
    if (starting) return true;
    for (const id of this.store.getSnapshot().openFileListIds) {
      const listing = this.store.getListing(id, 'files');
      if (
        listing.taskId &&
        (this.pageDemand.has(listing.taskId) ||
          this.pageReads.has(listing.taskId))
      )
        return true;
    }
    return false;
  }
  nextPage(taskId: string): Promise<boolean> {
    const state = this.store.getSnapshot();
    const task = state.tasks.get(taskId);
    if (!task || task.operation === 'scanUsage') {
      this.pageDemand.delete(taskId);
      return Promise.resolve(false);
    }
    let available = false;
    for (const listing of state.listings.values()) {
      if (
        listing.taskId === taskId &&
        listing.snapshot &&
        listing.loadState === 'loading'
      ) {
        available = true;
        break;
      }
    }
    if (!available) {
      this.pageDemand.delete(taskId);
      return Promise.resolve(false);
    }
    this.pageDemand.set(taskId, true);
    return this.pageReads.run(taskId, () => this.readPage(taskId));
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
    return this.recovery.run(taskId, () => this.sync(taskId));
  }
  /** Reports closure, not observation recovery. Check resourceLimited separately. */
  closeListing(directoryId: string, category: Category) {
    if (!this.store.setExpanded(directoryId, false, category)) return false;
    this.store.evictClosedListings();
    if (this.store.getSnapshot().resourceLimited)
      this.store.resumeObservations();
    return true;
  }
  async retryObservations(): Promise<boolean> {
    this.errorLog.clear();
    if (!this.store.resumeObservations()) return false;
    const scope = this.store.getSnapshot().session;
    if (!scope) return true;
    const ids = this.observationIds('loadingListings');
    const pending = this.observationIds('pendingStarts');
    for (const id of pending) {
      const start = this.pendingStarts.get(id);
      if (!start || !sameScope(this.store.getSnapshot().session, scope))
        continue;
      const dispatched = await this.control(async () => ({
        pending:
          this.pendingStarts.get(id) === start ? this.invokeStart(start) : null,
      }));
      if (!dispatched.pending) continue;
      await this.consumeStart(start, dispatched.pending);
      if (this.store.getSnapshot().resourceLimited) return false;
    }
    for (const id of ids) {
      if (
        !sameScope(this.store.getSnapshot().session, scope) ||
        this.store.getSnapshot().resourceLimited
      )
        return false;
      await this.nextPage(id);
    }
    if (!this.store.getSnapshot().resourceLimited) await this.readUsage(scope);
    return !this.store.getSnapshot().resourceLimited;
  }
  private async sync(taskId: string) {
    const scope = this.store.getSnapshot().session;
    if (!scope) return;
    const request = { ...this.scoped(scope), taskId };
    const response = await this.adapter.readTask(request);
    if (!sameScope(this.store.getSnapshot().session, scope)) return;
    if (!this.store.getSnapshot().tasks.has(taskId)) return;
    const work = unwrap(response, request.requestId);
    if (!work) return;
    this.store.applyTask(work);
    if (work.operation === 'scanUsage') await this.readUsage(scope);
    else if (this.pageDemand.has(taskId)) await this.nextPage(taskId);
  }
  setUsageViewport(scope: Scope, ids: readonly string[]) {
    if (sameScope(this.store.getSnapshot().session, scope))
      this.usageViewport = { scope, ids, wanted: new Set(ids) };
  }
  async readUsage(scope: Scope | null = this.store.getSnapshot().session) {
    if (!scope || !sameScope(this.store.getSnapshot().session, scope)) return;
    if (this.usageRead && sameScope(this.usageRead.scope, scope)) {
      this.usageRead.repeat();
      return this.usageRead.pending;
    }
    // A terminal scan event can arrive while an older batch is in flight.
    // Keep one trailing read of the latest viewport so that update isn't lost.
    let again = false;
    const pending = (async () => {
      try {
        do {
          again = false;
          await this.readUsageBatches(scope);
        } while (
          again &&
          sameScope(this.store.getSnapshot().session, scope) &&
          !this.store.getSnapshot().resourceLimited
        );
      } finally {
        if (this.usageRead?.scope === scope) this.usageRead = undefined;
      }
    })();
    this.usageRead = {
      scope,
      pending,
      repeat: () => {
        again = true;
      },
    };
    return pending;
  }
  private async readUsageBatches(scope: Scope) {
    const viewport = this.usageViewport;
    const ids =
      viewport && sameScope(scope, viewport.scope)
        ? viewport.ids
        : this.observationIds('directories');
    const batchSize = 32;
    for (let offset = 0; offset < ids.length; offset += batchSize) {
      if (!sameScope(this.store.getSnapshot().session, scope)) return;
      const request = {
        ...this.scoped(scope),
        directoryIds: this.store.budget.allocate(
          this.idSliceBytes(ids, offset, batchSize),
          () => ids.slice(offset, offset + batchSize),
        ),
      };
      const response = await this.adapter.readUsage(request);
      if (!sameScope(this.store.getSnapshot().session, scope)) return;
      if (viewport !== this.usageViewport) return;
      const usages = unwrap(response, request.requestId);
      if (usages) this.store.applyUsage(scope, usages, viewport?.wanted);
      if (this.store.getSnapshot().resourceLimited) return;
    }
  }
  private idSliceBytes(ids: readonly string[], offset: number, count: number) {
    let bytes = 512;
    for (let i = offset; i < Math.min(ids.length, offset + count); i++)
      bytes += 128 + ids[i].length * 2;
    return bytes;
  }
  private observationIds(
    kind: 'directories' | 'pendingTasks' | 'loadingListings' | 'pendingStarts',
  ): readonly string[] {
    // Both passes are synchronous. Only the charged IDs survive an async wait.
    const state = this.store.getSnapshot();
    const observedAt = this.now();
    const visit = (accept: (id: string) => void) => {
      if (kind === 'directories') {
        for (const entry of state.entries.values())
          if (entry.kind === 'directory') accept(entry.entryId);
      } else if (kind === 'pendingStarts') {
        this.pendingStarts.forEach((_value, id) => accept(id));
      } else if (kind === 'loadingListings') {
        for (const listing of state.listings.values())
          if (listing.loadState === 'loading' && listing.taskId)
            accept(listing.taskId);
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
    if (!scope || this.store.getSnapshot().resourceLimited) return;
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
    const { request, pending } = await this.control(async () => {
      const request = { ...this.scoped(scope), taskId };
      return { request, pending: this.adapter.cancelTask(request) };
    }, true);
    const result = unwrap(await pending, request.requestId);
    if (result && this.store.getSnapshot().tasks.has(taskId))
      this.store.applyTask(result.work);
    try {
      void this.synchronize(taskId).catch((error) => this.report(error));
    } catch (error) {
      // Native acceptance is authoritative. Observation recovery can fail under
      // data pressure without turning an accepted cancellation into a failure.
      this.report(error);
    }
    return result?.status ?? null;
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
    const { request, pending } = await this.control(async () => {
      const request = { ...this.request(), sessionId: scope.sessionId };
      return { request, pending: this.adapter.closeSession(request) };
    }, true);
    unwrap(await pending, request.requestId);
    if (sameScope(this.rootScope(), scope)) {
      this.currentScope = null;
      this.usageViewport = undefined;
      this.store.reset(null);
      this.lastPage.clear();
      this.pageDemand.clear();
      this.lastEvent.clear();
    }
  }
}
