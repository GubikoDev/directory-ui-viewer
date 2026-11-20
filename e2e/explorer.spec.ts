import { expect, test } from '@playwright/test';

test('v2 fixture flow wires folders, files, presentation, refresh and close', async ({
  page,
}) => {
  await page.goto('/');
  await page.getByRole('button', { name: '폴더 열기', exact: true }).click();
  await expect(
    page.getByRole('button', { name: 'A', exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole('button', { name: 'C', exact: true }),
  ).toBeVisible();
  await page.getByRole('button', { name: 'B', exact: true }).click();
  await expect(
    page.getByRole('cell', { name: 'file', exact: true }),
  ).toBeVisible();
  await page.getByRole('button', { name: '파일 목록 닫기' }).click();
  await expect(
    page.getByRole('cell', { name: 'file', exact: true }),
  ).toHaveCount(0);
  await page.getByRole('button', { name: '마인드맵', exact: true }).click();
  await expect(
    page.getByRole('button', { name: '마인드맵', exact: true }),
  ).toHaveAttribute('aria-pressed', 'true');
  await page.getByRole('button', { name: 'C', exact: true }).click();
  await expect(page.getByRole('region', { name: 'C 파일 목록' })).toBeVisible();
  await page.getByRole('button', { name: '새로고침', exact: true }).click();
  await expect(
    page.getByRole('dialog', { name: '다시 읽기 확인' }),
  ).toBeVisible();
  await page.getByRole('button', { name: '다시 읽기', exact: true }).click();
  await expect(
    page.getByRole('button', { name: 'A', exact: true }),
  ).toBeVisible();
  await page.getByLabel('현재 폴더 정보').click();
  await page.getByRole('button', { name: '연결 닫기', exact: true }).click();
  await expect(
    page.getByRole('button', { name: '폴더 열기', exact: true }),
  ).toBeVisible();
});
