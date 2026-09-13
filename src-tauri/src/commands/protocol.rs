//! Strict untrusted wire input. Native window/document context is never a field.
use crate::domain::{
    controller::Scope,
    model::{Category, FsIssue, IssueScope},
};
use serde::Deserialize;
const MAX_SAFE: u64 = 9_007_199_254_740_991;
pub const MAX_MESSAGE_BYTES: usize = 1_048_576;
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub protocol_version: u8,
    pub request_id: String,
    pub body: Command,
}
#[derive(Debug, Deserialize)]
#[serde(
    tag = "operation",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Command {
    ChooseRoot {},
    StartListing {
        session_id: String,
        generation: u64,
        directory_id: String,
        category: Category,
    },
    ReadListingPage {
        session_id: String,
        generation: u64,
        task_id: String,
        cursor: Option<String>,
        limit: usize,
    },
    StartUsageScan {
        session_id: String,
        generation: u64,
    },
    ReadUsage {
        session_id: String,
        generation: u64,
        directory_ids: Vec<String>,
    },
    ReadTask {
        session_id: String,
        generation: u64,
        task_id: String,
    },
    CancelTask {
        session_id: String,
        generation: u64,
        task_id: String,
    },
    RefreshRoot {
        session_id: String,
        expected_generation: u64,
    },
    CloseSession {
        session_id: String,
    },
}
#[derive(Debug, PartialEq, Eq)]
pub struct RequestNumber {
    pub epoch: u64,
    pub sequence: u64,
}
fn issue(code: &str) -> FsIssue {
    FsIssue {
        code: code.into(),
        operation: "request".into(),
        scope: IssueScope::Root,
        entry_id: None,
        native_code: None,
    }
}
fn identifier(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}
fn number(value: &str) -> Option<u64> {
    if value.starts_with('0') || value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse::<u64>().ok().filter(|n| *n <= MAX_SAFE)
}
impl Request {
    /// The caller must reserve the inbound message's managed allocation before
    /// parsing. This bound also caps serde allocation for malformed envelopes.
    pub fn parse(bytes: &[u8]) -> Result<Self, FsIssue> {
        if bytes.len() > MAX_MESSAGE_BYTES {
            return Err(issue("RESOURCE_LIMIT"));
        }
        let input: Self = serde_json::from_slice(bytes).map_err(|_| issue("INVALID_ARGUMENT"))?;
        input.validate()?;
        Ok(input)
    }
    pub fn validate(&self) -> Result<RequestNumber, FsIssue> {
        if self.protocol_version != 1 {
            return Err(issue("VERSION_UNSUPPORTED"));
        }
        if self.request_id.len() > 128 {
            return Err(issue("INVALID_ARGUMENT"));
        }
        let (epoch, sequence) = self
            .request_id
            .split_once(':')
            .ok_or_else(|| issue("INVALID_ARGUMENT"))?;
        let epoch = epoch
            .strip_prefix("native-epoch-")
            .and_then(number)
            .ok_or_else(|| issue("CLIENT_EXPIRED"))?;
        let sequence = number(sequence).ok_or_else(|| issue("INVALID_ARGUMENT"))?;
        if let Some(scope) = self.body.scope() {
            if !identifier(&scope.session_id, 128) || !(1..=MAX_SAFE).contains(&scope.generation) {
                return Err(issue("INVALID_ARGUMENT"));
            }
        }
        let valid = match &self.body {
            Command::StartListing { directory_id, .. } => identifier(directory_id, 256),
            Command::ReadListingPage {
                task_id,
                cursor,
                limit,
                ..
            } => {
                identifier(task_id, 128)
                    && cursor.as_ref().is_none_or(|c| identifier(c, 256))
                    && (1..=1000).contains(limit)
            }
            Command::ReadUsage { directory_ids, .. } => {
                directory_ids.len() <= 1000 && directory_ids.iter().all(|s| identifier(s, 256))
            }
            Command::ReadTask { task_id, .. } | Command::CancelTask { task_id, .. } => {
                identifier(task_id, 128)
            }
            Command::CloseSession { session_id } => identifier(session_id, 128),
            _ => true,
        };
        if !valid {
            return Err(issue("INVALID_ARGUMENT"));
        }
        Ok(RequestNumber { epoch, sequence })
    }
}
impl Command {
    pub fn scope(&self) -> Option<Scope> {
        self.scope_ref().map(|(session_id, generation)| Scope {
            session_id: session_id.into(),
            generation,
        })
    }
    fn scope_ref(&self) -> Option<(&str, u64)> {
        let (session_id, generation) = match self {
            Self::ChooseRoot {} | Self::CloseSession { .. } => return None,
            Self::StartListing {
                session_id,
                generation,
                ..
            }
            | Self::ReadListingPage {
                session_id,
                generation,
                ..
            }
            | Self::StartUsageScan {
                session_id,
                generation,
            }
            | Self::ReadUsage {
                session_id,
                generation,
                ..
            }
            | Self::ReadTask {
                session_id,
                generation,
                ..
            }
            | Self::CancelTask {
                session_id,
                generation,
                ..
            } => (session_id, generation),
            Self::RefreshRoot {
                session_id,
                expected_generation,
            } => (session_id, expected_generation),
        };
        Some((session_id, *generation))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    fn parse(body: Value) -> Result<Request, FsIssue> {
        Request::parse(
            &serde_json::to_vec(
                &json!({"protocolVersion":1,"requestId":"native-epoch-3:42","body":body}),
            )
            .unwrap(),
        )
    }
    #[test]
    fn caller_cannot_supply_paths_native_context_or_unknown_command_fields() {
        for body in [
            json!({"operation":"chooseRoot","path":"/tmp/fake"}),
            json!({"operation":"chooseRoot","window":"main"}),
            json!({"operation":"chooseRoot","document":1}),
            json!({"operation":"readTask","sessionId":"s","generation":1,"taskId":"t","extra":true}),
        ] {
            assert_eq!(parse(body).unwrap_err().code, "INVALID_ARGUMENT");
        }
        assert_eq!(Request::parse(br#"{"protocolVersion":1,"requestId":"native-epoch-3:42","body":{"operation":"chooseRoot"},"document":1}"#).unwrap_err().code, "INVALID_ARGUMENT");
        assert_eq!(
            parse(json!({"operation":"chooseRoot"}))
                .unwrap()
                .validate()
                .unwrap(),
            RequestNumber {
                epoch: 3,
                sequence: 42
            }
        );
    }
    #[test]
    fn limits_and_safe_integers_are_rejected_before_dispatch() {
        for n in [0, 1001] {
            assert!(parse(json!({"operation":"readListingPage","sessionId":"s","generation":1,"taskId":"t","cursor":null,"limit":n})).is_err());
        }
        for generation in [0, MAX_SAFE + 1] {
            assert!(
                parse(
                    json!({"operation":"startUsageScan","sessionId":"s","generation":generation})
                )
                .is_err()
            );
        }
        assert!(parse(json!({"operation":"readUsage","sessionId":"s","generation":1,"directoryIds":vec!["id";1001]})).is_err());
        assert!(parse(json!({"operation":"readUsage","sessionId":"s","generation":1,"directoryIds":vec!["id";1000]})).is_ok());
        assert_eq!(
            Request::parse(&vec![b' '; MAX_MESSAGE_BYTES + 1])
                .unwrap_err()
                .code,
            "RESOURCE_LIMIT"
        );
    }
    #[test]
    fn protocol_and_canonical_epoch_numbers_do_not_accept_lossy_or_ambiguous_values() {
        for id in [
            "native-epoch-03:1",
            "native-epoch-3:01",
            "native-epoch-3:1:2",
            "native-epoch-0:1",
            "native-epoch-3:9007199254740992",
            "other:1",
        ] {
            let input =
                json!({"protocolVersion":1,"requestId":id,"body":{"operation":"chooseRoot"}});
            assert!(Request::parse(&serde_json::to_vec(&input).unwrap()).is_err());
        }
        assert_eq!(Request::parse(br#"{"protocolVersion":2,"requestId":"native-epoch-3:1","body":{"operation":"chooseRoot"}}"#).unwrap_err().code, "VERSION_UNSUPPORTED");
    }
}
