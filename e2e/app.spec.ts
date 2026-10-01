import { expect, test } from '@playwright/test';

test('shows the application shell', async ({ page }) => {
  await page.goto('/');

  await expect(
    page.getByRole('heading', { name: 'Directory UI Viewer' }),
  ).toBeAttached();
});

test('ships the walkdir and same-file copyright and permission notices', async ({
  page,
}) => {
  await page.goto('/');
  await page
    .getByText('오픈소스 라이선스 · 탐색 엔진', { exact: true })
    .click();
  const notice = page.locator('details pre');
  await expect(notice).toBeVisible();
  await expect(notice).toContainText('walkdir 2.5.0');
  await expect(notice).toContainText('same-file 1.0.6');
  await expect(notice).toContainText('Copyright (c) 2015 Andrew Gallant');
  await expect(notice).toContainText('Copyright (c) 2017 Andrew Gallant');
  await expect(notice).toContainText(
    'Permission is hereby granted, free of charge',
  );
});
