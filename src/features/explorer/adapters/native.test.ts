import { beforeEach, describe, expect, it, vi } from 'vitest';
import { invoke } from '@tauri-apps/api/core';
import { NativeAdapter } from './native';
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
    vi.mocked(invoke).mockResolvedValue({
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
    vi.mocked(invoke).mockResolvedValueOnce(failure);
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
