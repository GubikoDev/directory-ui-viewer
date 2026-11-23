import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
} from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { App } from './App';
import { InternalApp } from './InternalApp';
import { SessionClient } from './features/explorer/client/session-client';
import {
  FixtureAdapter,
  fixtureEntry,
} from './features/explorer/adapters/fixture';

function gate() {
  let resolve!: () => void;
  const promise = new Promise<void>((release) => {
    resolve = release;
  });
  return { resolve, promise };
}

describe('App', () => {
  it('reports a synchronous page admission failure without losing the current view', async () => {
    const client = new SessionClient(
      new FixtureAdapter({
        entries: [
          fixtureEntry('A', null),
          ...Array.from({ length: 201 }, (_, i) =>
            fixtureEntry(`child-${i}`, 'A'),
          ),
        ],
      }),
    );
    await client.chooseRoot();
    const view = render(<InternalApp client={client} />);
    vi.spyOn(client, 'nextPage').mockImplementation(() => {
      throw new Error('RESOURCE_LIMIT');
    });
    fireEvent.click(screen.getByRole('button', { name: '폴더 다음 페이지' }));
    expect(await screen.findByRole('alert')).toHaveTextContent(
      'RESOURCE_LIMIT',
    );
    expect(screen.getByRole('button', { name: 'child-0' })).toBeInTheDocument();
    view.unmount();
  });
  it('requests files only from the file button, including a root with no directories', async () => {
    const client = new SessionClient(
      new FixtureAdapter({
        entries: [
          fixtureEntry('root', null),
          fixtureEntry('leaf.txt', 'root', 'regularFile'),
        ],
      }),
    );
    render(<App client={client} />);
    fireEvent.click(screen.getByRole('button', { name: '폴더 열기' }));
    const root = await screen.findByRole('button', { name: 'root' });
    fireEvent.click(root);
    expect(client.store.getListing('root', 'files').loadState).toBe(
      'notRequested',
    );
    fireEvent.click(screen.getByRole('button', { name: 'root 파일 목록' }));
    expect(await screen.findByRole('cell', { name: 'leaf.txt' })).toBeVisible();
    fireEvent.click(screen.getByRole('button', { name: '파일 목록 닫기' }));
    await waitFor(() =>
      expect(screen.queryByRole('cell', { name: 'leaf.txt' })).toBeNull(),
    );
  });
  it('opens the clicked panel while directory discovery and the size scan are pending, then gives the file read priority', async () => {
    const adapter = new FixtureAdapter({
      entries: [
        fixtureEntry('root', null),
        fixtureEntry('one', 'root'),
        fixtureEntry('two', 'root'),
        fixtureEntry('child', 'one'),
        fixtureEntry('direct.txt', 'one', 'regularFile'),
      ],
    });
    const directory = gate();
    const file = gate();
    const usage = gate();
    const read = adapter.startListing.bind(adapter);
    const start = vi
      .spyOn(adapter, 'startListing')
      .mockImplementation(async (request) => {
        const response = await read(request);
        if (request.directoryId === 'one')
          await (request.category === 'files'
            ? file.promise
            : directory.promise);
        return response;
      });
    const scan = adapter.startUsageScan.bind(adapter);
    vi.spyOn(adapter, 'startUsageScan').mockImplementation(async (request) => {
      const response = await scan(request);
      await usage.promise;
      return response;
    });
    const client = new SessionClient(adapter);
    const view = render(<App client={client} />);
    try {
      fireEvent.click(screen.getByRole('button', { name: '폴더 열기' }));
      await waitFor(() =>
        expect(start).toHaveBeenCalledWith(
          expect.objectContaining({
            directoryId: 'one',
            category: 'directories',
          }),
        ),
      );
      fireEvent.click(screen.getByRole('button', { name: 'one 파일 목록' }));
      const panel = await screen.findByRole('region', {
        name: 'one 파일 목록',
      });
      expect(panel).toHaveTextContent('읽는 중');
      await act(async () => directory.resolve());
      expect(screen.queryByRole('button', { name: 'child' })).toBeNull();
      expect(start).not.toHaveBeenCalledWith(
        expect.objectContaining({
          directoryId: 'two',
          category: 'directories',
        }),
      );
      await act(async () => file.resolve());
      expect(
        await screen.findByRole('cell', { name: 'direct.txt' }),
      ).toBeVisible();
      await waitFor(() =>
        expect(start).toHaveBeenCalledWith(
          expect.objectContaining({
            directoryId: 'two',
            category: 'directories',
          }),
        ),
      );
      fireEvent.click(screen.getByRole('button', { name: 'two 파일 목록' }));
      expect(
        await screen.findByRole('region', { name: 'two 파일 목록' }),
      ).toBeVisible();
      expect(
        screen.queryByRole('region', { name: 'one 파일 목록' }),
      ).toBeNull();
    } finally {
      await act(async () => {
        directory.resolve();
        file.resolve();
        usage.resolve();
      });
      view.unmount();
    }
  });
  it('identifies the application', () => {
    render(<App />);

    expect(
      screen.getByRole('heading', { name: 'Directory UI Viewer' }),
    ).toBeInTheDocument();
  });
});
