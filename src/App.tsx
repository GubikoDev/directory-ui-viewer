import engineLicenses from '../licenses/THIRD-PARTY-NOTICES.txt?raw';
import {
  useEffect,
  useRef,
  useMemo,
  useState,
  useSyncExternalStore,
} from 'react';
import { createAdapter } from './features/explorer/adapters/create';
import { useGraphViewport } from './features/explorer/ui/use-graph-viewport';
import { discoverDirectories } from './features/explorer/client/directory-discovery';
import { SessionClient } from './features/explorer/client/session-client';
import {
  terminal,
  type Entry,
  type RootSession,
} from './features/explorer/model/types';
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
  const [finishedTree, setFinishedTree] = useState<RootSession | null>(null);
  const discovering = !!state.session && state.session !== finishedTree;
  const discovery = useRef<ReturnType<typeof discoverDirectories> | undefined>(
    undefined,
  );
  useEffect(() => {
    if (!state.session) return;
    const session = state.session;
    const stop = discoverDirectories(
      client,
      session,
      (e) => setError(e instanceof Error ? e.message : 'INTERNAL'),
      () => setFinishedTree(session),
      () => setFinishedTree(null),
    );
    discovery.current = stop;
    return stop;
  }, [client, state.session]);
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
  const selectedUsage = selected ? state.usages.get(selected) : undefined;
  const limitedUsage = [selectedUsage?.allocated, selectedUsage?.logical].some(
    (measure) =>
      measure?.state === 'partial' && measure.reasons.includes('resourceLimit'),
  );
  const tasks = Array.from(state.tasks.values());
  const active = tasks.filter((t) => !terminal(t.phase));
  const scan = active.find((task) => task.operation === 'scanUsage');
  const failure =
    error || state.failedRoot?.issue.code || client.errors.at(-1)?.message;
  const { session, entries, listings, usages, mode } = state;
  const [folds, setFolds] = useState<{
    session: RootSession | null;
    ids: Set<string>;
  }>();
  const collapsed = folds?.session === session ? folds?.ids : undefined;
  const graph = useMemo(
    () =>
      layoutGraph(
        { session, entries, listings, usages, mode },
        showHidden,
        collapsed,
      ),
    [session, entries, listings, usages, mode, showHidden, collapsed],
  );
  const positions = useMemo(
    () => new Map(graph.nodes.map((n) => [n.id, n])),
    [graph],
  );
  const {
    ref: graphRef,
    canvasRef,
    bounds,
  } = useGraphViewport(
    session,
    mode,
    selected ? positions.get(selected) : undefined,
    positions,
  );
  const windowed = graph.nodes.length > 200;
  const intersects = (
    left: number,
    top: number,
    right: number,
    bottom: number,
  ) =>
    !bounds.width ||
    !bounds.height ||
    (right >= bounds.left - 200 &&
      left <= bounds.left + bounds.width + 200 &&
      bottom >= bounds.top - 200 &&
      top <= bounds.top + bounds.height + 200);
  const viewportNodes = graph.nodes.filter((n) =>
    intersects(n.x, n.y, n.x + n.width, n.y + n.height),
  );
  const visibleNodes = windowed ? viewportNodes : graph.nodes;
  // Layout and progress can change without changing the visible directories.
  // Only restart viewport work when its IDs (or the session) actually change.
  const viewportKey = JSON.stringify(
    Array.from(
      new Set([
        ...(session ? [session.rootEntryId] : []),
        ...(selected ? [selected] : []),
        ...viewportNodes.map((node) => node.id),
      ]),
    ),
  );
  useEffect(() => {
    if (!session) return;
    const ids: string[] = JSON.parse(viewportKey);
    client.setUsageViewport(session, ids);
    discovery.current?.setViewport(ids);
    // Coalesce scrolling and successive incoming graph pages.
    const timer = setTimeout(() => {
      void client.readUsage(session).catch((e) => {
        if (client.store.getSnapshot().session === session)
          setError(e instanceof Error ? e.message : 'INTERNAL');
      });
    }, 120);
    return () => clearTimeout(timer);
  }, [client, session, viewportKey]);
  const toggleBranch = (id: string, canFold: boolean) => {
    client.store.select(id);
    if (!canFold) return;
    if (!collapsed?.has(id)) {
      for (const fileId of client.store.getSnapshot().openFileListIds) {
        let parent = positions.get(fileId)?.parent;
        while (parent && parent !== id) parent = positions.get(parent)?.parent;
        if (parent === id) client.closeListing(fileId, 'files');
      }
    }
    setFolds((previous) => {
      const ids = new Set(previous?.session === session ? previous?.ids : []);
      if (ids.has(id)) ids.delete(id);
      else ids.add(id);
      return { session, ids };
    });
  };
  const openFiles = (id: string) =>
    run(async () => {
      client.store.select(id);
      for (const previous of client.store.getSnapshot().openFileListIds) {
        if (previous !== id) client.closeListing(previous, 'files');
      }
      if (
        !client.store.getSnapshot().openFileListIds.has(id) ||
        client.store.getListing(id, 'files').loadState === 'failed'
      )
        await client.list(id, 'files');
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
        <div className="root-name">
          {root && (
            <>
              <button
                className="root-picker"
                onClick={choose}
                disabled={choosing}
                aria-label="현재 폴더 변경"
              >
                {icon('imgRootIcon', 15)}
                <span>{root.displayName}</span>
              </button>
              <details className="root-info" key={state.session?.sessionId}>
                <summary aria-label="현재 폴더 정보">▾</summary>
                <div>
                  <strong>{root.displayName}</strong>
                  <p>
                    디스크 사용량{' '}
                    {formatMeasure(state.usages.get(root.entryId)?.allocated)}
                  </p>
                  <p>수정 {dateLabel(root)}</p>
                  <button onClick={() => run(() => client.close())}>
                    연결 닫기
                  </button>
                </div>
              </details>
            </>
          )}
        </div>
        <section className="readout" aria-label="작업 상태">
          {failure || state.resourceLimited ? (
            <span role="alert" className="danger">
              {failure === 'RESOURCE_LIMIT' || state.resourceLimited
                ? '앱의 자원 한도에 도달했습니다. 열린 목록을 닫은 뒤 작업을 다시 시도하세요.'
                : state.failedRoot
                  ? '폴더를 다시 선택해주세요.'
                  : failure}
            </span>
          ) : null}
          {(discovering || active.length > 0) && (
            <span role="status">
              <i className="dot running" />
              {active.some((t) => t.operation === 'listFiles')
                ? '파일 목록 읽는 중'
                : discovering
                  ? '폴더 찾는 중'
                  : scan
                    ? '용량 집계 중'
                    : '목록 읽는 중'}
              {scan &&
                (discovering ||
                  active.some((t) => t.operation === 'listFiles')) &&
                ' · 용량 집계 중'}
              {scan && ` · 항목 ${scan.processedEntries}`}
            </span>
          )}
          {state.resourceLimited && (
            <button onClick={() => run(() => client.retryObservations())}>
              다시 시도
            </button>
          )}
          {(discovering || active.length > 0) && (
            <button
              onClick={() =>
                run(() => {
                  discovery.current?.();
                  setFinishedTree(state.session);
                  return Promise.all(
                    active.map((t) => client.cancel(t.taskId)),
                  );
                })
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
          <p>폴더 구조와 용량을 처음부터 다시 읽을까요?</p>
          <div>
            <button
              className="primary"
              onClick={() => {
                setConfirmRefresh(false);
                setDetails(false);
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
          <section
            ref={graphRef}
            className="graph-scroll"
            aria-label="폴더 구조"
          >
            <div
              ref={canvasRef}
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
                  if (
                    !p ||
                    (windowed &&
                      !intersects(
                        Math.min(p.x, n.x),
                        Math.min(p.y, n.y),
                        Math.max(p.x + p.width, n.x + n.width),
                        Math.max(p.y + p.height, n.y + n.height),
                      ))
                  )
                    return null;
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
              {visibleNodes.map((n) => {
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
                const canFold =
                  collapsed?.has(n.id) ||
                  (listing.snapshot?.entryIds.length ?? 0) >
                    (showHidden ? 0 : hidden);
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
                        aria-expanded={
                          canFold ? !collapsed?.has(n.id) : undefined
                        }
                        onClick={() => toggleBranch(n.id, canFold)}
                      >
                        {canFold && (
                          <span aria-hidden="true">
                            {collapsed?.has(n.id) ? '▸' : '▾'}
                          </span>
                        )}
                        {icon('imgFolder', 14)}
                        <span>{item.displayName}</span>
                      </button>
                      {(listing.snapshot?.coverage === 'partial' &&
                        listing.loadState !== 'loading') ||
                      listing.loadState === 'failed' ? (
                        <span className="tag">일부 읽기 불가</span>
                      ) : null}
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
                          state.openFileListIds.has(n.id)
                            ? run(() => client.closeListing(n.id, 'files'))
                            : openFiles(n.id)
                        }
                      >
                        {icon('imgFileIcon', 11)}
                        파일 보기{' '}
                        {files.snapshot
                          ? `${files.snapshot.entryIds.length}${files.snapshot.nextCursor ? '+' : ''}`
                          : ''}
                      </button>
                    </div>
                  </div>
                );
              })}
              {fileLists.map((id) => {
                const n = positions.get(id)!;
                const listing = client.store.getListing(id, 'files');
                const ids = listing.snapshot?.entryIds ?? [];
                const shown = ids.filter((fid) => {
                  const h = state.entries.get(fid)?.hidden;
                  return showHidden || h?.state !== 'known' || !h.value;
                });
                const right = n.x + n.width + 12;
                const left =
                  bounds.width && right + 316 > bounds.left + bounds.width
                    ? Math.max(bounds.left + 4, n.x - 328)
                    : right;
                const top = bounds.height
                  ? Math.max(
                      bounds.top,
                      Math.min(n.y, bounds.top + bounds.height - 160),
                    )
                  : n.y;
                return (
                  <section
                    key={`files-${id}`}
                    className="file-panel"
                    aria-label={`${state.entries.get(id)?.displayName} 파일 목록`}
                    style={{ left, top }}
                  >
                    <header>
                      <strong>{state.entries.get(id)?.displayName}</strong>
                      <span className="tag">
                        직계 파일{' '}
                        {listing.loadState === 'notRequested' ||
                        (listing.loadState === 'loading' && !ids.length)
                          ? '—'
                          : ids.length}
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
                        {listing.loadState === 'loading' ||
                        listing.loadState === 'notRequested'
                          ? '읽는 중…'
                          : listing.loadState === 'failed' ||
                              listing.snapshot?.coverage === 'partial'
                            ? '파일 목록을 모두 읽지 못했습니다.'
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
              {limitedUsage && (
                <p role="status">
                  탐색 자원 한도에 도달해 확인한 용량만 표시합니다.
                </p>
              )}
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
                <button onClick={() => openFiles(entry.entryId)}>
                  파일 목록 보기
                </button>
                <button onClick={() => run(() => client.scan())}>
                  용량 집계
                </button>
                {entry.parentId && (
                  <button onClick={() => client.store.select(entry.parentId!)}>
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
          <button onClick={() => openFiles(entry.entryId)}>
            파일 목록 보기
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
