import type {
  DirectoryListing,
  Entry,
  DirectoryUsage,
  WorkRecord,
} from './types';
import type { ExplorerState } from './store';
import type { FrontendBudget, FrontendReservation } from '../runtime/memory';

/** Build while a normal store transaction has spare capacity. Pressure later
 * switches to this already-owned snapshot without allocating or mutating old
 * snapshots. Kept descendants retain their ancestor metadata for navigation. */
export function compactClosed(
  state: ExplorerState,
  budget: FrontendBudget,
  memory: FrontendReservation,
): ExplorerState {
  const selected = state.selectedId ?? state.session?.rootEntryId;
  const visible = (id: string, files: boolean) =>
    files
      ? state.openFileListIds.has(id)
      : id === selected || state.expandedIds.has(id);
  let closed = false;
  const retained = new Set<string>();
  const tasks = new Set<string>();
  if (state.session) retained.add(state.session.rootEntryId);
  if (selected) retained.add(selected);
  for (const id of state.expandedIds) retained.add(id);
  for (const id of state.openFileListIds) retained.add(id);
  for (const listing of state.listings.values()) {
    if (!visible(listing.directoryId, listing.category === 'files')) {
      closed = true;
      continue;
    }
    retained.add(listing.directoryId);
    if (listing.taskId) tasks.add(listing.taskId);
    for (const id of listing.snapshot?.entryIds ?? []) retained.add(id);
  }
  // Set iteration includes newly added ancestors; each ID is visited once.
  for (const id of retained) {
    const parent = state.entries.get(id)?.parentId;
    if (parent) retained.add(parent);
  }
  // A replaced revision can leave old entries/tasks even though its new
  // listing is open. Those old observations are no longer retained pages.
  for (const id of state.entries.keys()) if (!retained.has(id)) closed = true;
  for (const [id, task] of state.tasks)
    if (task.operation !== 'scanUsage' && !tasks.has(id)) closed = true;
  budget.retain(retained, memory);
  budget.retain(tasks, memory);
  if (!closed) return state;
  const listings = new Map<string, DirectoryListing>();
  for (const [key, listing] of state.listings)
    if (visible(listing.directoryId, listing.category === 'files'))
      listings.set(key, listing);
  const entries = new Map<string, Entry>();
  const usages = new Map<string, DirectoryUsage>();
  const work = new Map<string, WorkRecord>();
  for (const [id, entry] of state.entries)
    if (retained.has(id)) entries.set(id, entry);
  for (const [id, usage] of state.usages)
    if (retained.has(id)) usages.set(id, usage);
  for (const [id, task] of state.tasks)
    if (task.operation === 'scanUsage' || tasks.has(id)) work.set(id, task);
  return { ...state, entries, listings, usages, tasks: work };
}
