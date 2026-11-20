import {
  sameScope,
  terminal,
  type RootSession,
  type ListingStart,
} from '../model/types';
import type { SessionClient } from './session-client';

/** Only one directory read is in flight. Native listing jobs yield between I/O
 * batches; interactive file reads never have to wait behind a tree of jobs. */
export function discoverDirectories(
  client: SessionClient,
  scope: RootSession,
  onError: (error: unknown) => void,
  onDone: () => void = () => {},
  onResume: () => void = () => {},
) {
  const root = scope.rootEntryId;
  const queue = new Set<string>();
  const visited = new Map<string, boolean>();
  let directory: string | undefined = root;
  let viewportDriven = false;
  let idle = false;
  let start: ListingStart | undefined;
  let cursor: string | null = null;
  let stopped = false;
  let busy = false;
  let timer: ReturnType<typeof setTimeout> | undefined;
  const current = () => sameScope(client.store.getSnapshot().session, scope);
  const schedule = (delay = 0) => {
    if (!current() || stopped || idle || busy || timer !== undefined) return;
    timer = setTimeout(() => {
      timer = undefined;
      void step();
    }, delay);
  };
  const unsubscribe = client.store.subscribe(schedule);
  function stop() {
    if (stopped) return;
    stopped = true;
    queue.clear();
    visited.clear();
    unsubscribe();
    clearTimeout(timer);
    if (start && !terminal(start.work.phase) && current())
      void client.cancel(start.work.taskId).catch((error) => {
        if (current()) onError(error);
      });
  }
  function advance() {
    start = undefined;
    cursor = null;
    if (directory !== undefined)
      visited.set(
        directory,
        client.store.getListing(directory, 'directories').loadState !==
          'notRequested',
      );
    directory = undefined;
  }
  async function step() {
    if (
      stopped ||
      !current() ||
      client.store.getSnapshot().resourceLimited ||
      client.hasFileDemand()
    )
      return;
    let delay = 0;
    busy = true;
    try {
      if (directory === undefined) {
        directory = queue.values().next().value;
        if (directory !== undefined) queue.delete(directory);
      }
      if (directory === undefined) {
        if (viewportDriven) {
          idle = true;
          onDone();
          return;
        }
        // Include nodes discovered after the last usage notification.
        await client.readUsage(scope);
        if (!stopped && current()) onDone();
        stop();
        return;
      }
      if (directory === root) {
        // The normal session opening flow owns the first root listing.
        const listing = client.store.getListing(root, 'directories');
        if (
          listing.loadState === 'notRequested' ||
          listing.loadState === 'loading'
        ) {
          if (listing.snapshot?.nextCursor && listing.taskId) {
            await client.nextPage(listing.taskId);
            delay = 250;
          } else delay = 50;
          return;
        }
        if (!viewportDriven)
          for (const id of listing.snapshot?.entryIds ?? []) queue.add(id);
        advance();
      } else if (!start) {
        start = await client.startTreeListing(scope, directory);
        if (stopped && current()) await client.cancel(start.work.taskId);
      } else {
        const page = await client.readTreePage(start, cursor);
        if (stopped || !current()) return;
        if (!client.store.applyPage(page, true)) {
          if (client.store.getSnapshot().resourceLimited) return;
          throw new Error('INVALID_RESPONSE');
        }
        if (!viewportDriven)
          for (const child of page.entries) queue.add(child.entryId);
        cursor = page.nextCursor;
        if (!cursor && terminal(page.work.phase)) advance();
        else if (!page.entries.length) delay = 40;
      }
    } catch (error) {
      if (stopped || !current()) return;
      if (error instanceof Error && error.message === 'RESOURCE_LIMIT') {
        client.store.suspendObservations();
      } else {
        if (directory === undefined) {
          stop();
          onDone();
        } else advance();
      }
      onError(error);
    } finally {
      busy = false;
      schedule(delay);
    }
  }
  schedule();
  function setViewport(ids: readonly string[]) {
    if (stopped) return;
    viewportDriven = true;
    if (client.store.trimAutomaticBranches(ids, directory)) {
      const state = client.store.getSnapshot();
      for (const [id, hadListing] of visited) {
        if (
          !state.entries.has(id) ||
          (hadListing &&
            client.store.getListing(id, 'directories').loadState ===
              'notRequested')
        )
          visited.delete(id);
      }
    }
    queue.clear();
    for (const id of ids) {
      if (id !== directory && !visited.has(id)) queue.add(id);
    }
    if (idle && queue.size) {
      idle = false;
      onResume();
    }
    schedule();
  }
  return Object.assign(stop, { setViewport });
}
