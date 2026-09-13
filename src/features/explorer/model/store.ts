import {
  listingKey,
  sameScope,
  terminal,
  type Category,
  type DirectoryListing,
  type DirectoryUsage,
  type Entry,
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
  private state = emptyState();
  private listeners = new Set<() => void>();
  private sequences = new Map<string, number>();
  private listingOrders = new Map<string, number>();
  private cancelPending = new Set<string>();
  constructor(private readonly maxBytes = 32 * 1024 * 1024) {}
  getSnapshot = () => this.state;
  subscribe = (listener: () => void) => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };
  private publish(next: ExplorerState) {
    this.state = next;
    this.listeners.forEach((listener) => listener());
  }
  reset(root: RootData | null) {
    this.sequences.clear();
    this.listingOrders.clear();
    this.cancelPending.clear();
    const state = emptyState();
    state.mode = this.state.mode;
    if (root) {
      state.session = root.session;
      state.entries = new Map([[root.root.entryId, root.root]]);
    }
    this.publish(state);
  }
  setMode(mode: ExplorerState['mode']) {
    this.publish({ ...this.state, mode });
  }
  setExpanded(
    id: string,
    expanded: boolean,
    category: Category = 'directories',
  ) {
    if (this.state.entries.get(id)?.kind !== 'directory') return;
    const key = category === 'directories' ? 'expandedIds' : 'openFileListIds';
    const ids = new Set(this.state[key]);
    if (expanded) ids.add(id);
    else ids.delete(id);
    this.publish({ ...this.state, [key]: ids });
  }
  select(id: string | null) {
    if (id === null || this.state.entries.has(id))
      this.publish({ ...this.state, selectedId: id });
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
  applyTask(work: WorkRecord): boolean {
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
    const tasks = new Map(this.state.tasks).set(work.taskId, work);
    this.publish({ ...this.state, tasks });
    return true;
  }
  registerListing(start: ListingStart, requestOrder: number): boolean {
    if (!sameScope(this.state.session, start.work)) return false;
    const key = listingKey(start.directoryId, start.category);
    if (requestOrder < (this.listingOrders.get(key) ?? -1)) return false;
    if (!this.applyTask(start.work)) return false;
    this.listingOrders.set(key, requestOrder);
    const prior = this.state.listings.get(key);
    if (prior?.snapshot?.listingRevision === start.listingRevision) return true;
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
    this.publish({ ...this.state, listings });
    return true;
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
    this.sequences.set(event.taskId, event.sequence);
    return true;
  }
  markCancelPending(taskId: string) {
    this.cancelPending.add(taskId);
  }
  applyUsage(scope: Scope, usages: DirectoryUsage[]) {
    if (!sameScope(this.state.session, scope)) return;
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
        ...listing.snapshot.entryIds,
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
    // Conservative accounting includes UTF-16 strings, containers and normalized records.
    const bytes =
      JSON.stringify([...entries]).length * 2 +
      JSON.stringify([...listings]).length * 2 +
      entries.size * 256;
    if (bytes > this.maxBytes) {
      this.publish({ ...this.state, resourceLimited: true });
      return false;
    }
    this.applyTask(page.work);
    this.publish({ ...this.state, entries, listings });
    return true;
  }
}
