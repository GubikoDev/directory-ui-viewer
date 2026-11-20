import { expect, it, vi } from 'vitest';
import { FixtureAdapter, fixtureEntry } from '../adapters/fixture';
import { ExplorerStore } from '../model/store';
import { SessionClient } from './session-client';
import { discoverDirectories } from './directory-discovery';
import { layoutGraph } from '../ui/presentation';

it('publishes later pages beyond 240 siblings and preserves the graph when a file panel closes', async () => {
  const adapter = new FixtureAdapter({
    entries: [
      fixtureEntry('root', null),
      ...Array.from({ length: 245 }, (_, i) =>
        fixtureEntry(`dir-${i}`, 'root'),
      ),
      fixtureEntry('deep', 'dir-244'),
    ],
  });
  const client = new SessionClient(adapter);
  await client.chooseRoot();
  const stop = discoverDirectories(
    client,
    client.store.getSnapshot().session!,
    (error) => {
      throw error;
    },
  );
  try {
    await vi.waitFor(
      () => expect(client.store.getSnapshot().entries.has('deep')).toBe(true),
      { timeout: 4000 },
    );
    await client.list('dir-0', 'files');
    client.closeListing('dir-0', 'files');
    const graph = layoutGraph(client.store.getSnapshot(), false);
    expect(graph.nodes).toHaveLength(247);
    expect(graph.nodes.at(-1)?.id).toBe('deep');
  } finally {
    stop();
    client.dispose();
  }
});

it('keeps reading siblings after a denied subtree and never follows file links', async () => {
  const adapter = new FixtureAdapter({
    deniedIds: ['denied'],
    entries: [
      fixtureEntry('root', null),
      fixtureEntry('denied', 'root'),
      fixtureEntry('readable', 'root'),
      fixtureEntry('deep', 'readable'),
      fixtureEntry('link', 'readable', 'symlink'),
    ],
  });
  const client = new SessionClient(adapter);
  await client.chooseRoot();
  const done = vi.fn();
  const stop = discoverDirectories(
    client,
    client.store.getSnapshot().session!,
    (error) => {
      throw error;
    },
    done,
  );
  try {
    await vi.waitFor(() => expect(done).toHaveBeenCalledOnce());
    expect(client.store.getListing('denied', 'directories').loadState).toBe(
      'failed',
    );
    expect(client.store.getSnapshot().entries.has('link')).toBe(false);
  } finally {
    stop();
    client.dispose();
  }
});

it('discards an in-flight directory page when the session is refreshed', async () => {
  const client = new SessionClient(new FixtureAdapter());
  await client.chooseRoot();
  const read = client.readTreePage.bind(client);
  let release!: () => void;
  const held = new Promise<void>((resolve) => {
    release = resolve;
  });
  const page = vi
    .spyOn(client, 'readTreePage')
    .mockImplementationOnce(async (...args) => {
      const old = await read(...args);
      await held;
      return old;
    });
  const error = vi.fn();
  const stop = discoverDirectories(
    client,
    client.store.getSnapshot().session!,
    error,
  );
  try {
    await vi.waitFor(() => expect(page).toHaveBeenCalledOnce());
    await client.refresh();
    release();
    await page.mock.results[0].value;
    expect(client.store.getSnapshot().session?.generation).toBe(2);
    expect(client.store.getSnapshot().entries.has('C')).toBe(false);
    expect(error).not.toHaveBeenCalled();
  } finally {
    release();
    stop();
    client.dispose();
  }
});

it('automatically consumes every page of a nested directory', async () => {
  const client = new SessionClient(
    new FixtureAdapter({
      entries: [
        fixtureEntry('root', null),
        fixtureEntry('branch', 'root'),
        ...Array.from({ length: 201 }, (_, i) =>
          fixtureEntry(`child-${i}`, 'branch'),
        ),
      ],
    }),
  );
  await client.chooseRoot();
  const error = vi.fn();
  const stop = discoverDirectories(
    client,
    client.store.getSnapshot().session!,
    error,
  );
  try {
    await vi.waitFor(() =>
      expect(client.store.getListing('branch', 'directories').loadState).toBe(
        'settled',
      ),
    );
    expect(layoutGraph(client.store.getSnapshot(), false).nodes).toHaveLength(
      203,
    );
    expect(error).not.toHaveBeenCalled();
  } finally {
    stop();
    client.dispose();
  }
});

it('resumes on scroll and retains completed observations without rereading offscreen branches', async () => {
  const adapter = new FixtureAdapter({
    entries: [
      fixtureEntry('root', null),
      fixtureEntry('left', 'root'),
      fixtureEntry('right', 'root'),
      fixtureEntry('one', 'left'),
      fixtureEntry('two', 'right'),
    ],
  });
  const client = new SessionClient(adapter);
  await client.chooseRoot();
  const session = client.store.getSnapshot().session!;
  const listing = vi.spyOn(client, 'startTreeListing');
  const idle = vi.fn();
  const stop = discoverDirectories(
    client,
    session,
    (e) => {
      throw e;
    },
    idle,
  );
  try {
    stop.setViewport(['root', 'left']);
    await vi.waitFor(() => expect(idle).toHaveBeenCalledTimes(1));
    expect(client.store.getSnapshot().entries.has('one')).toBe(true);
    expect(listing.mock.calls.map(([, id]) => id)).toEqual(['left']);
    stop.setViewport(['root', 'right']);
    await vi.waitFor(() => expect(idle).toHaveBeenCalledTimes(2));
    expect(client.store.getSnapshot().entries.has('two')).toBe(true);
    stop.setViewport(['root', 'left']);
    expect(listing.mock.calls.map(([, id]) => id)).toEqual(['left', 'right']);
  } finally {
    stop();
    client.dispose();
  }
});

it('reopens a pruned branch and keeps the visible path and its siblings reachable', async () => {
  const branches = Array.from({ length: 64 }, (_, i) => 'branch-' + i);
  const adapter = new FixtureAdapter({
    entries: [
      fixtureEntry('root', null),
      ...branches.flatMap((id) => [
        fixtureEntry(id, 'root'),
        ...Array.from({ length: 8 }, (_, j) =>
          fixtureEntry(id + '-child-' + j, id),
        ),
      ]),
    ],
  });
  const client = new SessionClient(
    adapter,
    new ExplorerStore(256 * 1024 * 1024),
  );
  await client.chooseRoot();
  const session = client.store.getSnapshot().session!;
  const idle = vi.fn();
  const stop = discoverDirectories(
    client,
    session,
    (e) => {
      throw e;
    },
    idle,
  );
  try {
    stop.setViewport(branches);
    await vi.waitFor(() => expect(idle).toHaveBeenCalledTimes(1), {
      timeout: 5000,
    });
    stop.setViewport(['branch-63-child-0']);
    await vi.waitFor(() =>
      expect(client.store.getListing('branch-0', 'directories').loadState).toBe(
        'notRequested',
      ),
    );
    expect(
      client.store.getListing('branch-63', 'directories').snapshot?.entryIds,
    ).toContain('branch-63-child-0');
    expect(client.store.getSnapshot().entries.has('branch-0')).toBe(true);
    stop.setViewport(['branch-0']);
    await vi.waitFor(() =>
      expect(
        client.store.getListing('branch-0', 'directories').snapshot?.entryIds,
      ).toHaveLength(8),
    );
  } finally {
    stop();
    client.dispose();
  }
});
