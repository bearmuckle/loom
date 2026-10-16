use super::*;

/// Characters of a stored child result a multi-child listing carries. Selecting
/// one child returns the whole stored result instead, so nothing is unreachable.
const PROJECT_CHILD_RESULT_EXCERPT_CHARS: usize = 512;

/// Names the manager's escape from a blocked code child, so a completion guard
/// that no integration can satisfy is never a dead end.
const PROJECT_CHILD_COMPLETION_ESCAPE: &str = " If the child's work cannot be landed, close the task with control_project_child action `abandon`, which reports its uncommitted paths and releases this guard.";

/// Whether a delegated task has reached a state that produces no further
/// result.
fn delegated_task_is_terminal(status: loom_core::DelegatedTaskStatus) -> bool {
    matches!(
        status,
        loom_core::DelegatedTaskStatus::Completed
            | loom_core::DelegatedTaskStatus::Failed
            | loom_core::DelegatedTaskStatus::Cancelled
    )
}

impl ToolExtension for ProjectAgentTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions = Vec::new();
        if self.can_delegate {
            definitions.push(ToolDefinition {
                name: "delegate_project_task".to_owned(),
                description: "Create a bounded non-code child agent task in this project. The child is read-only: when the project has one clean Git repository it receives its own isolated checkout of your current revision, so delegate by objective and reference files by path instead of pasting their contents. Omit model_id or set it to `current` to reuse this agent's model; use a provider/model ID to choose another model.".to_owned(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "child_name": {"type": "string", "minLength": 1, "maxLength": 128},
                        "intent": {"type": "string", "minLength": 1, "maxLength": 16384},
                        "model_id": {"type": "string", "minLength": 1, "maxLength": 512, "description": "Optional provider/model ID. Omit this field or use `current` to reuse this agent's model."},
                        "context_references": {
                            "type": "array",
                            "maxItems": 128,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "label": {"type": "string"},
                                    "uri": {"type": "string"}
                                },
                                "required": ["label", "uri"],
                                "additionalProperties": false
                            }
                        },
                        "dependencies": {
                            "type": "array",
                            "maxItems": 128,
                            "items": {"type": "string", "format": "uuid"}
                        },
                        "permissions": {
                            "type": "object",
                            "properties": {
                                "delegation": {"type": "boolean"},
                                "branch_messaging": {"type": "boolean"},
                                "child_control": {"type": "boolean"},
                                "inspection": {"type": "boolean"},
                                "worktree_creation": {"type": "boolean"},
                                "review": {"type": "boolean"},
                                "integration": {"type": "boolean"}
                            },
                            "additionalProperties": false
                        }
                    },
                    "required": ["child_name", "intent"],
                    "additionalProperties": false
                }),
            });
        }
        if self.can_delegate_code {
            definitions.push(ToolDefinition {
                name: "delegate_project_code_task".to_owned(),
                description: "Create a bounded code-changing child task in an isolated Git worktree based on the project's clean current revision. The child must commit its result and report the commit. Only use this for source changes; the child cannot access the parent checkout. Describe the intended change rather than quoting code, because the child reads its own checkout.".to_owned(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "child_name": {"type": "string", "minLength": 1, "maxLength": 128},
                        "intent": {"type": "string", "minLength": 1, "maxLength": 16384},
                        "model_id": {"type": "string", "minLength": 1, "maxLength": 512, "description": "Optional provider/model ID. Omit this field or use `current` to reuse this agent's model."},
                        "context_references": {
                            "type": "array",
                            "maxItems": 128,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "label": {"type": "string"},
                                    "uri": {"type": "string"}
                                },
                                "required": ["label", "uri"],
                                "additionalProperties": false
                            }
                        },
                        "dependencies": {
                            "type": "array",
                            "maxItems": 128,
                            "items": {"type": "string", "format": "uuid"}
                        },
                        "permissions": {
                            "type": "object",
                            "properties": {
                                "delegation": {"type": "boolean"},
                                "branch_messaging": {"type": "boolean"},
                                "child_control": {"type": "boolean"},
                                "inspection": {"type": "boolean"},
                                "worktree_creation": {"type": "boolean"},
                                "review": {"type": "boolean"},
                                "integration": {"type": "boolean"}
                            },
                            "additionalProperties": false
                        }
                    },
                    "required": ["child_name", "intent"],
                    "additionalProperties": false
                }),
            });
        }
        if self.can_message || self.can_branch_message {
            definitions.push(ToolDefinition {
                name: "send_project_agent_message".to_owned(),
                description: "Send a durable message to an explicitly named project member. Direct parent-child routes use the direct messaging grant; non-adjacent routes require the sender and recipient to have independent branch-messaging grants. task_id supplies optional context and never selects the recipient. A direction, question, or answer wakes an idle or finished member so it can act on the message; progress, result, and blocker wait for the recipient's next turn. The result reports whether the message reached a live turn, woke the recipient, waits for a parked wait, or is queued, and list_project_children reports each child's undelivered inbox messages. The sender is bound to this agent run.".to_owned(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "target_session_id": {"type": "string", "format": "uuid"},
                        "task_id": {"type": "string", "format": "uuid"},
                        "kind": {"type": "string", "enum": ["progress", "result", "question", "blocker", "direction", "answer"]},
                        "body": {"type": "string", "minLength": 1, "maxLength": 16384}
                    },
                    "required": ["target_session_id", "kind", "body"],
                    "additionalProperties": false
                }),
            });
        }
        if self.can_branch_message {
            definitions.push(ToolDefinition {
                name: "list_project_message_recipients".to_owned(),
                description: "List only project members who have an explicit branch-messaging grant, so you can address an authorized non-adjacent recipient by session ID.".to_owned(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
            });
        }
        if self.can_inspect_children {
            definitions.push(ToolDefinition {
                name: "list_project_children".to_owned(),
                description: "Inspect the current status of your direct child agents and their delegated tasks in this project. Each child reports its stored result and whether that result reached this session, so a completed child whose result never arrived stays retrievable: pass its task_id to return the whole stored result instead of a bounded excerpt. Each child also reports how many durable inbox messages it has not read yet, so a message you sent can be told apart from one that is still undelivered.".to_owned(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "task_id": {"type": "string", "format": "uuid", "description": "Optional direct-child task ID; when given, only that child is listed and its stored result is returned in full."}
                    },
                    "additionalProperties": false
                }),
            });
        }
        if self.can_wait_children {
            definitions.push(ToolDefinition {
                name: "wait_for_project_children".to_owned(),
                description: "Wait until the listed direct child tasks are return-ready, releasing this manager's agent slot while they run. The result includes terminal child states and whether each child's own result reached your transcript; code results still need review and integration before the overall task is complete.".to_owned(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "task_ids": {
                            "type": "array",
                            "minItems": 1,
                            "maxItems": 50,
                            "items": {"type": "string", "format": "uuid"}
                        }
                    },
                    "required": ["task_ids"],
                    "additionalProperties": false
                }),
            });
        }
        if self.can_control_children {
            definitions.push(ToolDefinition {
                name: "control_project_child".to_owned(),
                description: "Continue a paused child, retry a failed child run, cancel a live child, or abandon a finished child whose work you will not land. Address children only by delegated task_id. Retry re-runs the failed tool step when a tool step is what failed; when the model or provider call is what failed there is no step to re-run, so the run resumes from its checkpoint and the child can finish and commit the work its checkout already holds. Abandon closes a finished child as failed, reports any uncommitted paths it left in its checkout, states that this work will not be landed, and releases the completion guard for that child; the checkout is preserved for inspection.".to_owned(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "task_id": {"type": "string", "format": "uuid"},
                        "action": {"type": "string", "enum": ["continue", "retry_failed_step", "cancel", "abandon"]}
                    },
                    "required": ["task_id", "action"],
                    "additionalProperties": false
                }),
            });
        }
        if self.can_review_children {
            definitions.push(ToolDefinition {
                name: "review_project_child".to_owned(),
                description: "Review a code child by task_id. Returns its checkout status and a bounded diff from the revision where its worktree was created.".to_owned(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "task_id": {"type": "string", "format": "uuid"}
                    },
                    "required": ["task_id"],
                    "additionalProperties": false
                }),
            });
        }
        if self.can_integrate_children {
            definitions.push(ToolDefinition {
                name: "integrate_project_child".to_owned(),
                description: "Integrate a completed, reviewed code child's committed revision into your clean parent checkout. Supply the base revision reported by review. Integration fast-forwards when possible and otherwise creates a merge commit, so your checkout may have advanced. A conflict is refused with its paths preserved; nothing is force-applied or partially landed.".to_owned(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "task_id": {"type": "string", "format": "uuid"},
                        "expected_parent_revision": {"type": "string", "minLength": 1, "maxLength": 64}
                    },
                    "required": ["task_id", "expected_parent_revision"],
                    "additionalProperties": false
                }),
            });
        }
        definitions
    }

    fn action_kind(&self, call: &ToolCall) -> Option<loom_core::ActionKind> {
        match call.name.as_str() {
            "delegate_project_task" if self.can_delegate => Some(loom_core::ActionKind::Write),
            "delegate_project_code_task" if self.can_delegate_code => {
                Some(loom_core::ActionKind::Write)
            }
            // Project messaging is a bounded internal coordination action. It
            // uses the low-risk policy tier so routine reports do not require
            // per-message approval; routing is checked against live project
            // membership and direct parent-child relationships.
            "send_project_agent_message" if self.can_message || self.can_branch_message => {
                Some(loom_core::ActionKind::Read)
            }
            "list_project_message_recipients" if self.can_branch_message => {
                Some(loom_core::ActionKind::Read)
            }
            "list_project_children" if self.can_inspect_children => {
                Some(loom_core::ActionKind::Read)
            }
            "wait_for_project_children" if self.can_wait_children => {
                Some(loom_core::ActionKind::Read)
            }
            "control_project_child" if self.can_control_children => {
                Some(loom_core::ActionKind::Write)
            }
            "review_project_child" if self.can_review_children => Some(loom_core::ActionKind::Read),
            "integrate_project_child" if self.can_integrate_children => {
                Some(loom_core::ActionKind::Write)
            }
            _ => None,
        }
    }

    fn execute(&self, call: &ToolCall) -> ToolResult {
        match call.name.as_str() {
            "delegate_project_task" if self.can_delegate => self.execute_delegation(call),
            "delegate_project_code_task" if self.can_delegate_code => {
                self.execute_code_delegation(call)
            }
            "send_project_agent_message" if self.can_message || self.can_branch_message => {
                self.execute_message(call)
            }
            "list_project_message_recipients" if self.can_branch_message => {
                self.execute_list_message_recipients(call)
            }
            "list_project_children" if self.can_inspect_children => {
                self.execute_list_children(call)
            }
            "wait_for_project_children" if self.can_wait_children => {
                self.execute_wait_for_children(call)
            }
            "control_project_child" if self.can_control_children => {
                self.execute_control_child(call)
            }
            "review_project_child" if self.can_review_children => self.execute_review_child(call),
            "integrate_project_child" if self.can_integrate_children => {
                self.execute_integrate_child(call)
            }
            _ => ToolResult::failure(call, format!("unknown project agent tool '{}'", call.name)),
        }
    }

    fn prepare_deferred(&self, call: &ToolCall) -> Result<Option<String>> {
        if call.name != "wait_for_project_children" || !self.can_wait_children {
            return Ok(None);
        }
        let arguments =
            serde_json::from_value::<WaitForProjectChildrenArguments>(call.arguments.clone())
                .map_err(|error| {
                    // The call must fail here rather than run as if no wait was
                    // requested: an unreadable payload cannot be parked.
                    let error = LoomError::new(
                        ErrorCode::MalformedPayload,
                        format!("invalid project child wait arguments: {error}"),
                        false,
                    );
                    log::error!(
                        "project child wait call {} could not be decoded: {}",
                        call.id,
                        error.message,
                    );
                    error
                })?;
        let tasks = self
            .load_wait_child_tasks(&arguments.task_ids)
            .inspect_err(|error| {
                // A transient persistence failure must not read as "nothing to
                // wait for": report it so the manager call fails instead.
                log::error!(
                    "project child wait call {} could not load its child tasks: {}",
                    call.id,
                    error.message,
                );
            })?;
        if tasks.iter().any(|task| {
            !matches!(
                task.status,
                loom_core::DelegatedTaskStatus::Completed
                    | loom_core::DelegatedTaskStatus::Failed
                    | loom_core::DelegatedTaskStatus::Cancelled
            )
        }) {
            Ok(Some(call.id.to_string()))
        } else {
            Ok(None)
        }
    }

    fn completion_blocker(&self) -> Option<String> {
        let Some(backend) = self.backend.upgrade() else {
            return Some(
                "project state is unavailable; verify child work before reporting completion"
                    .to_owned(),
            );
        };
        let persistence = backend.persistence.as_ref()?;
        let tasks = match persistence.list_project_tasks(self.project_id) {
            Ok(tasks) => tasks,
            Err(_) => {
                return Some(
                    "project child state could not be confirmed; inspect child tasks before reporting completion"
                        .to_owned(),
                );
            }
        };
        for task in tasks
            .iter()
            .filter(|task| task.requester_session_id == self.session_id)
        {
            // Active children no longer block a manager from ending its turn:
            // the manager may reply to the user and be woken when the child
            // reports back. Only unreviewed or unintegrated code results keep
            // the guard, because silently abandoning them risks losing work.
            if task.code_change && task.status == loom_core::DelegatedTaskStatus::Completed {
                match persistence.load_project_worktree_by_task(task.task_id) {
                    Ok(Some(worktree)) => {
                        let Some(result_revision) = worktree.result_revision.as_deref() else {
                            return Some(format!(
                                "completed code child task {} ({}) has not had its result reviewed.{PROJECT_CHILD_COMPLETION_ESCAPE}",
                                task.task_id, task.child_name
                            ));
                        };
                        if worktree.status != ProjectWorktreeStatus::Removed {
                            match project_child_worktree_status(&backend, &worktree) {
                                Ok(status)
                                    if status.clean
                                        && status.head.as_deref() == Some(result_revision) => {}
                                Ok(_) => {
                                    return Some(format!(
                                        "completed code child task {} ({}) changed after review or has uncommitted work; review its current checkout before reporting completion.{PROJECT_CHILD_COMPLETION_ESCAPE}",
                                        task.task_id, task.child_name
                                    ));
                                }
                                Err(_) => {
                                    return Some(format!(
                                        "the live checkout for code child task {} ({}) could not be confirmed.{PROJECT_CHILD_COMPLETION_ESCAPE}",
                                        task.task_id, task.child_name
                                    ));
                                }
                            }
                        }
                        if result_revision != worktree.base_revision {
                            match self.reviewed_result_is_integrated(&worktree, result_revision) {
                                Ok(true) => {}
                                Ok(false) => {
                                    let detail = match worktree.integrated_revision.as_deref() {
                                        Some(integrated_revision) => format!(
                                            "its recorded integration at {integrated_revision} does not contain the reviewed result {result_revision}"
                                        ),
                                        None => {
                                            "no integration is recorded for its result".to_owned()
                                        }
                                    };
                                    return Some(format!(
                                        "completed code child task {} ({}) has a reviewed result that still needs integration: {detail}.{PROJECT_CHILD_COMPLETION_ESCAPE}",
                                        task.task_id, task.child_name
                                    ));
                                }
                                Err(_) => {
                                    return Some(format!(
                                        "integration state for code child task {} ({}) could not be confirmed.{PROJECT_CHILD_COMPLETION_ESCAPE}",
                                        task.task_id, task.child_name
                                    ));
                                }
                            }
                        }
                    }
                    Ok(None) => {
                        return Some(format!(
                            "completed code child task {} ({}) has no durable worktree record to verify.{PROJECT_CHILD_COMPLETION_ESCAPE}",
                            task.task_id, task.child_name
                        ));
                    }
                    Err(_) => {
                        return Some(format!(
                            "integration state for code child task {} ({}) could not be confirmed.{PROJECT_CHILD_COMPLETION_ESCAPE}",
                            task.task_id, task.child_name
                        ));
                    }
                }
            }
        }
        None
    }
}

/// Live state of one finished code child's checkout, as the manager listing
/// reports it.
///
/// The listing is where a manager decides whether to retry, land, or abandon a
/// finished child, so the work that child left in its checkout has to be visible
/// there instead of only by opening the worktree path.
struct ProjectChildCheckoutState {
    uncommitted_paths: u32,
    additions: u32,
    deletions: u32,
    /// Whether the checkout is still on the revision it was created from, so it
    /// holds no commit of its own yet.
    head_is_base: bool,
}

impl ProjectAgentTools {
    /// Reads the reportable state from a live checkout status.
    fn project_child_checkout_state_from_status(
        status: &loom_vcs::GitRepositoryStatus,
        base_revision: &str,
    ) -> ProjectChildCheckoutState {
        ProjectChildCheckoutState {
            uncommitted_paths: u32::try_from(status.files.len()).unwrap_or(u32::MAX),
            additions: status
                .files
                .iter()
                .map(|file| file.index_additions.saturating_add(file.worktree_additions))
                .fold(0, u32::saturating_add),
            deletions: status
                .files
                .iter()
                .map(|file| file.index_deletions.saturating_add(file.worktree_deletions))
                .fold(0, u32::saturating_add),
            head_is_base: status.head.as_deref() == Some(base_revision),
        }
    }

    /// Inspects one code child's checkout for the manager listing.
    ///
    /// Only a terminal task's checkout is inspected: a live child's checkout is
    /// still moving, so a count taken now would change on its next turn and say
    /// nothing the manager can act on. A checkout that is already removed or
    /// being removed holds nothing to report. An inspection failure is returned
    /// so the caller can report it rather than claim the checkout is clean.
    fn inspect_project_child_checkout(
        &self,
        worktree: &ProjectWorktreeRecord,
        task_terminal: bool,
    ) -> Result<Option<ProjectChildCheckoutState>> {
        if !task_terminal
            || matches!(
                worktree.status,
                ProjectWorktreeStatus::Removed | ProjectWorktreeStatus::CleanupPending
            )
        {
            return Ok(None);
        }
        let backend = self.backend.upgrade().ok_or_else(|| {
            LoomError::new(
                ErrorCode::Internal,
                "project backend is no longer available",
                true,
            )
        })?;
        let status = project_child_worktree_status(&backend, worktree)?;
        Ok(Some(Self::project_child_checkout_state_from_status(
            &status,
            &worktree.base_revision,
        )))
    }

    /// Reports whether the reviewed result revision is contained in the parent
    /// revision recorded for the child's integration.
    ///
    /// Integration records the parent revision after integration, not
    /// necessarily the child revision: a fast-forward records the child
    /// revision, while a merge records the merge commit and the "already
    /// present" path records the parent HEAD. The guard's intent is that the
    /// reviewed revision is contained in what was integrated, so ancestry
    /// rather than equality decides the outcome.
    ///
    /// `Ok(false)` means no integration is recorded, or the recorded revision
    /// does not contain the result. `Err` means the parent repository could not
    /// be inspected, so the guard can report that state instead of asserting
    /// the result is un-integrated.
    fn reviewed_result_is_integrated(
        &self,
        worktree: &ProjectWorktreeRecord,
        result_revision: &str,
    ) -> Result<bool> {
        let Some(integrated_revision) = worktree.integrated_revision.as_deref() else {
            return Ok(false);
        };
        if integrated_revision == result_revision {
            return Ok(true);
        }
        let connection = self.connection().ok_or_else(|| {
            LoomError::new(
                ErrorCode::RecoveryRequired,
                "project backend is no longer available",
                true,
            )
        })?;
        connection
            .session_git(worktree.parent_session_id, worktree.parent_repository_id)?
            .is_ancestor_revision(result_revision, integrated_revision)
    }

    /// Manager-facing summary of the revision a delegated child checkout was
    /// created from, together with its drift from the default-branch tip that
    /// the recorded refresh resolved.
    ///
    /// The value is the refresh recorded when the checkout was created, so a
    /// cached `origin/*` ref is never presented as a current tip. `None` means
    /// the child has no durable worktree record yet.
    pub(crate) fn child_checkout_base(
        &self,
        task_id: loom_core::TaskId,
    ) -> Option<serde_json::Value> {
        let backend = self.backend.upgrade()?;
        let worktree = backend
            .persistence
            .as_ref()?
            .load_project_worktree_by_task(task_id)
            .ok()??;
        let refresh = worktree.base_refresh?;
        Some(serde_json::json!({
            "base_revision": worktree.base_revision,
            "summary": refresh.summary(),
            "reported_at": refresh.reported_at(),
            "refresh": refresh,
        }))
    }

    pub(crate) fn load_wait_child_tasks(
        &self,
        task_ids: &[loom_core::TaskId],
    ) -> Result<Vec<loom_core::DelegatedTaskRecord>> {
        if task_ids.is_empty() || task_ids.len() > 50 {
            return Err(LoomError::invalid_request(
                "wait_for_project_children requires between one and fifty task IDs",
            ));
        }
        let unique = task_ids.iter().copied().collect::<BTreeSet<_>>();
        if unique.len() != task_ids.len() {
            return Err(LoomError::invalid_request(
                "wait_for_project_children task IDs must be unique",
            ));
        }
        let backend = self.backend.upgrade().ok_or_else(|| {
            LoomError::new(
                ErrorCode::Internal,
                "project backend is no longer available",
                true,
            )
        })?;
        let persistence = backend.persistence.as_ref().ok_or_else(|| {
            LoomError::new(
                ErrorCode::UnsupportedCapability,
                "project child waits require durable storage",
                false,
            )
        })?;
        task_ids
            .iter()
            .map(|task_id| {
                let task = persistence
                    .load_delegated_task(*task_id)?
                    .ok_or_else(|| LoomError::not_found("delegated task", *task_id))?;
                if task.project_id != self.project_id
                    || task.requester_session_id != self.session_id
                {
                    return Err(LoomError::new(
                        ErrorCode::AuthorizationDenied,
                        "wait_for_project_children accepts only direct child task IDs",
                        false,
                    ));
                }
                Ok(task)
            })
            .collect()
    }

    pub(crate) fn execute_wait_for_children(&self, call: &ToolCall) -> ToolResult {
        let arguments =
            match serde_json::from_value::<WaitForProjectChildrenArguments>(call.arguments.clone())
            {
                Ok(arguments) => arguments,
                Err(error) => {
                    return ToolResult::failure(
                        call,
                        format!("invalid project child wait arguments: {error}"),
                    );
                }
            };
        match self.load_wait_child_tasks(&arguments.task_ids) {
            Ok(tasks)
                if tasks.iter().all(|task| {
                    matches!(
                        task.status,
                        loom_core::DelegatedTaskStatus::Completed
                            | loom_core::DelegatedTaskStatus::Failed
                            | loom_core::DelegatedTaskStatus::Cancelled
                    )
                }) =>
            {
                let Some(connection) = self.connection() else {
                    return ToolResult::failure(call, "project backend is no longer available");
                };
                let Some(persistence) = &connection.backend.persistence else {
                    return ToolResult::failure(
                        call,
                        "project child waits require durable storage",
                    );
                };
                let mut children = Vec::new();
                for task in &tasks {
                    // Return-ready says nothing about the payload: report whether
                    // the child's own result already reached this manager, so a
                    // silent child is visible instead of looking like a delivered
                    // result.
                    let delivery = match connection.project_child_result_delivery(persistence, task)
                    {
                        Ok(delivery) => delivery,
                        Err(error) => return ToolResult::failure(call, error.message),
                    };
                    let state = delivery.state(true);
                    children.push(serde_json::json!({
                        "task_id": task.task_id,
                        "child_name": task.child_name,
                        "status": task.status,
                        "code_change": task.code_change,
                        "result_state": state.as_str(),
                        "result_message_id": delivery.message.as_ref().map(|message| message.message_id),
                        "result_project_sequence": delivery.message.as_ref().map(|message| message.project_sequence),
                        "result_accepted_at": delivery.message.as_ref().map(|message| message.accepted_at),
                    }));
                }
                let missing = children
                    .iter()
                    .filter(|child| child["result_state"] == "missing")
                    .count();
                let note = if missing > 0 {
                    format!(
                        "{missing} of the return-ready children never reported a result; read them with list_project_children before reporting completion. Code child results still require review and integration."
                    )
                } else {
                    "result_state reports whether each child's own result has already reached your transcript; an undelivered result is delivered when this wait resumes. Code child results still require review and integration before the manager reports completion."
                        .to_owned()
                };
                ToolResult::success(
                    call,
                    serde_json::to_string(&serde_json::json!({
                        "return_ready": true,
                        "children": children,
                        "note": note,
                    }))
                    .unwrap_or_else(|error| format!("could not encode child wait result: {error}")),
                )
            }
            Ok(_) => ToolResult::failure(
                call,
                "child tasks are still active; retry through the durable wait continuation",
            ),
            Err(error) => ToolResult::failure(call, error.message),
        }
    }

    pub(crate) fn connection(&self) -> Option<InProcessConnection> {
        self.backend.upgrade().map(|backend| InProcessConnection {
            backend,
            negotiated_capabilities: Arc::new(Mutex::new(None)),
            auth: None,
        })
    }

    pub(crate) fn execute_delegation(&self, call: &ToolCall) -> ToolResult {
        self.execute_delegation_with_kind(call, false)
    }

    pub(crate) fn execute_code_delegation(&self, call: &ToolCall) -> ToolResult {
        self.execute_delegation_with_kind(call, true)
    }

    pub(crate) fn execute_delegation_with_kind(
        &self,
        call: &ToolCall,
        code_change: bool,
    ) -> ToolResult {
        let arguments =
            match serde_json::from_value::<DelegateProjectTaskArguments>(call.arguments.clone()) {
                Ok(arguments) => arguments,
                Err(error) => {
                    return ToolResult::failure(
                        call,
                        format!("invalid project delegation arguments: {error}"),
                    );
                }
            };
        let run_grants = loom_core::ProjectAgentPermissions {
            delegation: self.can_delegate,
            branch_messaging: self.can_branch_message,
            child_control: self.can_control_children,
            inspection: self.can_inspect_children,
            worktree_creation: self.can_delegate_code,
            review: self.can_review_children,
            integration: self.can_integrate_children,
        };
        if !project_permissions_are_subset(arguments.permissions, run_grants) {
            return ToolResult::failure(
                call,
                "this agent run cannot grant one or more requested project permissions to a child",
            );
        }
        let Some(backend) = self.backend.upgrade() else {
            return ToolResult::failure(call, "project backend is no longer available");
        };
        let connection = InProcessConnection {
            backend,
            negotiated_capabilities: Arc::new(Mutex::new(None)),
            auth: None,
        };
        match connection.load_project_snapshot(self.project_id) {
            Ok(project)
                if project.agents.iter().any(|agent| {
                    agent.session_id == self.session_id && agent.project_id == self.project_id
                }) => {}
            Ok(_) => {
                return ToolResult::failure(call, "project delegation grant is no longer valid");
            }
            Err(error) => return ToolResult::failure(call, error.message),
        }
        let request_id = RequestId::from_uuid(*call.id.as_uuid());
        let spec = DelegatedTaskSpec {
            intent: arguments.intent,
            model_id: delegated_child_model_id(arguments.model_id, &self.model_id),
            context_references: arguments.context_references,
            dependencies: arguments.dependencies,
            code_change,
            permissions: arguments.permissions,
        };
        match connection.create_project_child(
            request_id,
            self.session_id,
            arguments.child_name,
            spec,
        ) {
            Ok(ServerResponse::Project(ProjectResponse::ProjectChildCreated { task, child })) => {
                ToolResult {
                    tool_call_id: call.id,
                    name: call.name.clone(),
                    success: true,
                    output: serde_json::to_string(&serde_json::json!({
                        "task_id": task.task_id,
                        "child_session_id": child.session_id,
                        "status": task.status,
                        "child_name": task.child_name,
                        "intent": task.intent,
                        "checkout_base": self.child_checkout_base(task.task_id),
                    }))
                    .unwrap_or_else(|error| format!("could not encode delegation result: {error}")),
                }
            }
            Ok(_) => ToolResult::failure(call, "project delegation returned an unexpected result"),
            Err(error) => ToolResult::failure(call, error.message),
        }
    }

    pub(crate) fn execute_message(&self, call: &ToolCall) -> ToolResult {
        let arguments = match serde_json::from_value::<SendProjectAgentMessageArguments>(
            call.arguments.clone(),
        ) {
            Ok(arguments) => arguments,
            Err(error) => {
                return ToolResult::failure(
                    call,
                    format!("invalid project agent message arguments: {error}"),
                );
            }
        };
        if arguments.body.trim().is_empty() || arguments.body.len() > 16 * 1024 {
            return ToolResult::failure(call, "agent message body must contain 1 to 16384 bytes");
        }
        let Some(target_session_id) = arguments.target_session_id else {
            return ToolResult::failure(call, "target_session_id is required");
        };
        let Some(backend) = self.backend.upgrade() else {
            return ToolResult::failure(call, "project backend is no longer available");
        };
        let draft = loom_core::AgentMessageDraft {
            project_id: self.project_id,
            task_id: arguments.task_id,
            sender_session_id: self.session_id,
            target_session_id,
            kind: arguments.kind,
            body: arguments.body,
        };
        match backend.accept_project_agent_message(
            RequestId::from_uuid(*call.id.as_uuid()),
            self.session_id,
            self.can_message,
            self.can_branch_message,
            draft,
        ) {
            Ok(accepted) => ToolResult {
                tool_call_id: call.id,
                name: call.name.clone(),
                success: true,
                output: serde_json::to_string(&serde_json::json!({
                    "message_id": accepted.message.message_id,
                    "project_sequence": accepted.message.project_sequence,
                    "accepted_at": accepted.message.accepted_at,
                    "target_session_id": accepted.message.target_session_id,
                    "kind": accepted.message.kind,
                    "delivery": accepted.route.as_str(),
                    "delivery_note": accepted.route.note(),
                }))
                .unwrap_or_else(|error| {
                    format!("could not encode project message result: {error}")
                }),
            },
            Err(error) => ToolResult::failure(call, error.message),
        }
    }

    pub(crate) fn execute_list_message_recipients(&self, call: &ToolCall) -> ToolResult {
        if let Err(error) =
            serde_json::from_value::<ListProjectMessageRecipientsArguments>(call.arguments.clone())
        {
            return ToolResult::failure(
                call,
                format!("invalid project message-recipient query: {error}"),
            );
        }
        let Some(backend) = self.backend.upgrade() else {
            return ToolResult::failure(call, "project backend is no longer available");
        };
        let Some(persistence) = backend.persistence.as_ref() else {
            return ToolResult::failure(call, "project messaging requires durable storage");
        };
        let project = match persistence.load_project_snapshot(self.project_id) {
            Ok(Some(project)) => project,
            Ok(None) => return ToolResult::failure(call, "project no longer exists"),
            Err(error) => return ToolResult::failure(call, error.message),
        };
        let recipients = project
            .agents
            .iter()
            .filter(|agent| agent.session_id != self.session_id)
            .filter_map(|agent| {
                let name = if agent.session_id == project.root_session_id {
                    "project root".to_owned()
                } else {
                    project
                        .tasks
                        .iter()
                        .find(|task| task.target_session_id == agent.session_id)
                        .map(|task| task.child_name.clone())
                        .or_else(|| agent.task_summary.clone())
                        .unwrap_or_else(|| "project agent".to_owned())
                };
                match project_member_branch_messaging_enabled(
                    persistence,
                    project.root_session_id,
                    agent.session_id,
                ) {
                    Ok(true) => Some(Ok(serde_json::json!({
                        "session_id": agent.session_id,
                        "name": name,
                        "depth": agent.depth,
                        "parent_session_id": agent.parent_session_id,
                    }))),
                    Ok(false) => None,
                    Err(error) => Some(Err(error)),
                }
            })
            .collect::<Result<Vec<_>>>();
        match recipients {
            Ok(recipients) => ToolResult {
                tool_call_id: call.id,
                name: call.name.clone(),
                success: true,
                output: serde_json::to_string(&recipients)
                    .unwrap_or_else(|error| format!("could not encode recipients: {error}")),
            },
            Err(error) => ToolResult::failure(call, error.message),
        }
    }

    pub(crate) fn execute_list_children(&self, call: &ToolCall) -> ToolResult {
        let arguments =
            match serde_json::from_value::<ListProjectChildrenArguments>(call.arguments.clone()) {
                Ok(arguments) => arguments,
                Err(error) => {
                    return ToolResult::failure(
                        call,
                        format!("invalid project child query: {error}"),
                    );
                }
            };
        let Some(connection) = self.connection() else {
            return ToolResult::failure(call, "project backend is no longer available");
        };
        let project = match connection.load_project_snapshot(self.project_id) {
            Ok(project) => project,
            Err(error) => return ToolResult::failure(call, error.message),
        };
        if !project
            .agents
            .iter()
            .any(|agent| agent.session_id == self.session_id)
        {
            return ToolResult::failure(call, "project agent inspection grant is no longer valid");
        }
        let Some(persistence) = &connection.backend.persistence else {
            return ToolResult::failure(call, "project agent inspection requires durable storage");
        };
        let tasks = match persistence.list_project_tasks(self.project_id) {
            Ok(tasks) => tasks,
            Err(error) => return ToolResult::failure(call, error.message),
        };
        let sessions = match connection.backend.sessions() {
            Ok(sessions) => sessions,
            Err(error) => return ToolResult::failure(call, error.message),
        };
        let mut children = Vec::new();
        for agent in project
            .agents
            .iter()
            .filter(|agent| agent.parent_session_id == Some(self.session_id))
        {
            let task = tasks
                .iter()
                .find(|task| task.target_session_id == agent.session_id);
            if let Some(selected) = arguments.task_id
                && task.is_none_or(|task| task.task_id != selected)
            {
                continue;
            }
            let name = sessions
                .get(agent.session_id)
                .map(|session| session.name.clone())
                .unwrap_or_default();
            // The durable run summary carries the failure reason captured when
            // the child run failed, so the parent can tell an interrupted child
            // apart from one that produced nothing usable.
            let run = match persistence.load_latest_run_summary_for_session(agent.session_id) {
                Ok(run) => run,
                Err(error) => return ToolResult::failure(call, error.message),
            };
            // A message this manager sent is only known to be read when the
            // child's durable inbox cursor has passed it, so the count of
            // unread inbox messages is what makes a stranded message visible.
            let undelivered_messages =
                match project_member_undelivered_message_count(persistence, agent.session_id) {
                    Ok(count) => count,
                    Err(error) => return ToolResult::failure(call, error.message),
                };
            let worktree = match task {
                Some(task) => match persistence.load_project_worktree_by_task(task.task_id) {
                    Ok(worktree) => worktree,
                    Err(error) => return ToolResult::failure(call, error.message),
                },
                None => None,
            };
            // A finished code child's checkout is where its work still exists
            // when its run failed mid-flight, so the listing states that the work
            // is there instead of making the manager open a worktree path. A
            // checkout that cannot be inspected is reported as such rather than
            // claiming that the child left nothing behind.
            let (checkout_state, worktree_note) = match worktree.as_ref() {
                Some(worktree) => {
                    let task_terminal =
                        task.is_some_and(|task| delegated_task_is_terminal(task.status));
                    match self.inspect_project_child_checkout(worktree, task_terminal) {
                        Ok(state) => (state, None),
                        Err(error) => (
                            None,
                            Some(format!(
                                "the child checkout could not be inspected: {}",
                                error.message
                            )),
                        ),
                    }
                }
                None => (None, None),
            };
            // A completed child's result is durable whether or not it ever
            // reached this manager, so every child entry reports the stored
            // result and its delivery state. Selecting one task returns the whole
            // stored result instead of a bounded excerpt.
            let delivery = match task {
                Some(task) => match connection.project_child_result_delivery(persistence, task) {
                    Ok(delivery) => Some((task, delivery)),
                    Err(error) => return ToolResult::failure(call, error.message),
                },
                None => None,
            };
            let result_state = delivery
                .as_ref()
                .map(|(task, delivery)| delivery.state(delegated_task_is_terminal(task.status)));
            let (result_body, result_body_truncated) = match delivery.as_ref() {
                Some((_, delivery)) => match &delivery.message {
                    Some(message) if arguments.task_id.is_some() => {
                        (Some(message.body.clone()), false)
                    }
                    Some(message) => {
                        let excerpt = message
                            .body
                            .chars()
                            .take(PROJECT_CHILD_RESULT_EXCERPT_CHARS)
                            .collect::<String>();
                        let truncated = excerpt.len() < message.body.len();
                        (Some(excerpt), truncated)
                    }
                    None => (None, false),
                },
                None => (None, false),
            };
            children.push(serde_json::json!({
                "session_id": agent.session_id,
                "name": name,
                "state": agent.state,
                "task_summary": agent.task_summary.as_ref().map(|summary| summary.chars().take(256).collect::<String>()),
                "task_id": task.map(|task| task.task_id),
                "task_status": task.map(|task| task.status),
                "run_state": run.as_ref().map(|summary| summary.snapshot.state),
                "run_summary": run.as_ref().and_then(|summary| summary.snapshot.summary.as_ref()).map(|summary| summary.chars().take(256).collect::<String>()),
                "worktree_status": worktree.as_ref().map(|worktree| worktree.status),
                "result_revision": worktree.as_ref().and_then(|worktree| worktree.result_revision.clone()),
                "result_revision_is_base": worktree.as_ref().map(|worktree| worktree.result_revision.as_deref().is_none_or(|revision| revision == worktree.base_revision)),
                "worktree_uncommitted_path_count": checkout_state.as_ref().map(|state| state.uncommitted_paths),
                "worktree_uncommitted_line_delta": checkout_state.as_ref().map(|state| serde_json::json!({
                    "additions": state.additions,
                    "deletions": state.deletions,
                })),
                "worktree_head_is_base": checkout_state.as_ref().map(|state| state.head_is_base),
                "worktree_note": worktree_note,
                "result_state": result_state.map(|state| state.as_str()),
                "result_message_id": delivery.as_ref().and_then(|(_, delivery)| delivery.message.as_ref()).map(|message| message.message_id),
                "result_project_sequence": delivery.as_ref().and_then(|(_, delivery)| delivery.message.as_ref()).map(|message| message.project_sequence),
                "result_accepted_at": delivery.as_ref().and_then(|(_, delivery)| delivery.message.as_ref()).map(|message| message.accepted_at),
                "result_delivered": delivery.as_ref().is_some_and(|(_, delivery)| delivery.delivered()),
                "result_body": result_body,
                "result_body_truncated": result_body_truncated,
                "undelivered_messages": undelivered_messages,
                "updated_at": agent.updated_at,
            }));
        }
        if let Some(selected) = arguments.task_id
            && children.is_empty()
        {
            return ToolResult::failure(
                call,
                format!("{selected} is not one of your direct child tasks"),
            );
        }
        children.sort_by(|left, right| {
            right["updated_at"]
                .as_u64()
                .cmp(&left["updated_at"].as_u64())
        });
        let truncated = children.len() > 50;
        children.truncate(50);
        ToolResult {
            tool_call_id: call.id,
            name: call.name.clone(),
            success: true,
            output: serde_json::to_string(&serde_json::json!({
                "children": children,
                "truncated": truncated,
                "note": "result_state reports whether each child's stored result reached this session: delivered, undelivered (accepted for delivery but not yet in your transcript), missing (a finished child never reported a result), or pending. undelivered_messages counts the durable inbox messages the child has not read yet, so a message you sent that still waits is distinguishable from one the child consumed. For a finished code child, worktree_uncommitted_path_count and worktree_uncommitted_line_delta report what its checkout still holds uncommitted, worktree_head_is_base says whether that checkout has no commit of its own yet, and result_revision_is_base says whether no reviewed result revision other than its base has been recorded. Work that is never reviewed and integrated, or abandoned, is not landed. Pass a child's task_id to return its whole stored result."
            }))
            .unwrap_or_else(|error| format!("could not encode project child status: {error}")),
        }
    }

    pub(crate) fn execute_control_child(&self, call: &ToolCall) -> ToolResult {
        let arguments =
            match serde_json::from_value::<ControlProjectChildArguments>(call.arguments.clone()) {
                Ok(arguments) => arguments,
                Err(error) => {
                    return ToolResult::failure(
                        call,
                        format!("invalid project child control arguments: {error}"),
                    );
                }
            };
        let Some(connection) = self.connection() else {
            return ToolResult::failure(call, "project backend is no longer available");
        };
        match connection.control_project_child(
            self.session_id,
            self.project_id,
            arguments.task_id,
            arguments.action,
        ) {
            Ok(outcome) => {
                let mut output = serde_json::json!({
                    "task_id": outcome.task.task_id,
                    "status": outcome.task.status,
                    "child_session_id": outcome.task.target_session_id,
                    "run_state": outcome.run.as_ref().map(|snapshot| snapshot.state),
                    "run_summary": outcome.run.as_ref().and_then(|snapshot| snapshot.summary.clone()),
                });
                if arguments.action == ProjectChildControlAction::Abandon {
                    // The outcome names what the manager left behind and says
                    // that it stays behind, so a closed child with uncommitted
                    // work is actionable rather than silent.
                    output["uncommitted_paths"] = serde_json::json!(outcome.uncommitted_paths);
                    output["note"] = serde_json::json!(
                        "The child was closed as failed without landing its work, so these uncommitted paths will not be integrated; its checkout is preserved for inspection and the completion guard no longer requires this result."
                    );
                }
                ToolResult {
                    tool_call_id: call.id,
                    name: call.name.clone(),
                    success: true,
                    output: serde_json::to_string(&output).unwrap_or_else(|error| {
                        format!("could not encode project child control result: {error}")
                    }),
                }
            }
            Err(error) => ToolResult::failure(call, error.message),
        }
    }

    pub(crate) fn execute_review_child(&self, call: &ToolCall) -> ToolResult {
        let arguments =
            match serde_json::from_value::<ReviewProjectChildArguments>(call.arguments.clone()) {
                Ok(arguments) => arguments,
                Err(error) => {
                    return ToolResult::failure(
                        call,
                        format!("invalid project child review arguments: {error}"),
                    );
                }
            };
        let Some(connection) = self.connection() else {
            return ToolResult::failure(call, "project backend is no longer available");
        };
        match connection.get_project_child_review(
            self.project_id,
            self.session_id,
            arguments.task_id,
        ) {
            Ok(ServerResponse::Project(ProjectResponse::ProjectChildReview {
                worktree,
                status,
                diff,
            })) => {
                // Include the durable run state and summary so a failed child's
                // reason is visible alongside its retained (uncommitted) work.
                let run = connection
                    .backend
                    .persistence
                    .as_ref()
                    .and_then(|persistence| {
                        persistence
                            .load_delegated_task(worktree.task_id)
                            .ok()
                            .flatten()
                            .and_then(|task| {
                                persistence
                                    .load_latest_run_summary_for_session(task.target_session_id)
                                    .ok()
                                    .flatten()
                            })
                    });
                let output = serde_json::json!({
                    "type": "project_child_review",
                    "data": {
                        "worktree": worktree,
                        "status": status,
                        "diff": diff,
                        "run_state": run.as_ref().map(|summary| summary.snapshot.state),
                        "run_summary": run.as_ref().and_then(|summary| summary.snapshot.summary.clone()),
                    }
                });
                ToolResult {
                    tool_call_id: call.id,
                    name: call.name.clone(),
                    success: true,
                    output: serde_json::to_string(&output)
                        .unwrap_or_else(|error| format!("could not encode child review: {error}")),
                }
            }
            Ok(_) => {
                ToolResult::failure(call, "project child review returned an unexpected result")
            }
            Err(error) => ToolResult::failure(call, error.message),
        }
    }

    pub(crate) fn execute_integrate_child(&self, call: &ToolCall) -> ToolResult {
        let arguments = match serde_json::from_value::<IntegrateProjectChildArguments>(
            call.arguments.clone(),
        ) {
            Ok(arguments) => arguments,
            Err(error) => {
                return ToolResult::failure(
                    call,
                    format!("invalid project child integration arguments: {error}"),
                );
            }
        };
        if arguments.expected_parent_revision.trim().is_empty()
            || arguments.expected_parent_revision.len() > 64
        {
            return ToolResult::failure(
                call,
                "expected_parent_revision must contain 1 to 64 bytes",
            );
        }
        let Some(connection) = self.connection() else {
            return ToolResult::failure(call, "project backend is no longer available");
        };
        match connection.integrate_project_child(
            RequestId::from_uuid(*call.id.as_uuid()),
            self.project_id,
            self.session_id,
            arguments.task_id,
            arguments.expected_parent_revision,
        ) {
            Ok(ServerResponse::Project(ProjectResponse::ProjectChildWorktreeUpdated(worktree))) => {
                ToolResult {
                    tool_call_id: call.id,
                    name: call.name.clone(),
                    success: true,
                    output: serde_json::to_string(&serde_json::json!({
                        "task_id": worktree.task_id,
                        "status": worktree.status,
                        "integrated_revision": worktree.integrated_revision,
                    }))
                    .unwrap_or_else(|error| format!("could not encode child integration: {error}")),
                }
            }
            Ok(_) => ToolResult::failure(
                call,
                "project child integration returned an unexpected result",
            ),
            Err(error) => ToolResult::failure(call, error.message),
        }
    }
}
