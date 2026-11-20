import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { App } from './App';
import { InternalApp } from './InternalApp';
import { SessionClient } from './features/explorer/client/session-client';
import {
  FixtureAdapter,
  fixtureEntry,
} from './features/explorer/adapters/fixture';

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
  it('shows files automatically when the selected root has no directories', async () => {
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
    expect(await screen.findByRole('cell', { name: 'leaf.txt' })).toBeVisible();
    fireEvent.click(screen.getByRole('button', { name: '파일 목록 닫기' }));
    await waitFor(() =>
      expect(screen.queryByRole('cell', { name: 'leaf.txt' })).toBeNull(),
    );
  });
  it('identifies the application', () => {
    render(<App />);

    expect(
      screen.getByRole('heading', { name: 'Directory UI Viewer' }),
    ).toBeInTheDocument();
  });
});
