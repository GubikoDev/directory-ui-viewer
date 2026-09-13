import { invoke } from '@tauri-apps/api/core';
import { nativeInbox, responseKey } from './inbox';
import { nativeEvents } from './events';
import { encodeWire } from './encoding';
import { frontendBudget, type FrontendBudget } from '../runtime/memory';
import type {
  FilesystemAdapter,
  Request,
  Response,
  WorkEvent,
} from '../model/types';

type Input<K extends keyof FilesystemAdapter> = Parameters<
  FilesystemAdapter[K]
>[0];
type Reply<K extends keyof FilesystemAdapter> = Awaited<
  ReturnType<FilesystemAdapter[K]>
>;

function errorCode(error: unknown): string {
  const code =
    typeof error === 'object' &&
    error !== null &&
    'code' in error &&
    typeof error.code === 'string'
      ? error.code
      : error instanceof Error
        ? error.message
        : 'INTERNAL';
  return /^[A-Z][A-Z0-9_]{0,63}$/.test(code) ? code : 'INTERNAL';
}
function documentProof(): string {
  const proof = (window as Window & { __DIRECTORY_DOCUMENT_PROOF__?: string })
    .__DIRECTORY_DOCUMENT_PROOF__;
  if (window !== window.top || !proof || !/^[0-9a-f]{64}$/.test(proof))
    throw new Error('CLIENT_EXPIRED');
  return proof;
}
/** Native wire translation only. Paths and native contexts never enter this API. */
export class NativeAdapter implements FilesystemAdapter {
  constructor(private readonly budget: FrontendBudget = frontendBudget) {}
  private call<T>(command: string, make: () => unknown): Promise<T> {
    const bytes = encodeWire(make, this.budget);
    const length = bytes.byteLength;
    // Installed Tauri's postMessage fallback uses Array.from(Uint8Array) and
    // JSON.stringify. Cover numeric array slots, UTF-16 JSON and callback/fetch
    // metadata before invoke. Native/WebView process RSS is measured separately.
    return this.budget.allocateLifetime(16 * 1024 + length * 96, () =>
      invoke<T>(command, bytes).finally(() => {
        // Keep the encoded view alive through completion, including fallback.
        if (bytes.byteLength !== length) throw new Error('INVALID_RESPONSE');
      }),
    );
  }
  async connectClient(
    input: Input<'connectClient'>,
  ): Promise<Reply<'connectClient'>> {
    // Native connect_client returns only a generated epoch string (<64 bytes
    // including JSON syntax). This reservation precedes Tauri's decoding;
    // schema growth must revisit it rather than adding a large uncharged field.
    const replyMemory = this.budget.reserve(4096);
    try {
      const reply = await this.call<Reply<'connectClient'>>(
        'connect_client',
        () => ({ ...input, documentProof: documentProof() }),
      );
      if (
        !reply ||
        typeof reply.epoch !== 'string' ||
        !/^native-epoch-[1-9][0-9]{0,15}$/.test(reply.epoch) ||
        !Number.isSafeInteger(Number(reply.epoch.slice('native-epoch-'.length)))
      )
        throw new Error('INVALID_RESPONSE');
      return this.budget.retain(reply, replyMemory);
    } catch (error) {
      // The boundary preserves the bounded protocol code, not an arbitrary
      // framework cause graph. Such objects must not escape into observations.
      // eslint-disable-next-line preserve-caught-error
      throw new Error(errorCode(error));
    } finally {
      replyMemory.close();
    }
  }
  private async send<T>(
    operation: string,
    input: Request,
  ): Promise<Response<T>> {
    const { protocolVersion, requestId, ...fields } = input;
    let release: (() => void) | undefined;
    try {
      const document = documentProof();
      const inbox = nativeInbox(this.budget);
      const key = responseKey();
      inbox.open(key);
      release = () => inbox.close(key);
      await this.call('filesystem_request', () => ({
        documentProof: document,
        responseKey: key,
        request: {
          protocolVersion,
          requestId,
          body: { operation, ...fields },
        },
      }));
      return inbox.parse(key) as Response<T>;
    } catch (error) {
      const code = errorCode(error);
      return {
        requestId,
        outcome: 'error',
        error: { code, operation: 'request', scope: 'root' },
      };
    } finally {
      release?.();
    }
  }
  chooseRoot(input: Input<'chooseRoot'>): Promise<Reply<'chooseRoot'>> {
    return this.send('chooseRoot', input);
  }
  startListing(input: Input<'startListing'>): Promise<Reply<'startListing'>> {
    return this.send('startListing', input);
  }
  readListingPage(
    input: Input<'readListingPage'>,
  ): Promise<Reply<'readListingPage'>> {
    return this.send('readListingPage', input);
  }
  startUsageScan(
    input: Input<'startUsageScan'>,
  ): Promise<Reply<'startUsageScan'>> {
    return this.send('startUsageScan', input);
  }
  readUsage(input: Input<'readUsage'>): Promise<Reply<'readUsage'>> {
    return this.send('readUsage', input);
  }
  readTask(input: Input<'readTask'>): Promise<Reply<'readTask'>> {
    return this.send('readTask', input);
  }
  cancelTask(input: Input<'cancelTask'>): Promise<Reply<'cancelTask'>> {
    return this.send('cancelTask', input);
  }
  refreshRoot(input: Input<'refreshRoot'>): Promise<Reply<'refreshRoot'>> {
    return this.send('refreshRoot', input);
  }
  closeSession(input: Input<'closeSession'>): Promise<Reply<'closeSession'>> {
    return this.send('closeSession', input);
  }
  async subscribe(listener: (event: WorkEvent) => void): Promise<() => void> {
    documentProof();
    return nativeEvents(this.budget).subscribe(listener);
  }
}
