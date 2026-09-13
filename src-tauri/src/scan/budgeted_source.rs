//! Charge every source-owned directory cursor to the application's shared FD pool.
use super::usage::{Observation, ObservationSource};
use crate::{
    domain::model::{FsIssue, IssueScope},
    scheduler::{HandleBudget, HandlePermit},
};

pub struct BudgetedSource<S> {
    source: S,
    handles: HandleBudget,
}
pub struct BudgetedCursor<C> {
    // Close the actual cursor before releasing its accounting permit.
    cursor: C,
    _permit: HandlePermit,
}
impl<S> BudgetedSource<S> {
    pub fn new(source: S, handles: HandleBudget) -> Self {
        Self { source, handles }
    }
    pub fn inner(&self) -> &S {
        &self.source
    }
}
impl<S: ObservationSource> ObservationSource for BudgetedSource<S> {
    type Cursor = BudgetedCursor<S::Cursor>;
    fn open(&mut self, directory: &Observation) -> Result<Self::Cursor, FsIssue> {
        let permit = self.handles.acquire().map_err(|_| FsIssue {
            code: "RESOURCE_LIMIT".into(),
            operation: "openDirectory".into(),
            scope: IssueScope::Subtree,
            entry_id: None,
            native_code: None,
        })?;
        let cursor = self.source.open(directory)?;
        Ok(BudgetedCursor {
            cursor,
            _permit: permit,
        })
    }
    fn next(&mut self, cursor: &mut Self::Cursor) -> Result<Option<Observation>, FsIssue> {
        self.source.next(&mut cursor.cursor)
    }
}
