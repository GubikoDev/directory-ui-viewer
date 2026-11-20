import { useLayoutEffect, useRef, useState } from 'react';
import type { RootSession } from '../model/types';

import type { GraphNode } from './presentation';
import type { ExplorerState } from '../model/store';

export function useGraphViewport(
  session: RootSession | null,
  mode: ExplorerState['mode'],
  anchor: GraphNode | undefined,
  positions: ReadonlyMap<string, GraphNode>,
) {
  const previous = useRef<{
    session: RootSession | null;
    mode: ExplorerState['mode'];
    anchor?: GraphNode;
    positions: ReadonlyMap<string, GraphNode>;
  } | null>(null);
  const ref = useRef<HTMLElement>(null);
  const [bounds, setBounds] = useState({
    left: 0,
    top: 0,
    width: 0,
    height: 0,
  });
  useLayoutEffect(() => {
    const element = ref.current;
    if (!element || !session) return;
    const update = () => {
      const left = element.scrollLeft,
        top = element.scrollTop;
      const width = element.clientWidth,
        height = element.clientHeight;
      setBounds((prior) =>
        prior.left === left &&
        prior.top === top &&
        prior.width === width &&
        prior.height === height
          ? prior
          : { left, top, width, height },
      );
    };
    element.scrollLeft = 0;
    element.scrollTop = 0;
    update();
    element.addEventListener('scroll', update, { passive: true });
    const observer =
      typeof ResizeObserver === 'undefined' ? null : new ResizeObserver(update);
    observer?.observe(element);
    return () => {
      element.removeEventListener('scroll', update);
      observer?.disconnect();
    };
  }, [session]);
  useLayoutEffect(() => {
    const old = previous.current;
    previous.current = { session, mode, anchor, positions };
    const element = ref.current;
    if (!element || !anchor || !element.clientWidth) return;
    if ((old?.mode !== mode || old.session !== session) && mode === 'mindmap') {
      element.scrollLeft = anchor.x + (anchor.width - element.clientWidth) / 2;
      element.scrollTop = anchor.y + (anchor.height - element.clientHeight) / 2;
    } else if (
      old?.session === session &&
      old.mode === mode &&
      old.positions.size > positions.size
    ) {
      // Collapsing offscreen branches can move the row the user is reading.
      for (const node of old.positions.values()) {
        if (
          node.x + node.width <= element.scrollLeft ||
          node.x >= element.scrollLeft + element.clientWidth ||
          node.y + node.height <= element.scrollTop ||
          node.y >= element.scrollTop + element.clientHeight
        )
          continue;
        const next = positions.get(node.id);
        if (next) {
          element.scrollLeft += next.x - node.x;
          element.scrollTop += next.y - node.y;
          break;
        }
      }
    } else if (old?.session === session && old.anchor?.id === anchor.id) {
      // Keep a visible selection in place as new branches change the layout.
      const x = old.anchor.x - element.scrollLeft,
        y = old.anchor.y - element.scrollTop;
      if (
        x > -old.anchor.width &&
        x < element.clientWidth &&
        y > -old.anchor.height &&
        y < element.clientHeight
      ) {
        element.scrollLeft += anchor.x - old.anchor.x;
        element.scrollTop += anchor.y - old.anchor.y;
      }
    }
  }, [session, mode, anchor, positions]);
  return { ref, bounds };
}
