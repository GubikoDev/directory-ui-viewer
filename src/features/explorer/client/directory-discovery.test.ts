import { expect, it, vi } from 'vitest';
import { FixtureAdapter, fixtureEntry } from '../adapters/fixture';
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
