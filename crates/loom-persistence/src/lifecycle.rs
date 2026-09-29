use super::*;

impl FilePersistence {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self> {
        Self::open(path)
    }

    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if path.as_os_str().is_empty() {
            return Err(LoomError::invalid_request(
                "persistence path must not be empty",
            ));
        }

        Ok(Self {
            path,
            connection: Arc::new(Mutex::new(None)),
            owner_lock: Arc::new(Mutex::new(None)),
            in_memory: false,
        })
    }

    /// Opens a private, in-memory SQLite store that shares the file-backed
    /// schema, repository, and migration code path. Used for tests, previews,
    /// and ephemeral local state.
    pub fn in_memory() -> Self {
        Self {
            path: PathBuf::from(":memory:"),
            connection: Arc::new(Mutex::new(None)),
            owner_lock: Arc::new(Mutex::new(None)),
            in_memory: true,
        }
    }

    /// Opens the store as the exclusive writer for a backend process. The lock
    /// is advisory and remains held by this handle and its clones until they
    /// are all dropped. Read-only/diagnostic handles may still use `open`.
    pub fn open_exclusive_writer(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if path.as_os_str().is_empty() {
            return Err(LoomError::invalid_request(
                "persistence path must not be empty",
            ));
        }
        let absolute_path = if path.exists() {
            path.canonicalize().map_err(|error| {
                persistence_error(format!("could not resolve persistence path: {error}"), true)
            })?
        } else {
            if let Some(parent) = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                fs::create_dir_all(parent).map_err(|error| {
                    persistence_error(
                        format!(
                            "could not create persistence directory '{}': {error}",
                            parent.display()
                        ),
                        true,
                    )
                })?;
            }
            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            let parent = parent.canonicalize().map_err(|error| {
                persistence_error(
                    format!("could not resolve persistence directory: {error}"),
                    true,
                )
            })?;
            parent.join(path.file_name().ok_or_else(|| {
                LoomError::invalid_request("persistence path must name a database file")
            })?)
        };
        let mut lock_path = absolute_path.as_os_str().to_os_string();
        lock_path.push(".loom-owner.lock");
        let lock_file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(PathBuf::from(lock_path))
            .map_err(|error| {
                persistence_error(
                    format!("could not open persistence owner lock: {error}"),
                    true,
                )
            })?;
        match lock_file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(LoomError::conflict(format!(
                    "persistence database '{}' is already owned by another backend",
                    absolute_path.display()
                )));
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(persistence_error(
                    format!("could not acquire persistence owner lock: {error}"),
                    true,
                ));
            }
        }
        let persistence = Self::open(path)?;
        *persistence.owner_lock.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Persistence,
                "persistence owner lock state was poisoned",
                true,
            )
        })? = Some(lock_file);
        Ok(persistence)
    }

    /// Releases exclusive writer ownership after the backend has stopped and
    /// joined every worker. Cloned store handles share this ownership slot.
    pub fn release_exclusive_writer(&self) -> Result<()> {
        let lock_file = self
            .owner_lock
            .lock()
            .map_err(|_| {
                LoomError::new(
                    ErrorCode::Persistence,
                    "persistence owner lock state was poisoned",
                    true,
                )
            })?
            .take();
        if let Some(lock_file) = lock_file {
            lock_file.unlock().map_err(|error| {
                persistence_error(
                    format!("could not release persistence owner lock: {error}"),
                    true,
                )
            })?;
        }
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn exists(&self) -> bool {
        self.path.is_file()
    }

    /// Inspects the schema version of an existing database without modifying
    /// it or taking the writer lock. Tries a read-only SQLite connection first
    /// (which sees a WAL), then falls back to reading the file header directly
    /// so an unsupported database is never touched.
    pub fn schema_status(path: &Path) -> Result<SchemaStatus> {
        if !path.is_file() {
            return Ok(SchemaStatus::Absent);
        }
        if let Some(status) = schema_status_via_read_only_sqlite(path) {
            return Ok(status);
        }
        schema_status_from_header(path)
    }

    /// Deletes the database and its SQLite sidecar files so a new baseline can
    /// be created. Refuses while another backend holds the writer lock, so a
    /// wipe cannot corrupt a running instance.
    pub fn reset_database(path: &Path) -> Result<()> {
        if path.as_os_str().is_empty() {
            return Err(LoomError::invalid_request(
                "persistence path must not be empty",
            ));
        }
        if !path.exists() {
            return Ok(());
        }
        let mut lock_path = path.as_os_str().to_os_string();
        lock_path.push(".loom-owner.lock");
        let lock_file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(PathBuf::from(lock_path))
            .map_err(|error| {
                persistence_error(
                    format!("could not open persistence owner lock: {error}"),
                    true,
                )
            })?;
        match lock_file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(LoomError::conflict(format!(
                    "cannot wipe persistence database '{}' while another backend owns it",
                    path.display()
                )));
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(persistence_error(
                    format!("could not acquire persistence owner lock: {error}"),
                    true,
                ));
            }
        }
        for candidate in database_files(path) {
            match fs::remove_file(&candidate) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(persistence_error(
                        format!(
                            "could not remove persistence file '{}': {error}",
                            candidate.display()
                        ),
                        true,
                    ));
                }
            }
        }
        lock_file.unlock().map_err(|error| {
            persistence_error(
                format!("could not release persistence owner lock: {error}"),
                true,
            )
        })?;
        Ok(())
    }

    /// Persists the typed session catalog without rewriting unrelated state.
    pub fn save_state_with_sessions(&self, sessions: &SessionManagerState) -> Result<()> {
        self.save_state(DurableStateWrite {
            sessions,
            workspaces: None,
            settings: None,
            workspace_configs: None,
            providers: None,
            usage: None,
            idempotency: None,
            run_summaries: None,
            run_runtime_configs: None,
            run_context_checkpoints: None,
            run_plans: None,
            run_messages: None,
            run_activities: None,
            filesystem_records: None,
            feed: None,
        })
    }

    /// Persists the typed catalogs and reconnect feed atomically.
    pub fn save_state_with_catalogs_and_feed(
        &self,
        sessions: &SessionManagerState,
        workspaces: &WorkspaceManagerState,
        feed: Option<&DurableFeedState>,
    ) -> Result<()> {
        self.save_state(DurableStateWrite {
            sessions,
            workspaces: Some(workspaces),
            settings: None,
            workspace_configs: None,
            providers: None,
            usage: None,
            idempotency: None,
            run_summaries: None,
            run_runtime_configs: None,
            run_context_checkpoints: None,
            run_plans: None,
            run_messages: None,
            run_activities: None,
            filesystem_records: None,
            feed,
        })
    }

    /// Persists the typed session catalog and reconnect feed atomically.
    pub fn save_state_with_sessions_and_feed(
        &self,
        sessions: &SessionManagerState,
        feed: Option<&DurableFeedState>,
    ) -> Result<()> {
        self.save_state(DurableStateWrite {
            sessions,
            workspaces: None,
            settings: None,
            workspace_configs: None,
            providers: None,
            usage: None,
            idempotency: None,
            run_summaries: None,
            run_runtime_configs: None,
            run_context_checkpoints: None,
            run_plans: None,
            run_messages: None,
            run_activities: None,
            filesystem_records: None,
            feed,
        })
    }

    pub fn save_state(&self, write: DurableStateWrite<'_>) -> Result<()> {
        let connection = self.connection_for_write()?;
        let transaction = connection.unchecked_transaction().map_err(|error| {
            persistence_error(
                format!("could not begin persistence transaction: {error}"),
                true,
            )
        })?;
        save_session_rows(&transaction, write.sessions)?;
        if let Some(workspaces) = write.workspaces {
            save_workspace_rows(&transaction, workspaces)?;
        }
        if let Some(settings) = write.settings {
            save_session_settings_rows(&transaction, settings)?;
        }
        if let Some(workspace_configs) = write.workspace_configs {
            save_workspace_config_rows(&transaction, workspace_configs)?;
        }
        if let Some(providers) = write.providers {
            save_provider_config_rows(&transaction, &providers.configs)?;
            save_provider_health_rows(&transaction, &providers.health)?;
        }
        if let Some(usage) = write.usage {
            save_usage_totals(&transaction, usage)?;
        }
        if let Some(idempotency) = write.idempotency {
            save_idempotency_rows(&transaction, idempotency)?;
        }
        if let Some(run_summaries) = write.run_summaries {
            save_run_summary_rows(&transaction, run_summaries)?;
            save_run_attempt_rows(&transaction, run_summaries)?;
            save_run_execution_state_rows(&transaction, run_summaries)?;
        }
        if let Some(runtime_configs) = write.run_runtime_configs {
            save_run_runtime_config_rows(&transaction, runtime_configs)?;
        }
        if let Some(context_checkpoints) = write.run_context_checkpoints {
            save_run_context_checkpoint_rows(&transaction, context_checkpoints)?;
        }
        if let Some(run_plans) = write.run_plans {
            save_run_plan_rows(&transaction, run_plans, write.run_summaries)?;
        }
        if let Some(run_activities) = write.run_activities {
            save_run_activity_rows(&transaction, run_activities)?;
            save_run_tool_rows(&transaction, run_activities, write.run_summaries)?;
        }
        if let Some(run_messages) = write.run_messages {
            save_run_message_rows(&transaction, run_messages, None, true)?;
        }
        if let Some(filesystems) = write.filesystem_records {
            save_filesystem_records(&transaction, filesystems)?;
        }
        if let Some(feed) = write.feed {
            save_feed_rows(&transaction, feed)?;
        }
        transaction.commit().map_err(|error| {
            persistence_error(
                format!("could not commit persistence transaction: {error}"),
                true,
            )
        })
    }

    /// Atomically persists startup recovery updates without rewriting unrelated catalogs.
    pub fn save_recovery_updates(
        &self,
        summaries: &BTreeMap<RunId, DurableRunSummary>,
        feed: &DurableFeedState,
    ) -> Result<()> {
        let connection = self.connection_for_write()?;
        let transaction = connection.unchecked_transaction().map_err(|error| {
            persistence_error(
                format!("could not begin recovery update transaction: {error}"),
                true,
            )
        })?;
        save_run_summary_rows(&transaction, summaries)?;
        save_run_attempt_rows(&transaction, summaries)?;
        save_run_execution_state_rows(&transaction, summaries)?;
        save_feed_rows(&transaction, feed)?;
        transaction.commit().map_err(|error| {
            persistence_error(
                format!("could not commit recovery update transaction: {error}"),
                true,
            )
        })
    }

    /// Collects a bounded batch of content objects and blobs queued by reference changes.
    /// Ordinary writes process smaller batches; callers can repeat this method to drain a
    /// backlog without scanning all stored content on every transaction.
    pub fn collect_garbage(&self, max_candidates: usize) -> Result<()> {
        if !(1..=MAX_MANUAL_CONTENT_GC_CANDIDATES).contains(&max_candidates) {
            return Err(LoomError::invalid_request(format!(
                "content garbage-collection batch must be between 1 and {MAX_MANUAL_CONTENT_GC_CANDIDATES}"
            )));
        }
        let connection = self.connection_for_write()?;
        let transaction = connection.unchecked_transaction().map_err(|error| {
            persistence_error(
                format!("could not begin content garbage-collection transaction: {error}"),
                true,
            )
        })?;
        collect_unused_content(&transaction, max_candidates)?;
        transaction.commit().map_err(|error| {
            persistence_error(
                format!("could not commit content garbage collection: {error}"),
                true,
            )
        })
    }

    pub(crate) fn connection(&self) -> Result<CachedConnection<'_>> {
        self.cached_connection(false)
    }

    pub(crate) fn connection_for_write(&self) -> Result<CachedConnection<'_>> {
        if let Some(parent) = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(|error| {
                persistence_error(
                    format!(
                        "could not create persistence directory '{}': {error}",
                        parent.display()
                    ),
                    true,
                )
            })?;
        }
        self.cached_connection(true)
    }

    pub(crate) fn cached_connection(&self, create: bool) -> Result<CachedConnection<'_>> {
        let mut cached = self.connection.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Persistence,
                "persistence connection lock was poisoned",
                true,
            )
        })?;
        if cached.is_none() {
            if !create && !self.in_memory && !self.path.is_file() {
                return Err(persistence_error(
                    "persistence database does not exist".to_owned(),
                    false,
                ));
            }
            let connection = Connection::open(&self.path).map_err(|error| {
                persistence_error(
                    format!("could not open persistence database: {error}"),
                    true,
                )
            })?;
            connection
                .busy_timeout(std::time::Duration::from_secs(5))
                .map_err(|error| {
                    persistence_error(format!("could not configure persistence: {error}"), true)
                })?;
            connection
                .execute_batch("PRAGMA foreign_keys = ON; PRAGMA synchronous = FULL;")
                .map_err(|error| {
                    persistence_error(format!("could not configure persistence: {error}"), true)
                })?;
            initialize_schema(&connection)?;
            *cached = Some(connection);
        }
        Ok(CachedConnection(cached))
    }
}
