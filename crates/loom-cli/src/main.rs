use std::{
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use loom_agent::{AgentEvent, AgentRunState};
use loom_core::{
    AgentSessionId, AgentSessionState, Capability, CapabilitySet, ErrorCode, LoomError, ProjectId,
    RunId,
};
use loom_model::ModelId;
use loom_protocol::{
    CURRENT_PROTOCOL_VERSION, ClientRequest, RequestEnvelope, ServerEvent, ServerResponse,
};
use loom_server::{InProcessBackend, InProcessConnection};

fn main() -> Result<(), LoomError> {
    let Some(options) = parse_args(env::args().skip(1))? else {
        return Ok(());
    };
    let workspace_root = prepare_workspace(options.root.clone())?;
    let backend = InProcessBackend::new();
    let connection = backend.connect();

    negotiate(&connection)?;
    let session = create_session(&connection, &options.name)?;
    let run_id = start_run(&connection, session.id, &options, &workspace_root)?;

    println!("Loom native M1 shell");
    println!(
        "Connected in-process using protocol {}.{}",
        CURRENT_PROTOCOL_VERSION.major, CURRENT_PROTOCOL_VERSION.minor
    );
    println!("Workspace: {}", workspace_root.display());
    println!("Session {}: {}", session.id, session.name);
    println!("Task: {}", options.task);
    stream_run(&connection, session.id, run_id, options.manual_approval)?;
    Ok(())
}

struct CliOptions {
    name: String,
    task: String,
    model: ModelId,
    root: Option<PathBuf>,
    manual_approval: bool,
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Option<CliOptions>, LoomError> {
    let mut options = CliOptions {
        name: "M1 demo".to_owned(),
        task: "make a small repository change and validate it".to_owned(),
        model: ModelId::new("deterministic/demo"),
        root: None,
        manual_approval: false,
    };
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--name" => options.name = required_value(&mut args, "--name")?,
            "--task" => options.task = required_value(&mut args, "--task")?,
            "--model" => options.model = ModelId::new(required_value(&mut args, "--model")?),
            "--root" => options.root = Some(PathBuf::from(required_value(&mut args, "--root")?)),
            "--manual-approval" => options.manual_approval = true,
            "--help" | "-h" => {
                println!(
                    "Usage: loom [--name <name>] [--task <task>] [--model <id>] \
                     [--root <path>] [--manual-approval]"
                );
                println!(
                    "The default workspace is an isolated directory in the system temp folder."
                );
                return Ok(None);
            }
            value if value.starts_with("--name=") => {
                options.name = value["--name=".len()..].to_owned();
            }
            value if value.starts_with("--task=") => {
                options.task = value["--task=".len()..].to_owned();
            }
            value if value.starts_with("--model=") => {
                options.model = ModelId::new(value["--model=".len()..].to_owned());
            }
            value if value.starts_with("--root=") => {
                options.root = Some(PathBuf::from(value["--root=".len()..].to_owned()));
            }
            _ => {
                return Err(LoomError::invalid_request(format!(
                    "unknown argument '{argument}'"
                )));
            }
        }
    }
    Ok(Some(options))
}

fn required_value(
    args: &mut impl Iterator<Item = String>,
    flag: &str,
) -> Result<String, LoomError> {
    args.next()
        .ok_or_else(|| LoomError::invalid_request(format!("{flag} requires a value")))
}

fn prepare_workspace(root: Option<PathBuf>) -> Result<PathBuf, LoomError> {
    let root = root.unwrap_or_else(|| env::temp_dir().join("loom-m1-demo"));
    fs::create_dir_all(&root).map_err(|error| {
        LoomError::new(
            ErrorCode::ToolExecution,
            format!("could not create workspace '{}': {error}", root.display()),
            false,
        )
    })?;
    if root.ends_with("loom-m1-demo") {
        let readme = root.join("README.md");
        if !readme.exists() {
            fs::write(
                &readme,
                "Workspace used by the Loom M1 deterministic demo.\n",
            )
            .map_err(|error| {
                LoomError::new(
                    ErrorCode::ToolExecution,
                    format!("could not seed demo workspace: {error}"),
                    false,
                )
            })?;
        }
    }
    Ok(root)
}

fn negotiate(connection: &InProcessConnection) -> Result<(), LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::Negotiate {
        client_version: CURRENT_PROTOCOL_VERSION,
        capabilities: CapabilitySet::new([
            Capability::CreateAgentSession,
            Capability::ReadAgentSession,
            Capability::SubscribeSessionEvents,
            Capability::StartAgentRun,
            Capability::ReadAgentRun,
            Capability::ControlAgentRun,
            Capability::ApproveAgentAction,
            Capability::JsonProtocol,
        ]),
    }));
    match response.result? {
        ServerResponse::Negotiated(_) => Ok(()),
        response => Err(unexpected_response("negotiation", response)),
    }
}

fn create_session(
    connection: &InProcessConnection,
    name: &str,
) -> Result<loom_core::AgentSessionSnapshot, LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::CreateAgentSession {
        project_id: ProjectId::new(),
        name: name.to_owned(),
    }));
    match response.result? {
        ServerResponse::AgentSessionCreated(snapshot) => Ok(snapshot),
        response => Err(unexpected_response("session creation", response)),
    }
}

fn start_run(
    connection: &InProcessConnection,
    session_id: AgentSessionId,
    options: &CliOptions,
    workspace_root: &Path,
) -> Result<RunId, LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::StartAgentRun {
        session_id,
        task: options.task.clone(),
        model: options.model.clone(),
        workspace_root: workspace_root.display().to_string(),
        system_instructions: Some(
            "Work methodically, use the available tools, and report validation.".to_owned(),
        ),
        repository_instructions: Some(
            "Keep the demonstration change small and workspace-scoped.".to_owned(),
        ),
    }));
    match response.result? {
        ServerResponse::AgentRunStarted(snapshot) => Ok(snapshot.id),
        response => Err(unexpected_response("agent run start", response)),
    }
}

fn stream_run(
    connection: &InProcessConnection,
    session_id: AgentSessionId,
    run_id: RunId,
    manual_approval: bool,
) -> Result<(), LoomError> {
    let mut after = None;
    loop {
        let response = connection.request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
            session_id: Some(session_id),
            after_sequence: after,
        }));
        let events = match response.result? {
            ServerResponse::SessionEvents { events } => events,
            response => return Err(unexpected_response("event stream", response)),
        };
        if events.is_empty() {
            return Err(LoomError::new(
                ErrorCode::Internal,
                "agent run produced no further events",
                false,
            ));
        }

        let mut completed = false;
        for event in events {
            after = Some(event.sequence);
            render_event(&event);
            if let ServerEvent::Agent {
                event:
                    AgentEvent::ToolApprovalRequired {
                        run_id: event_run_id,
                        call,
                    },
            } = &event.event
            {
                if *event_run_id != run_id {
                    continue;
                }
                let approved = if manual_approval {
                    prompt_for_approval(&call.name)?
                } else {
                    println!("  approval: automatically approved for the demo");
                    true
                };
                let request = if approved {
                    ClientRequest::ApproveAgentAction {
                        run_id,
                        tool_call_id: call.id,
                    }
                } else {
                    ClientRequest::RejectAgentAction {
                        run_id,
                        tool_call_id: call.id,
                        reason: Some("denied at the native shell".to_owned()),
                    }
                };
                let response = connection.request(RequestEnvelope::new(request));
                response.result?;
            }
            if matches!(
                &event.event,
                ServerEvent::Agent {
                    event: AgentEvent::RunCompleted { snapshot }
                } if snapshot.id == run_id
            ) {
                completed = true;
            }
        }
        if completed {
            let response =
                connection.request(RequestEnvelope::new(ClientRequest::GetAgentRun { run_id }));
            let ServerResponse::AgentRun(snapshot) = response.result? else {
                return Err(LoomError::new(
                    ErrorCode::Internal,
                    "backend returned an unexpected final run response",
                    false,
                ));
            };
            println!(
                "Final summary [{}]: {}",
                run_state_name(snapshot.state),
                snapshot.summary.unwrap_or_else(|| "none".to_owned())
            );
            return Ok(());
        }
    }
}

fn prompt_for_approval(tool_name: &str) -> Result<bool, LoomError> {
    print!("  approve {tool_name}? [a]pprove/[d]eny: ");
    io::stdout().flush().map_err(|error| {
        LoomError::new(
            ErrorCode::Internal,
            format!("could not flush approval prompt: {error}"),
            false,
        )
    })?;
    let mut input = String::new();
    io::stdin().read_line(&mut input).map_err(|error| {
        LoomError::new(
            ErrorCode::Internal,
            format!("could not read approval decision: {error}"),
            false,
        )
    })?;
    Ok(matches!(
        input.trim().to_ascii_lowercase().as_str(),
        "a" | "approve"
    ))
}

fn render_event(envelope: &loom_protocol::ServerEventEnvelope) {
    print!("#{:<3} ", envelope.sequence);
    match &envelope.event {
        ServerEvent::AgentSessionCreated { snapshot } => println!(
            "session created: {} [{}]",
            snapshot.id,
            session_state_name(snapshot.state)
        ),
        ServerEvent::AgentSessionStateChanged { current, .. } => {
            println!("      session state -> {}", session_state_name(*current));
        }
        ServerEvent::Agent { event } => match event {
            AgentEvent::RunStarted { snapshot } => {
                println!("Run {} [{}]", snapshot.id, run_state_name(snapshot.state));
            }
            AgentEvent::PlanProposed { plan, .. } => {
                println!("Plan:");
                for (index, step) in plan.steps.iter().enumerate() {
                    println!("  {}. {}", index + 1, step.description);
                }
            }
            AgentEvent::AssistantMessageDelta { text, .. } => {
                println!("Assistant: {}", text.trim_end());
            }
            AgentEvent::ToolCallRequested { call, .. } => {
                println!(
                    "Tool requested: {} {}",
                    call.name,
                    serde_json::to_string(&call.arguments).unwrap_or_default()
                );
            }
            AgentEvent::ToolApprovalRequired { call, .. } => {
                println!("Approval required: {}", call.name);
            }
            AgentEvent::ToolApprovalDecided { decision, .. } => {
                println!("Approval decision: {decision:?}");
            }
            AgentEvent::ToolCallStarted { call, .. } => {
                println!("Tool started: {}", call.name);
            }
            AgentEvent::ToolOutputChunk { chunk, .. } => {
                println!("Tool output:\n{chunk}");
            }
            AgentEvent::ToolCallCompleted { result, .. } => {
                println!(
                    "Tool completed: {} [{}]",
                    result.name,
                    if result.success { "ok" } else { "failed" }
                );
            }
            AgentEvent::RunUsage { usage, .. } => println!(
                "Usage: {} input / {} output tokens",
                usage.input_tokens, usage.output_tokens
            ),
            AgentEvent::RunStateChanged { state, .. } => {
                println!("Run state -> {}", run_state_name(*state));
            }
            AgentEvent::RunCompleted { snapshot } => {
                println!("Run completed [{}]", run_state_name(snapshot.state))
            }
        },
    }
}

fn unexpected_response(operation: &str, response: ServerResponse) -> LoomError {
    LoomError::new(
        ErrorCode::Internal,
        format!("backend returned unexpected {operation} response: {response:?}"),
        false,
    )
}

const fn session_state_name(state: AgentSessionState) -> &'static str {
    match state {
        AgentSessionState::Idle => "idle",
        AgentSessionState::Queued => "queued",
        AgentSessionState::Planning => "planning",
        AgentSessionState::AwaitingApproval => "awaiting_approval",
        AgentSessionState::Executing => "executing",
        AgentSessionState::Evaluating => "evaluating",
        AgentSessionState::NeedsInput => "needs_input",
        AgentSessionState::Completed => "completed",
        AgentSessionState::Failed => "failed",
        AgentSessionState::Cancelled => "cancelled",
    }
}

const fn run_state_name(state: AgentRunState) -> &'static str {
    match state {
        AgentRunState::Planning => "planning",
        AgentRunState::Executing => "executing",
        AgentRunState::AwaitingApproval => "awaiting_approval",
        AgentRunState::Evaluating => "evaluating",
        AgentRunState::Completed => "completed",
        AgentRunState::Failed => "failed",
        AgentRunState::Cancelled => "cancelled",
    }
}
