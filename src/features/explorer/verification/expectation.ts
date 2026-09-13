import profiles from './profiles.json';
import type { FrontendBudget } from '../runtime/memory';
import type { ExpectedFixture } from './measure';

export function readExpectation(
  text: string,
  budget: FrontendBudget,
): ExpectedFixture {
  if (text.length > 2048) throw new Error('INVALID_FIXTURE_EXPECTATION');
  const value = budget.parse(text) as Record<string, unknown> | null;
  if (!value || typeof value !== 'object' || Array.isArray(value))
    throw new Error('INVALID_FIXTURE_EXPECTATION');
  for (const key in value)
    if (key !== 'profile' && key !== 'logical' && key !== 'allocated')
      throw new Error('INVALID_FIXTURE_EXPECTATION');
  if (
    typeof value.profile !== 'string' ||
    !Object.hasOwn(profiles, value.profile)
  )
    throw new Error('INVALID_FIXTURE_EXPECTATION');
  for (const key of ['logical', 'allocated']) {
    const bytes = value[key];
    if (
      typeof bytes !== 'string' ||
      !/^(0|[1-9][0-9]{0,38})$/.test(bytes) ||
      BigInt(bytes) >= 1n << 128n
    )
      throw new Error('INVALID_FIXTURE_EXPECTATION');
  }
  return budget.adopt({
    ...profiles[value.profile as keyof typeof profiles],
    logical: value.logical,
    allocated: value.allocated,
  } as ExpectedFixture);
}
