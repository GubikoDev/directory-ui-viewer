/** Private per-document response slots. Native IPC carries only the completion ack. */
export class ReplyInbox {
  private slots = new Map<
    string,
    { json?: string; error?: string; bytes: number }
  >();
  private used = 0;
  constructor(
    private readonly maxBytes = 8 * 1024 * 1024,
    private readonly maxPending = 256,
  ) {}
  get bytes() {
    return this.used;
  }
  get pending() {
    return this.slots.size;
  }
  open(key: string) {
    if (this.slots.has(key)) throw new Error('INVALID_ARGUMENT');
    if (this.slots.size >= this.maxPending || this.used + 512 > this.maxBytes)
      throw new Error('RESOURCE_LIMIT');
    this.slots.set(key, { bytes: 512 });
    this.used += 512;
  }
  receive = (key: string, json: string): boolean => {
    const slot = this.slots.get(key);
    if (!slot || slot.json !== undefined || slot.error !== undefined)
      return false;
    if (typeof json !== 'string' || json.length > 1_048_576) {
      slot.error = 'INVALID_RESPONSE';
    } else {
      const bytes = json.length * 2;
      if (this.used + bytes > this.maxBytes) slot.error = 'RESOURCE_LIMIT';
      else {
        slot.json = json;
        slot.bytes += bytes;
        this.used += bytes;
      }
    }
    return true;
  };
  read(key: string): string {
    const slot = this.slots.get(key);
    if (!slot || slot.error || slot.json === undefined)
      throw new Error(slot?.error ?? 'INVALID_RESPONSE');
    return slot.json;
  }
  close(key: string) {
    const slot = this.slots.get(key);
    if (slot) {
      this.used -= slot.bytes;
      this.slots.delete(key);
    }
  }
}
const inbox = new ReplyInbox();
export function nativeInbox(): ReplyInbox {
  const target = window as Window & {
    __DIRECTORY_RECEIVE__?: ReplyInbox['receive'];
  };
  if (!target.__DIRECTORY_RECEIVE__)
    Object.defineProperty(target, '__DIRECTORY_RECEIVE__', {
      value: inbox.receive,
    });
  if (target.__DIRECTORY_RECEIVE__ !== inbox.receive)
    throw new Error('CLIENT_EXPIRED');
  return inbox;
}
export function responseKey() {
  return Array.from(crypto.getRandomValues(new Uint8Array(16)), (byte) =>
    byte.toString(16).padStart(2, '0'),
  ).join('');
}
