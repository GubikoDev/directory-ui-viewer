import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { nativeInbox, responseKey } from './inbox';
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
  return typeof error === 'object' &&
    error !== null &&
    'code' in error &&
    typeof error.code === 'string'
    ? error.code
    : error instanceof Error
      ? error.message
      : 'INTERNAL';
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
  private encode(value: unknown): Uint8Array {
    const bytes = new TextEncoder().encode(JSON.stringify(value));
    if (bytes.byteLength > 1_048_576) throw new Error('RESOURCE_LIMIT');
    return bytes;
  }
  async connectClient(
    input: Input<'connectClient'>,
  ): Promise<Reply<'connectClient'>> {
    try {
      return await invoke(
        'connect_client',
        this.encode({ ...input, documentProof: documentProof() }),
      );
    } catch (error) {
      throw new Error(errorCode(error), { cause: error });
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
      const inbox = nativeInbox();
      const key = responseKey();
      inbox.open(key);
      release = () => inbox.close(key);
      await invoke(
        'filesystem_request',
        this.encode({
          documentProof: document,
          responseKey: key,
          request: {
            protocolVersion,
            requestId,
            body: { operation, ...fields },
          },
        }),
      );
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
  subscribe(listener: (event: WorkEvent) => void): Promise<() => void> {
    return listen<WorkEvent>('filesystem-work', (event) =>
      listener(event.payload),
    );
  }
}
