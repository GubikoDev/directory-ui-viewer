/** Managed observations and transport costs, separate from WebView process RSS. */
export const FRONTEND_BYTES = 32 * 1024 * 1024;
type Collector = { register(target: object, bytes: number): void };
type CollectorFactory = (release: (bytes: number) => void) => Collector;

function validBytes(bytes: number) {
  if (!Number.isSafeInteger(bytes) || bytes < 0)
    throw new Error('RESOURCE_LIMIT');
}
function primitiveBytes(value: unknown): number {
  if (typeof value === 'string') return 64 + value.length * 2;
  if (typeof value === 'function' || typeof value === 'symbol')
    throw new Error('INVALID_ARGUMENT');
  return 16;
}
/** Includes conservative container/registry overhead and owned primitive strings.
 * Child objects are charged individually so shared or retained children outlive
 * their original response/snapshot without losing their reservations. */
export function ownBytes(value: object): number {
  let bytes = 512;
  if (value instanceof Map) {
    value.forEach((child, key) => {
      bytes += 128 + primitiveBytes(key) + primitiveBytes(child);
    });
  } else if (value instanceof Set) {
    value.forEach((child) => {
      bytes += 128 + primitiveBytes(child);
    });
  } else if (Array.isArray(value)) {
    for (const child of value) bytes += 64 + primitiveBytes(child);
  } else {
    const prototype = Object.getPrototypeOf(value);
    if (prototype !== Object.prototype && prototype !== null)
      throw new Error('INVALID_ARGUMENT');
    for (const key in value) {
      if (Object.hasOwn(value, key))
        bytes +=
          128 +
          primitiveBytes(key) +
          primitiveBytes((value as Record<string, unknown>)[key]);
    }
  }
  validBytes(bytes);
  return bytes;
}
function children(value: object, visit: (value: unknown) => void) {
  if (value instanceof Map)
    value.forEach((child, key) => {
      visit(key);
      visit(child);
    });
  else if (value instanceof Set) value.forEach(visit);
  else if (Array.isArray(value)) {
    for (const child of value) visit(child);
  } else
    for (const key in value) {
      if (Object.hasOwn(value, key))
        visit((value as Record<string, unknown>)[key]);
    }
}

/** An explicit reservation covers allocations before their tracked owners exist. */
export class FrontendReservation {
  private remaining: number;
  constructor(
    private readonly budget: FrontendBudget,
    bytes: number,
    private readonly priority = false,
  ) {
    this.remaining = bytes;
  }
  get bytes() {
    return this.remaining;
  }
  belongsTo(budget: FrontendBudget) {
    return this.budget === budget;
  }
  resize(bytes: number) {
    validBytes(bytes);
    if (bytes > this.remaining)
      this.budget.acquire(bytes - this.remaining, this.priority);
    else this.budget.release(this.remaining - bytes);
    this.remaining = bytes;
  }
  transfer(bytes: number) {
    validBytes(bytes);
    if (bytes > this.remaining) throw new Error('RESOURCE_LIMIT');
    this.remaining -= bytes;
  }
  take(bytes: number): FrontendReservation {
    this.transfer(bytes);
    return new FrontendReservation(this.budget, bytes, this.priority);
  }
  close() {
    this.resize(0);
  }
}

export class FrontendBudget {
  private usedBytes = 0;
  private peakBytes = 0;
  private readonly tracked = new WeakMap<object, number>();
  private readonly opaque = new WeakSet<object>();
  private readonly collector: Collector;
  constructor(
    readonly limit = FRONTEND_BYTES,
    collector: CollectorFactory = (release) =>
      new FinalizationRegistry(release),
    readonly controlHeadroom = 0,
  ) {
    validBytes(limit);
    validBytes(controlHeadroom);
    if (controlHeadroom > limit) throw new Error('INVALID_RESERVATION');
    this.collector = collector((bytes) => this.release(bytes));
  }
  get used() {
    return this.usedBytes;
  }
  get peak() {
    return this.peakBytes;
  }
  get available() {
    return Math.max(0, this.limit - this.controlHeadroom - this.usedBytes);
  }
  acquire(bytes: number, priority = false) {
    validBytes(bytes);
    const ceiling = this.limit - (priority ? 0 : this.controlHeadroom);
    if (bytes > ceiling - this.usedBytes) throw new Error('RESOURCE_LIMIT');
    this.usedBytes += bytes;
    this.peakBytes = Math.max(this.peakBytes, this.usedBytes);
  }
  release(bytes: number) {
    validBytes(bytes);
    if (bytes > this.usedBytes) throw new Error('INVALID_RESERVATION');
    this.usedBytes -= bytes;
  }
  reserve(bytes: number, priority = false) {
    this.acquire(bytes, priority);
    return new FrontendReservation(this, bytes, priority);
  }
  /** Pure sizing; shared new references can overcount but never undercount.
   * Cyclic/unreasonably deep inputs are rejected without a traversal allocation. */
  untrackedBytes(value: unknown, depth = 0): number {
    if (value === null || typeof value !== 'object' || this.tracked.has(value))
      return 0;
    if (depth > 64) throw new Error('INVALID_ARGUMENT');
    let bytes = ownBytes(value);
    children(value, (child) => {
      bytes += this.untrackedBytes(child, depth + 1);
    });
    validBytes(bytes);
    return bytes;
  }
  retain<T>(value: T, memory: FrontendReservation): T {
    if (!memory.belongsTo(this)) throw new Error('INVALID_RESERVATION');
    if (this.untrackedBytes(value) > memory.bytes)
      throw new Error('RESOURCE_LIMIT');
    this.track(value, memory);
    return value;
  }
  /** Fixed manager/listener metadata only; variable collections are tracked separately. */
  retainLifetime(owner: object, bytes: number) {
    this.allocateLifetime(bytes, () => owner);
  }
  /** Preallocate opaque runtime owners (e.g. Promise/Map), whose variable
   * children are separately charged by the caller. Never traverse their internals. */
  allocateLifetime<T extends object>(
    bytes: number,
    factory: () => T,
    priority = false,
  ): T {
    const memory = this.reserve(bytes, priority);
    try {
      const owner = factory();
      this.retainLifetimeFrom(owner, memory, bytes);
      return owner;
    } finally {
      memory.close();
    }
  }
  /** Split an already admitted transaction across independent opaque owners. */
  retainLifetimeFrom(
    owner: object,
    memory: FrontendReservation,
    bytes: number,
  ) {
    validBytes(bytes);
    if (!memory.belongsTo(this) || this.tracked.has(owner))
      throw new Error('INVALID_RESERVATION');
    if (bytes > memory.bytes) throw new Error('RESOURCE_LIMIT');
    this.collector.register(owner, bytes);
    this.tracked.set(owner, bytes);
    this.opaque.add(owner);
    memory.transfer(bytes);
  }
  hasReservation(owner: object): boolean {
    return this.tracked.has(owner);
  }
  /** Mutable container capacity never shrinks on delete. Each growth charge
   * remains attached to that actual container until it is garbage-collected. */
  growLifetime(owner: object, bytes: number) {
    if (!this.opaque.has(owner)) throw new Error('INVALID_RESERVATION');
    const memory = this.reserve(bytes);
    try {
      this.collector.register(owner, bytes);
      this.tracked.set(owner, this.tracked.get(owner)! + bytes);
      memory.transfer(bytes);
    } finally {
      memory.close();
    }
  }
  private track(value: unknown, memory: FrontendReservation, depth = 0) {
    if (value === null || typeof value !== 'object' || this.tracked.has(value))
      return;
    if (depth > 64) throw new Error('INVALID_ARGUMENT');
    const bytes = ownBytes(value);
    if (bytes > memory.bytes) throw new Error('RESOURCE_LIMIT');
    this.collector.register(value, bytes);
    this.tracked.set(value, bytes);
    memory.transfer(bytes);
    children(value, (child) => this.track(child, memory, depth + 1));
    // Parsed records/arrays are observations, never mutable backing storage.
    // Map/Set owners must use copy-on-write builders; Object.freeze alone does
    // not freeze their entries, so they are deliberately not treated as such.
    if (!(value instanceof Map) && !(value instanceof Set))
      Object.freeze(value);
  }
  /** Adopts already-owned inputs (e.g. fixture values), not a preallocation API. */
  adopt<T>(value: T): T {
    const memory = this.reserve(this.untrackedBytes(value));
    try {
      this.retain(value, memory);
      return value;
    } finally {
      memory.close();
    }
  }
  /** The caller must bound the factory's whole transient allocation, including
   * objects discarded by the factory. Registered owners retain their own cost
   * until collection, even after the surrounding state or request is replaced. */
  allocate<T>(bytes: number, factory: () => T, priority = false): T {
    const memory = this.reserve(bytes, priority);
    try {
      const value = factory();
      this.retain(value, memory);
      return value;
    } finally {
      memory.close();
    }
  }
  parseBytes(json: string): number {
    // Count structural tokens outside strings before JSON.parse. String UTF-16
    // storage plus per-node/property/slot headroom bounds native wire objects.
    if (json.length > 1_048_576) throw new Error('RESOURCE_LIMIT');
    let bytes = 4096 + json.length * 2;
    let depth = 0;
    let quoted = false;
    let escaped = false;
    for (let i = 0; i < json.length; i++) {
      const char = json[i];
      if (quoted) {
        if (escaped) escaped = false;
        else if (char === '\\') escaped = true;
        else if (char === '"') quoted = false;
      } else if (char === '"') {
        quoted = true;
        bytes += 128;
      } else if (char === '{' || char === '[') {
        if (++depth > 64) throw new Error('INVALID_RESPONSE');
        bytes += 1024;
      } else if (char === '}' || char === ']') depth--;
      else if (char === ':' || char === ',') bytes += 512;
    }
    return bytes;
  }
  parse(
    json: string,
    priority = false,
    reserved?: FrontendReservation,
  ): unknown {
    const bytes = this.parseBytes(json);
    const memory = reserved ?? this.reserve(bytes, priority);
    try {
      if (!memory.belongsTo(this)) throw new Error('INVALID_RESERVATION');
      if (memory.bytes < bytes) throw new Error('RESOURCE_LIMIT');
      const value: unknown = JSON.parse(json);
      if (value === null || typeof value !== 'object')
        throw new Error('INVALID_RESPONSE');
      return this.retain(value, memory);
    } finally {
      if (!reserved) memory.close();
    }
  }
}

/** Exactly one pool for production store, client and adapter in this document. */
export const frontendBudget = new FrontendBudget(
  FRONTEND_BYTES,
  undefined,
  1024 * 1024,
);
