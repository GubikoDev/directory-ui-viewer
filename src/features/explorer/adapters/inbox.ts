import {
  frontendBudget,
  type FrontendBudget,
  type FrontendReservation,
} from '../runtime/memory';
/** Private per-document response slots. Native IPC carries only the completion ack. */
export class ReplyInbox {
  private slots: Map<
    string,
    {
      json?: string;
      error?: string;
      bytes: number;
      capacity: number;
      memory: FrontendReservation;
      priority: boolean;
      admit?: () => void;
    }
  >;
  private used = 0;
  private dispatchTail = Promise.resolve();
  constructor(
    private readonly maxBytes = 8 * 1024 * 1024,
    private readonly maxPending = 256,
    readonly budget: FrontendBudget = frontendBudget,
    private readonly controlSlots = 0,
    private readonly controlBytes = 0,
    priority = false,
  ) {
    if (!Number.isSafeInteger(maxPending) || maxPending < 0 || maxPending > 256)
      throw new Error('INVALID_ARGUMENT');
    // The private Map cannot escape this manager. Cover its maximum capacity,
    // slot/key bookkeeping and one synchronous response-key construction.
    const bytes = 4096 + maxPending * 256;
    const memory = budget.reserve(bytes, priority);
    try {
      this.slots = new Map();
      budget.retainLifetimeFrom(this, memory, bytes);
    } finally {
      memory.close();
    }
  }
  get bytes() {
    // Encoded storage/capacity, including preadmitted mutation credit. Parsed
    // observations have their own document-wide GC cost after a slot closes.
    return this.used;
  }
  get pending() {
    return this.slots.size;
  }
  /** Serialize native admission, not picker/worker completion. */
  dispatch(key: string, action: () => Promise<void>, priority = false) {
    const slot = this.slots.get(key);
    if (!slot || slot.admit) throw new Error('INVALID_ARGUMENT');
    const memory = this.budget.reserve(24 * 1024, priority);
    try {
      let resolve!: () => void;
      const admitted = new Promise<void>((done) => {
        resolve = done;
      });
      this.budget.retainLifetimeFrom(admitted, memory, 8 * 1024);
      const release = () => resolve();
      // The receiver can outlive the awaited promise. Charge its closure until
      // its own GC, including the deferred resolver it holds.
      this.budget.retainLifetimeFrom(release, memory, 8 * 1024);
      const result = this.dispatchTail.then(async () => {
        try {
          await action();
        } finally {
          release();
        }
      });
      this.budget.retainLifetimeFrom(result, memory, 8 * 1024);
      slot.admit = release;
      this.dispatchTail = admitted;
      return result;
    } finally {
      memory.close();
    }
  }
  admit = (key: string): boolean => {
    const slot = this.slots.get(key);
    if (!slot?.admit) return false;
    slot.admit();
    slot.admit = undefined;
    return true;
  };
  open(key: string, priority = false, replyCredit = 0) {
    if (this.slots.has(key)) throw new Error('INVALID_ARGUMENT');
    const capacity = Math.max(512, replyCredit);
    if (
      this.slots.size >= this.maxPending - (priority ? 0 : this.controlSlots) ||
      this.used + capacity > this.maxBytes - (priority ? 0 : this.controlBytes)
    )
      throw new Error('RESOURCE_LIMIT');
    const memory = this.budget.reserve(capacity, priority);
    this.slots.set(key, { bytes: 512, capacity, memory, priority });
    // Reserve the inbox limit as well as the shared pool before effects. The
    // credit also covers parsing, so this is conservative encoded admission.
    this.used += capacity;
  }
  receive = (key: string, json: string): boolean => {
    const slot = this.slots.get(key);
    if (!slot || slot.json !== undefined || slot.error !== undefined)
      return false;
    if (typeof json !== 'string' || json.length > 1_048_576) {
      slot.error = 'INVALID_RESPONSE';
    } else {
      const bytes = json.length * 2;
      const capacity = Math.max(slot.capacity, slot.bytes + bytes);
      if (
        this.used + capacity - slot.capacity >
        this.maxBytes - (slot.priority ? 0 : this.controlBytes)
      )
        slot.error = 'RESOURCE_LIMIT';
      else {
        try {
          slot.memory.resize(Math.max(slot.memory.bytes, slot.bytes + bytes));
          slot.json = json;
          slot.bytes += bytes;
          this.used += capacity - slot.capacity;
          slot.capacity = capacity;
        } catch {
          slot.error = 'RESOURCE_LIMIT';
        }
      }
    }
    // Ack means this slot is settled, including a local resource/format error.
    // It is not application success: read() propagates the recorded error after
    // native completion. Returning false here would lose the specific error.
    return true;
  };
  read(key: string): string {
    const slot = this.slots.get(key);
    if (!slot || slot.error || slot.json === undefined)
      throw new Error(slot?.error ?? 'INVALID_RESPONSE');
    return slot.json;
  }
  parse(
    key: string,
    prepare?: Parameters<FrontendBudget['parse']>[3],
  ): unknown {
    // Parsed descendants keep individual reservations after their response slot
    // is closed; a store/snapshot may retain them longer than the outer reply.
    const json = this.read(key);
    const slot = this.slots.get(key)!;
    const bytes = this.budget.parseBytes(json);
    // Mutation replies were admitted before invoking effects. Keep encoded
    // storage live while transferring the unused credit to parsed owners.
    slot.memory.resize(Math.max(slot.memory.bytes, slot.bytes + bytes));
    const parsed = slot.memory.take(bytes);
    try {
      return this.budget.parse(json, slot.priority, parsed, prepare);
    } finally {
      parsed.close();
    }
  }
  close(key: string) {
    const slot = this.slots.get(key);
    if (slot) {
      this.used -= slot.capacity;
      this.slots.delete(key);
      slot.memory.close();
    }
  }
}
let inbox: ReplyInbox | undefined;
export function nativeInbox(
  budget: FrontendBudget = frontendBudget,
  priority = false,
): ReplyInbox {
  const target = window as Window & {
    __DIRECTORY_RECEIVE__?: ReplyInbox['receive'];
    __DIRECTORY_ADMIT__?: ReplyInbox['admit'];
  };
  inbox ??= new ReplyInbox(
    8 * 1024 * 1024,
    256,
    budget,
    2,
    // One maximum cancel reply plus close reply and slot bookkeeping.
    736 * 1024,
    priority,
  );
  if (inbox.budget !== budget) throw new Error('INVALID_RESERVATION');
  if (!target.__DIRECTORY_RECEIVE__)
    Object.defineProperty(target, '__DIRECTORY_RECEIVE__', {
      value: inbox.receive,
    });
  if (target.__DIRECTORY_RECEIVE__ !== inbox.receive)
    throw new Error('CLIENT_EXPIRED');
  if (!target.__DIRECTORY_ADMIT__)
    Object.defineProperty(target, '__DIRECTORY_ADMIT__', {
      value: inbox.admit,
    });
  if (target.__DIRECTORY_ADMIT__ !== inbox.admit)
    throw new Error('CLIENT_EXPIRED');
  return inbox;
}
export function responseKey() {
  return Array.from(crypto.getRandomValues(new Uint8Array(16)), (byte) =>
    byte.toString(16).padStart(2, '0'),
  ).join('');
}
