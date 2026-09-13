import { describe, expect, it } from 'vitest';
import { FixtureAdapter, fixtureEntry } from './fixture';
import { unwrap } from '../client/session-client';
import type { Request } from '../model/types';
async function setup(entries = [fixtureEntry('A', null)]) {
  const adapter = new FixtureAdapter({ entries });
  const connection = await adapter.connectClient({
    protocolVersion: 1,
    clientNonce: 'document',
  });
  let number = 0;
  const request = (): Request => ({
    protocolVersion: 1,
    requestId: `${connection.epoch}:${++number}`,
  });
  const choose = request();
  const root = unwrap(await adapter.chooseRoot(choose), choose.requestId)!;
  const scoped = () => ({
    ...request(),
    sessionId: root.session.sessionId,
    generation: root.session.generation,
  });
  return { adapter, request, root, scoped };
}
describe('fixture API contract', () => {
  it('same nonce reconnect and duplicate refresh are idempotent', async () => {
    const { adapter, request, root } = await setup();
    expect(
      await adapter.connectClient({
        protocolVersion: 1,
        clientNonce: 'document',
      }),
    ).toEqual({ epoch: 'fixture-epoch' });
    await expect(
      adapter.connectClient({ protocolVersion: 1, clientNonce: 'other' }),
    ).rejects.toThrow('CLIENT_EXPIRED');
    const refresh = {
      ...request(),
      sessionId: root.session.sessionId,
      expectedGeneration: 1,
    };
    const once = await adapter.refreshRoot(refresh);
    const twice = await adapter.refreshRoot(refresh);
    expect(twice).toEqual(once);
    expect(unwrap(twice, refresh.requestId)?.session.generation).toBe(2);
  });
  it('evicted control request is never reexecuted', async () => {
    const { adapter, request, root } = await setup();
    const first = { ...request(), sessionId: root.session.sessionId };
    await adapter.closeSession(first);
    for (let i = 0; i < 257; i++)
      await adapter.closeSession({
        ...request(),
        sessionId: root.session.sessionId,
      });
    expect(await adapter.closeSession(first)).toMatchObject({
      outcome: 'error',
      error: { code: 'REQUEST_EXPIRED' },
    });
  });
  it('page cursor keeps immutable entries and is bound to task and limit', async () => {
    const { adapter, scoped } = await setup([
      fixtureEntry('A', null),
      fixtureEntry('B', 'A'),
      fixtureEntry('C', 'A'),
    ]);
    const startRequest = {
      ...scoped(),
      directoryId: 'A',
      category: 'directories' as const,
    };
    const start = unwrap(
      await adapter.startListing(startRequest),
      startRequest.requestId,
    )!;
    const firstRequest = {
      ...scoped(),
      taskId: start.work.taskId,
      cursor: null,
      limit: 1,
    };
    const first = unwrap(
      await adapter.readListingPage(firstRequest),
      firstRequest.requestId,
    )!;
    const againRequest = { ...firstRequest, ...scoped() };
    expect(
      unwrap(
        await adapter.readListingPage(againRequest),
        againRequest.requestId,
      ),
    ).toEqual(first);
    const wrongLimit = {
      ...scoped(),
      taskId: start.work.taskId,
      cursor: first.nextCursor,
      limit: 2,
    };
    expect(await adapter.readListingPage(wrongLimit)).toMatchObject({
      outcome: 'error',
      error: { code: 'INVALID_ARGUMENT' },
    });
    const next = { ...wrongLimit, ...scoped(), limit: 1 };
    expect(
      unwrap(await adapter.readListingPage(next), next.requestId),
    ).toMatchObject({ entries: [{ entryId: 'C' }], nextCursor: null });
  });
  it('cancel does not expose entries that were never collected', async () => {
    const { adapter, scoped } = await setup([
      fixtureEntry('A', null),
      fixtureEntry('B', 'A'),
    ]);
    adapter.deferTasks = true;
    const request = {
      ...scoped(),
      directoryId: 'A',
      category: 'directories' as const,
    };
    const start = unwrap(
      await adapter.startListing(request),
      request.requestId,
    )!;
    const initial = {
      ...scoped(),
      taskId: start.work.taskId,
      cursor: null,
      limit: 200,
    };
    const pending = unwrap(
      await adapter.readListingPage(initial),
      initial.requestId,
    )!;
    await adapter.cancelTask({ ...scoped(), taskId: start.work.taskId });
    const final = {
      ...scoped(),
      taskId: start.work.taskId,
      cursor: pending.nextCursor,
      limit: 200,
    };
    expect(
      unwrap(await adapter.readListingPage(final), final.requestId),
    ).toMatchObject({
      entries: [],
      coverage: 'partial',
      work: { phase: 'cancelled' },
    });
  });

  it('denied listing returns native error and partial coverage, never complete empty', async () => {
    const { adapter, scoped } = await setup();
    adapter.tree.deniedIds = ['A'];
    const request = {
      ...scoped(),
      directoryId: 'A',
      category: 'directories' as const,
    };
    const start = unwrap(
      await adapter.startListing(request),
      request.requestId,
    )!;
    const pageRequest = {
      ...scoped(),
      taskId: start.work.taskId,
      cursor: null,
      limit: 200,
    };
    expect(
      unwrap(await adapter.readListingPage(pageRequest), pageRequest.requestId),
    ).toMatchObject({
      coverage: 'partial',
      work: { phase: 'failed' },
      issues: { samples: [{ code: 'PERMISSION_DENIED', nativeCode: 13 }] },
    });
  });
});
