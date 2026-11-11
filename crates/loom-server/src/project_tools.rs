use super::*;

/// Characters of a stored child result a multi-child listing carries. Selecting
/// one child returns the whole stored result instead, so nothing is unreachable.
const PROJECT_CHILD_RESULT_EXCERPT_CHARS: usize = 512;

/// Names the manager's escape from a blocked code child, so a completion guard
/// that no integration can satisfy is never a dead end.
const PROJECT_CHILD_COMPLETION_ESCAPE: &str = " If the child's work cannot be landed, close the task with control_project_child action `abandon`, which reports its uncommitted paths and releases this guard.";

/// Whether the reviewed result of a code child reached the parent history.
///
/// A manager can land a reviewed change itself, so an absent integration record
/// is not proof that the work is unlanded. The guard therefore distinguishes a
/// recorded or ancestral integration from a patch that the parent history
/// already carries, and blocks only when neither holds.
enum ReviewedResultIntegration {
    /// The recorded integration contains the reviewed revision.
    Integrated,
    /// The parent history carries every reviewed patch at this revision.
    AlreadyPresent { revision: String },
    /// Nothing recorded or present in the parent history carries the result.
    Missing,
}

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
        let child_model_guidance = if self.can_delegate || self.can_delegate_code {
            match self.child_model_policy() {
                Ok(policy) => child_model_guidance(&policy),
                Err(error) => {
                    log::debug!(
                        "could not read project {} child models for tool guidance: {}",
                        self.project_id,
                        error.message
                    );
                    String::new()
                }
            }
        } else {
            String::new()
        };
        if self.can_delegate {
            definitions.push(ToolDefinition {
                name: "delegate_project_task".to_owned(),
                description: "Create a bounded non-code child agent task in this project. The child is read-only: when the project has one clean Git repository it receives its own isolated checkout of your current revision, so delegate by objective and reference files by path instead of pasting their contents. ".to_owned() + &child_model_guidance,
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "child_name": {"type": "string", "minLength": 1, "maxLength": 128},
                        "intent": {"type": "string", "minLength": 1, "maxLength": 16384},
                        "model_id": {"type": "string", "minLength": 1, "maxLength": loom_persistence::MAX_MODEL_ID_BYTES, "description": "Optional provider/model ID that overrides the project's child models. Omit this field, or use `current`, to let the project choose by kind and default, falling back to this agent's model."},
                        "kind": {"type": "string", "minLength": 1, "maxLength": loom_core::MAX_CHILD_MODEL_KIND_BYTES, "description": "Optional kind of work, such as cheap, coding, review, or vision. The project may map a kind to a model; an unmapped kind uses the project's default child model."},
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
                description: "Create a bounded code-changing child task in an isolated Git worktree based on the project's clean current revision. The child must commit its result and report the commit. Only use this for source changes; the child cannot access the parent checkout. Describe the intended change rather than quoting code, because the child reads its own checkout. Pass `paths` to declare the files its commit may touch, or `out_of_scope` to name the files it must leave alone; both are repository-relative, and a path covers itself and every path below it, so `crates/loom-vcs/src` covers `crates/loom-vcs/src/lib.rs`. Review and integration compare the child's commit with the declaration.".to_owned() + &child_model_guidance,
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "child_name": {"type": "string", "minLength": 1, "maxLength": 128},
                        "intent": {"type": "string", "minLength": 1, "maxLength": 16384},
                        "model_id": {"type": "string", "minLength": 1, "maxLength": loom_persistence::MAX_MODEL_ID_BYTES, "description": "Optional provider/model ID that overrides the project's child models. Omit this field, or use `current`, to let the project choose by kind and default, falling back to this agent's model."},
                        "kind": {"type": "string", "minLength": 1, "maxLength": loom_core::MAX_CHILD_MODEL_KIND_BYTES, "description": "Optional kind of work, such as cheap, coding, review, or vision. The project may map a kind to a model; an unmapped kind uses the project's default child model."},
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
                        "paths": {
                            "type": "array",
                            "maxItems": loom_core::MAX_PROJECT_CHILD_SCOPE_PATHS,
                            "items": {"type": "string", "minLength": 1, "maxLength": loom_core::MAX_PROJECT_CHILD_SCOPE_PATH_BYTES},
                            "description": "Repository-relative paths the child's commit may touch. A declared path covers itself and every path below it."
                        },
                        "out_of_scope": {
                            "type": "array",
                            "maxItems": loom_core::MAX_PROJECT_CHILD_SCOPE_PATHS,
                            "items": {"type": "string", "minLength": 1, "maxLength": loom_core::MAX_PROJECT_CHILD_SCOPE_PATH_BYTES},
                            "description": "Repository-relative paths the child's commit must leave alone; the inverse of `paths`, for when an allowlist is impractical. A declared path covers itself and every path below it."
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
                description: "Continue a paused child, retry a failed child run, cancel a live child, abandon a finished child whose work you will not land, record that you landed a reviewed result yourself, or rebase a finished child's own commits onto a newer base. Address children only by delegated task_id. Retry re-runs the failed tool step when a tool step is what failed; when the model or provider call is what failed there is no step to re-run, so the run resumes from its checkpoint and the child can finish and commit the work its checkout already holds. Abandon closes a finished child as failed, reports any uncommitted paths it left in its checkout, states that this work will not be landed, and releases the completion guard for that child; the checkout is preserved for inspection. mark_landed records a reviewed result you landed yourself — by cherry-picking, replaying, or squashing the child's commits — with the revision that carries the reviewed change and evidence of how it landed; the revision is checked against the reviewed content, so a revision that does not hold it is refused. rebase replays a finished child's own commits onto the revision you name, keeping each commit's author and message and creating no merge commit, then runs its bounded checks and records the new base and result revision; it refuses a dirty child checkout, a commit already contained in the target, a target that does not descend from the child's base, a conflicted replay, and a replay that would touch a file the reviewed change never touched, and restores the child's branch when a check fails.".to_owned(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "task_id": {"type": "string", "format": "uuid"},
                        "action": {"type": "string", "enum": ["continue", "retry_failed_step", "cancel", "abandon", "mark_landed", "rebase"]},
                        "revision": {"type": "string", "minLength": 1, "maxLength": 64, "description": "Revision in the parent repository that carries the reviewed change; required by mark_landed."},
                        "evidence": {"type": "string", "minLength": 1, "maxLength": 4096, "description": "How the reviewed change was landed, for example the cherry-pick or squash commit you made; required by mark_landed."},
                        "onto": {"type": "string", "minLength": 1, "maxLength": 64, "description": "Where rebase replays the child's own commits: a commit id of the parent repository, or `main` for the parent checkout's current head. Required by rebase and never merged with a new parent checkout."}
                    },
                    "required": ["task_id", "action"],
                    "additionalProperties": false
                }),
            });
        }
        if self.can_review_children {
            definitions.push(ToolDefinition {
                name: "review_project_child".to_owned(),
                description: "Review a code child by task_id. The default `stat` mode returns its checkout status, each changed path with its kind of change and line counts, and its commits with subjects and patch ids, so a large or move-only change is reviewed without its patch. It also reports whether the change stays inside the file scope the delegation declared: `in_scope` is true or false, every out-of-scope path is named with its line counts and whether it is a new file, and a child whose delegation declared no scope says so instead of reporting it in scope. `mode: \"diff\"` returns a bounded patch instead: `paths` limits it, `max_bytes` caps each file, `context` sets context lines, and every truncation is marked. Read the rest of a file with `read_child_file`.".to_owned(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "task_id": {"type": "string", "format": "uuid"},
                        "mode": {"type": "string", "enum": ["stat", "diff"], "description": "`stat` (default) or `diff`."},
                        "paths": {"type": "array", "maxItems": MAX_REVIEW_PATHS, "items": {"type": "string", "minLength": 1, "maxLength": 4096}, "description": "Paths a diff review is limited to."},
                        "max_bytes": {"type": "integer", "minimum": 1, "maximum": MAX_REVIEW_REQUEST_BYTES, "description": "Byte budget for one file in diff mode."},
                        "context": {"type": "integer", "minimum": 0, "maximum": MAX_REVIEW_CONTEXT_LINES, "description": "Unified context lines."}
                    },
                    "required": ["task_id"],
                    "additionalProperties": false
                }),
            });
            definitions.push(ToolDefinition {
                name: "read_child_file".to_owned(),
                description: "Read an exact line range of one file in a reviewed code child's checkout, with line numbers. The path must stay inside the checkout and a range is at most 400 lines. Use it for the rest of a file a review truncated.".to_owned(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "task_id": {"type": "string", "format": "uuid"},
                        "path": {"type": "string", "minLength": 1, "maxLength": 4096, "description": "Path relative to the checkout root."},
                        "line_start": {"type": "integer", "minimum": 1, "description": "First line, from 1."},
                        "line_end": {"type": "integer", "minimum": 1, "description": "Last line; defaults to a 400 line range."}
                    },
                    "required": ["task_id", "path"],
                    "additionalProperties": false
                }),
            });
        }
        if self.can_integrate_children {
            definitions.push(ToolDefinition {
                name: "integrate_project_child".to_owned(),
                description: "Integrate a completed, reviewed code child's committed revision into your clean parent checkout. Supply the base revision reported by review. Integration fast-forwards when possible and otherwise creates a merge commit, so your checkout may have advanced. A conflict is refused with its paths preserved; nothing is force-applied or partially landed. A commit that touches a path outside the file scope its delegation declared is refused and the offending paths are named; pass `allow_out_of_scope` with a `reason` to land it anyway, which records the reason and the accepted paths on the child's record.".to_owned(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "task_id": {"type": "string", "format": "uuid"},
                        "expected_parent_revision": {"type": "string", "minLength": 1, "maxLength": 64},
                        "allow_out_of_scope": {"type": "boolean", "description": "Land a commit that touches paths outside the declared file scope."},
                        "reason": {"type": "string", "minLength": 1, "maxLength": loom_core::MAX_PROJECT_CHILD_SCOPE_REASON_BYTES, "description": "Why the out-of-scope commit is landed; required with allow_out_of_scope and recorded with the override."}
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
            "review_project_child" | "read_child_file" if self.can_review_children => {
                Some(loom_core::ActionKind::Read)
            }
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
            "read_child_file" if self.can_review_children => self.execute_read_child_file(call),
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
                                Ok(ReviewedResultIntegration::Integrated) => {}
                                Ok(ReviewedResultIntegration::AlreadyPresent { revision }) => {
                                    // The manager landed the reviewed change
                                    // itself, so the guard records that instead
                                    // of refusing work that is already in the
                                    // parent history.
                                    self.record_project_child_patch_landing(
                                        &worktree,
                                        result_revision,
                                        &revision,
                                    );
                                }
                                Ok(ReviewedResultIntegration::Missing) => {
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
    /// A child whose run is still progressing keeps writing its checkout, so a
    /// count taken now would change on its next turn and say nothing the manager
    /// can act on; `checkout_is_settled` is false for that child and for one that
    /// has not started a run. Every stopped child — paused, waiting for input or
    /// approval, or finished — is settled, and its uncommitted work is reported,
    /// so a blocked child's work is visible without opening its path. A checkout
    /// that is already removed or being removed holds nothing to report. An
    /// inspection failure is returned so the caller can report it rather than
    /// claim the checkout is clean.
    fn inspect_project_child_checkout(
        &self,
        worktree: &ProjectWorktreeRecord,
        checkout_is_settled: bool,
    ) -> Result<Option<ProjectChildCheckoutState>> {
        if !checkout_is_settled
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

    /// Reports whether the reviewed result revision reached the parent history.
    ///
    /// Integration records the parent revision after integration, not
    /// necessarily the child revision: a fast-forward records the child
    /// revision, while a merge records the merge commit and the "already
    /// present" path records the parent HEAD. The guard's intent is that the
    /// reviewed change is in the parent history, so ancestry rather than
    /// equality decides a recorded integration, and a landing the manager
    /// performed itself is recognised by the reviewed content the recorded
    /// revision holds or by the same patch appearing in the parent history.
    ///
    /// [`ReviewedResultIntegration::Missing`] means none of those checks holds,
    /// and `Err` means the parent repository could not be inspected, so the
    /// guard can report that state instead of asserting the result is
    /// un-integrated.
    fn reviewed_result_is_integrated(
        &self,
        worktree: &ProjectWorktreeRecord,
        result_revision: &str,
    ) -> Result<ReviewedResultIntegration> {
        let connection = self.connection().ok_or_else(|| {
            LoomError::new(
                ErrorCode::RecoveryRequired,
                "project backend is no longer available",
                true,
            )
        })?;
        let parent_git =
            connection.session_git(worktree.parent_session_id, worktree.parent_repository_id)?;
        if let Some(integrated_revision) = worktree.integrated_revision.as_deref() {
            if integrated_revision == result_revision
                || parent_git.is_ancestor_revision(result_revision, integrated_revision)?
            {
                return Ok(ReviewedResultIntegration::Integrated);
            }
            // A landing Loom did not perform — a replay or a squash the manager
            // recorded — is identified by the reviewed content its revision
            // holds. A child that advanced after that landing no longer matches,
            // so the record cannot stand in for an integration that never
            // happened.
            if parent_git
                .change_carry(
                    &worktree.base_revision,
                    result_revision,
                    integrated_revision,
                )?
                .is_carried()
            {
                return Ok(ReviewedResultIntegration::Integrated);
            }
        }
        let Some(parent_revision) = parent_git.status()?.head else {
            return Ok(ReviewedResultIntegration::Missing);
        };
        // A parent that contains the reviewed revision already carries the
        // whole result, whatever the record says.
        if parent_git.is_ancestor_revision(result_revision, &parent_revision)? {
            return Ok(ReviewedResultIntegration::AlreadyPresent {
                revision: parent_revision,
            });
        }
        // A manager who cherry-picks or replays the reviewed change leaves no
        // integration and no record of its own, so the parent history is
        // searched for the same patches. Every reviewed commit has to be
        // present: a result that landed only partly is reported as missing, so
        // it still blocks and the manager is pushed to the integrate or abandon
        // path.
        let reviewed = parent_git.patch_commits_since(&worktree.base_revision, result_revision)?;
        if reviewed.is_empty() {
            return Ok(ReviewedResultIntegration::Missing);
        }
        let landed = parent_git.patch_commits_since(&worktree.base_revision, &parent_revision)?;
        let mut landed_by_patch = BTreeMap::new();
        for commit in &landed {
            landed_by_patch
                .entry(commit.patch_id)
                .or_insert(commit.revision.as_str());
        }
        // The walks are newest first, so the first match is the parent commit
        // that carries the reviewed tip of the child.
        let mut carried = None;
        for commit in &reviewed {
            let Some(revision) = landed_by_patch.get(&commit.patch_id) else {
                return Ok(ReviewedResultIntegration::Missing);
            };
            carried.get_or_insert((*revision).to_owned());
        }
        match carried {
            Some(revision) => Ok(ReviewedResultIntegration::AlreadyPresent { revision }),
            None => Ok(ReviewedResultIntegration::Missing),
        }
    }

    /// Records that the parent history already carries a reviewed result.
    ///
    /// This is bookkeeping for a landing the manager performed itself: nothing
    /// is applied to any checkout and no child is archived from here, because
    /// the manager has not asked for either. A write that fails is logged and
    /// never turns a genuinely landed result into a refusal.
    fn record_project_child_patch_landing(
        &self,
        worktree: &ProjectWorktreeRecord,
        result_revision: &str,
        landed_revision: &str,
    ) {
        let Some(backend) = self.backend.upgrade() else {
            return;
        };
        let Some(persistence) = backend.persistence.as_ref() else {
            return;
        };
        let mut landed = worktree.clone();
        landed.status = ProjectWorktreeStatus::Integrated;
        landed.result_revision = Some(result_revision.to_owned());
        landed.integrated_revision = Some(landed_revision.to_owned());
        landed.landed_by = Some(ProjectWorktreeLandedBy::AlreadyPresent);
        landed.conflict_paths.clear();
        landed.error = None;
        landed.updated_at = Timestamp::now();
        if let Err(error) = persistence.save_project_worktree(&landed) {
            log::warn!(
                "could not record that the parent history already carries the reviewed result of code child task {}: {}",
                landed.task_id,
                error.message
            );
        }
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
            identity: Arc::new(Mutex::new(ConnectionIdentity::default())),
            pre_login: false,
            max_response_bytes: None,
        })
    }

    /// Resolves the model this agent uses at the time of the call.
    ///
    /// The captured model records what the run started or was restored with,
    /// and a model switch during the run updates only the live run's state, so
    /// an alias that reuses this agent's model has to read the running run
    /// instead of the captured id. A session without a registered running run
    /// (a direct tool call in a test, or a run that is not executing) keeps the
    /// captured id.
    pub(crate) fn current_model_id(&self) -> ModelId {
        let Some(backend) = self.backend.upgrade() else {
            return self.model_id.clone();
        };
        // Clone the candidate handles and release the run registry before any
        // run lock is taken, so a run transition never waits behind this read.
        let handles = match backend.runs() {
            Ok(runs) => runs
                .values()
                .filter(|handle| handle.session_id == self.session_id)
                .cloned()
                .collect::<Vec<_>>(),
            Err(_) => Vec::new(),
        };
        let live = handles
            .into_iter()
            .filter(|handle| handle.is_running())
            .max_by_key(|handle| handle.run_id);
        match live {
            Some(handle) => handle.state().task.model,
            None => self.model_id.clone(),
        }
    }

    /// The child model policy of this agent's project, read on every call so a
    /// change applies to the next delegation without restarting the run.
    pub(crate) fn child_model_policy(&self) -> Result<loom_core::ProjectChildModelPolicy> {
        let Some(backend) = self.backend.upgrade() else {
            return Ok(loom_core::ProjectChildModelPolicy::default());
        };
        match backend.persistence.as_ref() {
            Some(persistence) => persistence.load_project_child_model_policy(self.project_id),
            None => Ok(loom_core::ProjectChildModelPolicy::default()),
        }
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
            identity: Arc::new(Mutex::new(ConnectionIdentity::default())),
            pre_login: false,
            max_response_bytes: None,
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
        let policy = match self.child_model_policy() {
            Ok(policy) => policy,
            Err(error) => {
                return ToolResult::failure(
                    call,
                    format!(
                        "could not read the project's child models: {}",
                        error.message
                    ),
                );
            }
        };
        let choice = delegated_child_model(
            arguments.model_id.as_deref(),
            arguments.kind.as_deref(),
            &policy,
            &self.current_model_id(),
        );
        let kind = arguments
            .kind
            .as_deref()
            .and_then(|kind| loom_core::normalize_child_model_kind(kind).ok());
        // A declared file scope is the allowlist or the explicit deny list the
        // child's commit is checked against at review and integration time. An
        // explicitly empty declaration is refused rather than read as none.
        let declared_scope = match (
            arguments.paths.as_deref(),
            arguments.out_of_scope.as_deref(),
        ) {
            (None, None) => None,
            (Some(_), Some(_)) => {
                return ToolResult::failure(
                    call,
                    "declare a child's file scope with `paths` or `out_of_scope`, not both",
                );
            }
            (Some(paths), None) => Some((loom_core::ProjectChildScopeMode::Paths, paths)),
            (None, Some(paths)) => Some((loom_core::ProjectChildScopeMode::OutOfScope, paths)),
        };
        let scope = match declared_scope {
            Some((mode, paths)) => match loom_core::ProjectChildScope::new(mode, paths) {
                Ok(scope) => Some(scope),
                Err(error) => return ToolResult::failure(call, error.message),
            },
            None => None,
        };
        if scope.is_some() && !code_change {
            return ToolResult::failure(
                call,
                "a declared file scope applies to a code child's commit; declare it on a code task",
            );
        }
        let request_id = RequestId::from_uuid(*call.id.as_uuid());
        let spec = DelegatedTaskSpec {
            intent: arguments.intent,
            model_id: choice.model_id,
            context_references: arguments.context_references,
            dependencies: arguments.dependencies,
            code_change,
            permissions: arguments.permissions,
            scope: scope.clone(),
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
                        "model_id": task.model_id,
                        "model_source": choice.source,
                        "kind": kind,
                        "scope": scope,
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
        // Read the child names before the per-child inspection below. That
        // inspection restores a child filesystem, which locks the same session
        // manager, and holding this non-reentrant guard across it self-deadlocks
        // the tool thread and stalls every request behind the lock.
        let names = {
            let sessions = match connection.backend.sessions() {
                Ok(sessions) => sessions,
                Err(error) => return ToolResult::failure(call, error.message),
            };
            project
                .agents
                .iter()
                .filter(|agent| agent.parent_session_id == Some(self.session_id))
                .map(|agent| {
                    (
                        agent.session_id,
                        sessions
                            .get(agent.session_id)
                            .map(|session| session.name.clone())
                            .unwrap_or_default(),
                    )
                })
                .collect::<BTreeMap<_, _>>()
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
            let name = names.get(&agent.session_id).cloned().unwrap_or_default();
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
            // child whose run stopped before it finished — paused, or blocked
            // waiting for input or approval — is in the same position: its
            // checkout is no longer moving, and what it wrote is the only place
            // that work exists, so it is reported too. A run that is still
            // progressing is skipped because its checkout is in motion, and so is
            // a child that has not started a run at all. A checkout that cannot
            // be inspected is reported as such rather than claiming that the
            // child left nothing behind.
            let (checkout_state, worktree_note) = match worktree.as_ref() {
                Some(worktree) => {
                    let task_terminal =
                        task.is_some_and(|task| delegated_task_is_terminal(task.status));
                    let checkout_is_settled = task_terminal
                        || run.as_ref().is_some_and(|summary| {
                            !matches!(
                                summary.snapshot.state,
                                AgentRunState::Planning
                                    | AgentRunState::Executing
                                    | AgentRunState::Evaluating
                            )
                        });
                    match self.inspect_project_child_checkout(worktree, checkout_is_settled) {
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
                "model_id": task.map(|task| task.model_id.as_str()),
                "run_state": run.as_ref().map(|summary| summary.snapshot.state),
                "run_summary": run.as_ref().and_then(|summary| summary.snapshot.summary.as_ref()).map(|summary| summary.chars().take(256).collect::<String>()),
                "worktree_status": worktree.as_ref().map(|worktree| worktree.status),
                "result_revision": worktree.as_ref().and_then(|worktree| worktree.result_revision.clone()),
                "result_revision_is_base": worktree.as_ref().map(|worktree| worktree.result_revision.as_deref().is_none_or(|revision| revision == worktree.base_revision)),
                "integrated_revision": worktree.as_ref().and_then(|worktree| worktree.integrated_revision.clone()),
                "scope": worktree.as_ref().and_then(|worktree| worktree.scope.clone()),
                "scope_override": worktree.as_ref().and_then(|worktree| worktree.scope_override.clone()),
                "landed_by": worktree.as_ref().and_then(|worktree| worktree.landed_by),
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
                "note": "result_state reports whether each child's stored result reached this session: delivered, undelivered (accepted for delivery but not yet in your transcript), missing (a finished child never reported a result), or pending. undelivered_messages counts the durable inbox messages the child has not read yet, so a message you sent that still waits is distinguishable from one the child consumed. For a code child whose run is not progressing — finished, paused, or blocked waiting for input or approval — worktree_uncommitted_path_count and worktree_uncommitted_line_delta report what its checkout still holds uncommitted, worktree_head_is_base says whether that checkout has no commit of its own yet, and result_revision_is_base says whether no reviewed result revision other than its base has been recorded. integrated_revision names the revision its reviewed result landed at, and landed_by says how it got there: integrated (Loom fast-forwarded, merged, or completed the record), already_present (the parent history already carried the reviewed patch), or replay (you recorded the landing yourself). A child whose run is still running reports none of the checkout fields, because its checkout is in motion. Work that is never reviewed and integrated, or abandoned, is not landed. Pass a child's task_id to return its whole stored result."
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
        // A landing names the revision that carries the reviewed change and the
        // manager's evidence for it; every other action carries neither.
        let landing = match arguments.action {
            ProjectChildControlAction::MarkLanded => {
                let Some(revision) = arguments.revision else {
                    return ToolResult::failure(
                        call,
                        "mark_landed requires the revision that carries the reviewed change",
                    );
                };
                let Some(evidence) = arguments.evidence else {
                    return ToolResult::failure(
                        call,
                        "mark_landed requires evidence of how the reviewed change was landed",
                    );
                };
                Some(ProjectChildLanding { revision, evidence })
            }
            _ => None,
        };
        match connection.control_project_child(
            self.session_id,
            self.project_id,
            arguments.task_id,
            arguments.action,
            landing,
            arguments.onto,
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
                if let Some(worktree) = outcome.worktree.as_ref() {
                    // A landing names the revision that now carries the
                    // reviewed change, so the manager can confirm what was
                    // recorded without listing the child again.
                    output["integrated_revision"] = serde_json::json!(worktree.integrated_revision);
                    output["landed_by"] = serde_json::json!(worktree.landed_by);
                    output["note"] = serde_json::json!(
                        "The reviewed result is recorded as landed at this revision, so the completion guard no longer requires this child; nothing was applied to your checkout."
                    );
                }
                if let Some(rebase) = outcome.rebase.as_ref() {
                    // A rebase changed the child's own revision, so the report
                    // names both pairs and what ran to validate the result.
                    output = serde_json::json!({
                        "task_id": outcome.task.task_id,
                        "status": outcome.task.status,
                        "child_session_id": outcome.task.target_session_id,
                        "run_state": outcome.run.as_ref().map(|snapshot| snapshot.state),
                        "run_summary": outcome.run.as_ref().and_then(|snapshot| snapshot.summary.clone()),
                        "previous_base_revision": &rebase.previous_base_revision,
                        "previous_result_revision": &rebase.previous_result_revision,
                        "base_revision": &rebase.base_revision,
                        "result_revision": &rebase.result_revision,
                        "commit_ids_changed": rebase.commit_ids_changed,
                        "commits_before": rebase.commits_before,
                        "commits_after": rebase.commits_after,
                        "checks": rebase.checks.iter().map(|check| serde_json::json!({
                            "name": &check.name,
                            "command": &check.command,
                            "outcome": check.outcome.as_str(),
                            "detail": &check.detail,
                        })).collect::<Vec<_>>(),
                        "check_timeout_secs": rebase.check_timeout_secs,
                        "note": "The child's own commits were replayed onto base_revision and its branch now points at result_revision; it keeps each commit's author and message and created no merge commit. The checks above ran in the child checkout: a check reported as skipped did not run and says why. Review the child again before integrating it, because its result revision changed."
                    });
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
        let max_bytes = match arguments.max_bytes {
            None => DEFAULT_REVIEW_MAX_BYTES,
            Some(0) => {
                return ToolResult::failure(call, "max_bytes must be at least 1 byte");
            }
            Some(bytes) => match usize::try_from(bytes) {
                Ok(bytes) if bytes <= MAX_REVIEW_REQUEST_BYTES => bytes,
                _ => {
                    return ToolResult::failure(
                        call,
                        format!("max_bytes must be at most {MAX_REVIEW_REQUEST_BYTES} bytes"),
                    );
                }
            },
        };
        let context = match arguments.context {
            None => 0,
            Some(context) if context > MAX_REVIEW_CONTEXT_LINES => {
                return ToolResult::failure(
                    call,
                    format!("context must be at most {MAX_REVIEW_CONTEXT_LINES} lines"),
                );
            }
            Some(context) => u32::try_from(context).unwrap_or(0),
        };
        if arguments.paths.len() > MAX_REVIEW_PATHS {
            return ToolResult::failure(
                call,
                format!("a review may name at most {MAX_REVIEW_PATHS} paths"),
            );
        }
        let Some(connection) = self.connection() else {
            return ToolResult::failure(call, "project backend is no longer available");
        };
        match connection.get_project_child_review_view(
            self.project_id,
            self.session_id,
            arguments.task_id,
            &ProjectChildReviewRequest {
                mode: arguments.mode,
                paths: &arguments.paths,
                max_bytes,
                context,
            },
        ) {
            Ok(view) => {
                // Include the durable run state and summary so a failed child's
                // reason is visible alongside its retained (uncommitted) work.
                let run = connection
                    .backend
                    .persistence
                    .as_ref()
                    .and_then(|persistence| {
                        persistence
                            .load_delegated_task(view.worktree.task_id)
                            .ok()
                            .flatten()
                            .and_then(|task| {
                                persistence
                                    .load_latest_run_summary_for_session(task.target_session_id)
                                    .ok()
                                    .flatten()
                            })
                    });
                let output = project_child_review_output(&view, max_bytes, context, run.as_ref());
                ToolResult {
                    tool_call_id: call.id,
                    name: call.name.clone(),
                    success: true,
                    output: serde_json::to_string(&output)
                        .unwrap_or_else(|error| format!("could not encode child review: {error}")),
                }
            }
            Err(error) => ToolResult::failure(call, error.message),
        }
    }

    pub(crate) fn execute_read_child_file(&self, call: &ToolCall) -> ToolResult {
        let arguments =
            match serde_json::from_value::<ReadProjectChildFileArguments>(call.arguments.clone()) {
                Ok(arguments) => arguments,
                Err(error) => {
                    return ToolResult::failure(
                        call,
                        format!("invalid child file read arguments: {error}"),
                    );
                }
            };
        let line_start = match arguments.line_start {
            None => None,
            Some(line_start) => match usize::try_from(line_start) {
                Ok(line_start) => Some(line_start),
                Err(_) => {
                    return ToolResult::failure(call, "line_start does not fit this platform");
                }
            },
        };
        let line_end = match arguments.line_end {
            None => None,
            Some(line_end) => match usize::try_from(line_end) {
                Ok(line_end) => Some(line_end),
                Err(_) => {
                    return ToolResult::failure(call, "line_end does not fit this platform");
                }
            },
        };
        let Some(connection) = self.connection() else {
            return ToolResult::failure(call, "project backend is no longer available");
        };
        match connection.read_project_child_file(
            self.project_id,
            self.session_id,
            arguments.task_id,
            &arguments.path,
            line_start,
            line_end,
        ) {
            Ok(slice) => {
                let lines = slice
                    .lines
                    .iter()
                    .enumerate()
                    .map(|(offset, text)| {
                        serde_json::json!({
                            "line": slice.line_start + offset,
                            "text": text,
                        })
                    })
                    .collect::<Vec<_>>();
                let output = serde_json::json!({
                    "type": "project_child_file",
                    "data": {
                        "task_id": arguments.task_id,
                        "path": slice.path,
                        "line_start": slice.line_start,
                        "line_end": slice.line_end,
                        "total_lines": slice.total_lines,
                        "truncated": slice.truncated,
                        "lines": lines,
                        "note": "Line numbers count from 1. Request the next range with read_child_file when truncated is true or when the file is longer than this range."
                    }
                });
                ToolResult {
                    tool_call_id: call.id,
                    name: call.name.clone(),
                    success: true,
                    output: serde_json::to_string(&output).unwrap_or_else(|error| {
                        format!("could not encode child file read: {error}")
                    }),
                }
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
        // An out-of-scope landing is recorded with the manager's reason, so it is
        // refused here when the reason is missing rather than landing silently.
        let scope_override = if arguments.allow_out_of_scope {
            let reason = arguments.reason.unwrap_or_default();
            let reason = reason.trim();
            if reason.is_empty() || reason.len() > loom_core::MAX_PROJECT_CHILD_SCOPE_REASON_BYTES {
                return ToolResult::failure(
                    call,
                    format!(
                        "allow_out_of_scope requires a reason of 1 to {} bytes stating why the out-of-scope commit is landed",
                        loom_core::MAX_PROJECT_CHILD_SCOPE_REASON_BYTES
                    ),
                );
            }
            Some(ProjectChildScopeOverrideRequest {
                reason: reason.to_owned(),
            })
        } else {
            None
        };
        let Some(connection) = self.connection() else {
            return ToolResult::failure(call, "project backend is no longer available");
        };
        match connection.integrate_project_child(
            RequestId::from_uuid(*call.id.as_uuid()),
            self.project_id,
            self.session_id,
            arguments.task_id,
            arguments.expected_parent_revision,
            scope_override,
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
                        "scope": worktree.scope,
                        "scope_override": worktree.scope_override,
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

/// The tool result body of one child review.
///
/// A `stat` review never carries a patch body, and its file and commit listings
/// are bounded by the caller's budget. A `diff` review carries only the patch
/// the service already bounded, and names every path whose patch it cut short
/// or never reached.
fn project_child_review_output(
    view: &ProjectChildReviewView,
    max_bytes: usize,
    context: u32,
    run: Option<&loom_persistence::DurableRunSummary>,
) -> serde_json::Value {
    let mut data = serde_json::json!({
        "worktree": &view.worktree,
        "status": &view.status,
        "base_revision": view.worktree.base_revision,
        "head_revision": view.status.head,
        "run_state": run.map(|summary| summary.snapshot.state),
        "run_summary": run.and_then(|summary| summary.snapshot.summary.clone()),
        "scope": project_child_scope_report(view.worktree.scope.as_ref(), &view.stat),
    });
    if let Some(diff) = view.diff.as_ref() {
        data["mode"] = serde_json::json!("diff");
        data["max_bytes"] = serde_json::json!(max_bytes);
        data["context"] = serde_json::json!(context);
        data["files"] = serde_json::to_value(&diff.files).unwrap_or_default();
        data["truncated_files"] = serde_json::json!(diff.truncated_paths);
        data["omitted_files"] = serde_json::json!(diff.omitted_paths);
        data["patch_bytes"] = serde_json::json!(
            diff.files
                .iter()
                .map(|file| file.patch.len())
                .sum::<usize>()
        );
        data["truncated"] =
            serde_json::json!(!diff.truncated_paths.is_empty() || !diff.omitted_paths.is_empty());
        data["note"] = serde_json::json!(
            "Each patch is bounded per file and in total. A cut patch ends with a marker line and its path is listed in truncated_files, while omitted_files names every path the budget never reached. Read any part of a file with read_child_file(task_id, path, line_start, line_end)."
        );
        return serde_json::json!({"type": "project_child_review", "data": data});
    }
    data["mode"] = serde_json::json!("stat");
    data["totals"] = serde_json::json!({
        "files": view.stat.len(),
        "additions": view.stat.iter().map(|stat| u64::from(stat.additions)).sum::<u64>(),
        "deletions": view.stat.iter().map(|stat| u64::from(stat.deletions)).sum::<u64>(),
        "commits": view.commits.len(),
    });
    // The listings are the only part of a summary that grows with the change,
    // so the caller's budget trims them and the status stays whole: a review
    // that named no path would be unable to say what changed at all.
    data["files"] = serde_json::json!([]);
    data["commits"] = serde_json::json!([]);
    let mut remaining = max_bytes.saturating_sub(
        serde_json::to_string(&data)
            .map(|encoded| encoded.len())
            .unwrap_or(0),
    );
    let mut files = Vec::new();
    for stat in &view.stat {
        let entry = serde_json::to_value(stat).unwrap_or_default();
        let length = serde_json::to_string(&entry).map_or(0, |encoded| encoded.len() + 1);
        if length > remaining {
            break;
        }
        remaining -= length;
        files.push(entry);
    }
    let mut commits = Vec::new();
    for commit in &view.commits {
        let entry = serde_json::json!({
            "revision": commit.revision,
            "subject": commit.summary,
            "patch_id": format!("{:016x}", commit.patch_id),
        });
        let length = serde_json::to_string(&entry).map_or(0, |encoded| encoded.len() + 1);
        if length > remaining {
            break;
        }
        remaining -= length;
        commits.push(entry);
    }
    let omitted_files = view.stat.len() - files.len();
    let omitted_commits = view.commits.len() - commits.len();
    data["files"] = serde_json::json!(files);
    data["commits"] = serde_json::json!(commits);
    data["omitted_files"] = serde_json::json!(omitted_files);
    data["omitted_commits"] = serde_json::json!(omitted_commits);
    data["truncated"] = serde_json::json!(omitted_files > 0 || omitted_commits > 0);
    data["note"] = serde_json::json!(
        "A stat review carries no patch bodies: each path reports its kind of change and line counts, and each commit its subject and patch id. The file and commit listings are bounded by max_bytes, while the worktree record and checkout status are always returned. Pass mode `diff` with paths, max_bytes, and context for a bounded patch, or read an exact line range with read_child_file(task_id, path, line_start, line_end)."
    );
    serde_json::json!({"type": "project_child_review", "data": data})
}

/// The scope part of a child review.
///
/// A review that names no declaration says so instead of reporting the change
/// in scope, because an undeclared scope checks nothing. A declared one reports
/// whether the change stays inside it, and names every out-of-scope path with
/// its line counts and whether the child created it.
fn project_child_scope_report(
    scope: Option<&loom_core::ProjectChildScope>,
    stat: &[loom_core::GitRevisionFileStat],
) -> serde_json::Value {
    let Some(scope) = scope else {
        return serde_json::json!({
            "declared": false,
            "note": "No file scope was declared for this child, so its change is not checked against one. A delegation declares one with `paths` or `out_of_scope`.",
        });
    };
    let out_of_scope = scope.out_of_scope(stat).collect::<Vec<_>>();
    let total = out_of_scope.len();
    let new_files = out_of_scope
        .iter()
        .filter(|stat| stat.change == loom_core::GitRevisionChangeKind::New)
        .map(|stat| stat.path.clone())
        .collect::<Vec<_>>();
    // A review is bounded whatever the size of the change, so the named paths
    // stop at the same bound a diff review accepts and the rest are counted.
    let truncated = out_of_scope.len() > MAX_REVIEW_PATHS;
    let out_of_scope = out_of_scope
        .iter()
        .take(MAX_REVIEW_PATHS)
        .map(|stat| {
            serde_json::json!({
                "path": stat.path,
                "change": stat.change,
                "additions": stat.additions,
                "deletions": stat.deletions,
                "binary": stat.binary,
                "new": stat.change == loom_core::GitRevisionChangeKind::New,
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "declared": true,
        "mode": scope.mode,
        "paths": scope.paths,
        "in_scope": total == 0,
        "out_of_scope": out_of_scope,
        "out_of_scope_total": total,
        "new_out_of_scope_files": new_files.iter().take(MAX_REVIEW_PATHS).cloned().collect::<Vec<_>>(),
        "new_out_of_scope_files_total": new_files.len(),
        "truncated": truncated,
        "note": "A declared path covers itself and every path below it, so a directory declaration covers the files in it. The out-of-scope paths are the ones this review would refuse at integration, and they stop at the review's path bound while out_of_scope_total stays exact.",
    })
}

/// The sentence a delegation tool description ends with: how this project
/// chooses a child's model, so the agent knows which kinds exist and what each
/// runs on without a separate lookup.
pub(crate) fn child_model_guidance(policy: &loom_core::ProjectChildModelPolicy) -> String {
    if policy.is_empty() {
        return " This project sets no child models, so a child reuses your model unless you \
                 pass model_id."
            .to_owned();
    }
    let mut parts = policy
        .by_kind
        .iter()
        .map(|(kind, model)| format!("kind `{kind}` runs on `{model}`"))
        .collect::<Vec<_>>();
    match &policy.default_model_id {
        Some(model) => parts.push(format!("any other child runs on `{model}`")),
        None => parts.push("any other child reuses your model".to_owned()),
    }
    format!(
        " This project chooses child models: {}. Pass `kind` to pick the kind of work; pass \
         model_id only to override the project.",
        parts.join("; ")
    )
}
