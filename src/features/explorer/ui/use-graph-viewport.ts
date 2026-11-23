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
  const canvasRef = useRef<HTMLDivElement>(null);
  const scaleRef = useRef(1);
  const [bounds, setBounds] = useState({
    left: 0,
    top: 0,
    width: 0,
    height: 0,
  });
  useLayoutEffect(() => {
    const element = ref.current;
    const canvas = canvasRef.current;
    if (!element || !canvas || !session) return;
    scaleRef.current = 1;
    canvas.style.zoom = '1';
    const update = () => {
      const scale = scaleRef.current;
      const left = element.scrollLeft / scale,
        top = element.scrollTop / scale;
      const width = element.clientWidth / scale,
        height = element.clientHeight / scale;
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
    const zoomAt = (requested: number, clientX: number, clientY: number) => {
      if (!Number.isFinite(requested)) return;
      const scale = Math.max(0.25, Math.min(4, requested));
      const ratio = scale / scaleRef.current;
      const rect = element.getBoundingClientRect();
      const x = clientX - rect.left;
      const y = clientY - rect.top;
      const left = (element.scrollLeft + x) * ratio - x;
      const top = (element.scrollTop + y) * ratio - y;
      canvas.style.zoom = String(scale);
      scaleRef.current = scale;
      element.scrollLeft = left;
      element.scrollTop = top;
      update();
    };
    // WebKit reports cumulative gesture scale. Chromium sends ctrl-wheel deltas.
    let gestureBase: number | undefined;
    const gestureStart = (event: Event) => {
      event.preventDefault();
      gestureBase = scaleRef.current;
    };
    const gestureChange = (event: Event) => {
      if (gestureBase === undefined) return;
      event.preventDefault();
      const pinch = event as MouseEvent & { scale: number };
      zoomAt(gestureBase * pinch.scale, pinch.clientX, pinch.clientY);
    };
    const gestureEnd = (event: Event) => {
      event.preventDefault();
      gestureBase = undefined;
    };
    const wheel = (event: WheelEvent) => {
      if (!event.ctrlKey) return;
      event.preventDefault();
      if (gestureBase !== undefined) return;
      const unit =
        event.deltaMode === 1
          ? 16
          : event.deltaMode === 2
            ? element.clientHeight
            : 1;
      zoomAt(
        scaleRef.current * Math.exp((-event.deltaY * unit) / 100),
        event.clientX,
        event.clientY,
      );
    };
    const active = { passive: false };
    element.addEventListener('wheel', wheel, active);
    element.addEventListener('gesturestart', gestureStart, active);
    element.addEventListener('gesturechange', gestureChange, active);
    element.addEventListener('gestureend', gestureEnd, active);
    element.addEventListener('scroll', update, { passive: true });
    const observer =
      typeof ResizeObserver === 'undefined' ? null : new ResizeObserver(update);
    observer?.observe(element);
    return () => {
      element.removeEventListener('wheel', wheel);
      element.removeEventListener('gesturestart', gestureStart);
      element.removeEventListener('gesturechange', gestureChange);
      element.removeEventListener('gestureend', gestureEnd);
      element.removeEventListener('scroll', update);
      observer?.disconnect();
    };
  }, [session]);
  useLayoutEffect(() => {
    const old = previous.current;
    previous.current = { session, mode, anchor, positions };
    const element = ref.current;
    if (!element || !anchor || !element.clientWidth) return;
    const scale = scaleRef.current;
    if ((old?.mode !== mode || old.session !== session) && mode === 'mindmap') {
      element.scrollLeft =
        (anchor.x + anchor.width / 2) * scale - element.clientWidth / 2;
      element.scrollTop =
        (anchor.y + anchor.height / 2) * scale - element.clientHeight / 2;
    } else if (
      old?.session === session &&
      old.mode === mode &&
      old.positions.size > positions.size
    ) {
      // Collapsing offscreen branches can move the row the user is reading.
      for (const node of old.positions.values()) {
        if (
          (node.x + node.width) * scale <= element.scrollLeft ||
          node.x * scale >= element.scrollLeft + element.clientWidth ||
          (node.y + node.height) * scale <= element.scrollTop ||
          node.y * scale >= element.scrollTop + element.clientHeight
        )
          continue;
        const next = positions.get(node.id);
        if (next) {
          element.scrollLeft += (next.x - node.x) * scale;
          element.scrollTop += (next.y - node.y) * scale;
          break;
        }
      }
    } else if (old?.session === session && old.anchor?.id === anchor.id) {
      // Keep a visible selection in place as new branches change the layout.
      const x = old.anchor.x * scale - element.scrollLeft,
        y = old.anchor.y * scale - element.scrollTop;
      if (
        x > -old.anchor.width * scale &&
        x < element.clientWidth &&
        y > -old.anchor.height * scale &&
        y < element.clientHeight
      ) {
        element.scrollLeft += (anchor.x - old.anchor.x) * scale;
        element.scrollTop += (anchor.y - old.anchor.y) * scale;
      }
    }
  }, [session, mode, anchor, positions]);
  return { ref, canvasRef, bounds };
}
