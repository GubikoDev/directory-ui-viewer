import { beforeEach, describe, expect, it, vi } from 'vitest';
import { invoke } from '@tauri-apps/api/core';
import { NativeAdapter } from './native';
import { nativeInbox } from './inbox';
import { nativeEvents } from './events';
import wire from '../model/wire-fixture.json';
import { FrontendBudget, ownBytes } from '../runtime/memory';
function deliverOnce(response: unknown) {
  vi.mocked(invoke).mockImplementationOnce(async (_command, args) => {
    const envelope = JSON.parse(new TextDecoder().decode(args as Uint8Array));
    expect(
      nativeInbox().receive(envelope.responseKey, JSON.stringify(response)),
    ).toBe(true);
    return undefined as never;
  });
}
vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));
vi.mock('@tauri-apps/api/event', () => ({ listen: vi.fn() }));
const request = { protocolVersion: 1 as const, requestId: 'native-epoch-1:2' };
beforeEach(() => {
  vi.clearAllMocks();
  Object.defineProperty(window, '__DIRECTORY_DOCUMENT_PROOF__', {
    value: 'a'.repeat(64),
    configurable: true,
  });
});
describe('native wire boundary', () => {
  it('does not split one document receiver across independent budgets', () => {
    nativeInbox();
    nativeEvents();
    expect(() => nativeInbox(new FrontendBudget())).toThrow(
      'INVALID_RESERVATION',
    );
    expect(() => nativeEvents(new FrontendBudget())).toThrow(
      'INVALID_RESERVATION',
    );
  });
  it('precharges the IPC fallback before dispatch and retains a connected epoch observation', async () => {
    const records = new Map<object, number>();
    const budget = new FrontendBudget(128 * 1024, () => ({
      register: (owner, bytes) => {
        records.set(owner, bytes);
      },
    }));
    const reply = { epoch: 'native-epoch-1' };
    vi.mocked(invoke).mockImplementationOnce(async (_command, args) => {
      const length = (args as Uint8Array).byteLength;
      expect(budget.used).toBeGreaterThanOrEqual(
        16 * 1024 + length * 96 + 4096,
      );
      return reply as never;
    });
    expect(
      await new NativeAdapter(budget).connectClient({
        protocolVersion: 1,
        clientNonce: 'n',
      }),
    ).toBe(reply);
    expect(records.get(reply)).toBe(ownBytes(reply));
    vi.mocked(invoke).mockClear();
    const small = new NativeAdapter(new FrontendBudget(24 * 1024));
    await expect(
      small.connectClient({ protocolVersion: 1, clientNonce: 'n' }),
    ).rejects.toThrow('RESOURCE_LIMIT');
    expect(invoke).not.toHaveBeenCalled();
  });
  it('rejects an unsafe or malformed connected epoch', async () => {
    for (const epoch of [
      'native-epoch-9007199254740992',
      'native-epoch-0',
      'bad',
    ]) {
      vi.mocked(invoke).mockResolvedValueOnce({ epoch });
      await expect(
        new NativeAdapter().connectClient({
          protocolVersion: 1,
          clientNonce: 'n',
        }),
      ).rejects.toThrow('INVALID_RESPONSE');
    }
  });
  it('keeps only a bounded protocol error code instead of retaining framework cause graphs', async () => {
    vi.mocked(invoke).mockRejectedValueOnce({
      code: 'x'.repeat(100_000),
      extra: { retained: true },
    });
    const error = await new NativeAdapter()
      .connectClient({ protocolVersion: 1, clientNonce: 'n' })
      .catch((value: unknown) => value);
    expect(error).toBeInstanceOf(Error);
    expect((error as Error).message).toBe('INTERNAL');
    expect((error as Error).cause).toBeUndefined();
  });
  it('subscribes through the document receiver without the framework event queue', async () => {
    const listener = vi.fn();
    const stop = await new NativeAdapter().subscribe(listener);
    expect(nativeEvents().receive(JSON.stringify(wire.event))).toBe(true);
    expect(listener).toHaveBeenCalledWith(wire.event);
    stop();
    expect(nativeEvents().receive(JSON.stringify(wire.event))).toBe(false);
    expect(invoke).not.toHaveBeenCalled();
  });
  it('reports a structured connection rejection without losing its code', async () => {
    vi.mocked(invoke).mockRejectedValueOnce({ code: 'CLIENT_EXPIRED' });
    await expect(
      new NativeAdapter().connectClient({
        protocolVersion: 1,
        clientNonce: 'n',
      }),
    ).rejects.toThrow('CLIENT_EXPIRED');
  });
  it('sends a bounded binary envelope with document proof and typed operation', async () => {
    deliverOnce({
      requestId: request.requestId,
      outcome: 'cancelled',
    });
    const adapter = new NativeAdapter();
    await adapter.chooseRoot(request);
    const [command, input] = vi.mocked(invoke).mock.calls[0];
    expect(command).toBe('filesystem_request');
    expect(ArrayBuffer.isView(input)).toBe(true);
    expect(JSON.parse(new TextDecoder().decode(input as Uint8Array))).toEqual({
      documentProof: 'a'.repeat(64),
      responseKey: expect.stringMatching(/^[0-9a-f]{32}$/),
      request: { ...request, body: { operation: 'chooseRoot' } },
    });
  });
  it('preserves refresh failure scope and turns boundary rejection into a matching error response', async () => {
    const adapter = new NativeAdapter();
    const failure = {
      requestId: request.requestId,
      outcome: 'error',
      error: { code: 'IO_ERROR', operation: 'refreshRoot', scope: 'root' },
      currentScope: { sessionId: 's', generation: 2 },
    };
    deliverOnce(failure);
    expect(
      await adapter.refreshRoot({
        ...request,
        sessionId: 's',
        expectedGeneration: 1,
      }),
    ).toEqual(failure);
    vi.mocked(invoke).mockRejectedValueOnce({ code: 'CLIENT_EXPIRED' });
    expect(await adapter.chooseRoot(request)).toMatchObject({
      requestId: request.requestId,
      outcome: 'error',
      error: { code: 'CLIENT_EXPIRED' },
    });
  });
  it('does not invoke native code without a valid main-document proof', async () => {
    Object.defineProperty(window, '__DIRECTORY_DOCUMENT_PROOF__', {
      value: undefined,
    });
    expect(await new NativeAdapter().chooseRoot(request)).toMatchObject({
      outcome: 'error',
      error: { code: 'CLIENT_EXPIRED' },
    });
    expect(invoke).not.toHaveBeenCalled();
  });
});
