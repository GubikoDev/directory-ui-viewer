import type { FrontendBudget } from '../runtime/memory';

const MAX_BYTES = 1_048_576;
type Emit = (byte: number) => void;

function ascii(text: string, emit: Emit) {
  for (let i = 0; i < text.length; i++) emit(text.charCodeAt(i));
}
function string(text: string, emit: Emit) {
  emit(34);
  for (let i = 0; i < text.length; i++) {
    const code = text.charCodeAt(i);
    if (code === 34 || code === 92) {
      emit(92);
      emit(code);
    } else if (code < 32 || (code >= 0xd800 && code <= 0xdfff)) {
      const next = text.charCodeAt(i + 1);
      if (
        code >= 0xd800 &&
        code <= 0xdbff &&
        next >= 0xdc00 &&
        next <= 0xdfff
      ) {
        const scalar = 0x10000 + ((code - 0xd800) << 10) + next - 0xdc00;
        emit(0xf0 | (scalar >> 18));
        emit(0x80 | ((scalar >> 12) & 63));
        emit(0x80 | ((scalar >> 6) & 63));
        emit(0x80 | (scalar & 63));
        i++;
      } else {
        const short =
          code === 8
            ? 'b'
            : code === 9
              ? 't'
              : code === 10
                ? 'n'
                : code === 12
                  ? 'f'
                  : code === 13
                    ? 'r'
                    : null;
        if (short) {
          emit(92);
          emit(short.charCodeAt(0));
        } else {
          ascii('\\u', emit);
          for (let shift = 12; shift >= 0; shift -= 4)
            emit('0123456789abcdef'.charCodeAt((code >> shift) & 15));
        }
      }
    } else if (code < 128) emit(code);
    else if (code < 2048) {
      emit(0xc0 | (code >> 6));
      emit(0x80 | (code & 63));
    } else {
      emit(0xe0 | (code >> 12));
      emit(0x80 | ((code >> 6) & 63));
      emit(0x80 | (code & 63));
    }
  }
  emit(34);
}
function visit(value: unknown, emit: Emit, depth = 0): void {
  if (depth > 64) throw new Error('INVALID_ARGUMENT');
  if (value === null) return ascii('null', emit);
  if (typeof value === 'string') return string(value, emit);
  if (typeof value === 'boolean') return ascii(value ? 'true' : 'false', emit);
  if (typeof value === 'number' && Number.isFinite(value))
    return ascii(String(value), emit);
  if (!value || typeof value !== 'object') throw new Error('INVALID_ARGUMENT');
  if (Array.isArray(value)) {
    emit(91);
    for (let i = 0; i < value.length; i++) {
      if (i > 0) emit(44);
      const field = Object.getOwnPropertyDescriptor(value, String(i));
      if (field && !('value' in field)) throw new Error('INVALID_ARGUMENT');
      visit(field?.value ?? null, emit, depth + 1);
    }
    emit(93);
    return;
  }
  const prototype = Object.getPrototypeOf(value);
  if (prototype !== Object.prototype && prototype !== null)
    throw new Error('INVALID_ARGUMENT');
  emit(123);
  let first = true;
  for (const key in value) {
    const field = Object.getOwnPropertyDescriptor(value, key);
    if (!field) continue;
    if (!('value' in field)) throw new Error('INVALID_ARGUMENT');
    if (field.value === undefined) continue;
    if (!first) emit(44);
    first = false;
    string(key, emit);
    emit(58);
    visit(field.value, emit, depth + 1);
  }
  emit(125);
}

/** Exact JSON UTF-8, without allocating an intermediate JSON string or TextEncoder
 * copy. Only plain protocol records/arrays are accepted; no getters/toJSON hooks.
 * make() constructs small envelope wrappers and borrows already-owned arguments;
 * it must not allocate an arbitrary uncharged source tree. The 16KiB scratch
 * covers these wrappers and the bounded traversal, not borrowed model data. */
export function encodeWire(
  make: () => unknown,
  budget: FrontendBudget,
): Uint8Array {
  const scratch = budget.reserve(16 * 1024);
  try {
    const value = make();
    let length = 0;
    visit(value, () => {
      if (++length > MAX_BYTES) throw new Error('RESOURCE_LIMIT');
    });
    const memory = budget.reserve(length + 1024);
    try {
      const buffer = new ArrayBuffer(length);
      const bytes = new Uint8Array(buffer);
      let offset = 0;
      visit(value, (byte) => {
        if (offset >= bytes.length) throw new Error('INVALID_ARGUMENT');
        bytes[offset++] = byte;
      });
      if (offset !== length) throw new Error('INVALID_ARGUMENT');
      budget.retainLifetimeFrom(buffer, memory, length + 512);
      budget.retainLifetimeFrom(bytes, memory, 512);
      return bytes;
    } finally {
      memory.close();
    }
  } finally {
    scratch.close();
  }
}
