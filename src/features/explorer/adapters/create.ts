import { isTauri } from '@tauri-apps/api/core';
import { FixtureAdapter } from './fixture';
import { NativeAdapter } from './native';

export function createAdapter() {
  if (isTauri()) return new NativeAdapter();
  return new FixtureAdapter();
}
