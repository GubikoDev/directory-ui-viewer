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
  const branch = page.getByRole('button', { name: 'B', exact: true });
  const child = page.getByRole('button', { name: 'C', exact: true });
  await branch.click();
  await expect(child).toHaveCount(0);
  await expect(branch).toHaveAttribute('aria-expanded', 'false');
  await expect(page.getByRole('region', { name: 'B 파일 목록' })).toHaveCount(
    0,
  );
  await branch.click();
  await expect(child).toBeVisible();
  await page.getByRole('button', { name: 'B 파일 목록' }).click();
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
  await page.getByRole('button', { name: 'C 파일 목록', exact: true }).click();
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

test('pinch scales the graph around the pointer and leaves ordinary scroll available', async ({
  page,
}) => {
  await page.goto('/');
  await page.getByRole('button', { name: '폴더 열기', exact: true }).click();
  const graph = page.getByRole('region', { name: '폴더 구조' });
  await expect(
    page.getByRole('button', { name: 'C', exact: true }),
  ).toBeVisible();
  const result = await graph.evaluate((el) => {
    const canvas = el.querySelector<HTMLElement>('.graph-canvas')!;
    const box = el.getBoundingClientRect();
    const point = { clientX: box.left + 100, clientY: box.top + 100 };
    function gesture(type: string, scale: number) {
      el.dispatchEvent(
        Object.assign(new MouseEvent(type, { ...point, cancelable: true }), {
          scale,
        }),
      );
    }
    const scrollAllowed = el.dispatchEvent(
      new WheelEvent('wheel', { deltaY: 12, cancelable: true }),
    );
    gesture('gesturestart', 1);
    gesture('gesturechange', 2);
    gesture('gestureend', 2);
    const anchored = [el.scrollLeft, el.scrollTop, Number(canvas.style.zoom)];
    el.dispatchEvent(
      new WheelEvent('wheel', {
        ...point,
        ctrlKey: true,
        deltaY: 25,
        cancelable: true,
      }),
    );
    return { scrollAllowed, anchored, reduced: Number(canvas.style.zoom) };
  });
  expect(result.scrollAllowed).toBe(true);
  expect(result.anchored).toEqual([100, 100, 2]);
  expect(result.reduced).toBeGreaterThan(1);
  expect(result.reduced).toBeLessThan(2);
});
