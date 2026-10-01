import engineLicenses from '../licenses/THIRD-PARTY-NOTICES.txt?raw';
import { useEffect, useState, useSyncExternalStore } from 'react';
import { createAdapter } from './features/explorer/adapters/create';
import { SessionClient } from './features/explorer/client/session-client';
import { terminal, type Entry } from './features/explorer/model/types';
import {
  formatBytes,
  formatMeasure,
  layoutGraph,
} from './features/explorer/ui/presentation';
import { InternalApp } from './InternalApp';

function Icon({
  name,
  size = 16,
  light = false,
}: {
  name: string;
  size?: number;
  light?: boolean;
}) {
  return (
    <img
      src={`/design/${light ? 'light' : 'dark'}-${name}.svg`}
      alt=""
      width={size}
      height={size}
    />
  );
}
function dateLabel(entry: Entry) {
  return entry.modifiedAt.state === 'known'
    ? new Date(entry.modifiedAt.value).toLocaleDateString('ko-KR', {
        month: 'short',
        day: 'numeric',
      })
    : '—';
}
export function App({ client }: { client?: SessionClient }) {
  return import.meta.env.VITE_INTERNAL_BENCHMARK === '1' ? (
    <InternalApp client={client} />
  ) : (
    <ExplorerApp client={client} />
  );
}
function ExplorerApp({ client: provided }: { client?: SessionClient }) {
  const [client] = useState(
    () => provided ?? new SessionClient(createAdapter()),
  );
  const state = useSyncExternalStore(
    client.store.subscribe,
    client.store.getSnapshot,
  );
  const [error, setError] = useState('');
  const [choosing, setChoosing] = useState(false);
  const [confirmRefresh, setConfirmRefresh] = useState(false);
  const [showHidden, setShowHidden] = useState(false);
  const [light, setLight] = useState(false);
  const [details, setDetails] = useState(false);
  const [limits, setLimits] = useState<ReadonlyMap<string, number>>(new Map());
  useEffect(() => () => client.dispose(), [client]);
  const run = (action: () => unknown | Promise<unknown>) => {
    setError('');
    void Promise.resolve()
      .then(action)
      .catch((e) => setError(e instanceof Error ? e.message : 'INTERNAL'));
  };
  const choose = () =>
    run(async () => {
      setChoosing(true);
      try {
        if (await client.chooseRoot()) {
          setDetails(false);
          setLimits(new Map());
        }
      } finally {
        setChoosing(false);
      }
    });
  const selected = state.selectedId ?? state.session?.rootEntryId;
  const entry = selected ? state.entries.get(selected) : undefined;
  const root = state.session
    ? state.entries.get(state.session.rootEntryId)
    : undefined;
  const tasks = Array.from(state.tasks.values());
  const active = tasks.filter((t) => !terminal(t.phase));
  const failure =
    error || state.failedRoot?.issue.code || client.errors.at(-1)?.message;
  const graph = layoutGraph(state, showHidden, limits);
  const positions = new Map(graph.nodes.map((n) => [n.id, n]));
  const open = (id: string) =>
    run(async () => {
      client.store.select(id);
      const listing = client.store.getListing(id, 'directories');
      client.store.setExpanded(id, true);
      if (!listing.snapshot || listing.loadState === 'failed')
        await client.list(id, 'directories');
    });
  const fileLists = Array.from(state.openFileListIds).filter((id) =>
    positions.has(id),
  );
  const icon = (name: string, size?: number) => (
    <Icon
      name={name}
      size={size}
      light={
        light &&
        !['imgX', 'imgFile', 'imgLink', 'imgFolderOpen1'].includes(name)
      }
    />
  );
  return (
    <main className={light ? 'explorer light' : 'explorer'}>
      <h1 className="sr-only">Directory UI Viewer</h1>
      <header className="topbar">
        <div className="window-lights" aria-hidden="true">
          {['imgLight', 'imgLight1', 'imgLight2'].map((name) => (
            <Icon key={name} name={name} size={11} />
          ))}
        </div>
        <div className="root-name">
          {root && icon('imgRootIcon', 15)}
          <span>{root?.displayName ?? ''}</span>
        </div>
        <section className="readout" aria-label="작업 상태">
          {failure ? (
            <span role="alert" className="danger">
              {failure} · 폴더를 다시 선택해주세요
            </span>
          ) : active.length ? (
            <span role="status">
              <i className="dot running" />
              읽는 중 · 항목 {active[0].processedEntries} · {active.length}개
              작업
            </span>
          ) : state.resourceLimited ? (
            <button onClick={() => run(() => client.retryObservations())}>
              관측 다시 읽기
            </button>
          ) : null}
          {active.length > 0 && (
            <button
              onClick={() =>
                run(() =>
                  Promise.all(active.map((t) => client.cancel(t.taskId))),
                )
              }
            >
              중지
            </button>
          )}
        </section>
        <div className="view-switch" aria-label="표현 방식">
          <button
            aria-pressed={state.mode === 'organization'}
            onClick={() => run(() => client.store.setMode('organization'))}
          >
            조직도
          </button>
          <button
            aria-pressed={state.mode === 'mindmap'}
            onClick={() => run(() => client.store.setMode('mindmap'))}
          >
            마인드맵
          </button>
        </div>
        <button
          className="icon-button"
          aria-label="숨김 항목 표시"
          aria-pressed={showHidden}
          title="숨김 항목 표시"
          onClick={() => setShowHidden(!showHidden)}
        >
          {icon('imgEyeOff')}
        </button>
        <button
          className="icon-button"
          aria-label="새로고침"
          title="다시 읽기"
          disabled={!client.hasSession}
          onClick={() => setConfirmRefresh(!confirmRefresh)}
        >
          {icon('imgRefresh')}
        </button>
        <button
          className="icon-button"
          aria-label="폴더 선택"
          title="폴더 선택"
          disabled={choosing}
          onClick={choose}
        >
          {icon('imgFolderOpen')}
        </button>
        <span className="toolbar-separator" />
        <button
          className="theme-button"
          aria-label="테마 전환"
          onClick={() => setLight(!light)}
        >
          {icon('imgGenDot', 5)}
        </button>
      </header>
      {confirmRefresh && (
        <div
          className="refresh-confirm"
          role="dialog"
          aria-label="다시 읽기 확인"
        >
          <p>다시 읽으면 펼친 폴더가 접힙니다</p>
          <div>
            <button
              className="primary"
              onClick={() => {
                setConfirmRefresh(false);
                setDetails(false);
                setLimits(new Map());
                run(() => client.refresh());
              }}
            >
              다시 읽기
            </button>
            <button onClick={() => setConfirmRefresh(false)}>취소</button>
            <span>새 세대로 읽기</span>
          </div>
        </div>
      )}
      {!state.session ? (
        <section className="empty-state">
          {icon('imgFolderOpen1', 40)}
          <h2>
            {state.failedRoot
              ? '폴더를 찾을 수 없습니다'
              : choosing
                ? '폴더를 여는 중'
                : '폴더를 선택해주세요'}
          </h2>
          <button className="primary" onClick={choose} disabled={choosing}>
            {state.failedRoot ? '폴더 다시 선택' : '폴더 열기'}
          </button>
        </section>
      ) : (
        <div className="workspace">
          <section className="graph-scroll" aria-label="폴더 구조">
            <div
              className="graph-canvas"
              style={{ width: graph.width, height: graph.height }}
            >
              <svg
                className="edges"
                width={graph.width}
                height={graph.height}
                aria-hidden="true"
              >
                {graph.nodes.map((n) => {
                  const p = n.parent ? positions.get(n.parent) : undefined;
                  if (!p) return null;
                  const x = p.x + p.width;
                  const y = p.y + p.height / 2;
                  const endY = n.y + n.height / 2;
                  return (
                    <path
                      key={n.id}
                      d={
                        state.mode === 'mindmap'
                          ? `M ${p.x + p.width / 2} ${y} L ${n.x + n.width / 2} ${endY}`
                          : `M ${x} ${y} H ${x + 36} Q ${x + 46} ${y} ${x + 46} ${y + Math.sign(endY - y) * Math.min(10, Math.abs(endY - y))} V ${endY} H ${n.x}`
                      }
                    />
                  );
                })}
              </svg>
              {graph.nodes.map((n) => {
                const item = state.entries.get(n.id)!;
                const usage = state.usages.get(n.id);
                const listing = client.store.getListing(n.id, 'directories');
                const files = client.store.getListing(n.id, 'files');
                const hidden = (listing.snapshot?.entryIds ?? []).filter(
                  (id) => {
                    const h = state.entries.get(id)?.hidden;
                    return h?.state === 'known' && h.value;
                  },
                ).length;
                const isActive =
                  usage?.scanState === 'running' ||
                  usage?.scanState === 'queued';
                const partial =
                  usage?.allocated.state === 'partial' ||
                  usage?.allocated.state === 'unavailable';
                return (
                  <div
                    key={n.id}
                    className={
                      selected === n.id ? 'folder-node selected' : 'folder-node'
                    }
                    style={{
                      left: n.x,
                      top: n.y,
                      width: n.width,
                      height: n.height,
                    }}
                  >
                    <div className="node-name">
                      <button
                        className="folder-select"
                        aria-label={item.displayName}
                        onClick={() => open(n.id)}
                      >
                        {icon('imgFolder', 14)}
                        <span>{item.displayName}</span>
                      </button>
                      {hidden > 0 && <span className="tag">숨김 {hidden}</span>}
                    </div>
                    <div className="node-value">
                      <button
                        aria-label={`${item.displayName} 상세`}
                        onClick={() => {
                          client.store.select(n.id);
                          setDetails(true);
                        }}
                      >
                        <i
                          className={
                            isActive
                              ? 'dot running'
                              : partial
                                ? 'dot warning'
                                : 'dot settled'
                          }
                        />
                        {formatMeasure(usage?.allocated)}
                      </button>
                      <button
                        className="file-toggle"
                        aria-label={`${item.displayName} 파일 목록`}
                        aria-expanded={state.openFileListIds.has(n.id)}
                        onClick={() =>
                          run(() =>
                            state.openFileListIds.has(n.id)
                              ? client.closeListing(n.id, 'files')
                              : client.list(n.id, 'files'),
                          )
                        }
                      >
                        {icon('imgFileIcon', 11)}
                        {files.snapshot
                          ? `${files.snapshot.entryIds.length}${files.snapshot.nextCursor ? '+' : ''}`
                          : '—'}
                      </button>
                    </div>
                  </div>
                );
              })}
              {graph.nodes.map((n) => {
                const listing = client.store.getListing(n.id, 'directories');
                if (
                  n.id !== state.session?.rootEntryId &&
                  !state.expandedIds.has(n.id)
                )
                  return null;
                const visibleCount = (listing.snapshot?.entryIds ?? []).filter(
                  (id) => {
                    const h = state.entries.get(id)?.hidden;
                    return showHidden || h?.state !== 'known' || !h.value;
                  },
                ).length;
                const limit = limits.get(n.id) ?? 12;
                return visibleCount > limit || listing.snapshot?.nextCursor ? (
                  <button
                    key={`more-${n.id}`}
                    className="more-folders"
                    style={{ left: n.x, top: n.y + n.height + 8 }}
                    aria-label="폴더 다음 페이지"
                    onClick={() =>
                      run(async () => {
                        if (visibleCount <= limit && listing.taskId)
                          await client.nextPage(listing.taskId);
                        setLimits((old) => new Map(old).set(n.id, limit + 12));
                      })
                    }
                  >
                    그 외 폴더 더 보기
                  </button>
                ) : null;
              })}
              {fileLists.map((id) => {
                const n = positions.get(id)!;
                const listing = client.store.getListing(id, 'files');
                const ids = listing.snapshot?.entryIds ?? [];
                const shown = ids.filter((fid) => {
                  const h = state.entries.get(fid)?.hidden;
                  return showHidden || h?.state !== 'known' || !h.value;
                });
                return (
                  <section
                    key={`files-${id}`}
                    className="file-panel"
                    aria-label={`${state.entries.get(id)?.displayName} 파일 목록`}
                    style={{ left: n.x + n.width + 20, top: n.y + 80 }}
                  >
                    <header>
                      <strong>{state.entries.get(id)?.displayName}</strong>
                      <span className="tag">
                        직계 파일 {ids.length}
                        {listing.snapshot?.nextCursor ? '+' : ''} · 숨김{' '}
                        {ids.length - shown.length}
                      </span>
                      <button
                        aria-label="파일 목록 닫기"
                        onClick={() =>
                          run(() => client.closeListing(id, 'files'))
                        }
                      >
                        {icon('imgX', 13)}
                      </button>
                    </header>
                    <table>
                      <thead>
                        <tr>
                          <th>이름</th>
                          <th>크기</th>
                          <th>수정</th>
                        </tr>
                      </thead>
                      <tbody>
                        {shown.map((fid) => {
                          const file = state.entries.get(fid)!;
                          return (
                            <tr key={fid}>
                              <td>
                                {icon(
                                  file.kind === 'symlink'
                                    ? 'imgLink'
                                    : 'imgFile',
                                  12,
                                )}
                                <span title={file.displayName}>
                                  {file.displayName}
                                </span>
                                {file.kind === 'symlink' && (
                                  <span className="tag">링크</span>
                                )}
                              </td>
                              <td>
                                {file.ownLogicalBytes.state === 'known'
                                  ? formatBytes(file.ownLogicalBytes.value)
                                  : '—'}
                              </td>
                              <td>{dateLabel(file)}</td>
                            </tr>
                          );
                        })}
                      </tbody>
                    </table>
                    {!shown.length && (
                      <p className="panel-empty">
                        {listing.loadState === 'loading'
                          ? '읽는 중…'
                          : '표시할 파일이 없습니다.'}
                      </p>
                    )}
                    {listing.snapshot?.nextCursor && listing.taskId && (
                      <button
                        className="more-files"
                        onClick={() =>
                          run(() => client.nextPage(listing.taskId!))
                        }
                      >
                        파일 다음 페이지
                      </button>
                    )}
                  </section>
                );
              })}
            </div>
          </section>
          {details && entry && (
            <aside className="detail-panel" aria-label="현재 폴더">
              <header>
                <h2>
                  {icon('imgFolder', 14)}
                  {entry.displayName}
                </h2>
                <button
                  aria-label="상세 닫기"
                  onClick={() => setDetails(false)}
                >
                  {icon('imgX', 13)}
                </button>
              </header>
              <p className="detail-label">용량</p>
              <div className="measure-cards">
                <div>
                  <span>논리 크기</span>
                  <strong>
                    {formatMeasure(state.usages.get(entry.entryId)?.logical)}
                  </strong>
                </div>
                <div>
                  <span>디스크 사용량</span>
                  <strong>
                    {formatMeasure(state.usages.get(entry.entryId)?.allocated)}
                  </strong>
                </div>
              </div>
              <p className="detail-note">
                노드 크기는 디스크 사용량 기준. 링크 대상은 세지 않습니다.
              </p>
              <dl>
                <dt>수정</dt>
                <dd>{dateLabel(entry)}</dd>
                <dt>숨김</dt>
                <dd>
                  {entry.hidden.state === 'known'
                    ? entry.hidden.value
                      ? '예'
                      : '아니요'
                    : '미확인'}
                </dd>
                <dt>종류</dt>
                <dd>
                  {entry.specialType.state === 'known'
                    ? entry.specialType.value
                    : '폴더 · 특수 종류 미지원'}
                </dd>
                <dt>관측 세대</dt>
                <dd>{state.session.generation}세대</dd>
              </dl>
              <div className="detail-actions">
                <button
                  onClick={() => run(() => client.list(entry.entryId, 'files'))}
                >
                  파일 목록 보기
                </button>
                <button onClick={() => run(() => client.scan())}>
                  용량 집계
                </button>
                <button
                  onClick={() =>
                    run(() => client.store.setExpanded(entry.entryId, false))
                  }
                >
                  폴더 접기
                </button>
                {entry.parentId && (
                  <button onClick={() => open(entry.parentId!)}>
                    상위 폴더
                  </button>
                )}
              </div>
            </aside>
          )}
        </div>
      )}
      {entry && (
        <div className="selection-actions" aria-label="선택 폴더 동작">
          <span>{entry.displayName}</span>
          <button onClick={() => setDetails(true)}>상세 보기</button>
          <button
            onClick={() => run(() => client.list(entry.entryId, 'files'))}
          >
            파일 목록 보기
          </button>
          <button
            disabled={!client.hasSession}
            onClick={() => run(() => client.close())}
          >
            연결 닫기
          </button>
        </div>
      )}
      <details className="licenses">
        <summary>오픈소스 라이선스 · 탐색 엔진</summary>
        <pre>{engineLicenses}</pre>
      </details>
      {state.session?.platform === 'fixture' && (
        <span className="sample-label">브라우저 샘플</span>
      )}
      {state.session &&
        !client.store.getListing(selected!, 'directories').snapshot?.entryIds
          .length &&
        client.store.getListing(selected!, 'directories').snapshot?.coverage ===
          'complete' && (
          <span className="empty-folder-note">하위 폴더가 없습니다.</span>
        )}
      <span className="sr-only" data-generation={state.session?.generation}>
        {tasks.map((t) => `${t.operation}: ${t.phase}`).join(' · ')}
      </span>
    </main>
  );
}
