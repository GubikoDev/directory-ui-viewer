/// <reference types="node" />

import { spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { createInterface } from 'node:readline';
import { resolve } from 'node:path';
import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
} from '@testing-library/react';
import { expect, it, vi } from 'vitest';
import { invoke } from '@tauri-apps/api/core';
import { App } from './App';
import { SessionClient } from './features/explorer/client/session-client';
import { NativeAdapter } from './features/explorer/adapters/native';
import { nativeInbox } from './features/explorer/adapters/inbox';
vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
  isTauri: () => true,
}));
vi.mock('@tauri-apps/api/event', () => ({ listen: vi.fn() }));

// Opt-in: compile examples/ui_smoke_bridge first. Tauri's transport is replaced
// by stdio; UI, SessionClient, NativeAdapter and the real native engine remain.
it.skipIf(process.env.DUV_NATIVE_UI_SMOKE !== '1')(
  'UI receives real macOS engine responses through NativeAdapter',
  async () => {
    Object.defineProperty(window, '__DIRECTORY_DOCUMENT_PROOF__', {
      value: 'a'.repeat(64),
      configurable: true,
    });
    const processBridge: ChildProcessWithoutNullStreams = spawn(
      resolve('src-tauri/target/debug/examples/ui_smoke_bridge'),
    );
    const lines = createInterface({ input: processBridge.stdout });
    const pending: Array<{
      resolve: (value: unknown) => void;
      reject: (error: Error) => void;
    }> = [];
    lines.on('line', (line) => pending.shift()?.resolve(JSON.parse(line)));
    processBridge.on('exit', () => {
      for (const waiter of pending.splice(0))
        waiter.reject(new Error('BRIDGE_EXITED'));
    });
    const operations: string[] = [];
    const send = (command: string, payload: unknown) =>
      new Promise((resolve) => {
        pending.push({ resolve, reject: () => resolve(null) });
        processBridge.stdin.write(`${JSON.stringify({ command, payload })}\n`);
      });
    vi.mocked(invoke).mockImplementation(async (command, args) => {
      const payload = JSON.parse(new TextDecoder().decode(args as Uint8Array));
      if (command === 'connect_client')
        return (await send(command, payload)) as never;
      operations.push(payload.request.body.operation);
      const reply = await send(command, payload);
      expect(
        nativeInbox().receive(payload.responseKey, JSON.stringify(reply)),
      ).toBe(true);
      return undefined as never;
    });
    const client = new SessionClient(new NativeAdapter());
    const view = render(<App client={client} />);
    try {
      fireEvent.click(screen.getByRole('button', { name: '폴더 열기' }));
      await waitFor(
        () =>
          expect(client.store.getSnapshot().session?.platform).toBe('macos'),
        { timeout: 10000 },
      );
      await act(async () => {
        await client.poll();
      });
      const child = await screen.findByRole(
        'button',
        { name: 'child' },
        { timeout: 10000 },
      );
      fireEvent.click(child);
      await waitFor(() =>
        expect(client.store.getSnapshot().selectedId).not.toBe(
          client.store.getSnapshot().session?.rootEntryId,
        ),
      );
      fireEvent.click(screen.getByRole('button', { name: 'child 파일 목록' }));
      await act(async () => {
        await client.poll();
      });
      expect(
        await screen.findByText('value.txt', {}, { timeout: 10000 }),
      ).toBeInTheDocument();
      await act(async () => {
        await client.poll();
      });
      const selected = client.store.getSnapshot().selectedId!;
      await waitFor(() =>
        expect(
          client.store.getSnapshot().usages.get(selected)?.logical,
        ).toMatchObject({ state: 'complete' }),
      );
      const before = operations.filter((x) => x === 'startUsageScan').length;
      fireEvent.click(screen.getByRole('button', { name: '마인드맵' }));
      expect(operations.filter((x) => x === 'startUsageScan')).toHaveLength(
        before,
      );
      fireEvent.click(screen.getByRole('button', { name: '새로고침' }));
      fireEvent.click(screen.getByRole('button', { name: '다시 읽기' }));
      await waitFor(() =>
        expect(client.store.getSnapshot().session?.generation).toBe(2),
      );
      fireEvent.click(screen.getByRole('button', { name: '연결 닫기' }));
      await waitFor(() => expect(client.hasSession).toBe(false));
      for (const operation of [
        'chooseRoot',
        'startListing',
        'startUsageScan',
        'readListingPage',
        'readUsage',
        'refreshRoot',
        'closeSession',
      ])
        expect(operations).toContain(operation);
    } finally {
      view.unmount();
      processBridge.stdin.end();
      await new Promise<void>((resolve) =>
        processBridge.once('exit', () => resolve()),
      );
      lines.close();
    }
  },
  30000,
);
