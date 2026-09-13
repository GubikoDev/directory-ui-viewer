import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { App } from './App';
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
    const view = render(<App client={client} />);
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
  it('identifies the application', () => {
    render(<App />);

    expect(
      screen.getByRole('heading', { name: 'Directory UI Viewer' }),
    ).toBeInTheDocument();
  });
});
