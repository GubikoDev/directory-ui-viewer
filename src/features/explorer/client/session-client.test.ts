import { afterEach, describe, expect, it, vi } from 'vitest';
import { FixtureAdapter, fixtureEntry } from '../adapters/fixture';
import { ExplorerStore } from '../model/store';
import { SessionClient, unwrap } from './session-client';
import type { Request, Response, RootData } from '../model/types';
const clients: SessionClient[] = [];
function setup(adapter = new FixtureAdapter()) {
  const client = new SessionClient(
    adapter,
    new ExplorerStore(),
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
