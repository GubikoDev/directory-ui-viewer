import { describe, expect, it } from 'vitest';
import { fixtureEntry } from '../adapters/fixture';
import {
  listingKey,
  type DirectoryListing,
  type DirectoryUsage,
  type Entry,
} from '../model/types';
import { layoutGraph, type GraphNode } from './presentation';

function mindMap(entries: Entry[]): Parameters<typeof layoutGraph>[0] {
  const listings = new Map<string, DirectoryListing>();
  for (const entry of entries) {
    if (!entry.parentId) continue;
    const directoryId = entry.parentId;
    const key = listingKey(directoryId, 'directories');
    let listing = listings.get(key);
    if (!listing) {
      listing = {
        directoryId,
        category: 'directories',
        loadState: 'settled',
        snapshot: {
          entryIds: [],
          listingRevision: '1',
          coverage: 'complete',
          nextCursor: null,
          observedAt: entry.observedAt,
          issues: { counts: {}, samples: [] },
        },
      };
      listings.set(key, listing);
    }
    listing.snapshot!.entryIds.push(entry.entryId);
  }
  return {
    session: {
      rootEntryId: 'root',
      sessionId: 'fixture',
      generation: 1,
      platform: 'fixture',
      capacity: { foreground: 'available', background: 'available' },
    },
    entries: new Map(entries.map((entry) => [entry.entryId, entry])),
    listings,
    usages: new Map(),
    mode: 'mindmap',
  };
}

function findOverlap(nodes: GraphNode[]) {
  for (const [index, a] of nodes.entries()) {
    for (let i = index + 1; i < nodes.length; i++) {
      const b = nodes[i];
      if (
        a.x < b.x + b.width &&
        b.x < a.x + a.width &&
        a.y < b.y + b.height &&
        b.y < a.y + a.height
      ) {
        return [a.id, b.id];
      }
    }
  }
}

describe('mind map layout', () => {
  it('keeps nodes apart across many levels with a single leaf', () => {
    const entries = [
      fixtureEntry('root', null),
      ...Array.from({ length: 16 }, (_, i) =>
        fixtureEntry(`folder-${i}`, i === 0 ? 'root' : `folder-${i - 1}`),
      ),
    ];
    const graph = layoutGraph(mindMap(entries), false);
    expect(graph.nodes).toHaveLength(entries.length);
    expect(findOverlap(graph.nodes)).toBeUndefined();
  });

  it('keeps crowded and uneven rings clear after size changes and branch folding', () => {
    const entries = [
      fixtureEntry('root', null),
      fixtureEntry('first', 'root'),
      fixtureEntry('first-child', 'first'),
      ...Array.from({ length: 96 }, (_, i) =>
        fixtureEntry(`folder-${i}`, 'root'),
      ),
      fixtureEntry('last', 'root'),
      fixtureEntry('last-child', 'last'),
    ];
    const state = mindMap(entries);
    const initial = layoutGraph(state, false);
    expect(initial.nodes).toHaveLength(entries.length);
    expect(findOverlap(initial.nodes)).toBeUndefined();

    const sized = {
      ...state,
      usages: new Map<string, DirectoryUsage>(
        entries.map(({ entryId, observedAt }, i) => [
          entryId,
          {
            directoryId: entryId,
            usageRevision: 1,
            scanState: 'settled',
            logical: { state: 'unknown' },
            allocated: {
              state: 'complete',
              bytes: i % 2 ? '10000000000' : '0',
            },
            observedAt,
          },
        ]),
      ),
    };
    const folded = layoutGraph(sized, false, new Set(['first']));
    expect(folded.nodes.map(({ id }) => id)).not.toContain('first-child');
    expect(findOverlap(folded.nodes)).toBeUndefined();

    const reopened = layoutGraph(sized, false);
    expect(reopened.nodes).toHaveLength(entries.length);
    expect(reopened.nodes.find(({ id }) => id === 'first')).toMatchObject({
      width: 244,
      height: 72,
    });
    expect(findOverlap(reopened.nodes)).toBeUndefined();
    expect([reopened.width, reopened.height]).toEqual([
      initial.width,
      initial.height,
    ]);
    for (const [i, { x, y, width, height }] of reopened.nodes.entries()) {
      const original = initial.nodes[i];
      expect(x + width / 2).toBeCloseTo(original.x + original.width / 2);
      expect(y + height / 2).toBeCloseTo(original.y + original.height / 2);
      expect(x).toBeGreaterThanOrEqual(0);
      expect(y).toBeGreaterThanOrEqual(0);
      expect(x + width).toBeLessThanOrEqual(reopened.width);
      expect(y + height).toBeLessThanOrEqual(reopened.height);
    }
  });
});
