use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;

/// Exact nonnegative wire integer. Never serializes as an IEEE-754 number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Bytes(String);
impl From<u64> for Bytes {
    fn from(value: u64) -> Self {
        Self(value.to_string())
    }
}
impl<'de> Deserialize<'de> for Bytes {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        if value.is_empty()
            || (value.len() > 1 && value.starts_with('0'))
            || !value.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(serde::de::Error::custom("INVALID_BYTES"));
        }
        Ok(Self(value))
    }
}
macro_rules! wire_enum {
    ($name:ident { $($variant:ident),* $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "camelCase")]
        pub enum $name { $($variant),* }
    }
}
wire_enum!(Reason {
    Scanning,
    ReadError,
    Cancelled,
    Unsupported,
    ChangedDuringScan,
    Overflow,
    ResourceLimit,
    ExcludedByPolicy
});
wire_enum!(Kind {
    Directory,
    RegularFile,
    Symlink,
    Other,
    Unknown
});
wire_enum!(SpecialType {
    None,
    MacAlias,
    MacPackage,
    LinuxDesktopEntry,
    Other
});
wire_enum!(Platform {
    Macos,
    Linux,
    Fixture
});
wire_enum!(CapacityState { Available, Limited });
wire_enum!(Phase {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled
});
wire_enum!(Operation {
    ListDirectories,
    ListFiles,
    ScanUsage
});
wire_enum!(WaitReason {
    Draining,
    Slot,
    PageDemand
});
wire_enum!(IssueScope {
    Root,
    Entry,
    Subtree
});
wire_enum!(ScanState {
    NotRequested,
    Queued,
    Running,
    Settled,
    Failed,
    Cancelled
});
wire_enum!(FollowPolicy { Never });
wire_enum!(Category { Directories, Files });
wire_enum!(Coverage { Complete, Partial });
impl Phase {
    pub fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FsIssue {
    pub code: String,
    pub operation: String,
    pub scope: IssueScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_code: Option<i32>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase", deny_unknown_fields)]
pub enum Field<T> {
    Known { value: T },
    Unknown,
    Unsupported,
    Error { issue: FsIssue },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase", deny_unknown_fields)]
pub enum Measure {
    Unknown,
    Complete {
        bytes: Bytes,
    },
    Partial {
        #[serde(rename = "observedBytes")]
        observed_bytes: Bytes,
        reasons: Vec<Reason>,
    },
    Unavailable {
        reason: Reason,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Issues {
    pub counts: BTreeMap<String, Bytes>,
    pub samples: Vec<FsIssue>,
}
impl Issues {
    pub fn record(&mut self, issue: FsIssue) {
        let prior = self
            .counts
            .get(&issue.code)
            .and_then(|b| b.0.parse::<u64>().ok())
            .unwrap_or(0);
        self.counts
            .insert(issue.code.clone(), prior.saturating_add(1).into());
        if self.samples.len() < 32 {
            self.samples.push(issue);
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Capacity {
    pub background: CapacityState,
    pub foreground: CapacityState,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RootSession {
    pub session_id: String,
    pub generation: u64,
    pub root_entry_id: String,
    pub platform: Platform,
    pub capacity: Capacity,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Entry {
    pub entry_id: String,
    pub parent_id: Option<String>,
    pub display_name: String,
    pub kind: Kind,
    pub hidden: Field<bool>,
    pub special_type: Field<SpecialType>,
    pub modified_at: Field<String>,
    pub own_logical_bytes: Field<Bytes>,
    pub own_allocated_bytes: Field<Bytes>,
    pub observed_at: String,
    pub follow_policy: FollowPolicy,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RootData {
    pub session: RootSession,
    pub root: Entry,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkRecord {
    pub session_id: String,
    pub generation: u64,
    pub task_id: String,
    pub operation: Operation,
    pub target_id: String,
    pub phase: Phase,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait_reason: Option<WaitReason>,
    pub sequence: u64,
    pub processed_entries: Bytes,
    pub processed_directories: Bytes,
    pub observed_at: String,
    pub issues: Issues,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DirectoryUsage {
    pub directory_id: String,
    pub usage_revision: u64,
    pub scan_state: ScanState,
    pub logical: Measure,
    pub allocated: Measure,
    pub observed_at: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ListingPage {
    pub work: WorkRecord,
    pub listing_revision: String,
    pub directory_id: String,
    pub category: Category,
    pub entries: Vec<Entry>,
    pub cursor: Option<String>,
    pub next_cursor: Option<String>,
    pub coverage: Coverage,
    pub issues: Issues,
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wire_contract_roundtrips_exactly() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../src/features/explorer/model/wire-fixture.json"
        ))
        .unwrap();
        let root: RootData = serde_json::from_value(fixture["root"].clone()).unwrap();
        let work: WorkRecord = serde_json::from_value(fixture["work"].clone()).unwrap();
        let usage: DirectoryUsage = serde_json::from_value(fixture["usage"].clone()).unwrap();
        assert_eq!(serde_json::to_value(root).unwrap(), fixture["root"]);
        assert_eq!(serde_json::to_value(work).unwrap(), fixture["work"]);
        assert_eq!(serde_json::to_value(usage).unwrap(), fixture["usage"]);
    }
    #[test]
    fn bytes_reject_lossy_or_noncanonical_values() {
        for value in ["-1", "01", "1e3", "1.0", ""] {
            assert!(serde_json::from_value::<Bytes>(serde_json::json!(value)).is_err());
        }
        assert!(serde_json::from_str::<Bytes>("9007199254740993").is_err());
        assert_eq!(
            serde_json::to_string(&Bytes::from(9007199254740993)).unwrap(),
            "\"9007199254740993\""
        );
    }
    #[test]
    fn error_samples_are_bounded_without_losing_counts() {
        let mut issues = Issues::default();
        for _ in 0..10_000 {
            issues.record(FsIssue {
                code: "PERMISSION_DENIED".into(),
                operation: "list".into(),
                scope: IssueScope::Entry,
                entry_id: None,
                native_code: Some(13),
            });
        }
        assert_eq!(issues.samples.len(), 32);
        assert_eq!(issues.counts["PERMISSION_DENIED"], Bytes::from(10_000));
    }
}
