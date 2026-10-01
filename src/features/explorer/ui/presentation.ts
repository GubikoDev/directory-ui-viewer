import type { ExplorerState } from '../model/store';
import { listingKey, type Measure } from '../model/types';

export function formatBytes(bytes: string) {
  const value = BigInt(bytes);
  const units = ['B', 'KB', 'MB', 'GB', 'TB', 'PB', 'EB'];
  let divisor = 1n;
  let unit = 0;
  while (value >= divisor * 1000n && unit < units.length - 1) {
    divisor *= 1000n;
    unit++;
  }
  const hundredths = (value * 100n) / divisor;
  return `${unit === 0 ? value : Number(hundredths) / 100} ${units[unit]}`;
}
export function formatMeasure(value?: Measure) {
  if (!value || value.state === 'unknown') return '—';
  if (value.state === 'unavailable') return '읽기 불가';
  return `${formatBytes(value.state === 'complete' ? value.bytes : value.observedBytes)}${value.state === 'partial' ? '+' : ''}`;
}
export function nodeSize(value?: Measure): [number, number] {
  if (!value || (value.state !== 'complete' && value.state !== 'partial'))
    return [196, 62];
  const bytes = BigInt(
    value.state === 'complete' ? value.bytes : value.observedBytes,
  );
  if (bytes >= 10_000_000_000n) return [244, 72];
  if (bytes >= 1_000_000_000n) return [220, 66];
  if (bytes >= 100_000_000n) return [196, 62];
  return [172, 58];
}
export interface GraphNode {
  id: string;
  parent: string | null;
  depth: number;
  x: number;
  y: number;
  width: number;
  height: number;
}
/** Layout only reads observations; it never acquires filesystem access. */
export function layoutGraph(
  state: ExplorerState,
  showHidden: boolean,
  limits: ReadonlyMap<string, number>,
) {
  const root = state.session?.rootEntryId;
  const nodes: GraphNode[] = [];
  if (!root) return { nodes, width: 1440, height: 850 };
  const seen = new Set<string>();
  const stack = [{ id: root, parent: null as string | null, depth: 0 }];
  while (stack.length && nodes.length < 240) {
    const item = stack.pop()!;
    if (seen.has(item.id)) continue;
    const entry = state.entries.get(item.id);
    if (!entry || entry.kind !== 'directory') continue;
    seen.add(item.id);
    const [width, height] = nodeSize(state.usages.get(item.id)?.allocated);
    nodes.push({ ...item, x: 0, y: 0, width, height });
    if (item.id !== root && !state.expandedIds.has(item.id)) continue;
    const children =
      state.listings.get(listingKey(item.id, 'directories'))?.snapshot
        ?.entryIds ?? [];
    const visible = children
      .filter((id) => {
        const hidden = state.entries.get(id)?.hidden;
        return showHidden || hidden?.state !== 'known' || !hidden.value;
      })
      .slice(0, limits.get(item.id) ?? 12);
    for (let i = visible.length - 1; i >= 0; i--)
      stack.push({ id: visible[i], parent: item.id, depth: item.depth + 1 });
  }
  const children = new Map<string, GraphNode[]>();
  for (const node of nodes) {
    if (node.parent) {
      const list = children.get(node.parent) ?? [];
      list.push(node);
      children.set(node.parent, list);
    }
  }
  let leaf = 0;
  for (let i = nodes.length - 1; i >= 0; i--) {
    const node = nodes[i];
    const branch = children.get(node.id);
    node.x = 72 + node.depth * 320;
    node.y = branch?.length
      ? (branch[0].y + branch[branch.length - 1].y) / 2
      : 100 + leaf++ * 112;
  }
  if (state.mode === 'mindmap') {
    const maxDepth = Math.max(...nodes.map((n) => n.depth), 1);
    const radius = Math.max(320, leaf * 24);
    const center = radius + 280;
    for (const node of nodes) {
      const angle =
        ((node.y - 100) / Math.max(leaf * 112, 112)) * Math.PI * 2 -
        Math.PI / 2;
      const distance = (node.depth / maxDepth) * radius;
      node.x = center + Math.cos(angle) * distance - node.width / 2;
      node.y = center + Math.sin(angle) * distance - node.height / 2;
    }
  }
  return {
    nodes,
    width: Math.max(1440, ...nodes.map((n) => n.x + n.width + 120)),
    height: Math.max(850, ...nodes.map((n) => n.y + n.height + 100)),
  };
}
