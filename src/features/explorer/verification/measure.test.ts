import { afterEach, describe, expect, it } from 'vitest';
import { SessionClient } from '../client/session-client';
import { ExplorerStore } from '../model/store';
import { FrontendBudget } from '../runtime/memory';
import { FixtureAdapter, fixtureEntry } from '../adapters/fixture';
import { measureApprovedRoot, type ExpectedFixture } from './measure';
import { readExpectation } from './expectation';

const clients: SessionClient[] = [];
afterEach(() => {
  for (const client of clients) client.dispose();
  clients.length = 0;
});
async function setup() {
  const adapter = new FixtureAdapter({
    entries: [
      fixtureEntry('root', null),
      ...Array.from({ length: 200 }, (_, i) =>
        fixtureEntry(`private-name-${i}`, 'root', 'regularFile'),
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
  });
  const client = new SessionClient(
    adapter,
    new ExplorerStore(new FrontendBudget()),
  );
  clients.push(client);
  await client.chooseRoot();
  const expected: ExpectedFixture = {
    category: 'files',
    firstPageEntries: 200,
    entries: '0',
    logical: '42',
    allocated: '64',
    partial: false,
  };
  // FixtureAdapter deliberately reports zero processed entries. These tests
  // validate the recorder's comparisons, not real scanning or native latency.
  return { client, adapter, expected };
}
describe('opt-in approved-root measurement', () => {
  it('records ten refreshed generations, compares oracles, and distinguishes terminal cancellation', async () => {
    const { client, adapter, expected } = await setup();
    let now = 0;
    const report = await measureApprovedRoot(
      client,
      expected,
      {
        now: () => now,
        wait: async () => {
          now += 50;
        },
        paint: async () => {
          now += 32;
        },
      },
      new AbortController().signal,
    );
    expect(report.layer).toBe('fixture');
    expect(report.stoppedBy).toBeNull();
    expect(report.trials).toHaveLength(10);
    expect(adapter.calls.refreshRoot).toBe(20);
    expect(
      report.trials.every(
        (trial) =>
          trial.logicalMatches && trial.allocatedMatches && trial.entriesMatch,
      ),
    ).toBe(true);
    expect(
      report.trials.every((trial) => trial.cancelStatus === 'alreadyTerminal'),
    ).toBe(true);
    expect(
      report.trials.every((trial) => trial.cancelLatencyPass === null),
    ).toBe(true);
    expect(report.trials.every((trial) => trial.firstPageMs === 32)).toBe(true);
    expect(JSON.stringify(report)).not.toContain('private-name');
    expect(JSON.stringify(report)).not.toContain('session-');
  });
  it('reports oracle mismatch instead of claiming a passing result', async () => {
    const { client, expected } = await setup();
    expected.logical = '43';
    expected.entries = '200';
    const report = await measureApprovedRoot(
      client,
      expected,
      { now: () => 0, wait: async () => {}, paint: async () => {} },
      new AbortController().signal,
    );
    expect(report.stoppedBy).toBeNull();
    expect(report.trials).toHaveLength(10);
    expect(
      report.trials.every(
        (trial) => !trial.logicalMatches && !trial.entriesMatch,
      ),
    ).toBe(true);
  });
  it('keeps completed trials when a later frame fails and removes arbitrary error text', async () => {
    const { client, expected } = await setup();
    let frames = 0;
    const report = await measureApprovedRoot(
      client,
      expected,
      {
        now: () => 0,
        wait: async () => {},
        paint: async () => {
          if (++frames === 2) throw new Error('/private/user-path');
        },
      },
      new AbortController().signal,
    );
    expect(report.trials).toHaveLength(1);
    expect(report.stoppedBy).toBe('INTERNAL');
    expect(JSON.stringify(report)).not.toContain('/private');
  });
  it('stops before refreshing an aborted measurement', async () => {
    const { client, adapter, expected } = await setup();
    const abort = new AbortController();
    abort.abort();
    const report = await measureApprovedRoot(
      client,
      expected,
      { now: () => 0, wait: async () => {}, paint: async () => {} },
      abort.signal,
    );
    expect(report.stoppedBy).toBe('MEASUREMENT_CANCELLED');
    expect(report.trials).toHaveLength(0);
    expect(adapter.calls.refreshRoot).toBeUndefined();
  });
  it('accepts only fixed profile expectations and unsigned u128 totals', () => {
    const budget = new FrontendBudget();
    expect(
      readExpectation(
        '{"profile":"flat10k","logical":"42","allocated":"64"}',
        budget,
      ),
    ).toMatchObject({
      firstPageEntries: 200,
      entries: '10000',
      partial: false,
    });
    for (const text of [
      '{"profile":"tiny","logical":"0","allocated":"0"}',
      '{"profile":"deep","logical":"-1","allocated":"0"}',
      '{"profile":"deep","logical":"0","allocated":"0","path":"/private"}',
      '{"profile":"flat10k","logical":"340282366920938463463374607431768211456","allocated":"0"}',
    ])
      expect(() => readExpectation(text, budget)).toThrow(
        'INVALID_FIXTURE_EXPECTATION',
      );
  });
});
