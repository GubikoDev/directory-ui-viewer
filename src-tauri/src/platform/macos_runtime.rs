//! Native session ownership. Paths enter only through a trusted picker callback;
//! prepare_* run on a fixed worker, finish_* revalidate before publication.
//! This module does not deserialize native document/window contexts from IPC.
mod prepare;
use super::{
    macos::{ApprovedRoot, fs_issue},
    macos_listing::{NativeListingSource, metadata_entry},
    macos_usage::NativeUsageSource,
};
use crate::{
    domain::{
        controller::{ControlError, NativeContext, Scope, SelectionTicket, SessionController},
        index::{DirectoryIndex, DirectoryKey, IndexError, Origin, SharedDirectoryIndex},
        model::{
            Category, DirectoryUsage, Entry, FsIssue, IssueScope, ListingPage, Platform, RootData,
            RootSession, WorkEvent, WorkRecord,
        },
    },
    listing::{
        cache::{CacheError, ListingCache, ListingHandle},
        service::ListingService,
    },
    runtime::{
        clock::Clock,
        events::EventSink,
        memory::{Charged, ManagedBudgets, Reservation},
    },
    scan::{
        service::{CancelStatus, UsageError, UsageService},
        usage::ScanLimits,
    },
    scheduler::Scheduler,
};
use std::{
    os::unix::ffi::OsStrExt,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
const TTL_MS: u64 = 300_000;
const MESSAGE_BYTES: usize = 1_048_576;
static NEXT_RUNTIME: AtomicU64 = AtomicU64::new(1);
static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);
fn issue(code: &str) -> FsIssue {
    FsIssue {
        code: code.into(),
        operation: "session".into(),
        scope: IssueScope::Root,
        entry_id: None,
        native_code: None,
    }
}
fn control(e: ControlError) -> FsIssue {
    issue(match e {
        ControlError::ClientExpired => "CLIENT_EXPIRED",
        ControlError::InvalidArgument => "INVALID_ARGUMENT",
        ControlError::RequestExpired => "REQUEST_EXPIRED",
        ControlError::StaleGeneration => "STALE_GENERATION",
        ControlError::SessionClosed => "SESSION_CLOSED",
        ControlError::ResourceLimit => "RESOURCE_LIMIT",
    })
}
fn index_error(e: IndexError) -> FsIssue {
    issue(match e {
        IndexError::ResourceLimit => "RESOURCE_LIMIT",
        IndexError::EntryExpired => "ENTRY_EXPIRED",
        IndexError::InvalidArgument => "INVALID_ARGUMENT",
        IndexError::EntryChanged => "ENTRY_CHANGED",
    })
}
fn cache_error(e: CacheError) -> FsIssue {
    issue(match e {
        CacheError::ResourceLimit => "RESOURCE_LIMIT",
        CacheError::TaskExpired => "TASK_EXPIRED",
        CacheError::CursorExpired => "CURSOR_EXPIRED",
        CacheError::InvalidArgument => "INVALID_ARGUMENT",
    })
}
fn usage_error(e: UsageError) -> FsIssue {
    issue(match e {
        UsageError::ResourceLimit => "RESOURCE_LIMIT",
        UsageError::TaskExpired => "TASK_EXPIRED",
        UsageError::EntryExpired => "ENTRY_EXPIRED",
        UsageError::SessionClosed => "SESSION_CLOSED",
        UsageError::InvalidArgument => "INVALID_ARGUMENT",
    })
}
#[derive(Clone)]
pub struct RuntimeResources {
    pub scheduler: Arc<Scheduler>,
    pub budgets: ManagedBudgets,
    pub clock: Arc<dyn Clock>,
    pub events: Arc<dyn EventSink>,
}
struct RootGrant {
    root: Arc<ApprovedRoot>,
    name: Vec<u8>,
    _memory: Reservation,
}
/// Opaque preparation result: cannot be constructed from a path or serialized
/// entry supplied by the frontend. It owns the exact validated native object.
pub struct PreparedRoot {
    selection: Option<SelectionTicket>,
    grant: Arc<RootGrant>,
    entry: Entry,
}
/// Only one current generation is exposed. Old owners may live while I/O drains.
struct Generation {
    scope: Scope,
    grant: Arc<RootGrant>,
    root_id: String,
    index: SharedDirectoryIndex,
    listing: ListingService,
    usage: UsageService,
    events: Arc<GenerationEvents>,
    _memory: Reservation,
}
struct GenerationEvents {
    enabled: Mutex<bool>,
    sink: Arc<dyn EventSink>,
}
impl EventSink for GenerationEvents {
    fn try_emit(&self, event: WorkEvent) -> bool {
        // Sink contract forbids I/O, waiting and service reentry. Closing this
        // gate completes before old-generation service finalizers can emit.
        let enabled = self.enabled.lock().unwrap();
        *enabled && self.sink.try_emit(event)
    }
}
impl Generation {
    fn new(
        resources: &RuntimeResources,
        scope: Scope,
        prepared: PreparedRoot,
    ) -> Result<Self, FsIssue> {
        let memory = resources
            .budgets
            .work
            .reserve(16 * 1024)
            .map_err(|_| issue("RESOURCE_LIMIT"))?;
        let index: SharedDirectoryIndex = DirectoryIndex::with_budgets(
            format!("{}-g{}", scope.session_id, scope.generation),
            resources.budgets.background_index.clone(),
            resources.budgets.foreground_index.clone(),
        )
        .into();
        index.bind_scope(&scope).map_err(index_error)?;
        let root_id = index
            .register(
                DirectoryKey {
                    parent_id: None,
                    raw_name: vec![],
                },
                Origin::Foreground,
            )
            .map_err(index_error)?;
        index
            .bind_identity(
                &root_id,
                prepared.grant.root.directory().identity().fingerprint(),
            )
            .map_err(index_error)?;
        let mut entry = prepared.entry;
        entry.entry_id = root_id.clone();
        entry.parent_id = None;
        index.expose(&root_id, entry).map_err(index_error)?;
        let events = Arc::new(GenerationEvents {
            enabled: Mutex::new(true),
            sink: resources.events.clone(),
        });
        let usage = UsageService::with_budgets(
            scope.clone(),
            root_id.clone(),
            index.clone(),
            resources.scheduler.clone(),
            resources.clock.clone(),
            events.clone(),
            resources.budgets.clone(),
        )
        .map_err(usage_error)?;
        let listing = ListingService::with_events(
            scope.clone(),
            index.clone(),
            resources.scheduler.clone(),
            ListingCache::with_budget(resources.budgets.listings.clone(), TTL_MS, MESSAGE_BYTES),
            resources.clock.clone(),
            events.clone(),
        );
        Ok(Self {
            scope,
            grant: prepared.grant,
            root_id,
            index,
            listing,
            usage,
            events,
            _memory: memory,
        })
    }
    fn data(&self) -> RootData {
        RootData {
            session: RootSession {
                session_id: self.scope.session_id.clone(),
                generation: self.scope.generation,
                root_entry_id: self.root_id.clone(),
                platform: Platform::Macos,
                capacity: self.index.capacity(),
            },
            root: self
                .index
                .entry(&self.root_id)
                .expect("prepared root is exposed"),
        }
    }
    fn close(&self) {
        *self.events.enabled.lock().unwrap() = false;
        self.listing.close();
        self.usage.close();
    }
}
impl Drop for Generation {
    fn drop(&mut self) {
        self.close();
    }
}
struct State {
    root_work_scope: Scope,
    controller: SessionController,
    grant: Option<Arc<RootGrant>>,
    current: Option<Arc<Generation>>,
    refresh_pending: bool,
    failure: Option<FsIssue>,
}
/// A refresh keeps the original grant, not its former pathname. No Clone: a
/// preparation has one completion owner, and stale completion is rejected.
pub struct RefreshTicket {
    context: NativeContext,
    epoch: u64,
    scope: Scope,
    grant: Arc<RootGrant>,
}
/// An admitted refresh failure must carry the advanced scope to the client.
/// A stale callback does not claim to describe the current session.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshFailure {
    pub error: FsIssue,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_scope: Option<Scope>,
}
impl RefreshFailure {
    fn uncommitted(error: FsIssue) -> Self {
        Self {
            error,
            current_scope: None,
        }
    }
}
impl RefreshTicket {
    pub fn scope(&self) -> &Scope {
        &self.scope
    }
}
pub struct NativeRuntime {
    state: Mutex<State>,
    resources: RuntimeResources,
    _memory: Reservation,
}
impl NativeRuntime {
    pub fn new(context: NativeContext, resources: RuntimeResources) -> Result<Self, FsIssue> {
        let memory = resources
            .budgets
            .work
            .reserve(32 * 1024)
            .map_err(|_| issue("RESOURCE_LIMIT"))?;
        Ok(Self {
            state: Mutex::new(State {
                root_work_scope: Scope {
                    session_id: format!(
                        "native-preparation-{}",
                        NEXT_RUNTIME.fetch_add(1, Ordering::Relaxed)
                    ),
                    generation: 1,
                },
                controller: SessionController::new(context),
                grant: None,
                current: None,
                refresh_pending: false,
                failure: None,
            }),
            resources,
            _memory: memory,
        })
    }
    pub fn connect(&self, context: &NativeContext, nonce: &str) -> Result<u64, FsIssue> {
        self.state
            .lock()
            .unwrap()
            .controller
            .connect(context, nonce)
            .map_err(control)
    }
    /// Trusted native lifecycle only. Call before accepting the new document.
    pub fn document_loaded(&self, context: NativeContext) {
        let retired = {
            let mut s = self.state.lock().unwrap();
            // SessionController ignores duplicate callbacks for the same document.
            if !s.controller.document_loaded(context) {
                return;
            }
            s.refresh_pending = false;
            s.failure = None;
            (
                s.current.take(),
                s.grant.take(),
                Self::supersede_root_work(&mut s),
            )
        };
        self.resources.scheduler.cancel_scope(&retired.2);
        Self::retire((retired.0, retired.1), true);
    }
    pub fn begin_selection(
        &self,
        context: &NativeContext,
        epoch: u64,
    ) -> Result<SelectionTicket, FsIssue> {
        self.state
            .lock()
            .unwrap()
            .controller
            .begin_selection(context, epoch)
            .map_err(control)
    }
    /// Fixed worker only. Validate before any selected-candidate I/O and after it.
    pub fn prepare_selection(
        &self,
        ticket: &SelectionTicket,
        path: &Path,
    ) -> Result<PreparedRoot, FsIssue> {
        self.state
            .lock()
            .unwrap()
            .controller
            .validate_selection(ticket)
            .map_err(control)?;
        let memory = self
            .resources
            .budgets
            .work
            .reserve(8192)
            .map_err(|_| issue("RESOURCE_LIMIT"))?;
        let root = Arc::new(
            ApprovedRoot::open_picker_candidate(
                path,
                self.resources.scheduler.handles(),
                self.resources.budgets.work.clone(),
            )
            .map_err(|e| fs_issue(e, IssueScope::Root, "chooseRoot"))?,
        );
        let name = path.file_name().map(|n| n.as_bytes()).unwrap_or(b"/");
        if name.len() > crate::domain::index::MAX_COMPONENT_BYTES {
            return Err(issue("INVALID_ARGUMENT"));
        }
        let mut prepared = self.observe(Arc::new(RootGrant {
            root,
            name: name.to_vec(),
            _memory: memory,
        }))?;
        prepared.selection = Some(ticket.clone());
        self.state
            .lock()
            .unwrap()
            .controller
            .validate_selection(ticket)
            .map_err(control)?;
        Ok(prepared)
    }
    fn observe(&self, grant: Arc<RootGrant>) -> Result<PreparedRoot, FsIssue> {
        let stat = grant
            .root
            .directory()
            .metadata()
            .map_err(|e| fs_issue(e, IssueScope::Root, "rootMetadata"))?;
        let entry = metadata_entry(
            &grant.name,
            &stat,
            self.resources.clock.sample().observed_at,
        );
        Ok(PreparedRoot {
            grant,
            entry,
            selection: None,
        })
    }
    /// None means native picker cancellation. Any failure preserves current root.
    pub fn finish_selection(
        &self,
        ticket: SelectionTicket,
        result: Result<Option<PreparedRoot>, FsIssue>,
    ) -> Result<Option<RootData>, FsIssue> {
        let preparation = (|| {
            self.state
                .lock()
                .unwrap()
                .controller
                .validate_selection(&ticket)
                .map_err(control)?;
            let Some(prepared) = result? else {
                return Ok(None);
            };
            if prepared.selection.as_ref() != Some(&ticket) {
                return Err(issue("CLIENT_EXPIRED"));
            }
            let serial = NEXT_SESSION
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
                .map_err(|_| issue("RESOURCE_LIMIT"))?;
            Generation::new(
                &self.resources,
                Scope {
                    session_id: format!("native-session-{serial}"),
                    generation: 1,
                },
                prepared,
            )
            .map(|g| Some(Arc::new(g)))
        })();
        let mut s = self.state.lock().unwrap();
        if let Err(e) = s.controller.validate_selection(&ticket) {
            let _ = s.controller.dismiss_selection(&ticket);
            return Err(control(e));
        }
        let next = match preparation {
            Ok(Some(next)) => next,
            other => {
                s.controller.dismiss_selection(&ticket).map_err(control)?;
                return other.map(|_| None);
            }
        };
        s.controller
            .accept_selection(&ticket, next.scope.session_id.clone())
            .map_err(control)?;
        let data = next.data();
        let retired = (
            s.current.replace(next.clone()),
            s.grant.replace(next.grant.clone()),
            Self::supersede_root_work(&mut s),
        );
        s.refresh_pending = false;
        s.failure = None;
        drop(s);
        self.resources.scheduler.cancel_scope(&retired.2);
        Self::retire((retired.0, retired.1), true);
        Ok(Some(data))
    }
    pub fn begin_refresh(
        &self,
        context: &NativeContext,
        epoch: u64,
        expected: &Scope,
    ) -> Result<RefreshTicket, FsIssue> {
        let (ticket, old, work_scope) = {
            let mut s = self.state.lock().unwrap();
            s.controller
                .validate(context, epoch, Some(expected))
                .map_err(control)?;
            let grant = s.grant.clone().ok_or_else(|| issue("SESSION_CLOSED"))?;
            let scope = s.controller.refresh(expected).map_err(control)?;
            s.refresh_pending = true;
            s.failure = None;
            (
                RefreshTicket {
                    context: context.clone(),
                    epoch,
                    scope,
                    grant,
                },
                s.current.take(),
                Self::supersede_root_work(&mut s),
            )
        };
        self.resources.scheduler.cancel_scope(&work_scope);
        Self::retire((old, None), false);
        Ok(ticket)
    }
    fn validate_refresh(&self, ticket: &RefreshTicket) -> Result<(), FsIssue> {
        let s = self.state.lock().unwrap();
        s.controller
            .validate(&ticket.context, ticket.epoch, Some(&ticket.scope))
            .map_err(control)?;
        if !s.refresh_pending {
            return Err(issue("STALE_GENERATION"));
        }
        Ok(())
    }
    /// Fixed worker only; preserves original object even after root path replacement.
    pub fn prepare_refresh(&self, ticket: &RefreshTicket) -> Result<PreparedRoot, FsIssue> {
        self.validate_refresh(ticket)?;
        let result = self.observe(ticket.grant.clone());
        self.validate_refresh(ticket)?;
        result
    }
    pub fn finish_refresh(
        &self,
        ticket: RefreshTicket,
        result: Result<PreparedRoot, FsIssue>,
    ) -> Result<RootData, RefreshFailure> {
        self.validate_refresh(&ticket)
            .map_err(RefreshFailure::uncommitted)?;
        // The token must not be paired with a different prepared root even by an
        // accidental internal call; no frontend-controlled fields establish this.
        let next = result.and_then(|p| {
            if p.selection.is_some() || !Arc::ptr_eq(&p.grant, &ticket.grant) {
                return Err(issue("ENTRY_CHANGED"));
            }
            Generation::new(&self.resources, ticket.scope.clone(), p).map(Arc::new)
        });
        let mut s = self.state.lock().unwrap();
        s.controller
            .validate(&ticket.context, ticket.epoch, Some(&ticket.scope))
            .map_err(control)
            .map_err(RefreshFailure::uncommitted)?;
        if !s.refresh_pending {
            return Err(RefreshFailure::uncommitted(issue("STALE_GENERATION")));
        }
        s.refresh_pending = false;
        match next {
            Ok(next) => {
                let data = next.data();
                s.current = Some(next);
                Ok(data)
            }
            Err(e) => {
                s.failure = Some(e.clone());
                Err(RefreshFailure {
                    error: e,
                    current_scope: Some(ticket.scope),
                })
            }
        }
    }
    fn supersede_root_work(state: &mut State) -> Scope {
        let previous = state.root_work_scope.clone();
        state.root_work_scope.generation = state
            .root_work_scope
            .generation
            .checked_add(1)
            .expect("root work epoch exhausted");
        previous
    }
    fn retire(
        (generation, grant): (Option<Arc<Generation>>, Option<Arc<RootGrant>>),
        revoke: bool,
    ) {
        if revoke {
            if let Some(grant) = grant {
                grant.root.revoke();
            }
        }
        if let Some(generation) = generation {
            generation.close();
        }
    }
    pub fn close_session(
        &self,
        context: &NativeContext,
        epoch: u64,
        session_id: &str,
    ) -> Result<bool, FsIssue> {
        let retired = {
            let mut s = self.state.lock().unwrap();
            s.controller
                .validate(context, epoch, None)
                .map_err(control)?;
            if session_id.is_empty() || session_id.len() > 128 {
                return Err(issue("INVALID_ARGUMENT"));
            }
            // With no active root, close has no effect regardless of how old the
            // identifier is. Never close a different currently active session.
            match s.controller.scope.as_ref() {
                None => return Ok(false),
                Some(current) if current.session_id != session_id => {
                    return Err(issue("SESSION_CLOSED"));
                }
                _ => (),
            }
            s.controller.close();
            s.failure = None;
            s.refresh_pending = false;
            (
                s.current.take(),
                s.grant.take(),
                Self::supersede_root_work(&mut s),
            )
        };
        self.resources.scheduler.cancel_scope(&retired.2);
        Self::retire((retired.0, retired.1), true);
        Ok(true)
    }
    fn with_generation<T>(
        &self,
        context: &NativeContext,
        epoch: u64,
        scope: &Scope,
        operation: impl FnOnce(&Generation) -> Result<T, FsIssue>,
    ) -> Result<T, FsIssue> {
        let generation = {
            let s = self.state.lock().unwrap();
            s.controller
                .validate(context, epoch, Some(scope))
                .map_err(control)?;
            s.current
                .clone()
                .ok_or_else(|| s.failure.clone().unwrap_or_else(|| issue("RESOURCE_LIMIT")))?
        };
        let result = operation(&generation);
        self.state
            .lock()
            .unwrap()
            .controller
            .validate(context, epoch, Some(scope))
            .map_err(control)?;
        result
    }
    pub fn root_data(
        &self,
        context: &NativeContext,
        epoch: u64,
        scope: &Scope,
    ) -> Result<RootData, FsIssue> {
        self.with_generation(context, epoch, scope, |g| Ok(g.data()))
    }
    pub fn start_listing(
        &self,
        context: &NativeContext,
        epoch: u64,
        scope: &Scope,
        id: &str,
        category: Category,
    ) -> Result<(ListingHandle, WorkRecord), FsIssue> {
        self.with_generation(context, epoch, scope, |g| {
            g.index.key(id).map_err(index_error)?;
            let time = self.resources.clock.sample();
            let source = NativeListingSource::new(
                g.grant.root.clone(),
                g.index.clone(),
                g.root_id.clone(),
                self.resources.clock.clone(),
            )?;
            let handle = g
                .listing
                .start(
                    id,
                    category,
                    200,
                    &time.observed_at,
                    time.monotonic_ms,
                    source,
                )
                .map_err(cache_error)?;
            let work = g
                .listing
                .work(&handle.task_id, time.monotonic_ms)
                .map_err(cache_error)?;
            Ok((handle, work))
        })
    }
    pub fn read_listing(
        &self,
        context: &NativeContext,
        epoch: u64,
        scope: &Scope,
        task: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Charged<ListingPage>, FsIssue> {
        self.with_generation(context, epoch, scope, |g| {
            g.listing
                .charged_page(
                    task,
                    cursor,
                    limit,
                    self.resources.clock.sample().monotonic_ms,
                    &self.resources.budgets.work,
                )
                .map_err(cache_error)
        })
    }
    pub fn start_usage(
        &self,
        context: &NativeContext,
        epoch: u64,
        scope: &Scope,
    ) -> Result<WorkRecord, FsIssue> {
        self.with_generation(context, epoch, scope, |g| {
            let source =
                NativeUsageSource::new(g.grant.root.clone(), &self.resources.budgets.work)?;
            g.usage
                .start(
                    NativeUsageSource::root_request(),
                    source,
                    ScanLimits::default(),
                )
                .map_err(usage_error)
        })
    }
    pub fn read_usage(
        &self,
        context: &NativeContext,
        epoch: u64,
        scope: &Scope,
        ids: &[String],
    ) -> Result<Charged<Vec<DirectoryUsage>>, FsIssue> {
        self.with_generation(context, epoch, scope, |g| {
            g.usage.charged_usage(ids).map_err(usage_error)
        })
    }
    pub fn read_task(
        &self,
        context: &NativeContext,
        epoch: u64,
        scope: &Scope,
        task: &str,
    ) -> Result<WorkRecord, FsIssue> {
        self.with_generation(context, epoch, scope, |g| match g.usage.work(task) {
            Err(UsageError::TaskExpired) => g
                .listing
                .work(task, self.resources.clock.sample().monotonic_ms)
                .map_err(cache_error),
            result => result.map_err(usage_error),
        })
    }
    pub fn cancel_task(
        &self,
        context: &NativeContext,
        epoch: u64,
        scope: &Scope,
        task: &str,
    ) -> Result<(CancelStatus, WorkRecord), FsIssue> {
        self.with_generation(context, epoch, scope, |g| match g.usage.cancel(task) {
            Err(UsageError::TaskExpired) => {
                let now = self.resources.clock.sample().monotonic_ms;
                let previous = g.listing.work(task, now).map_err(cache_error)?;
                let status = if previous.phase.terminal() {
                    CancelStatus::AlreadyTerminal
                } else {
                    g.listing.cancel(task).map_err(cache_error)?;
                    CancelStatus::Accepted
                };
                Ok((status, g.listing.work(task, now).map_err(cache_error)?))
            }
            result => result.map_err(usage_error),
        })
    }
}
impl Drop for NativeRuntime {
    fn drop(&mut self) {
        let s = self.state.get_mut().unwrap();
        s.controller.close();
        self.resources.scheduler.cancel_scope(&s.root_work_scope);
        Self::retire((s.current.take(), s.grant.take()), true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{clock::SystemClock, events::DiscardEvents};
    use std::{
        fs,
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };
    #[test]
    fn close_does_not_wait_for_admitted_reader_and_discards_its_late_snapshot() {
        let path =
            std::env::temp_dir().join(format!("duv-runtime-read-barrier-{}", std::process::id()));
        fs::create_dir(&path).unwrap();
        let context = NativeContext {
            window: "main".into(),
            document: 1,
        };
        let resources = RuntimeResources {
            scheduler: Arc::new(Scheduler::default()),
            budgets: ManagedBudgets::default(),
            clock: Arc::new(SystemClock::default()),
            events: Arc::new(DiscardEvents),
        };
        let runtime = Arc::new(NativeRuntime::new(context.clone(), resources.clone()).unwrap());
        let epoch = runtime.connect(&context, "one").unwrap();
        let ticket = runtime.begin_selection(&context, epoch).unwrap();
        let prepared = runtime.prepare_selection(&ticket, &path).unwrap();
        let data = runtime
            .finish_selection(ticket, Ok(Some(prepared)))
            .unwrap()
            .unwrap();
        let scope = Scope {
            session_id: data.session.session_id,
            generation: 1,
        };
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let reader = {
            let runtime = runtime.clone();
            let context = context.clone();
            let scope = scope.clone();
            thread::spawn(move || {
                runtime.with_generation(&context, epoch, &scope, |g| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(g.data())
                })
            })
        };
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let start = Instant::now();
        runtime
            .close_session(&context, epoch, &scope.session_id)
            .unwrap();
        assert!(start.elapsed() < Duration::from_millis(100));
        assert_eq!(
            resources.scheduler.handles().open(),
            1,
            "admitted reader still owns the revoked root"
        );
        assert!(resources.budgets.foreground_index.used() > 0);
        release_tx.send(()).unwrap();
        assert_eq!(reader.join().unwrap().unwrap_err().code, "SESSION_CLOSED");
        assert_eq!(resources.scheduler.handles().open(), 0);
        assert_eq!(resources.budgets.foreground_index.used(), 0);
        drop(runtime);
        assert_eq!(resources.budgets.used(), 0);
        fs::remove_dir(path).unwrap();
    }
}
