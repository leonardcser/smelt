use std::collections::{BTreeMap, HashMap, HashSet};
#[cfg(test)]
use std::collections::{BTreeSet, VecDeque};

use rusqlite::{Connection, OptionalExtension, Savepoint, Transaction, TransactionBehavior};

use crate::compression::ObjectCompression;
use crate::error::{Result, StoreError};
use crate::history::StoredTranscriptBlock;
use crate::meta::{SessionCostUsd, SessionIdentity, SessionMetadata};
use crate::object::{checked_i64, object, put_object, sha256_hex};
use crate::session_commit::{
    HistoryIndex, SaveReceipt, SessionCommit, SessionCommitFailure, SideTableSuffixes,
    StartupRecoveryReceipt, StoreHead, StoredTurn, SubmitTurn, SubmitTurnReceipt, TurnId, TurnKind,
    TurnState, TurnTransition, TurnTransitionReceipt,
};

mod sequence;
#[cfg(test)]
use sequence::LEAF_TARGET_BYTES;
pub(crate) use sequence::*;
mod revision;
pub(crate) use revision::*;
mod session;
pub(crate) use session::*;
mod extent;
pub(crate) use extent::*;
mod lifecycle;
pub(crate) use lifecycle::*;
mod reclamation;
pub(crate) use reclamation::*;

#[cfg(test)]
mod reachability;
#[cfg(test)]
use reachability::*;
#[cfg(test)]
mod tests;
