import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
} from '@testing-library/react';
import { afterEach, expect, it, vi } from 'vitest';
import { App } from '../../../App';
import { FixtureAdapter, fixtureEntry } from '../adapters/fixture';
import { SessionClient } from '../client/session-client';
import { ExplorerStore } from '../model/store';
import { FrontendBudget } from '../runtime/memory';
import { VerificationPanel } from './VerificationPanel';

afterEach(() => vi.unstubAllEnvs());

async function setup() {
  const client = new SessionClient(
    new FixtureAdapter({
      entries: [
        fixtureEntry('root', null),
        ...Array.from({ length: 200 }, (_, i) =>
          fixtureEntry(`fixture-${i}`, 'root'),
        ),
      ],
      usages: [
        {
          directoryId: 'root',
          usageRevision: 1,
          scanState: 'settled',
          logical: { state: 'complete', bytes: '42' },
          allocated: { state: 'complete', bytes: '64' },
          observedAt: '2026-09-14T00:00:00Z',
        },
      ],
    }),
    new ExplorerStore(new FrontendBudget()),
  );
  await client.chooseRoot();
  return client;
}

function begin() {
  fireEvent.change(screen.getByLabelText('테스트 기대값'), {
    target: {
      value: '{"profile":"hardlinks","logical":"42","allocated":"64"}',
    },
  });
  fireEvent.click(screen.getByRole('button', { name: '10회 측정' }));
}

it('renders a refreshed page through App and preserves an incomplete run and fixture mismatches', async () => {
  vi.stubEnv('VITE_INTERNAL_BENCHMARK', '1');
  const client = await setup();
  const refresh = client.refresh.bind(client);
  let refreshes = 0;
  vi.spyOn(client, 'refresh').mockImplementation(async () => {
    if (++refreshes === 3) throw new Error('MEASUREMENT_INTERRUPTED');
    await refresh();
  });
  const view = render(<App client={client} />);
  begin();
  await waitFor(
    () => expect(screen.getByLabelText('측정 결과 JSON')).toBeInTheDocument(),
    { timeout: 5000 },
  );
  const report = JSON.parse(
    (screen.getByLabelText('측정 결과 JSON') as HTMLTextAreaElement).value,
  );
  expect(report.layer).toBe('fixture');
  expect(report.stoppedBy).toBe('MEASUREMENT_INTERRUPTED');
  expect(report.trials).toHaveLength(1);
  // The fixture has 200 directory rows and reports zero processed entries;
  // merely rendering 200 rows must not pass the 10,200-entry hardlink oracle.
  expect(
    report.trials.every(
      (trial: { entriesMatch: boolean }) => !trial.entriesMatch,
    ),
  ).toBe(true);
  expect(
    screen.getByRole('list', { name: '항목 목록' }).childElementCount,
  ).toBe(200);
  view.unmount();
});

it('rejects a rendered page from a different generation', async () => {
  const client = await setup();
  const view = render(
    <VerificationPanel
      client={client}
      rendered={() => ({ generation: -1, count: 200 })}
    />,
  );
  begin();
  const report = JSON.parse(
    ((await screen.findByLabelText('측정 결과 JSON')) as HTMLTextAreaElement)
      .value,
  );
  expect(report.stoppedBy).toBe('FIRST_PAGE_NOT_RENDERED');
  expect(report.trials).toHaveLength(0);
  view.unmount();
  client.dispose();
});

it('does not show measurement controls in a normal app build', () => {
  vi.stubEnv('VITE_INTERNAL_BENCHMARK', '');
  const view = render(<App />);
  expect(
    screen.queryByRole('region', { name: '내부 성능 측정' }),
  ).not.toBeInTheDocument();
  view.unmount();
});

it('keeps the run occupied until an in-flight request drains after a stop request', async () => {
  const client = await setup();
  let release!: () => void;
  const pending = new Promise<void>((resolve) => {
    release = resolve;
  });
  const refresh = vi.spyOn(client, 'refresh').mockReturnValue(pending);
  const view = render(
    <VerificationPanel
      client={client}
      rendered={() => ({ generation: 1, count: 200 })}
    />,
  );
  begin();
  fireEvent.click(screen.getByRole('button', { name: '측정 중지 요청' }));
  expect(screen.getByRole('status')).toHaveTextContent(
    '진행 중인 요청의 응답을 기다립니다',
  );
  expect(screen.getByRole('button', { name: '10회 측정' })).toBeDisabled();
  expect(screen.queryByLabelText('측정 결과 JSON')).not.toBeInTheDocument();
  await act(async () => release());
  const report = JSON.parse(
    ((await screen.findByLabelText('측정 결과 JSON')) as HTMLTextAreaElement)
      .value,
  );
  expect(report.stoppedBy).toBe('MEASUREMENT_CANCELLED');
  expect(report.trials).toHaveLength(0);
  expect(refresh).toHaveBeenCalledTimes(1);
  view.unmount();
  client.dispose();
});
