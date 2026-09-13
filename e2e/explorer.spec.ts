import { expect, test } from '@playwright/test';

test('fixture flow exercises lazy folders, files, presentation, refresh and close', async ({
  page,
}) => {
  await page.goto('/');
  await page.getByRole('button', { name: '폴더 선택', exact: true }).click();
  await expect(
    page.getByRole('heading', { name: 'A', exact: true }),
  ).toBeVisible();
  await page.getByRole('button', { name: 'B', exact: true }).click();
  await expect(
    page.getByRole('button', { name: 'C', exact: true }),
  ).toBeVisible();
  await page.getByRole('button', { name: '파일 목록 보기' }).click();
  await expect(
    page.getByText('file (regularFile)', { exact: true }),
  ).toBeVisible();
  const work = page.getByRole('region', { name: '작업 상태' });
  const before = await work.innerText();
  await page
    .getByRole('button', { name: '표현 전환 (organization)', exact: true })
    .click();
  await expect.poll(() => work.innerText()).toBe(before);
  await expect(
    page.getByText('file (regularFile)', { exact: true }),
  ).toBeVisible();
  await page.getByRole('button', { name: 'C', exact: true }).click();
  await expect(page.getByText('하위 폴더가 없습니다.')).toBeVisible();
  await page.getByRole('button', { name: '새로고침', exact: true }).click();
  await expect(
    page.getByRole('heading', { name: 'A', exact: true }),
  ).toBeVisible();
  await page.getByRole('button', { name: '연결 닫기', exact: true }).click();
  await expect(page.getByRole('region', { name: '현재 폴더' })).toHaveCount(0);
  await expect(
    page.getByRole('button', { name: '연결 닫기', exact: true }),
  ).toBeDisabled();
});
