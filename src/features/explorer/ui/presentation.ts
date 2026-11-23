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
const largestNodeWidth = 244;
const largestNodeHeight = 72;

export function nodeSize(value?: Measure): [number, number] {
  if (!value || (value.state !== 'complete' && value.state !== 'partial'))
    return [196, 62];
  const bytes = BigInt(
    value.state === 'complete' ? value.bytes : value.observedBytes,
  );
  if (bytes >= 10_000_000_000n) return [largestNodeWidth, largestNodeHeight];
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

const minimumGraphWidth = 1440;
const minimumGraphHeight = 850;

function layoutMindMap(nodes: GraphNode[], children: Map<string, GraphNode[]>) {
  // Use y for leaf slots until the branch angles are known.
  let leafCount = 0;
  for (let i = nodes.length - 1; i >= 0; i--) {
    const node = nodes[i];
    const branch = children.get(node.id);
    node.y = branch
      ? (branch[0].y + branch[branch.length - 1].y) / 2
      : leafCount++;
  }

  const rings: {
    firstSlot: number;
    lastSlot: number;
    minSlotGap: number;
    radius: number;
  }[] = [];
  // Preorder visits each depth in descending slot order.
  for (const { depth, y: slot } of nodes) {
    if (depth === 0) continue;
    const ring = rings[depth - 1];
    if (ring) {
      ring.minSlotGap = Math.min(ring.minSlotGap, ring.lastSlot - slot);
      ring.lastSlot = slot;
    } else {
      rings.push({
        firstSlot: slot,
        lastSlot: slot,
        minSlotGap: Infinity,
        radius: 0,
      });
    }
  }

  // A circle of this diameter encloses every card with 12 px to spare.
  // Using the largest size keeps centers stable as usage measurements arrive.
  const clearance = Math.hypot(largestNodeWidth, largestNodeHeight) + 24;
  let radius = 0;
  for (const ring of rings) {
    // Include the gap between the first and last cards across the seam.
    const slotGap = Math.min(
      ring.minSlotGap,
      leafCount - ring.firstSlot + ring.lastSlot,
    );
    const requiredRadius =
      ring.firstSlot === ring.lastSlot
        ? 0
        : clearance / (2 * Math.sin((Math.PI * slotGap) / leafCount));
    radius = Math.max(radius + clearance, requiredRadius);
    ring.radius = radius;
  }

  let halfWidth = minimumGraphWidth / 2;
  let halfHeight = minimumGraphHeight / 2;
  for (const node of nodes) {
    const angle = (node.y / leafCount) * 2 * Math.PI - Math.PI / 2;
    const distance = node.depth === 0 ? 0 : rings[node.depth - 1].radius;
    const x = Math.cos(angle) * distance;
    const y = Math.sin(angle) * distance;
    node.x = x - node.width / 2;
    node.y = y - node.height / 2;
    halfWidth = Math.max(halfWidth, Math.abs(x) + largestNodeWidth / 2 + 120);
    halfHeight = Math.max(
      halfHeight,
      Math.abs(y) + largestNodeHeight / 2 + 100,
    );
  }
  for (const node of nodes) {
    node.x += halfWidth;
    node.y += halfHeight;
  }
  return { nodes, width: 2 * halfWidth, height: 2 * halfHeight };
}

/** Layout only reads observations; it never acquires filesystem access. */
export function layoutGraph(
  state: Pick<
    ExplorerState,
    'session' | 'entries' | 'listings' | 'usages' | 'mode'
  >,
  showHidden: boolean,
  collapsed?: ReadonlySet<string>,
) {
  const root = state.session?.rootEntryId;
  const nodes: GraphNode[] = [];
  if (!root)
    return { nodes, width: minimumGraphWidth, height: minimumGraphHeight };
  const seen = new Set<string>();
  const stack = [{ id: root, parent: null as string | null, depth: 0 }];
  while (stack.length) {
    const item = stack.pop()!;
    if (seen.has(item.id)) continue;
    const entry = state.entries.get(item.id);
    if (!entry || entry.kind !== 'directory') continue;
    seen.add(item.id);
    const [width, height] = nodeSize(state.usages.get(item.id)?.allocated);
    nodes.push({ ...item, x: 0, y: 0, width, height });
    if (collapsed?.has(item.id)) continue;
    const children =
      state.listings.get(listingKey(item.id, 'directories'))?.snapshot
        ?.entryIds ?? [];
    const visible = children.filter((id) => {
      const hidden = state.entries.get(id)?.hidden;
      return showHidden || hidden?.state !== 'known' || !hidden.value;
    });
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
  if (state.mode === 'mindmap' && nodes.length) {
    return layoutMindMap(nodes, children);
  }

  // Keep the root and first branch in view as more siblings arrive.
  let row = 0;
  for (const node of nodes) {
    node.x = 72 + node.depth * 320;
    node.y = 100 + row * 112;
    if (!children.has(node.id)) row++;
  }
  return {
    nodes,
    width: nodes.reduce(
      (max, n) => Math.max(max, n.x + n.width + 120),
      minimumGraphWidth,
    ),
    height: nodes.reduce(
      (max, n) => Math.max(max, n.y + n.height + 100),
      minimumGraphHeight,
    ),
  };
}
