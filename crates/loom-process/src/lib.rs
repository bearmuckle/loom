use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    io::{BufReader, Read, Write},
    path::{Component, Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use loom_core::{ErrorCode, EventSequence, LoomError, Result, TaskId, TerminalId, Timestamp};
pub use loom_protocol::{
    TaskArtifact, TaskEvent, TaskEventRecord, TaskEvidenceLink, TaskKind, TaskSnapshot, TaskSpec,
    TaskStatus, TerminalEvent, TerminalEventRecord, TerminalSnapshot, TerminalStatus,
    TerminalStream,
};

const DEFAULT_EVENT_LIMIT: usize = 1024;
const DEFAULT_TASK_OUTPUT_LIMIT: usize = 64 * 1024;
const MAX_OUTPUT_CHUNK: usize = 8 * 1024;

#[derive(Debug)]
struct TerminalState {
    snapshot: TerminalSnapshot,
    events: VecDeque<TerminalEventRecord>,
    next_sequence: EventSequence,
}

#[derive(Debug)]
struct TerminalHandle {
    state: Mutex<TerminalState>,
    child: Mutex<Option<Child>>,
    stdin: Mutex<Option<ChildStdin>>,
    cancel_requested: AtomicBool,
}

#[derive(Clone, Debug)]
pub struct TerminalManager {
    sessions: Arc<Mutex<BTreeMap<TerminalId, Arc<TerminalHandle>>>>,
    event_limit: usize,
}

impl Default for TerminalManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalManager {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(BTreeMap::new())),
            event_limit: DEFAULT_EVENT_LIMIT,
        }
    }

    pub fn open(
        &self,
        command: impl Into<String>,
        args: Vec<String>,
        cwd: impl Into<PathBuf>,
    ) -> Result<TerminalSnapshot> {
        let command = command.into();
        if command.trim().is_empty() {
            return Err(LoomError::invalid_request(
                "terminal command must not be empty",
            ));
        }
        let cwd = cwd.into();
        if !cwd.is_dir() {
            return Err(LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("terminal cwd '{}' is not a directory", cwd.display()),
                false,
            ));
        }
        let mut child = Command::new(&command)
            .args(&args)
            .current_dir(&cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| {
                LoomError::new(
                    ErrorCode::ToolExecution,
                    format!("could not start terminal '{command}': {error}"),
                    false,
                )
            })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            LoomError::new(
                ErrorCode::Internal,
                "terminal stdout pipe was not available",
                false,
            )
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            LoomError::new(
                ErrorCode::Internal,
                "terminal stderr pipe was not available",
                false,
            )
        })?;
        let stdin = child.stdin.take();
        let id = TerminalId::new();
        let now = Timestamp::now();
        let snapshot = TerminalSnapshot {
            id,
            command,
            args,
            cwd: cwd.display().to_string(),
            status: TerminalStatus::Starting,
            started_at: now,
            updated_at: now,
            exited_at: None,
            exit_code: None,
            rows: 24,
            columns: 80,
        };
        let handle = Arc::new(TerminalHandle {
            state: Mutex::new(TerminalState {
                snapshot: snapshot.clone(),
                events: VecDeque::new(),
                next_sequence: EventSequence::default(),
            }),
            child: Mutex::new(Some(child)),
            stdin: Mutex::new(stdin),
            cancel_requested: AtomicBool::new(false),
        });
        self.sessions
            .lock()
            .map_err(|_| internal_lock_error("terminal manager"))?
            .insert(id, Arc::clone(&handle));
        append_terminal_event(
            &handle,
            self.event_limit,
            TerminalEvent::StateChanged {
                status: TerminalStatus::Running,
            },
            |snapshot| snapshot.status = TerminalStatus::Running,
        )?;

        spawn_reader(
            Arc::clone(&handle),
            self.event_limit,
            stdout,
            TerminalStream::Stdout,
        );
        spawn_reader(
            Arc::clone(&handle),
            self.event_limit,
            stderr,
            TerminalStream::Stderr,
        );
        let event_limit = self.event_limit;
        thread::spawn({
            let handle = Arc::clone(&handle);
            move || wait_for_terminal(handle, event_limit)
        });
        snapshot_after(&handle)
    }

    pub fn get(&self, id: TerminalId) -> Result<TerminalSnapshot> {
        let handle = self.handle(id)?;
        snapshot_after(&handle)
    }

    pub fn write_input(&self, id: TerminalId, input: &str) -> Result<()> {
        let handle = self.handle(id)?;
        let mut stdin = handle
            .stdin
            .lock()
            .map_err(|_| internal_lock_error("terminal stdin"))?;
        let stdin = stdin.as_mut().ok_or_else(|| {
            LoomError::new(
                ErrorCode::InvalidState,
                "terminal input is no longer available",
                false,
            )
        })?;
        stdin.write_all(input.as_bytes()).map_err(|error| {
            LoomError::new(
                ErrorCode::ToolExecution,
                format!("could not write terminal input: {error}"),
                false,
            )
        })?;
        stdin.flush().map_err(|error| {
            LoomError::new(
                ErrorCode::ToolExecution,
                format!("could not flush terminal input: {error}"),
                false,
            )
        })
    }

    pub fn resize(&self, id: TerminalId, rows: u16, columns: u16) -> Result<TerminalSnapshot> {
        if rows == 0 || columns == 0 {
            return Err(LoomError::invalid_request(
                "terminal rows and columns must be greater than zero",
            ));
        }
        let handle = self.handle(id)?;
        append_terminal_event(
            &handle,
            self.event_limit,
            TerminalEvent::Resized { rows, columns },
            |snapshot| {
                snapshot.rows = rows;
                snapshot.columns = columns;
            },
        )?;
        snapshot_after(&handle)
    }

    pub fn cancel(&self, id: TerminalId) -> Result<TerminalSnapshot> {
        let handle = self.handle(id)?;
        let snapshot = snapshot_after(&handle)?;
        if matches!(
            snapshot.status,
            TerminalStatus::Exited | TerminalStatus::Cancelled | TerminalStatus::Failed
        ) {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "terminal has already exited",
                false,
            ));
        }
        handle.cancel_requested.store(true, Ordering::SeqCst);
        let mut child = handle
            .child
            .lock()
            .map_err(|_| internal_lock_error("terminal child"))?;
        if let Some(child) = child.as_mut() {
            child.kill().map_err(|error| {
                LoomError::new(
                    ErrorCode::ToolExecution,
                    format!("could not cancel terminal: {error}"),
                    false,
                )
            })?;
        }
        snapshot_after(&handle)
    }

    pub fn events_since(
        &self,
        id: TerminalId,
        after: Option<EventSequence>,
    ) -> Result<Vec<TerminalEventRecord>> {
        let handle = self.handle(id)?;
        Ok(handle
            .state
            .lock()
            .map_err(|_| internal_lock_error("terminal state"))?
            .events
            .iter()
            .filter(|event| after.is_none_or(|sequence| event.sequence > sequence))
            .cloned()
            .collect())
    }

    fn handle(&self, id: TerminalId) -> Result<Arc<TerminalHandle>> {
        self.sessions
            .lock()
            .map_err(|_| internal_lock_error("terminal manager"))?
            .get(&id)
            .cloned()
            .ok_or_else(|| LoomError::not_found("terminal", id))
    }
}

fn spawn_reader<R: Read + Send + 'static>(
    handle: Arc<TerminalHandle>,
    event_limit: usize,
    reader: R,
    stream: TerminalStream,
) {
    thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut buffer = vec![0_u8; MAX_OUTPUT_CHUNK];
        loop {
            let read = match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) => {
                    let _ = append_terminal_event(
                        &handle,
                        event_limit,
                        TerminalEvent::Output {
                            stream,
                            chunk: format!("[terminal output error: {error}]"),
                        },
                        |_| {},
                    );
                    break;
                }
            };
            let chunk = String::from_utf8_lossy(&buffer[..read]).into_owned();
            let _ = append_terminal_event(
                &handle,
                event_limit,
                TerminalEvent::Output { stream, chunk },
                |_| {},
            );
        }
    });
}

fn wait_for_terminal(handle: Arc<TerminalHandle>, event_limit: usize) {
    let status = loop {
        let mut child = match handle.child.lock() {
            Ok(child) => child,
            Err(_) => return,
        };
        let result = child.as_mut().map(Child::try_wait);
        drop(child);
        match result {
            Some(Ok(Some(status))) => break Some(status),
            Some(Ok(None)) => {}
            Some(Err(_)) | None => break None,
        }
        thread::sleep(Duration::from_millis(10));
    };
    let cancelled = handle.cancel_requested.load(Ordering::SeqCst);
    let exit_code = status.and_then(|status| status.code());
    let terminal_status = if cancelled {
        TerminalStatus::Cancelled
    } else if status.is_some_and(|status| status.success()) {
        TerminalStatus::Exited
    } else {
        TerminalStatus::Failed
    };
    let _ = append_terminal_event(
        &handle,
        event_limit,
        TerminalEvent::Exited {
            status: terminal_status,
            exit_code,
        },
        |snapshot| {
            snapshot.status = terminal_status;
            snapshot.exit_code = exit_code;
            snapshot.exited_at = Some(Timestamp::now());
        },
    );
}

fn append_terminal_event(
    handle: &TerminalHandle,
    event_limit: usize,
    event: TerminalEvent,
    update: impl FnOnce(&mut TerminalSnapshot),
) -> Result<()> {
    let mut state = handle
        .state
        .lock()
        .map_err(|_| internal_lock_error("terminal state"))?;
    update(&mut state.snapshot);
    state.snapshot.updated_at = Timestamp::now();
    state.next_sequence = state.next_sequence.next();
    let record = TerminalEventRecord {
        sequence: state.next_sequence,
        terminal_id: state.snapshot.id,
        event,
    };
    state.events.push_back(record);
    while state.events.len() > event_limit {
        state.events.pop_front();
    }
    Ok(())
}

fn snapshot_after(handle: &TerminalHandle) -> Result<TerminalSnapshot> {
    Ok(handle
        .state
        .lock()
        .map_err(|_| internal_lock_error("terminal state"))?
        .snapshot
        .clone())
}

#[derive(Debug)]
struct TaskState {
    snapshot: TaskSnapshot,
    events: VecDeque<TaskEventRecord>,
    next_sequence: EventSequence,
    output_limit: usize,
}

#[derive(Debug)]
struct TaskHandle {
    id: TaskId,
    state: Mutex<TaskState>,
    child: Mutex<Option<Child>>,
    cancel_requested: AtomicBool,
    artifact_paths: Vec<String>,
    root: PathBuf,
}

#[derive(Clone, Debug)]
pub struct TaskSupervisor {
    root: PathBuf,
    tasks: Arc<Mutex<BTreeMap<TaskId, Arc<TaskHandle>>>>,
    event_limit: usize,
}

impl TaskSupervisor {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        if !root.is_dir() {
            return Err(LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("task root '{}' is not a directory", root.display()),
                false,
            ));
        }
        Ok(Self {
            root: fs::canonicalize(root).map_err(|error| {
                LoomError::new(
                    ErrorCode::WorkspaceAccessDenied,
                    format!("could not resolve task root: {error}"),
                    false,
                )
            })?,
            tasks: Arc::new(Mutex::new(BTreeMap::new())),
            event_limit: DEFAULT_EVENT_LIMIT,
        })
    }

    pub fn start(&self, spec: TaskSpec) -> Result<TaskSnapshot> {
        if spec.command.trim().is_empty() {
            return Err(LoomError::invalid_request("task command must not be empty"));
        }
        let cwd = self.resolve_relative(spec.cwd.as_deref().unwrap_or("."))?;
        for artifact_path in &spec.artifact_paths {
            validate_relative_path(artifact_path)?;
        }
        let id = TaskId::new();
        let now = Timestamp::now();
        let output_limit = spec
            .output_limit_bytes
            .unwrap_or(DEFAULT_TASK_OUTPUT_LIMIT)
            .max(1);
        let snapshot = TaskSnapshot {
            id,
            kind: spec.kind,
            label: if spec.label.trim().is_empty() {
                spec.command.clone()
            } else {
                spec.label.clone()
            },
            command: spec.command.clone(),
            args: spec.args.clone(),
            cwd: cwd.display().to_string(),
            status: TaskStatus::Queued,
            started_at: now,
            updated_at: now,
            completed_at: None,
            exit_code: None,
            output: String::new(),
            output_truncated: false,
            artifacts: Vec::new(),
            evidence: Vec::new(),
        };
        let mut child = Command::new(&spec.command)
            .args(&spec.args)
            .current_dir(&cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .spawn()
            .map_err(|error| {
                LoomError::new(
                    ErrorCode::ToolExecution,
                    format!("could not start task '{}': {error}", spec.command),
                    false,
                )
            })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            LoomError::new(
                ErrorCode::Internal,
                "task stdout pipe was not available",
                false,
            )
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            LoomError::new(
                ErrorCode::Internal,
                "task stderr pipe was not available",
                false,
            )
        })?;
        let handle = Arc::new(TaskHandle {
            id,
            state: Mutex::new(TaskState {
                snapshot: snapshot.clone(),
                events: VecDeque::new(),
                next_sequence: EventSequence::default(),
                output_limit,
            }),
            child: Mutex::new(Some(child)),
            cancel_requested: AtomicBool::new(false),
            artifact_paths: spec.artifact_paths,
            root: self.root.clone(),
        });
        self.tasks
            .lock()
            .map_err(|_| internal_lock_error("task supervisor"))?
            .insert(id, Arc::clone(&handle));
        append_task_event(
            &handle,
            self.event_limit,
            TaskEvent::StateChanged {
                status: TaskStatus::Running,
            },
            |snapshot| snapshot.status = TaskStatus::Running,
        )?;
        spawn_task_reader(Arc::clone(&handle), self.event_limit, stdout);
        spawn_task_reader(Arc::clone(&handle), self.event_limit, stderr);
        let event_limit = self.event_limit;
        thread::spawn({
            let handle = Arc::clone(&handle);
            move || wait_for_task(handle, event_limit)
        });
        snapshot_after_task(&handle)
    }

    pub fn get(&self, id: TaskId) -> Result<TaskSnapshot> {
        let handle = self.handle(id)?;
        snapshot_after_task(&handle)
    }

    pub fn list(&self) -> Result<Vec<TaskSnapshot>> {
        let handles = self
            .tasks
            .lock()
            .map_err(|_| internal_lock_error("task supervisor"))?
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut snapshots = handles
            .iter()
            .map(|handle| snapshot_after_task(handle))
            .collect::<Result<Vec<_>>>()?;
        snapshots.sort_by_key(|snapshot| std::cmp::Reverse(snapshot.updated_at));
        Ok(snapshots)
    }

    pub fn cancel(&self, id: TaskId) -> Result<TaskSnapshot> {
        let handle = self.handle(id)?;
        let snapshot = snapshot_after_task(&handle)?;
        if matches!(
            snapshot.status,
            TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
        ) {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "task has already finished",
                false,
            ));
        }
        handle.cancel_requested.store(true, Ordering::SeqCst);
        let mut child = handle
            .child
            .lock()
            .map_err(|_| internal_lock_error("task child"))?;
        if let Some(child) = child.as_mut() {
            child.kill().map_err(|error| {
                LoomError::new(
                    ErrorCode::ToolExecution,
                    format!("could not cancel task: {error}"),
                    false,
                )
            })?;
        }
        snapshot_after_task(&handle)
    }

    pub fn events_since(
        &self,
        id: TaskId,
        after: Option<EventSequence>,
    ) -> Result<Vec<TaskEventRecord>> {
        let handle = self.handle(id)?;
        Ok(handle
            .state
            .lock()
            .map_err(|_| internal_lock_error("task state"))?
            .events
            .iter()
            .filter(|event| after.is_none_or(|sequence| event.sequence > sequence))
            .cloned()
            .collect())
    }

    fn handle(&self, id: TaskId) -> Result<Arc<TaskHandle>> {
        self.tasks
            .lock()
            .map_err(|_| internal_lock_error("task supervisor"))?
            .get(&id)
            .cloned()
            .ok_or_else(|| LoomError::not_found("task", id))
    }

    fn resolve_relative(&self, relative: &str) -> Result<PathBuf> {
        validate_relative_path(relative)?;
        let path = Path::new(relative);
        let resolved = fs::canonicalize(self.root.join(path)).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not resolve task path '{relative}': {error}"),
                false,
            )
        })?;
        if !resolved.starts_with(&self.root) {
            return Err(LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("task path '{relative}' must stay inside the workspace root"),
                false,
            ));
        }

        Ok(resolved)
    }
}

fn validate_relative_path(relative: &str) -> Result<()> {
    let path = Path::new(relative);
    if path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
    {
        return Err(LoomError::new(
            ErrorCode::WorkspaceAccessDenied,
            format!("task path '{relative}' must stay inside the workspace root"),
            false,
        ));
    }
    Ok(())
}

fn spawn_task_reader<R: Read + Send + 'static>(
    handle: Arc<TaskHandle>,
    event_limit: usize,
    reader: R,
) {
    thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut buffer = vec![0_u8; MAX_OUTPUT_CHUNK];
        loop {
            let read = match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) => {
                    let _ = append_task_output(
                        &handle,
                        event_limit,
                        format!("[task output error: {error}]"),
                    );
                    break;
                }
            };
            let chunk = String::from_utf8_lossy(&buffer[..read]).into_owned();
            let _ = append_task_output(&handle, event_limit, chunk);
        }
    });
}

fn append_task_output(handle: &TaskHandle, event_limit: usize, chunk: String) -> Result<()> {
    let mut state = handle
        .state
        .lock()
        .map_err(|_| internal_lock_error("task state"))?;
    let remaining = state
        .output_limit
        .saturating_sub(state.snapshot.output.len());
    if remaining == 0 {
        state.snapshot.output_truncated = true;
        return Ok(());
    }
    let output = if chunk.len() > remaining {
        state.snapshot.output_truncated = true;
        let mut cutoff = remaining;
        while cutoff > 0 && !chunk.is_char_boundary(cutoff) {
            cutoff -= 1;
        }
        chunk[..cutoff].to_owned()
    } else {
        chunk
    };
    state.snapshot.output.push_str(&output);
    state.snapshot.updated_at = Timestamp::now();
    state.next_sequence = state.next_sequence.next();
    let sequence = state.next_sequence;
    let task_id = state.snapshot.id;
    state.events.push_back(TaskEventRecord {
        sequence,
        task_id,
        event: TaskEvent::OutputChunk { chunk: output },
    });
    while state.events.len() > event_limit {
        state.events.pop_front();
    }
    Ok(())
}

fn wait_for_task(handle: Arc<TaskHandle>, event_limit: usize) {
    let status = loop {
        let mut child = match handle.child.lock() {
            Ok(child) => child,
            Err(_) => return,
        };
        let result = child.as_mut().map(Child::try_wait);
        drop(child);
        match result {
            Some(Ok(Some(status))) => break Some(status),
            Some(Ok(None)) => {}
            Some(Err(_)) | None => break None,
        }
        thread::sleep(Duration::from_millis(10));
    };
    let cancelled = handle.cancel_requested.load(Ordering::SeqCst);
    let exit_code = status.and_then(|status| status.code());
    let task_status = if cancelled {
        TaskStatus::Cancelled
    } else if status.is_some_and(|status| status.success()) {
        TaskStatus::Completed
    } else {
        TaskStatus::Failed
    };
    let artifacts = handle
        .artifact_paths
        .iter()
        .map(|path| {
            let resolved = handle.root.join(path);
            let metadata = fs::canonicalize(&resolved)
                .ok()
                .filter(|resolved| resolved.starts_with(&handle.root))
                .and_then(|resolved| fs::metadata(resolved).ok());
            TaskArtifact {
                path: path.clone(),
                exists: metadata.is_some(),
                size: metadata.map_or(0, |metadata| metadata.len()),
            }
        })
        .collect::<Vec<_>>();
    let evidence = artifacts
        .iter()
        .map(|artifact| TaskEvidenceLink {
            label: format!("{} evidence", artifact.path),
            uri: format!("loom://task/{}/artifact/{}", handle.id, artifact.path),
            artifact_path: artifact.path.clone(),
            exists: artifact.exists,
        })
        .collect::<Vec<_>>();
    let _ = append_task_event(
        &handle,
        event_limit,
        TaskEvent::Completed {
            status: task_status,
            exit_code,
            artifacts: artifacts.clone(),
        },
        |snapshot| {
            snapshot.status = task_status;
            snapshot.exit_code = exit_code;
            snapshot.completed_at = Some(Timestamp::now());
            snapshot.artifacts = artifacts;
            snapshot.evidence = evidence;
        },
    );
}

fn append_task_event(
    handle: &TaskHandle,
    event_limit: usize,
    event: TaskEvent,
    update: impl FnOnce(&mut TaskSnapshot),
) -> Result<()> {
    let mut state = handle
        .state
        .lock()
        .map_err(|_| internal_lock_error("task state"))?;
    update(&mut state.snapshot);
    state.snapshot.updated_at = Timestamp::now();
    state.next_sequence = state.next_sequence.next();
    let sequence = state.next_sequence;
    let task_id = state.snapshot.id;
    state.events.push_back(TaskEventRecord {
        sequence,
        task_id,
        event,
    });
    while state.events.len() > event_limit {
        state.events.pop_front();
    }
    Ok(())
}

fn snapshot_after_task(handle: &TaskHandle) -> Result<TaskSnapshot> {
    Ok(handle
        .state
        .lock()
        .map_err(|_| internal_lock_error("task state"))?
        .snapshot
        .clone())
}

fn internal_lock_error(resource: &str) -> LoomError {
    LoomError::new(
        ErrorCode::Internal,
        format!("{resource} lock was poisoned"),
        true,
    )
}

#[cfg(test)]
mod tests {
    use std::{fs, thread, time::Duration};

    use loom_core::ProjectId;

    use super::*;

    fn command(script: &str) -> (String, Vec<String>) {
        if cfg!(windows) {
            ("cmd".to_owned(), vec!["/C".to_owned(), script.to_owned()])
        } else {
            ("sh".to_owned(), vec!["-c".to_owned(), script.to_owned()])
        }
    }

    fn wait_terminal(manager: &TerminalManager, id: TerminalId) -> TerminalSnapshot {
        for _ in 0..100 {
            let snapshot = manager.get(id).unwrap();
            if matches!(
                snapshot.status,
                TerminalStatus::Exited | TerminalStatus::Failed | TerminalStatus::Cancelled
            ) {
                return snapshot;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("terminal did not finish");
    }

    #[test]
    fn terminal_streams_output_and_records_resize_and_exit() {
        let root = std::env::temp_dir().join(format!("loom-process-{}", ProjectId::new()));
        fs::create_dir_all(&root).unwrap();
        let (program, args) = command("printf hello");
        let manager = TerminalManager::new();
        let terminal = manager.open(program, args, &root).unwrap();
        manager.resize(terminal.id, 40, 120).unwrap();
        let finished = wait_terminal(&manager, terminal.id);
        assert_eq!(finished.status, TerminalStatus::Exited);
        let events = manager.events_since(terminal.id, None).unwrap();
        assert!(events.iter().any(|event| {
            matches!(
                event.event,
                TerminalEvent::Output { ref chunk, .. } if chunk.contains("hello")
            )
        }));
        assert!(events.iter().any(|event| {
            matches!(
                event.event,
                TerminalEvent::Resized {
                    rows: 40,
                    columns: 120
                }
            )
        }));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn terminal_input_and_cancellation_are_explicit() {
        let root = std::env::temp_dir().join(format!("loom-process-{}", ProjectId::new()));
        fs::create_dir_all(&root).unwrap();
        let (program, args) = command(if cfg!(windows) {
            "set /p value & echo %value%"
        } else {
            "read value; printf \"$value\""
        });
        let manager = TerminalManager::new();
        let terminal = manager.open(program, args, &root).unwrap();
        manager.write_input(terminal.id, "input\n").unwrap();
        let finished = wait_terminal(&manager, terminal.id);
        assert_eq!(finished.status, TerminalStatus::Exited);
        let (program, args) = command(if cfg!(windows) {
            "ping -n 3 127.0.0.1 > nul"
        } else {
            "sleep 2"
        });
        let cancellable = manager.open(program, args, &root).unwrap();
        manager.cancel(cancellable.id).unwrap();
        let cancelled = wait_terminal(&manager, cancellable.id);
        assert_eq!(cancelled.status, TerminalStatus::Cancelled);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn task_output_is_bounded_and_artifacts_are_reported() {
        let root = std::env::temp_dir().join(format!("loom-task-{}", ProjectId::new()));
        fs::create_dir_all(&root).unwrap();
        let (program, args) = command(if cfg!(windows) {
            "echo artifact>artifact.txt & echo 1234567890"
        } else {
            "printf artifact > artifact.txt; printf 1234567890"
        });
        let supervisor = TaskSupervisor::new(&root).unwrap();
        let task = supervisor
            .start(TaskSpec {
                kind: TaskKind::Test,
                label: "bounded test".to_owned(),
                command: program,
                args,
                cwd: None,
                output_limit_bytes: Some(5),
                artifact_paths: vec!["artifact.txt".to_owned()],
            })
            .unwrap();
        for _ in 0..100 {
            let current = supervisor.get(task.id).unwrap();
            if matches!(
                current.status,
                TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
            ) {
                assert!(current.output.len() <= 5);
                assert!(current.output_truncated);
                assert!(current.artifacts[0].exists);
                assert!(current.evidence[0].uri.starts_with("loom://task/"));
                fs::remove_dir_all(root).unwrap();
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("task did not finish");
    }
}
