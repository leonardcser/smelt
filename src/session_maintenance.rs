use std::time::Instant;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum SessionMigrationState {
    Current,
    WouldMigrate,
    Migrated,
    Future,
    Unrecognized,
    Orphaned,
    Busy,
    Failed,
}

#[derive(Debug, serde::Serialize)]
struct SessionMigrationOutput {
    session_id: String,
    status: SessionMigrationState,
    #[serde(skip_serializing_if = "Option::is_none")]
    from_version: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    to_version: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    supported_version: Option<i32>,
    duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Default, serde::Serialize)]
struct SessionMigrationSummary {
    total: usize,
    current: usize,
    would_migrate: usize,
    migrated: usize,
    future: usize,
    unrecognized: usize,
    orphaned: usize,
    busy: usize,
    failed: usize,
}

#[derive(Debug, serde::Serialize)]
struct SessionMigrationBatchOutput {
    dry_run: bool,
    sessions: Vec<SessionMigrationOutput>,
    summary: SessionMigrationSummary,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum SessionOrphanQuarantineState {
    NotOrphaned,
    WouldQuarantine,
    Quarantined,
    Busy,
    Failed,
}

#[derive(Debug, serde::Serialize)]
struct SessionOrphanQuarantineOutput {
    session_id: String,
    status: SessionOrphanQuarantineState,
    #[serde(skip_serializing_if = "Option::is_none")]
    schema_version: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    quarantine_path: Option<String>,
    duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Default, serde::Serialize)]
struct SessionOrphanQuarantineSummary {
    total: usize,
    not_orphaned: usize,
    would_quarantine: usize,
    quarantined: usize,
    busy: usize,
    failed: usize,
}

#[derive(Debug, serde::Serialize)]
struct SessionOrphanQuarantineBatchOutput {
    dry_run: bool,
    sessions: Vec<SessionOrphanQuarantineOutput>,
    summary: SessionOrphanQuarantineSummary,
}

fn print_json<T: serde::Serialize>(value: &T) -> Result<(), String> {
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    serde_json::to_writer(&mut handle, value).map_err(|error| error.to_string())?;
    use std::io::Write;
    writeln!(handle).map_err(|error| error.to_string())
}

fn resolve_session_ids(reference: Option<&str>, all: bool) -> Result<Vec<String>, String> {
    if all {
        smelt_core::session::session_ids_result().map_err(|error| error.to_string())
    } else {
        Ok(vec![smelt_core::session::resolve_prefix(
            reference.expect("required by clap"),
        )
        .map_err(|error| error.to_string())?
        .into_string()])
    }
}

fn operation_duration_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}

fn session_error_is_busy(error: &smelt_core::session::SessionStoreError) -> bool {
    matches!(
        error,
        smelt_core::session::SessionStoreError::ReadOnlyOwnerConflict { .. }
            | smelt_core::session::SessionStoreError::Busy { .. }
    )
}

fn actionable_session_error(error: &smelt_core::session::SessionStoreError) -> String {
    match error {
        smelt_core::session::SessionStoreError::ReadOnlyOwnerConflict { owner }
            if owner == "unknown owner" =>
        {
            "session is open in another smelt process; close it and retry".into()
        }
        smelt_core::session::SessionStoreError::Orphaned { found } => format!(
            "orphaned session schema version {found} has no canonical identity or session content; review it with `smelt session quarantine-orphans <SESSION> --dry-run`"
        ),
        _ => error.to_string(),
    }
}

fn migration_error_output(
    session_id: String,
    from_version: Option<i32>,
    to_version: Option<i32>,
    started: Instant,
    error: smelt_core::session::SessionStoreError,
) -> SessionMigrationOutput {
    let (status, from_version, to_version, error_kind) = match &error {
        smelt_core::session::SessionStoreError::Orphaned { found } => (
            SessionMigrationState::Orphaned,
            Some(*found),
            None,
            "orphaned",
        ),
        error if session_error_is_busy(error) => (
            SessionMigrationState::Busy,
            from_version,
            to_version,
            "busy",
        ),
        _ => (
            SessionMigrationState::Failed,
            from_version,
            to_version,
            error.code(),
        ),
    };
    SessionMigrationOutput {
        session_id,
        status,
        from_version,
        to_version,
        supported_version: None,
        duration_ms: operation_duration_ms(started),
        error_kind: Some(error_kind.into()),
        error: Some(actionable_session_error(&error)),
    }
}

fn inspect_or_migrate_session(session_id: String, dry_run: bool) -> SessionMigrationOutput {
    let started = Instant::now();
    let status = match smelt_core::session::session_schema_status_result(&session_id) {
        Ok(status) => status,
        Err(error) => {
            return migration_error_output(session_id, None, None, started, error);
        }
    };
    match status {
        smelt_store::SessionSchemaStatus::Current { version } => SessionMigrationOutput {
            session_id,
            status: SessionMigrationState::Current,
            from_version: Some(version),
            to_version: Some(version),
            supported_version: None,
            duration_ms: operation_duration_ms(started),
            error_kind: None,
            error: None,
        },
        smelt_store::SessionSchemaStatus::Upgradeable { found, target } if dry_run => {
            SessionMigrationOutput {
                session_id,
                status: SessionMigrationState::WouldMigrate,
                from_version: Some(found),
                to_version: Some(target),
                supported_version: None,
                duration_ms: operation_duration_ms(started),
                error_kind: None,
                error: None,
            }
        }
        smelt_store::SessionSchemaStatus::Upgradeable { found, target } => {
            match smelt_core::session::migrate_session_schema_result(&session_id) {
                Ok(migration) => SessionMigrationOutput {
                    session_id,
                    status: if migration.migrated {
                        SessionMigrationState::Migrated
                    } else {
                        SessionMigrationState::Current
                    },
                    from_version: Some(migration.from_version),
                    to_version: Some(migration.to_version),
                    supported_version: None,
                    duration_ms: operation_duration_ms(started),
                    error_kind: None,
                    error: None,
                },
                Err(error) => migration_error_output(
                    session_id,
                    Some(found),
                    Some(target),
                    started,
                    error,
                ),
            }
        }
        smelt_store::SessionSchemaStatus::Future { found, supported } => {
            let error = smelt_core::session::SessionStoreError::UnsupportedSchema {
                found,
                supported,
            };
            SessionMigrationOutput {
                session_id,
                status: SessionMigrationState::Future,
                from_version: Some(found),
                to_version: None,
                supported_version: Some(supported),
                duration_ms: operation_duration_ms(started),
                error_kind: Some(error.code().into()),
                error: Some(error.to_string()),
            }
        }
        smelt_store::SessionSchemaStatus::Unrecognized { found, supported } => {
            SessionMigrationOutput {
                session_id,
                status: SessionMigrationState::Unrecognized,
                from_version: Some(found),
                to_version: None,
                supported_version: Some(supported),
                duration_ms: operation_duration_ms(started),
                error_kind: Some("unrecognized_schema".into()),
                error: Some(format!(
                    "unrecognized session schema version {found}; supported migrations start at 1 and target {supported}"
                )),
            }
        }
        smelt_store::SessionSchemaStatus::Orphaned { found } => migration_error_output(
            session_id,
            Some(found),
            None,
            started,
            smelt_core::session::SessionStoreError::Orphaned { found },
        ),
        smelt_store::SessionSchemaStatus::Corrupt { found, reason } => migration_error_output(
            session_id,
            Some(found),
            None,
            started,
            smelt_core::session::SessionStoreError::Corrupt { context: reason },
        ),
    }
}

fn summarize_migrations(outputs: &[SessionMigrationOutput]) -> SessionMigrationSummary {
    let mut summary = SessionMigrationSummary {
        total: outputs.len(),
        ..SessionMigrationSummary::default()
    };
    for output in outputs {
        match output.status {
            SessionMigrationState::Current => summary.current += 1,
            SessionMigrationState::WouldMigrate => summary.would_migrate += 1,
            SessionMigrationState::Migrated => summary.migrated += 1,
            SessionMigrationState::Future => summary.future += 1,
            SessionMigrationState::Unrecognized => summary.unrecognized += 1,
            SessionMigrationState::Orphaned => summary.orphaned += 1,
            SessionMigrationState::Busy => summary.busy += 1,
            SessionMigrationState::Failed => summary.failed += 1,
        }
    }
    summary
}

fn print_migration_output(output: &SessionMigrationOutput) {
    let status = match output.status {
        SessionMigrationState::Current => "current",
        SessionMigrationState::WouldMigrate => "would_migrate",
        SessionMigrationState::Migrated => "migrated",
        SessionMigrationState::Future => "future",
        SessionMigrationState::Unrecognized => "unrecognized",
        SessionMigrationState::Orphaned => "orphaned",
        SessionMigrationState::Busy => "busy",
        SessionMigrationState::Failed => "failed",
    };
    println!("session: {}", output.session_id);
    println!("status: {status}");
    if let Some(version) = output.from_version {
        println!("from_version: {version}");
    }
    if let Some(version) = output.to_version {
        println!("to_version: {version}");
    }
    if let Some(version) = output.supported_version {
        println!("supported_version: {version}");
    }
    println!("duration_ms: {}", output.duration_ms);
    if let Some(kind) = &output.error_kind {
        println!("error_kind: {kind}");
    }
    if let Some(error) = &output.error {
        println!("error: {error}");
    }
}

pub(crate) fn run_migrate(
    reference: Option<&str>,
    all: bool,
    dry_run: bool,
    json: bool,
) -> Result<bool, String> {
    let sessions = resolve_session_ids(reference, all)?
        .into_iter()
        .map(|session_id| inspect_or_migrate_session(session_id, dry_run))
        .collect::<Vec<_>>();
    let summary = summarize_migrations(&sessions);
    let successful = summary.future == 0
        && summary.unrecognized == 0
        && summary.orphaned == 0
        && summary.busy == 0
        && summary.failed == 0;
    let output = SessionMigrationBatchOutput {
        dry_run,
        sessions,
        summary,
    };
    if json {
        print_json(&output)?;
    } else {
        for (index, session) in output.sessions.iter().enumerate() {
            if index != 0 {
                println!();
            }
            print_migration_output(session);
        }
        if !output.sessions.is_empty() {
            println!();
        }
        println!("total: {}", output.summary.total);
        println!("current: {}", output.summary.current);
        println!("would_migrate: {}", output.summary.would_migrate);
        println!("migrated: {}", output.summary.migrated);
        println!("future: {}", output.summary.future);
        println!("unrecognized: {}", output.summary.unrecognized);
        println!("orphaned: {}", output.summary.orphaned);
        println!("busy: {}", output.summary.busy);
        println!("failed: {}", output.summary.failed);
    }
    Ok(successful)
}

fn orphan_quarantine_error_output(
    session_id: String,
    schema_version: Option<i32>,
    started: Instant,
    error: smelt_core::session::SessionStoreError,
) -> SessionOrphanQuarantineOutput {
    let busy = session_error_is_busy(&error);
    SessionOrphanQuarantineOutput {
        session_id,
        status: if busy {
            SessionOrphanQuarantineState::Busy
        } else {
            SessionOrphanQuarantineState::Failed
        },
        schema_version,
        quarantine_path: None,
        duration_ms: operation_duration_ms(started),
        error_kind: Some(if busy { "busy" } else { error.code() }.into()),
        error: Some(actionable_session_error(&error)),
    }
}

fn inspect_or_quarantine_orphan(
    session_id: String,
    dry_run: bool,
) -> SessionOrphanQuarantineOutput {
    let started = Instant::now();
    let found = match smelt_core::session::session_schema_status_result(&session_id) {
        Ok(smelt_store::SessionSchemaStatus::Orphaned { found }) => found,
        Ok(smelt_store::SessionSchemaStatus::Corrupt { reason, .. }) => {
            return orphan_quarantine_error_output(
                session_id,
                None,
                started,
                smelt_core::session::SessionStoreError::Corrupt { context: reason },
            );
        }
        Ok(_) => {
            return SessionOrphanQuarantineOutput {
                session_id,
                status: SessionOrphanQuarantineState::NotOrphaned,
                schema_version: None,
                quarantine_path: None,
                duration_ms: operation_duration_ms(started),
                error_kind: None,
                error: None,
            };
        }
        Err(error) => {
            return orphan_quarantine_error_output(session_id, None, started, error);
        }
    };
    if dry_run {
        return SessionOrphanQuarantineOutput {
            session_id,
            status: SessionOrphanQuarantineState::WouldQuarantine,
            schema_version: Some(found),
            quarantine_path: None,
            duration_ms: operation_duration_ms(started),
            error_kind: None,
            error: None,
        };
    }
    match smelt_core::session::quarantine_orphaned_session_result(&session_id) {
        Ok(Some(quarantine)) => SessionOrphanQuarantineOutput {
            session_id,
            status: SessionOrphanQuarantineState::Quarantined,
            schema_version: Some(quarantine.schema_version),
            quarantine_path: Some(quarantine.quarantine_path.display().to_string()),
            duration_ms: operation_duration_ms(started),
            error_kind: None,
            error: None,
        },
        Ok(None) => SessionOrphanQuarantineOutput {
            session_id,
            status: SessionOrphanQuarantineState::NotOrphaned,
            schema_version: None,
            quarantine_path: None,
            duration_ms: operation_duration_ms(started),
            error_kind: None,
            error: None,
        },
        Err(error) => orphan_quarantine_error_output(session_id, Some(found), started, error),
    }
}

fn summarize_orphan_quarantines(
    outputs: &[SessionOrphanQuarantineOutput],
) -> SessionOrphanQuarantineSummary {
    let mut summary = SessionOrphanQuarantineSummary {
        total: outputs.len(),
        ..SessionOrphanQuarantineSummary::default()
    };
    for output in outputs {
        match output.status {
            SessionOrphanQuarantineState::NotOrphaned => summary.not_orphaned += 1,
            SessionOrphanQuarantineState::WouldQuarantine => summary.would_quarantine += 1,
            SessionOrphanQuarantineState::Quarantined => summary.quarantined += 1,
            SessionOrphanQuarantineState::Busy => summary.busy += 1,
            SessionOrphanQuarantineState::Failed => summary.failed += 1,
        }
    }
    summary
}

fn print_orphan_quarantine_output(output: &SessionOrphanQuarantineOutput) {
    let status = match output.status {
        SessionOrphanQuarantineState::NotOrphaned => "not_orphaned",
        SessionOrphanQuarantineState::WouldQuarantine => "would_quarantine",
        SessionOrphanQuarantineState::Quarantined => "quarantined",
        SessionOrphanQuarantineState::Busy => "busy",
        SessionOrphanQuarantineState::Failed => "failed",
    };
    println!("session: {}", output.session_id);
    println!("status: {status}");
    if let Some(version) = output.schema_version {
        println!("schema_version: {version}");
    }
    if let Some(path) = &output.quarantine_path {
        println!("quarantine_path: {path}");
    }
    println!("duration_ms: {}", output.duration_ms);
    if let Some(kind) = &output.error_kind {
        println!("error_kind: {kind}");
    }
    if let Some(error) = &output.error {
        println!("error: {error}");
    }
}

pub(crate) fn run_quarantine_orphans(
    reference: Option<&str>,
    all: bool,
    dry_run: bool,
    json: bool,
) -> Result<bool, String> {
    let all_sessions = resolve_session_ids(reference, all)?
        .into_iter()
        .map(|session_id| inspect_or_quarantine_orphan(session_id, dry_run))
        .collect::<Vec<_>>();
    let summary = summarize_orphan_quarantines(&all_sessions);
    let successful = summary.busy == 0 && summary.failed == 0;
    let sessions = if all {
        all_sessions
            .into_iter()
            .filter(|session| session.status != SessionOrphanQuarantineState::NotOrphaned)
            .collect()
    } else {
        all_sessions
    };
    let output = SessionOrphanQuarantineBatchOutput {
        dry_run,
        sessions,
        summary,
    };
    if json {
        print_json(&output)?;
    } else {
        for (index, session) in output.sessions.iter().enumerate() {
            if index != 0 {
                println!();
            }
            print_orphan_quarantine_output(session);
        }
        if !output.sessions.is_empty() {
            println!();
        }
        println!("total: {}", output.summary.total);
        println!("not_orphaned: {}", output.summary.not_orphaned);
        println!("would_quarantine: {}", output.summary.would_quarantine);
        println!("quarantined: {}", output.summary.quarantined);
        println!("busy: {}", output.summary.busy);
        println!("failed: {}", output.summary.failed);
    }
    Ok(successful)
}
