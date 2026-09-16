//! Share native approval-handle permits, or charge non-native source cursors.
//! walkdir-owned directory streams are bounded separately by cursor depth.
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
    _permit: Option<HandlePermit>,
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
    fn bind_directory(
        &mut self,
        observation: &Observation,
        id: &str,
        index: &crate::domain::index::SharedDirectoryIndex,
    ) -> Result<(), FsIssue> {
        self.source.bind_directory(observation, id, index)
    }
    fn refresh_root(&mut self, root: &Observation) -> Result<Observation, FsIssue> {
        self.check_pool()?;
        self.source.refresh_root(root)
    }
    fn open(&mut self, directory: &Observation) -> Result<Self::Cursor, FsIssue> {
        self.check_pool()?;
        let permit = if self.source.handle_budget().is_some() {
            None
        } else {
            Some(self.handles.acquire().map_err(|_| FsIssue {
                code: "RESOURCE_LIMIT".into(),
                operation: "openDirectory".into(),
                scope: IssueScope::Subtree,
                entry_id: None,
                native_code: None,
            })?)
        };
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

impl<S: ObservationSource> BudgetedSource<S> {
    fn check_pool(&self) -> Result<(), FsIssue> {
        if self
            .source
            .handle_budget()
            .is_some_and(|pool| !pool.same_pool(&self.handles))
        {
            return Err(FsIssue {
                code: "INVALID_ARGUMENT".into(),
                operation: "openDirectory".into(),
                scope: IssueScope::Root,
                entry_id: None,
                native_code: None,
            });
        }
        Ok(())
    }
}
