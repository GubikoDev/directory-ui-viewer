import { afterEach, describe, expect, it, vi } from 'vitest';
import { FixtureAdapter, fixtureEntry } from '../adapters/fixture';
import { ExplorerStore } from '../model/store';
import { FrontendBudget } from '../runtime/memory';
import { SessionClient, unwrap } from './session-client';
import type { Request, Response, RootData } from '../model/types';
const clients: SessionClient[] = [];
function setup(adapter = new FixtureAdapter(), budget?: FrontendBudget) {
  const client = new SessionClient(
    adapter,
    new ExplorerStore(budget),
    'test-document',
  );
  clients.push(client);
  return { client, adapter, store: client.store };
}
afterEach(() => {
  clients.forEach((client) => client.dispose());
  clients.length = 0;
  vi.useRealTimers();
});
describe('SessionClient with deterministic adapter', () => {
  it('reports an accepted close while further observation admission remains unavailable', async () => {
    const budget = new FrontendBudget();
    const { client, adapter, store } = setup(undefined, budget);
    await client.chooseRoot();
    await client.list('B', 'files');
    const starts = adapter.calls.startListing;
    const pressure = vi.spyOn(budget, 'reserve').mockImplementation(() => {
      throw new Error('RESOURCE_LIMIT');
    });
    expect(client.closeListing('B', 'files')).toBe(true);
    expect(store.getListing('B', 'files').loadState).toBe('notRequested');
    expect(store.getSnapshot().resourceLimited).toBe(true);
    pressure.mockRestore();
    expect(await client.retryObservations()).toBe(true);
    expect(store.getSnapshot().resourceLimited).toBe(false);
    expect(adapter.calls.startListing).toBe(starts);
  });
  it('recovers two pending usage requests through replay of the same native task', async () => {
    const budget = new FrontendBudget();
    const { client, adapter, store } = setup(undefined, budget);
    await client.chooseRoot();
    const state = store.getSnapshot();
    store.reset({ session: state.session!, root: state.entries.get('A')! });
    let release!: () => void;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    let reached = 0;
    const original = adapter.startUsageScan.bind(adapter);
    const delay = async (input: Parameters<typeof original>[0]) => {
      const response = await original(input);
      reached++;
      await gate;
      return response;
    };
    const start = vi
      .spyOn(adapter, 'startUsageScan')
      .mockImplementationOnce(delay)
      .mockImplementationOnce(delay);
    const first = client.scan();
    const second = client.scan();
    await vi.waitFor(() => expect(reached).toBe(2));
    const pressure = budget.reserve(budget.available);
    release();
    await Promise.all([first, second]);
    expect(store.getSnapshot().resourceLimited).toBe(true);
    expect(store.getSnapshot().tasks.size).toBe(0);
    pressure.close();
    expect(await client.retryObservations()).toBe(true);
    expect(start).toHaveBeenCalledTimes(4);
    expect(start.mock.calls[2][0]).toBe(start.mock.calls[0][0]);
    expect(start.mock.calls[3][0]).toBe(start.mock.calls[1][0]);
    const responses = await Promise.all(
      start.mock.results.map((call) => call.value),
    );
    expect(responses[2]).toBe(responses[0]);
    expect(responses[3]).toBe(responses[1]);
    const ids = responses.map(
      (response) => response.outcome === 'ok' && response.data.taskId,
    );
    expect(new Set(ids).size).toBe(1);
    expect([...store.getSnapshot().tasks.keys()]).toEqual([ids[0]]);
  });
  it.each(['listing', 'usage'] as const)(
    'replays the original %s request after registration pressure without a new effect',
    async (kind) => {
      const budget = new FrontendBudget();
      const { client, adapter, store } = setup(undefined, budget);
      await client.chooseRoot();
      if (kind === 'usage') {
        const state = store.getSnapshot();
        store.reset({
          session: state.session!,
          root: state.entries.get(state.session!.rootEntryId)!,
        });
      }
      const listing = adapter.startListing.bind(adapter);
      const usage = adapter.startUsageScan.bind(adapter);
      let pressure!: ReturnType<FrontendBudget['reserve']>;
      const exhaustAfter = async <T>(pending: Promise<T>): Promise<T> => {
        const response = await pending;
        pressure = budget.reserve(budget.available);
        return response;
      };
      const calls =
        kind === 'listing'
          ? vi
              .spyOn(adapter, 'startListing')
              .mockImplementationOnce((input) => exhaustAfter(listing(input)))
          : vi
              .spyOn(adapter, 'startUsageScan')
              .mockImplementationOnce((input) => exhaustAfter(usage(input)));
      await (kind === 'listing' ? client.list('B', 'files') : client.scan());
      expect(store.getSnapshot().resourceLimited).toBe(true);
      expect(
        [...store.getSnapshot().tasks.values()].some(
          (task) =>
            task.operation === (kind === 'listing' ? 'listFiles' : 'scanUsage'),
        ),
      ).toBe(false);
      const first = calls.mock.calls[0][0];
      const response = await calls.mock.results[0].value;
      expect(response.outcome).toBe('ok');
      pressure.close();
      expect(await client.retryObservations()).toBe(true);
      expect(calls).toHaveBeenCalledTimes(2);
      expect(calls.mock.calls[1][0]).toBe(first);
      const replay = await calls.mock.results[1].value;
      expect(replay).toBe(response);
      if (response.outcome === 'ok') {
        const task =
          'work' in response.data ? response.data.work : response.data;
        expect(store.getSnapshot().tasks.get(task.taskId)).toEqual(task);
      }
      await client.retryObservations();
      expect(calls).toHaveBeenCalledTimes(2);
    },
  );
  it.each([
    'expired',
    'nativeResourceError',
    'closed',
    'closedBeforeDispatch',
    'superseded',
  ] as const)(
    'does not resurrect or restart a rejected registration when %s',
    async (ending) => {
      const budget = new FrontendBudget();
      const { client, adapter, store } = setup(undefined, budget);
      await client.chooseRoot();
      const original = adapter.startListing.bind(adapter);
      let pressure!: ReturnType<FrontendBudget['reserve']>;
      const start = vi
        .spyOn(adapter, 'startListing')
        .mockImplementationOnce(async (input) => {
          const response = await original(input);
          pressure = budget.reserve(budget.available);
          return response;
        });
      await client.list('B', 'files');
      expect(store.getSnapshot().resourceLimited).toBe(true);
      pressure.close();
      if (ending === 'expired' || ending === 'nativeResourceError') {
        const code =
          ending === 'expired' ? 'REQUEST_EXPIRED' : 'RESOURCE_LIMIT';
        start.mockImplementationOnce(async (input) => ({
          requestId: input.requestId,
          outcome: 'error',
          error: {
            code,
            operation: 'startListing',
            scope: 'root',
          },
        }));
        await expect(client.retryObservations()).rejects.toThrow(code);
        expect(start.mock.calls[1][0]).toBe(start.mock.calls[0][0]);
      } else if (ending === 'closed') {
        client.closeListing('B', 'files');
      } else if (ending === 'closedBeforeDispatch') {
        const retry = client.retryObservations();
        client.closeListing('B', 'files');
        await retry;
        expect(start).toHaveBeenCalledOnce();
      } else {
        await client.list('B', 'files');
        expect(start.mock.calls[1][0].requestId).not.toBe(
          start.mock.calls[0][0].requestId,
        );
      }
      const count = start.mock.calls.length;
      await client.retryObservations();
      expect(start).toHaveBeenCalledTimes(count);
      expect(store.getListing('B', 'files').loadState).toBe(
        ending === 'superseded' ? 'settled' : 'notRequested',
      );
    },
  );
  it.each(['start', 'page'] as const)(
    'drops a late %s response after closing a file listing',
    async (stage) => {
      const { client, adapter, store } = setup();
      await client.chooseRoot();
      let release!: () => void;
      const gate = new Promise<void>((resolve) => {
        release = resolve;
      });
      let reached = false;
      if (stage === 'start') {
        const original = adapter.startListing.bind(adapter);
        vi.spyOn(adapter, 'startListing').mockImplementation(async (input) => {
          const response = await original(input);
          reached = true;
          await gate;
          return response;
        });
      } else {
        const original = adapter.readListingPage.bind(adapter);
        vi.spyOn(adapter, 'readListingPage').mockImplementation(
          async (input) => {
            const response = await original(input);
            reached = true;
            await gate;
            return response;
          },
        );
      }
      const pending = client.list('B', 'files');
      await vi.waitFor(() => expect(reached).toBe(true));
      client.closeListing('B', 'files');
      release();
      await pending;
      expect(store.getListing('B', 'files').loadState).toBe('notRequested');
      expect(
        [...store.getSnapshot().entries.values()].some(
          (entry) => entry.kind === 'regularFile',
        ),
      ).toBe(false);
      expect(
        [...store.getSnapshot().tasks.values()].some(
          (work) => work.operation === 'listFiles',
        ),
      ).toBe(false);
    },
  );
  it('drops a late task recovery after reclaiming its listing', async () => {
    vi.useFakeTimers();
    const { client, adapter, store } = setup();
    await client.chooseRoot();
    adapter.deferTasks = true;
    await client.list('B', 'files');
    const task = store.getListing('B', 'files').taskId!;
    let release!: () => void;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    const original = adapter.readTask.bind(adapter);
    const read = vi
      .spyOn(adapter, 'readTask')
      .mockImplementation(async (input) => {
        const response = await original(input);
        await gate;
        return response;
      });
    vi.setSystemTime(Date.now() + 2000);
    const pending = client.poll();
    expect(read).toHaveBeenCalledOnce();
    client.closeListing('B', 'files');
    release();
    await pending;
    expect(store.getSnapshot().tasks.has(task)).toBe(false);
    expect(store.getListing('B', 'files').loadState).toBe('notRequested');
  });
  it('retries a pressure-rejected page with the same task and cursor without restarting scans', async () => {
    vi.useFakeTimers();
    const adapter = new FixtureAdapter({
      entries: [
        fixtureEntry('A', null),
        ...Array.from({ length: 450 }, (_, i) =>
          fixtureEntry('child-' + i, 'A'),
        ),
      ],
    });
    const budget = new FrontendBudget();
    const { client, store } = setup(adapter, budget);
    await client.chooseRoot();
    const task = store.getListing('A', 'directories').taskId!;
    const starts = {
      listing: adapter.calls.startListing,
      scan: adapter.calls.startUsageScan,
    };
    const original = adapter.readListingPage.bind(adapter);
    let pressure!: ReturnType<FrontendBudget['reserve']>;
    vi.spyOn(adapter, 'readListingPage').mockImplementationOnce(
      async (input) => {
        const response = await original(input);
        pressure = budget.reserve(budget.available);
        return response;
      },
    );
    vi.setSystemTime(Date.now() + 250);
    expect(await client.nextPage(task)).toBe(false);
    expect(store.getSnapshot().resourceLimited).toBe(true);
    pressure.close();
    vi.setSystemTime(Date.now() + 250);
    expect(await client.retryObservations()).toBe(true);
    expect(
      store.getListing('A', 'directories').snapshot?.entryIds,
    ).toHaveLength(400);
    expect(store.getSnapshot().resourceLimited).toBe(false);
    expect(adapter.calls.startListing).toBe(starts.listing);
    expect(adapter.calls.startUsageScan).toBe(starts.scan);
  });
  it('keeps two dispatch slots for cancel and close behind a full ordinary queue', async () => {
    const { client, adapter, store } = setup();
    await client.chooseRoot();
    const task = [...store.getSnapshot().tasks.values()][0];
    const ordinary = Array.from({ length: 62 }, () =>
      client.list('B', 'directories'),
    );
    const overflow = client.list('B', 'directories');
    const cancelled = client.cancel(task.taskId);
    const closed = client.close();
    const results = await Promise.allSettled([
      ...ordinary,
      overflow,
      cancelled,
      closed,
    ]);
    expect(results[62]).toMatchObject({
      status: 'rejected',
      reason: { message: 'RESOURCE_LIMIT' },
    });
    expect(results[63].status).toBe('fulfilled');
    expect(results[64].status).toBe('fulfilled');
    expect(adapter.calls.cancelTask).toBe(1);
    expect(adapter.calls.closeSession).toBe(1);
    expect(store.getSnapshot().session).toBeNull();
  });
  it('returns accepted cancellation without waiting for a recovery read', async () => {
    const { client, adapter, store } = setup();
    adapter.deferTasks = true;
    adapter.dropEvents = true;
    await client.chooseRoot();
    const task = [...store.getSnapshot().tasks.values()][0];
    const original = adapter.readTask.bind(adapter);
    let release!: () => void;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    const read = vi
      .spyOn(adapter, 'readTask')
      .mockImplementation(async (request) => {
        await gate;
        return original(request);
      });
    await client.cancel(task.taskId);
    expect(read).toHaveBeenCalledOnce();
    expect(store.getSnapshot().tasks.get(task.taskId)?.phase).toBe('cancelled');
    release();
    await client.close();
  });
  it('accepts cancellation under data pressure and reports recovery failure separately', async () => {
    const budget = new FrontendBudget(undefined, undefined, 1024 * 1024);
    const { client, adapter, store } = setup(new FixtureAdapter(), budget);
    adapter.deferTasks = true;
    adapter.dropEvents = true;
    await client.chooseRoot();
    const task = [...store.getSnapshot().tasks.values()][0];
    const blocker = budget.reserve(budget.available);
    try {
      await expect(client.cancel(task.taskId)).resolves.toBeUndefined();
      expect(adapter.calls.cancelTask).toBe(1);
      expect(client.errors.at(-1)?.message).toBe('RESOURCE_LIMIT');
    } finally {
      blocker.close();
    }
  });
  it('dispatches close while a cancellation response remains unresolved', async () => {
    const { client, adapter, store } = setup();
    adapter.deferTasks = true;
    await client.chooseRoot();
    const task = [...store.getSnapshot().tasks.values()][0];
    const original = adapter.cancelTask.bind(adapter);
    let release!: () => void;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    const cancel = vi
      .spyOn(adapter, 'cancelTask')
      .mockImplementation(async (request) => {
        const response = await original(request);
        await gate;
        return response;
      });
    const pending = client.cancel(task.taskId);
    await vi.waitFor(() => expect(cancel).toHaveBeenCalledOnce());
    await client.close();
    expect(store.getSnapshot().session).toBeNull();
    release();
    await pending;
    expect(store.getSnapshot().session).toBeNull();
  });
  it('does not throw a synchronous recovery admission failure through an event producer', async () => {
    const budget = new FrontendBudget();
    const { client, adapter, store } = setup(new FixtureAdapter(), budget);
    adapter.deferTasks = true;
    await client.chooseRoot();
    const task = [...store.getSnapshot().tasks.values()][0];
    vi.spyOn(store, 'acceptEvent').mockReturnValue(true);
    const blocker = budget.reserve(budget.limit - budget.used);
    expect(() => adapter.finish(task.taskId)).not.toThrow();
    expect(client.errors.at(-1)?.message).toBe('RESOURCE_LIMIT');
    blocker.close();
  });
  it('does not hold close behind an unresolved listing start response', async () => {
    const { client, adapter, store } = setup();
    await client.chooseRoot();
    const original = adapter.startListing.bind(adapter);
    let release!: () => void;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    const start = vi
      .spyOn(adapter, 'startListing')
      .mockImplementation(async (request) => {
        const response = await original(request);
        await gate;
        return response;
      });
    const listing = client.list('B', 'directories');
    await vi.waitFor(() => expect(start).toHaveBeenCalledOnce());
    await client.close();
    expect(store.getSnapshot().session).toBeNull();
    release();
    await listing;
    expect(store.getSnapshot().session).toBeNull();
  });
  it('bounds queued dispatch and drops old lifecycle work before adapter invocation', async () => {
    const { client, adapter } = setup();
    await client.chooseRoot();
    const before = adapter.calls.startListing;
    const requests = Array.from({ length: 65 }, () =>
      client.list('B', 'directories'),
    );
    client.dispose();
    const results = await Promise.allSettled(requests);
    expect(
      results
        .slice(0, 62)
        .every(
          (result) =>
            result.status === 'rejected' &&
            result.reason.message === 'CLIENT_EXPIRED',
        ),
    ).toBe(true);
    expect(results[62]).toMatchObject({
      status: 'rejected',
      reason: { message: 'RESOURCE_LIMIT' },
    });
    expect(adapter.calls.startListing).toBe(before);
  });
  it('coalesces polling across a slow task response and stops on close', async () => {
    vi.useFakeTimers();
    const { client, adapter } = setup();
    adapter.deferTasks = true;
    adapter.dropEvents = true;
    await client.chooseRoot();
    const original = adapter.readTask.bind(adapter);
    let release!: () => void;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    const read = vi
      .spyOn(adapter, 'readTask')
      .mockImplementation(async (request) => {
        await gate;
        return original(request);
      });
    vi.setSystemTime(Date.now() + 2000);
    const first = client.poll();
    expect(client.poll()).toBe(first);
    expect(read).toHaveBeenCalledOnce();
    await client.close();
    release();
    await first;
    expect(read).toHaveBeenCalledOnce();
  });
  it('admits usage ID arrays before starting an adapter read', async () => {
    const budget = new FrontendBudget();
    const { client, adapter } = setup(new FixtureAdapter(), budget);
    await client.chooseRoot();
    const calls = adapter.calls.readUsage;
    const blocker = budget.reserve(budget.limit - budget.used);
    await expect(client.readUsage()).rejects.toThrow('RESOURCE_LIMIT');
    expect(adapter.calls.readUsage).toBe(calls);
    blocker.close();
  });
  it('keeps native control ownership when an admitted root cannot fit the view', async () => {
    const budget = new FrontendBudget(undefined, undefined, 1024 * 1024);
    const { client, adapter, store } = setup(new FixtureAdapter(), budget);
    let blocker!: ReturnType<FrontendBudget['reserve']>;
    const choose = adapter.chooseRoot.bind(adapter);
    vi.spyOn(adapter, 'chooseRoot').mockImplementation(async (request) => {
      const result = await choose(request);
      blocker = budget.reserve(budget.available);
      return result;
    });
    try {
      await expect(client.chooseRoot()).rejects.toThrow('RESOURCE_LIMIT');
      expect(client.hasSession).toBe(true);
      expect(store.getSnapshot().session).toBeNull();
      expect(adapter.calls.startUsageScan ?? 0).toBe(0);
      expect(adapter.calls.startListing ?? 0).toBe(0);
      await client.close();
      expect(adapter.calls.closeSession).toBe(1);
      expect(client.hasSession).toBe(false);
    } finally {
      blocker?.close();
    }
  });
  it('does not invoke root selection when dispatch admission fails', async () => {
    const budget = new FrontendBudget();
    const { client, adapter } = setup(new FixtureAdapter(), budget);
    await client.connect();
    const blocker = budget.reserve(budget.available);
    try {
      await expect(client.chooseRoot()).rejects.toThrow('RESOURCE_LIMIT');
      expect(adapter.calls.chooseRoot ?? 0).toBe(0);
      expect(client.hasSession).toBe(false);
    } finally {
      blocker.close();
    }
  });
  it('retries the native advanced generation even when its failure view was rejected', async () => {
    const budget = new FrontendBudget();
    const { client, adapter, store } = setup(new FixtureAdapter(), budget);
    await client.chooseRoot();
    let blocker!: ReturnType<FrontendBudget['reserve']>;
    const refresh = adapter.refreshRoot.bind(adapter);
    vi.spyOn(adapter, 'refreshRoot').mockImplementationOnce(async (request) => {
      const result = await refresh(request);
      blocker = budget.reserve(budget.available);
      return result;
    });
    adapter.failNextRefresh = true;
    try {
      await expect(client.refresh()).rejects.toThrow('IO_ERROR');
      expect(store.getSnapshot().session).toBeNull();
      expect(store.getSnapshot().failedRoot).toBeNull();
      expect(client.hasSession).toBe(true);
    } finally {
      blocker?.close();
    }
    await client.refresh();
    expect(store.getSnapshot().session?.generation).toBe(3);
    expect(adapter.calls.startUsageScan).toBe(2);
  });
  it('loads only requested direct entries, starts independent scan, and does not rescan on view switch', async () => {
    const { client, adapter, store } = setup();
    await client.chooseRoot();
    expect([...store.getSnapshot().entries.keys()]).toEqual(['A', 'B']);
    expect(store.getListing('B', 'directories').loadState).toBe('notRequested');
    await client.list('B', 'directories');
    await client.list('B', 'files');
    expect(store.getListing('B', 'directories').snapshot?.entryIds).toEqual([
      'C',
    ]);
    expect(store.getListing('B', 'files').snapshot?.entryIds).toEqual(['file']);
    store.setMode('mindmap');
    store.setMode('organization');
    expect(adapter.calls.startUsageScan).toBe(1);
  });
  it('preserves selected root on picker cancellation; refresh changes generation once', async () => {
    const { client, adapter, store } = setup();
    await client.chooseRoot();
    const session = store.getSnapshot().session;
    adapter.cancelNextChoice = true;
    expect(await client.chooseRoot()).toBe(false);
    expect(store.getSnapshot().session).toBe(session);
    await client.refresh();
    expect(store.getSnapshot().session?.generation).toBe(2);
    await client.close();
    expect(store.getSnapshot().session).toBeNull();
  });
  it('recovers missing terminal events without disk restart and stops after terminal', async () => {
    vi.useFakeTimers();
    const { client, adapter, store } = setup();
    adapter.deferTasks = true;
    adapter.dropEvents = true;
    await client.chooseRoot();
    expect(store.getListing('A', 'directories').loadState).toBe('loading');
    for (const task of store.getSnapshot().tasks.values())
      adapter.finish(task.taskId);
    await vi.advanceTimersByTimeAsync(1000);
    expect(store.getListing('A', 'directories').snapshot?.entryIds).toEqual([
      'B',
    ]);
    expect(
      [...store.getSnapshot().tasks.values()].every(
        (task) => task.phase === 'completed',
      ),
    ).toBe(true);
    const readCount = adapter.calls.readTask;
    await vi.advanceTimersByTimeAsync(3000);
    expect(adapter.calls.readTask).toBe(readCount);
    expect(adapter.calls.startListing).toBe(1);
    client.dispose();
    expect(adapter.subscriberCount).toBe(0);
  });
  it('deduplicates concurrent page reads, enforces waiting interval and keeps pages separate', async () => {
    vi.useFakeTimers();
    const adapter = new FixtureAdapter({
      entries: [
        fixtureEntry('A', null),
        ...Array.from({ length: 450 }, (_, i) =>
          fixtureEntry(`child-${i}`, 'A'),
        ),
      ],
    });
    const { client, store } = setup(adapter);
    await client.chooseRoot();
    const task = store.getListing('A', 'directories').taskId!;
    expect(
      store.getListing('A', 'directories').snapshot?.entryIds,
    ).toHaveLength(200);
    await Promise.all([client.nextPage(task), client.nextPage(task)]);
    expect(adapter.calls.readListingPage).toBe(1);
    await vi.advanceTimersByTimeAsync(250);
    await Promise.all([client.nextPage(task), client.nextPage(task)]);
    expect(adapter.calls.readListingPage).toBe(2);
    await vi.advanceTimersByTimeAsync(250);
    await client.nextPage(task);
    expect(store.getListing('A', 'directories')).toMatchObject({
      loadState: 'settled',
      snapshot: { nextCursor: null },
    });
    expect(
      store.getListing('A', 'directories').snapshot?.entryIds,
    ).toHaveLength(450);
  });
  it('does not consume undisplayed pages during status recovery', async () => {
    vi.useFakeTimers();
    const adapter = new FixtureAdapter({
      entries: [
        fixtureEntry('A', null),
        ...Array.from({ length: 450 }, (_, i) =>
          fixtureEntry(`child-${i}`, 'A'),
        ),
      ],
    });
    const { client, store } = setup(adapter);
    await client.chooseRoot();
    await vi.advanceTimersByTimeAsync(5000);
    expect(adapter.calls.readListingPage).toBe(1);
    expect(
      store.getListing('A', 'directories').snapshot?.entryIds,
    ).toHaveLength(200);
  });

  it('cancelled work stays cancelled after a late finish', async () => {
    const { client, adapter, store } = setup();
    adapter.deferTasks = true;
    await client.chooseRoot();
    const task = [...store.getSnapshot().tasks.values()].find(
      (task) => task.operation === 'scanUsage',
    )!;
    await client.cancel(task.taskId);
    adapter.finish(task.taskId);
    expect(store.getSnapshot().tasks.get(task.taskId)?.phase).toBe('cancelled');
  });
  it('coalesces concurrent picker requests without invalidating the first choice', async () => {
    const { client, adapter } = setup();
    const first = client.chooseRoot();
    const second = client.chooseRoot();
    expect(first).toBe(second);
    expect(await first).toBe(true);
    expect(adapter.calls.chooseRoot).toBe(1);
  });

  it('close is not held behind an open picker and late picker does not restore a session', async () => {
    const { client, adapter, store } = setup();
    await client.chooseRoot();
    let resolve!: (response: Response<RootData>) => void;
    let pickerRequest!: Request;
    vi.spyOn(adapter, 'chooseRoot').mockImplementation((request) => {
      pickerRequest = request;
      return new Promise((done) => {
        resolve = done;
      });
    });
    const oldRoot = {
      session: store.getSnapshot().session!,
      root: store.getSnapshot().entries.get('A')!,
    };
    const choice = client.chooseRoot();
    await vi.waitFor(() => expect(resolve).toBeDefined());
    await client.close();
    expect(store.getSnapshot().session).toBeNull();
    resolve({
      requestId: pickerRequest.requestId,
      outcome: 'ok',
      data: oldRoot,
    });
    expect(await choice).toBe(false);
    expect(store.getSnapshot().session).toBeNull();
  });
  it('rejects mismatched response IDs and never converts protocol errors to success', () => {
    expect(() =>
      unwrap({ requestId: 'a', outcome: 'ok', data: 0 }, 'b'),
    ).toThrow('INVALID_RESPONSE');
    expect(() =>
      unwrap(
        {
          requestId: 'a',
          outcome: 'error',
          error: { code: 'OUTSIDE_ROOT', operation: 'read', scope: 'entry' },
        },
        'a',
      ),
    ).toThrow('OUTSIDE_ROOT');
  });
});

it('refresh failure discards prior observations and retries using the returned generation', async () => {
  const { client, adapter, store } = setup();
  await client.chooseRoot();
  const sessionId = store.getSnapshot().session!.sessionId;
  adapter.failNextRefresh = true;
  const first = client.refresh();
  expect(client.refresh()).toBe(first);
  await expect(first).rejects.toThrow('IO_ERROR');
  expect(store.getSnapshot()).toMatchObject({
    session: null,
    failedRoot: {
      scope: { sessionId, generation: 2 },
      issue: { code: 'IO_ERROR' },
    },
  });
  expect(store.getSnapshot().entries.size).toBe(0);
  expect(store.getSnapshot().tasks.size).toBe(0);
  expect(store.getSnapshot().usages.size).toBe(0);
  expect(adapter.calls.startUsageScan).toBe(1);
  await client.refresh();
  expect(store.getSnapshot().session?.generation).toBe(3);
  expect(store.getSnapshot().failedRoot).toBeNull();
  expect(adapter.calls.startUsageScan).toBe(2);
});

it('can close a failed refreshed root without reselecting it', async () => {
  const { client, adapter, store } = setup();
  await client.chooseRoot();
  adapter.failNextRefresh = true;
  await expect(client.refresh()).rejects.toThrow('IO_ERROR');
  await client.close();
  expect(store.getSnapshot().session).toBeNull();
  expect(store.getSnapshot().failedRoot).toBeNull();
  expect(adapter.calls.closeSession).toBe(1);
});

it('close is not held behind refresh I/O and its late failure cannot restore a root', async () => {
  const { client, adapter, store } = setup();
  await client.chooseRoot();
  const session = store.getSnapshot().session!;
  let request!: Request;
  let resolve!: (value: Response<RootData>) => void;
  vi.spyOn(adapter, 'refreshRoot').mockImplementation((input) => {
    request = input;
    return new Promise((done) => {
      resolve = done;
    });
  });
  const pending = client.refresh();
  await vi.waitFor(() => expect(request).toBeDefined());
  await client.close();
  expect(adapter.calls.closeSession).toBe(1);
  resolve({
    requestId: request.requestId,
    outcome: 'error',
    error: { code: 'IO_ERROR', operation: 'refreshRoot', scope: 'root' },
    currentScope: { sessionId: session.sessionId, generation: 2 },
  });
  await expect(pending).rejects.toThrow('IO_ERROR');
  expect(store.getSnapshot().session).toBeNull();
  expect(store.getSnapshot().failedRoot).toBeNull();
});

it('rejects a refresh failure that claims a different session scope', async () => {
  const { client, adapter, store } = setup();
  await client.chooseRoot();
  const session = store.getSnapshot().session;
  vi.spyOn(adapter, 'refreshRoot').mockImplementation(async (request) => ({
    requestId: request.requestId,
    outcome: 'error',
    error: { code: 'IO_ERROR', operation: 'refreshRoot', scope: 'root' },
    currentScope: { sessionId: 'other', generation: 2 },
  }));
  await expect(client.refresh()).rejects.toThrow('INVALID_RESPONSE');
  expect(store.getSnapshot().session).toBe(session);
  expect(store.getSnapshot().failedRoot).toBeNull();
});
