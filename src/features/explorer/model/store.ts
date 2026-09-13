import {
  FrontendBudget,
  frontendBudget,
  ownBytes,
  type FrontendReservation,
} from '../runtime/memory';
import {
  listingKey,
  sameScope,
  terminal,
  type Category,
  type DirectoryListing,
  type DirectoryUsage,
  type Entry,
  type FsIssue,
  type ListingPage,
  type ListingStart,
  type RootData,
  type RootSession,
  type Scope,
  type WorkEvent,
  type WorkRecord,
} from './types';

export interface ExplorerState {
  session: RootSession | null;
  failedRoot: { scope: Scope; issue: FsIssue } | null;
  entries: ReadonlyMap<string, Entry>;
  listings: ReadonlyMap<string, DirectoryListing>;
  usages: ReadonlyMap<string, DirectoryUsage>;
  tasks: ReadonlyMap<string, WorkRecord>;
  mode: 'organization' | 'mindmap';
  expandedIds: ReadonlySet<string>;
  openFileListIds: ReadonlySet<string>;
  selectedId: string | null;
  resourceLimited: boolean;
}
const emptyState = (): ExplorerState => ({
  session: null,
  failedRoot: null,
  entries: new Map(),
  listings: new Map(),
  usages: new Map(),
  tasks: new Map(),
  mode: 'organization',
  expandedIds: new Set(),
  openFileListIds: new Set(),
  selectedId: null,
  resourceLimited: false,
});

/** Pure normalized observations shared by every view; no Tauri or disk access. */
export class ExplorerStore {
  private state!: ExplorerState;
  private pressureState!: ExplorerState;
  private listeners!: Set<() => void>;
  private sequences!: Map<string, number>;
  private listingOrders!: Map<string, number>;
  private cancelPending!: Set<string>;
  private emptySequences!: Map<string, number>;
  private emptyOrders!: Map<string, number>;
  private emptyCancelled!: Set<string>;
  private closed!: Record<
    ExplorerState['mode'],
    { state: ExplorerState; pressure: ExplorerState }
  >;
  private transaction?: FrontendReservation;
  readonly budget: FrontendBudget;
  constructor(budget: FrontendBudget | number = frontendBudget) {
    this.budget =
      typeof budget === 'number' ? new FrontendBudget(budget) : budget;
    this.budget.retainLifetime(this, 4096);
    this.budget.allocate(64 * 1024, () => {
      const organization = emptyState();
      const mindmap: ExplorerState = { ...emptyState(), mode: 'mindmap' };
      this.closed = {
        organization: {
          state: organization,
          pressure: { ...organization, resourceLimited: true },
        },
        mindmap: {
          state: mindmap,
          pressure: { ...mindmap, resourceLimited: true },
        },
      };
      this.state = organization;
      this.pressureState = this.closed.organization.pressure;
      this.listeners = new Set();
      this.sequences = new Map();
      this.listingOrders = new Map();
      this.cancelPending = new Set();
      this.emptySequences = this.sequences;
      this.emptyOrders = this.listingOrders;
      this.emptyCancelled = this.cancelPending;
      return {
        closed: this.closed,
        state: this.state,
        pressure: this.pressureState,
        sequences: this.sequences,
        orders: this.listingOrders,
        cancelled: this.cancelPending,
      };
    });
  }
  getSnapshot = () => this.state;
  subscribe = (listener: () => void) => {
    if (!this.listeners.has(listener) && this.listeners.size >= 16)
      throw new Error('RESOURCE_LIMIT');
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };
  private collectionBytes(): number {
    let bytes =
      ownBytes(this.state.entries) +
      ownBytes(this.state.listings) +
      ownBytes(this.state.tasks) +
      ownBytes(this.state.usages) +
      ownBytes(this.state.expandedIds) +
      ownBytes(this.state.openFileListIds) +
      ownBytes(this.sequences) +
      ownBytes(this.listingOrders) +
      ownBytes(this.cancelPending);
    for (const listing of this.state.listings.values()) {
      if (listing.snapshot) bytes += ownBytes(listing.snapshot.entryIds);
    }
    return bytes;
  }
  /** Reserve old container copies, page array/Set scratch, new slots and new
   * incoming observations before mutation. Existing record payloads are shared,
   * so their full graphs are not multiplied into the transaction allowance. */
  private change<T>(
    input: unknown,
    fallback: T,
    update: () => T,
    replacing = false,
    growthBytes = 0,
  ): T {
    if (this.transaction) return update();
    const previous = this.state;
    const pressure = this.pressureState;
    const sequences = this.sequences;
    const orders = this.listingOrders;
    const cancelled = this.cancelPending;
    let memory: FrontendReservation | undefined;
    try {
      const bytes =
        16 * 1024 +
        this.budget.untrackedBytes(input) +
        growthBytes +
        (replacing ? 0 : 3 * this.collectionBytes());
      memory = this.budget.reserve(bytes);
      this.transaction = memory;
      this.budget.retain(input, memory);
      const result = update();
      this.budget.retain(this.sequences, memory);
      this.budget.retain(this.listingOrders, memory);
      this.budget.retain(this.cancelPending, memory);
      return result;
    } catch (error) {
      this.state = previous;
      this.pressureState = pressure;
      this.sequences = sequences;
      this.listingOrders = orders;
      this.cancelPending = cancelled;
      if (error instanceof Error && error.message === 'RESOURCE_LIMIT') {
        // Already built with the current snapshot: signaling pressure allocates
        // no new observation when another owner has exhausted the shared pool.
        this.state = pressure;
        return fallback;
      }
      throw error;
    } finally {
      this.transaction = undefined;
      memory?.close();
      if (this.state !== previous)
        this.listeners.forEach((listener) => listener());
    }
  }
  private publish(next: ExplorerState) {
    if (!this.transaction) throw new Error('INVALID_RESERVATION');
    this.budget.retain(next, this.transaction);
    const pressure = next.resourceLimited
      ? next
      : { ...next, resourceLimited: true };
    this.budget.retain(pressure, this.transaction);
    this.state = next;
    this.pressureState = pressure;
  }
  reset(root: RootData | null) {
    if (root === null) {
      const before = this.state;
      const closed = this.closed[before.mode];
      this.state = closed.state;
      this.pressureState = closed.pressure;
      this.sequences = this.emptySequences;
      this.listingOrders = this.emptyOrders;
      this.cancelPending = this.emptyCancelled;
      if (!this.transaction && before !== this.state)
        this.listeners.forEach((listener) => listener());
      return true;
    }
    return this.change(
      root,
      false,
      () => {
        this.sequences = new Map();
        this.listingOrders = new Map();
        this.cancelPending = new Set();
        const state = emptyState();
        state.mode = this.state.mode;
        if (root) {
          state.session = root.session;
          state.entries = new Map([[root.root.entryId, root.root]]);
        }
        this.publish(state);
        return true;
      },
      true,
    );
  }
  failRoot(scope: Scope, issue: FsIssue) {
    return this.change(
      { scope, issue },
      false,
      () => {
        this.reset(null);
        this.publish({ ...this.state, failedRoot: { scope, issue } });
        return true;
      },
      true,
    );
  }
  setMode(mode: ExplorerState['mode']) {
    return this.change(mode, undefined, () => {
      this.publish({ ...this.state, mode });
    });
  }
  setExpanded(
    id: string,
    expanded: boolean,
    category: Category = 'directories',
  ) {
    if (this.state.entries.get(id)?.kind !== 'directory') return;
    const key = category === 'directories' ? 'expandedIds' : 'openFileListIds';
    return this.change(id, undefined, () => {
      const ids = new Set(this.state[key]);
      if (expanded) ids.add(id);
      else ids.delete(id);
      this.publish({ ...this.state, [key]: ids });
    });
  }
  select(id: string | null) {
    if (id === null || this.state.entries.has(id))
      this.change(id, undefined, () =>
        this.publish({ ...this.state, selectedId: id }),
      );
  }
  getListing(id: string, category: Category): DirectoryListing {
    return (
      this.state.listings.get(listingKey(id, category)) ?? {
        directoryId: id,
        category,
        loadState: 'notRequested',
      }
    );
  }
  private acceptsTask(work: WorkRecord): boolean {
    if (!sameScope(this.state.session, work)) return false;
    const prior = this.state.tasks.get(work.taskId);
    if (
      prior &&
      (work.sequence < prior.sequence ||
        (terminal(prior.phase) && work.phase !== prior.phase))
    )
      return false;
    if (prior && work.sequence === prior.sequence && work.phase !== prior.phase)
      return false;
    if (this.cancelPending.has(work.taskId) && !terminal(work.phase))
      return false;
    return true;
  }
  applyTask(work: WorkRecord): boolean {
    if (!this.acceptsTask(work)) return false;
    return this.change(work, false, () => {
      const tasks = new Map(this.state.tasks).set(work.taskId, work);
      this.publish({ ...this.state, tasks });
      return true;
    });
  }
  registerListing(start: ListingStart, requestOrder: number): boolean {
    if (!sameScope(this.state.session, start.work)) return false;
    const key = listingKey(start.directoryId, start.category);
    if (requestOrder < (this.listingOrders.get(key) ?? -1)) return false;
    if (!this.acceptsTask(start.work)) return false;
    return this.change(start, false, () => {
      const tasks = new Map(this.state.tasks).set(
        start.work.taskId,
        start.work,
      );
      this.listingOrders = new Map(this.listingOrders).set(key, requestOrder);
      const prior = this.state.listings.get(key);
      if (prior?.snapshot?.listingRevision === start.listingRevision) {
        this.publish({ ...this.state, tasks });
        return true;
      }
      const listings = new Map(this.state.listings).set(key, {
        directoryId: start.directoryId,
        category: start.category,
        taskId: start.work.taskId,
        loadState: 'loading' as const,
        snapshot: {
          listingRevision: start.listingRevision,
          entryIds: [],
          nextCursor: null,
          coverage: 'partial' as const,
          observedAt: start.work.observedAt,
          issues: start.work.issues,
        },
      });
      this.publish({ ...this.state, tasks, listings });
      return true;
    });
  }
  acceptEvent(event: WorkEvent): boolean {
    const task = this.state.tasks.get(event.taskId);
    if (
      event.protocolVersion !== 1 ||
      !sameScope(this.state.session, event) ||
      !task ||
      terminal(task.phase) ||
      (this.cancelPending.has(event.taskId) && event.kind !== 'terminal')
    )
      return false;
    if (
      event.sequence <=
      Math.max(this.sequences.get(event.taskId) ?? -1, task.sequence)
    )
      return false;
    return this.change(event, false, () => {
      this.sequences = new Map(this.sequences).set(
        event.taskId,
        event.sequence,
      );
      return true;
    });
  }
  markCancelPending(taskId: string) {
    if (!this.state.tasks.has(taskId)) return;
    this.change(taskId, undefined, () => {
      this.cancelPending = new Set(this.cancelPending).add(taskId);
    });
  }
  applyUsage(scope: Scope, usages: DirectoryUsage[]) {
    if (!sameScope(this.state.session, scope)) return;
    let growth = 0;
    for (const usage of usages) growth += 512 + 2 * usage.directoryId.length;
    return this.change(
      usages,
      undefined,
      () => {
        const next = new Map(this.state.usages);
        for (const usage of usages) {
          if (this.state.entries.get(usage.directoryId)?.kind !== 'directory')
            continue;
          if (
            (next.get(usage.directoryId)?.usageRevision ?? -1) >=
            usage.usageRevision
          )
            continue;
          next.set(usage.directoryId, usage);
        }
        this.publish({ ...this.state, usages: next });
      },
      false,
      growth,
    );
  }
  applyPage(page: ListingPage): boolean {
    if (!sameScope(this.state.session, page.work)) return false;
    const key = listingKey(page.directoryId, page.category);
    const listing = this.state.listings.get(key);
    if (
      listing?.taskId !== page.work.taskId ||
      listing.snapshot?.listingRevision !== page.listingRevision
    )
      return false;
    // Only accept the next expected page. Retransmission cannot append duplicate IDs.
    if (
      page.cursor !== listing.snapshot.nextCursor ||
      (listing.loadState !== 'loading' && page.cursor === null)
    )
      return false;
    const snapshot = listing.snapshot;
    const latestWork = this.state.tasks.get(page.work.taskId);
    if (this.cancelPending.has(page.work.taskId) && !terminal(page.work.phase))
      return false;
    if (
      latestWork &&
      terminal(latestWork.phase) &&
      latestWork.phase !== 'completed' &&
      page.work.phase !== latestWork.phase
    )
      return false;
    const effectiveWork =
      latestWork && latestWork.sequence > page.work.sequence
        ? latestWork
        : page.work;
    let growth = 0;
    for (const entry of page.entries) {
      if (
        entry.parentId !== page.directoryId ||
        (page.category === 'directories') !== (entry.kind === 'directory')
      )
        return false;
      // New entry-map slots plus ID arrays, their concatenation and dedup Set.
      growth += 2048 + 8 * entry.entryId.length;
    }
    return this.change(
      page,
      false,
      () => {
        const entries = new Map(this.state.entries);
        for (const entry of page.entries) {
          if (
            entry.parentId !== page.directoryId ||
            (page.category === 'directories') !== (entry.kind === 'directory')
          )
            return false;
          entries.set(entry.entryId, entry);
        }
        const entryIds = [
          ...new Set([
            ...snapshot.entryIds,
            ...page.entries.map((entry) => entry.entryId),
          ]),
        ];
        const nextListing: DirectoryListing = {
          ...listing,
          loadState:
            !terminal(effectiveWork.phase) || page.nextCursor !== null
              ? 'loading'
              : effectiveWork.phase === 'completed'
                ? 'settled'
                : effectiveWork.phase,
          snapshot: {
            listingRevision: page.listingRevision,
            entryIds,
            nextCursor: page.nextCursor,
            coverage: page.coverage,
            observedAt: page.work.observedAt,
            issues: page.issues,
          },
        };
        const listings = new Map(this.state.listings).set(key, nextListing);
        const tasks = this.acceptsTask(page.work)
          ? new Map(this.state.tasks).set(page.work.taskId, page.work)
          : this.state.tasks;
        this.publish({ ...this.state, entries, listings, tasks });
        return true;
      },
      false,
      growth,
    );
  }
}
