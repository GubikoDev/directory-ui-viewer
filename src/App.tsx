import { useEffect, useState, useSyncExternalStore } from 'react';
import { createAdapter } from './features/explorer/adapters/create';
import { SessionClient } from './features/explorer/client/session-client';
import { terminal, type Measure } from './features/explorer/model/types';

function measure(value?: Measure) {
  if (!value || value.state === 'unknown') return '미확인';
  if (value.state === 'complete') return `${value.bytes} B`;
  if (value.state === 'partial')
    return `${value.observedBytes} B (부분: ${value.reasons.join(', ')})`;
  return `집계 불가: ${value.reason}`;
}
/** Temporary verification surface; final navigation and visual design are deferred. */
export function App({ client: provided }: { client?: SessionClient }) {
  const [client] = useState(
    () => provided ?? new SessionClient(createAdapter()),
  );
  const state = useSyncExternalStore(
    client.store.subscribe,
    client.store.getSnapshot,
  );
  const [error, setError] = useState('');
  const [choosing, setChoosing] = useState(false);
  useEffect(() => () => client.dispose(), [client]);
  const run = (action: () => Promise<unknown>) => {
    setError('');
    void action().catch((e) =>
      setError(e instanceof Error ? e.message : 'INTERNAL'),
    );
  };
  const selected = state.selectedId ?? state.session?.rootEntryId;
  const entry = selected ? state.entries.get(selected) : undefined;
  const folders = selected
    ? client.store.getListing(selected, 'directories')
    : undefined;
  const files = selected
    ? client.store.getListing(selected, 'files')
    : undefined;
  const ids = [
    ...(folders?.snapshot?.entryIds ?? []),
    ...(files?.snapshot?.entryIds ?? []),
  ];
  const choose = () =>
    run(async () => {
      setChoosing(true);
      try {
        await client.chooseRoot();
      } finally {
        setChoosing(false);
      }
    });
  const open = (id: string) =>
    run(async () => {
      client.store.select(id);
      client.store.setExpanded(id, true);
      await client.list(id, 'directories');
    });
  return (
    <main className="min-h-screen bg-slate-950 p-6 text-slate-100">
      <h1 className="text-2xl font-semibold">Directory UI Viewer</h1>
      <p className="my-3 text-slate-400">
        내부 기능 검증 화면 ·{' '}
        {state.session?.platform === 'macos'
          ? 'macOS 읽기 전용'
          : '폴더 선택 전 / 브라우저에서는 샘플 데이터'}
      </p>
      <div className="my-4 flex flex-wrap gap-3">
        <button disabled={choosing} onClick={choose}>
          폴더 선택
        </button>
        <button
          disabled={!client.hasSession}
          onClick={() => run(() => client.refresh())}
        >
          새로고침
        </button>
        <button
          disabled={!client.hasSession}
          onClick={() => run(() => client.close())}
        >
          연결 닫기
        </button>
        <button
          onClick={() =>
            client.store.setMode(
              state.mode === 'organization' ? 'mindmap' : 'organization',
            )
          }
        >
          표현 전환 ({state.mode})
        </button>
      </div>
      {(error || state.failedRoot) && (
        <p role="alert">{error || state.failedRoot?.issue.code}</p>
      )}
      {state.resourceLimited && (
        <p role="status">메모리 한도에 도달했습니다.</p>
      )}
      {entry && (
        <section aria-label="현재 폴더">
          <h2 className="my-3 text-xl">{entry.displayName}</h2>
          <p>논리 용량: {measure(state.usages.get(entry.entryId)?.logical)}</p>
          <p>
            할당 용량: {measure(state.usages.get(entry.entryId)?.allocated)}
          </p>
          <div className="my-3 flex gap-3">
            {entry.parentId && (
              <button onClick={() => open(entry.parentId!)}>상위 폴더</button>
            )}
            <button
              onClick={() => run(() => client.list(entry.entryId, 'files'))}
            >
              파일 목록 보기
            </button>
            <button onClick={() => run(() => client.scan())}>용량 집계</button>
          </div>
          <ul
            aria-label="항목 목록"
            className={
              state.mode === 'mindmap'
                ? 'my-4 flex flex-wrap gap-4'
                : 'my-4 space-y-2'
            }
          >
            {ids.map((id) => {
              const item = state.entries.get(id);
              if (!item) return null;
              return (
                <li key={id} className="rounded border border-slate-700 p-2">
                  {item.kind === 'directory' ? (
                    <button onClick={() => open(id)}>{item.displayName}</button>
                  ) : (
                    <span>
                      {item.displayName} ({item.kind})
                    </span>
                  )}
                  {item.kind === 'directory' && (
                    <span className="ml-4 text-slate-400">
                      {measure(state.usages.get(id)?.logical)}
                    </span>
                  )}
                </li>
              );
            })}
          </ul>
          {folders?.snapshot?.coverage === 'complete' && ids.length === 0 && (
            <p>하위 폴더가 없습니다.</p>
          )}
          {[folders, files].map((listing, i) =>
            listing?.snapshot?.nextCursor && listing.taskId ? (
              <button
                key={i}
                onClick={() => run(() => client.nextPage(listing.taskId!))}
              >
                {listing.category === 'files' ? '파일' : '폴더'} 다음 페이지
              </button>
            ) : null,
          )}
        </section>
      )}
      <section aria-label="작업 상태">
        <h2 className="my-3 text-xl">작업 상태</h2>
        <ul>
          {[...state.tasks.values()].map((task) => (
            <li key={task.taskId}>
              {task.operation}: {task.phase} {task.waitReason ?? ''} ·{' '}
              {task.processedEntries}개
              {!terminal(task.phase) && (
                <button
                  className="ml-3"
                  onClick={() => run(() => client.cancel(task.taskId))}
                >
                  취소
                </button>
              )}
              {Object.keys(task.issues.counts).length > 0 && (
                <span> · {Object.keys(task.issues.counts).join(', ')}</span>
              )}
            </li>
          ))}
        </ul>
      </section>
    </main>
  );
}
