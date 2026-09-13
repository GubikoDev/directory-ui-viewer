import { useEffect, useRef, useState } from 'react';
import type { SessionClient } from '../client/session-client';
import { measureApprovedRoot } from './measure';
import { readExpectation } from './expectation';

function frame(signal: AbortSignal): Promise<void> {
  return new Promise((resolve, reject) => {
    const stop = () => {
      cancelAnimationFrame(id);
      reject(new Error('MEASUREMENT_CANCELLED'));
    };
    const id = requestAnimationFrame(() => {
      signal.removeEventListener('abort', stop);
      resolve();
    });
    signal.addEventListener('abort', stop, { once: true });
    if (signal.aborted) stop();
  });
}

/** Enabled only in an explicitly instrumented build, not the normal product. */
export function VerificationPanel({
  client,
  rendered,
}: {
  client: SessionClient;
  rendered(): { generation: number; count: number };
}) {
  const [expectation, setExpectation] = useState('');
  const [report, setReport] = useState<{ text: string }>();
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  const [stopping, setStopping] = useState(false);
  const active = useRef<AbortController | null>(null);
  useEffect(() => () => active.current?.abort(), []);
  const measure = async () => {
    if (active.current) return;
    const controller = new AbortController();
    active.current = controller;
    setBusy(true);
    setStopping(false);
    setError('');
    setReport(undefined);
    try {
      const expected = readExpectation(expectation, client.store.budget);
      const result = await measureApprovedRoot(
        client,
        expected,
        {
          now: () => performance.now(),
          wait: () => new Promise<void>((resolve) => setTimeout(resolve, 50)),
          paint: async () => {
            const generation = client.store.getSnapshot().session?.generation;
            await frame(controller.signal);
            await frame(controller.signal);
            const view = rendered();
            if (
              view.generation !== generation ||
              view.count < expected.firstPageEntries
            )
              throw new Error('FIRST_PAGE_NOT_RENDERED');
          },
        },
        controller.signal,
      );
      setReport(
        client.store.budget.allocate(128 * 1024, () => ({
          text: JSON.stringify(result, null, 2),
        })),
      );
    } catch (cause) {
      const message = cause instanceof Error ? cause.message : '';
      setError(/^[A-Z_]{1,64}$/.test(message) ? message : 'MEASUREMENT_FAILED');
    } finally {
      active.current = null;
      setBusy(false);
    }
  };
  return (
    <section aria-label="내부 성능 측정" className="mt-6 border-t pt-4">
      <h2>선택한 테스트 루트 10회 측정</h2>
      <p>
        테스트 생성기가 출력한 expectation JSON을 입력합니다. 경로·파일 이름은
        결과에 포함하지 않습니다. 측정 중 다른 조작은 결과를 무효화할 수
        있습니다.
      </p>
      <textarea
        aria-label="테스트 기대값"
        maxLength={2048}
        value={expectation}
        disabled={busy}
        onChange={(event) => setExpectation(event.target.value)}
      />
      <div>
        <button
          disabled={busy || !client.hasSession}
          onClick={() => void measure()}
        >
          10회 측정
        </button>
        <button
          disabled={!busy || stopping}
          onClick={() => {
            setStopping(true);
            active.current?.abort();
          }}
        >
          측정 중지 요청
        </button>
      </div>
      {busy && (
        <p role="status">
          {stopping
            ? '중지 요청됨: 진행 중인 요청의 응답을 기다립니다.'
            : '측정 중입니다.'}
        </p>
      )}
      {error && <p role="alert">{error}</p>}
      {report && (
        <textarea
          aria-label="측정 결과 JSON"
          readOnly
          value={report.text}
          rows={16}
          className="w-full"
        />
      )}
    </section>
  );
}
