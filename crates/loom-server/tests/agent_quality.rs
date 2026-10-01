//! Task-level quality measurements and thresholds (audit B5).
//!
//! These fixtures exercise representative agent tasks end to end on the
//! deterministic provider so the five user-facing signals in
//! `docs/quality.md` are measured with explicit thresholds. Run with:
//!
//! ```sh
//! cargo test -p loom-server --test agent_quality -- --nocapture
//! ```

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use loom_agent::{AgentEvent, AgentRunState, AgentRuntime, AgentTask};
use loom_core::ApprovalPolicy;
use loom_model::{FinishReason, ModelId, ModelStreamEvent, ToolCall, ToolDefinition};
use loom_providers::{DeterministicProvider, deterministic_descriptor};
use loom_tools::{ToolExecutor, ToolResult};

/// Every representative task must reach `Completed`.
const MIN_SUCCESS_RATE: f64 = 1.0;
/// Every task that hit a tool error must still recover and complete.
const MIN_TOOL_ERROR_RECOVERY_RATE: f64 = 1.0;
/// A task must not take more model turns than this.
const MAX_TURNS_PER_TASK: usize = 8;
/// Time from starting a task to the first streamed transcript delta.
const MAX_TIME_TO_FIRST_OUTPUT: Duration = Duration::from_secs(10);
/// Median `search_text` latency over the fixture workspace.
const MAX_SEARCH_LATENCY_P50: Duration = Duration::from_secs(2);

struct TaskMeasurement {
    name: &'static str,
    turns: usize,
    time_to_first_output: Duration,
    tool_errors: usize,
    completed: bool,
}

impl TaskMeasurement {
    fn recovered(&self) -> bool {
        self.tool_errors > 0 && self.completed
    }
}

/// The scripted tasks used by the quality gate. Each fixture is a small but
/// representative slice of real work: exploration, a mutating edit followed by
/// validation, and a failed tool call that the agent must recover from.
fn fixtures() -> Vec<(&'static str, Vec<Vec<ModelStreamEvent>>)> {
    let tool_call = |name: &str, arguments: serde_json::Value| ModelStreamEvent::ToolCallDelta {
        call: ToolCall {
            id: loom_core::ToolCallId::new(),
            name: name.to_owned(),
            arguments,
        },
    };
    let text = |value: &str| ModelStreamEvent::TextDelta {
        text: value.to_owned(),
    };
    let tool_turn = || ModelStreamEvent::Completed {
        reason: FinishReason::ToolCall,
    };
    let stop_turn = || ModelStreamEvent::Completed {
        reason: FinishReason::Stop,
    };

    vec![
        (
            "inspect_workspace",
            vec![
                vec![
                    text("I'll inspect the workspace before answering.\n"),
                    tool_call("list_files", serde_json::json!({"path": "."})),
                    tool_call("read_file", serde_json::json!({"path": "README.md"})),
                    tool_turn(),
                ],
                vec![text("The workspace contains a README.\n"), stop_turn()],
            ],
        ),
        (
            "edit_and_validate",
            vec![
                vec![
                    text("I'll apply the requested change.\n"),
                    tool_call(
                        "apply_patch",
                        serde_json::json!({
                            "path": "result.txt",
                            "old_text": "",
                            "new_text": "done\n",
                        }),
                    ),
                    tool_turn(),
                ],
                vec![
                    text("I'll validate the change.\n"),
                    tool_call(
                        "run_command",
                        serde_json::json!({"command": "echo", "args": ["ok"]}),
                    ),
                    tool_turn(),
                ],
                vec![text("The task is complete.\n"), stop_turn()],
            ],
        ),
        (
            "recover_from_tool_error",
            vec![
                vec![
                    text("I'll edit the file.\n"),
                    tool_call(
                        "apply_patch",
                        serde_json::json!({
                            "path": "recovered.txt",
                            "old_text": "text that is not present",
                            "new_text": "attempted\n",
                        }),
                    ),
                    tool_turn(),
                ],
                vec![
                    text("That failed; I'll create the file instead.\n"),
                    tool_call(
                        "apply_patch",
                        serde_json::json!({
                            "path": "recovered.txt",
                            "old_text": "",
                            "new_text": "recovered\n",
                        }),
                    ),
                    tool_turn(),
                ],
                vec![text("The file is now created.\n"), stop_turn()],
            ],
        ),
    ]
}

fn workspace(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "loom-quality-{name}-{}",
        loom_core::AgentSessionId::new()
    ));
    fs::create_dir_all(&root).unwrap();
    root
}

fn run_fixture(
    name: &'static str,
    steps: Vec<Vec<ModelStreamEvent>>,
    root: &Path,
) -> TaskMeasurement {
    // Give the exploration fixture a file to read so its tool calls succeed.
    fs::write(root.join("README.md"), "# fixture workspace\n").unwrap();
    let tools = ToolExecutor::new(root).unwrap();
    let task = AgentTask::new(name, ModelId::new("deterministic/demo")).unwrap();
    let provider = DeterministicProvider {
        descriptor: deterministic_descriptor(),
        steps,
        cursor: 0,
    };
    let mut runtime = AgentRuntime::new_with_policy(
        loom_core::AgentSessionId::new(),
        task,
        Box::new(provider),
        tools,
        ApprovalPolicy::auto_approve(),
    );

    let first_output = Arc::new(Mutex::new(None::<Duration>));
    let started = Instant::now();
    let observed = Arc::clone(&first_output);
    runtime.set_event_observer(Arc::new(move |event: &AgentEvent| {
        if matches!(event, AgentEvent::AssistantMessageDelta { .. }) {
            let mut observed = observed.lock().unwrap_or_else(|error| error.into_inner());
            if observed.is_none() {
                *observed = Some(started.elapsed());
            }
        }
    }));

    let events = runtime.start().expect("fixture run should not fail");
    let time_to_first_output = first_output
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .unwrap_or_else(|| started.elapsed());
    let turns = events
        .iter()
        .filter(|event| matches!(event, AgentEvent::StepStarted { .. }))
        .count();
    let tool_errors = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                AgentEvent::ToolCallCompleted { result, .. } if !result.success
            )
        })
        .count();

    TaskMeasurement {
        name,
        turns,
        time_to_first_output,
        tool_errors,
        completed: runtime.snapshot().state == AgentRunState::Completed,
    }
}

fn search_latency(root: &Path) -> Duration {
    // A workspace large enough that search traversal is measurable but small
    // enough to stay fast on a loaded CI runner.
    for directory in 0..20 {
        let directory = root.join(format!("dir-{directory}"));
        fs::create_dir_all(&directory).unwrap();
        for file in 0..15 {
            let contents = format!(
                "module fixture_{file};\n\n// filler line\n// filler line\n{}\n",
                if file == 7 {
                    "let needle = 1;"
                } else {
                    "let value = 0;"
                }
            );
            fs::write(directory.join(format!("file-{file}.rs")), contents).unwrap();
        }
    }
    let executor = ToolExecutor::new(root).unwrap();
    let call = ToolCall {
        id: loom_core::ToolCallId::new(),
        name: "search_text".to_owned(),
        arguments: serde_json::json!({"query": "needle", "regex": false}),
    };
    let mut samples = Vec::new();
    for _ in 0..20 {
        let started = Instant::now();
        let result = executor.execute_with_cancel(&call, &loom_model::CancellationToken::new());
        assert!(result.success, "search fixture should succeed");
        samples.push(started.elapsed());
    }
    samples.sort_unstable();
    samples[samples.len() / 2]
}

#[test]
fn representative_tasks_meet_the_quality_thresholds() {
    let meetings = fixtures()
        .into_iter()
        .map(|(name, steps)| {
            let root = workspace(name);
            let measurement = run_fixture(name, steps, &root);
            let _ = fs::remove_dir_all(&root);
            measurement
        })
        .collect::<Vec<_>>();

    let search_root = workspace("search");
    let search_p50 = search_latency(&search_root);
    let _ = fs::remove_dir_all(&search_root);

    let total = meetings.len();
    let completed = meetings.iter().filter(|task| task.completed).count();
    let success_rate = completed as f64 / total as f64;
    let errored = meetings.iter().filter(|task| task.tool_errors > 0).count();
    let recovered = meetings.iter().filter(|task| task.recovered()).count();
    let recovery_rate = if errored == 0 {
        1.0
    } else {
        recovered as f64 / errored as f64
    };
    let max_turns = meetings.iter().map(|task| task.turns).max().unwrap_or(0);
    let max_first_output = meetings
        .iter()
        .map(|task| task.time_to_first_output)
        .max()
        .unwrap_or_default();

    println!("task quality measurements:");
    for task in &meetings {
        println!(
            "  {:<24} completed={:<5} turns={:<2} first_output={:>8.3}ms tool_errors={}",
            task.name,
            task.completed,
            task.turns,
            task.time_to_first_output.as_secs_f64() * 1000.0,
            task.tool_errors,
        );
    }
    println!(
        "  rates: success={success_rate:.2} tool_error_recovery={recovery_rate:.2} \
         max_turns={max_turns} max_first_output={:.3}ms search_p50={:.3}ms",
        max_first_output.as_secs_f64() * 1000.0,
        search_p50.as_secs_f64() * 1000.0,
    );
    println!(
        "  thresholds: success>={MIN_SUCCESS_RATE:.2} recovery>={MIN_TOOL_ERROR_RECOVERY_RATE:.2} \
         turns<={MAX_TURNS_PER_TASK} first_output<={}ms search_p50<={}ms",
        MAX_TIME_TO_FIRST_OUTPUT.as_millis(),
        MAX_SEARCH_LATENCY_P50.as_millis(),
    );

    assert!(
        success_rate >= MIN_SUCCESS_RATE,
        "successful-completion rate {success_rate:.2} is below {MIN_SUCCESS_RATE:.2}"
    );
    assert!(
        recovery_rate >= MIN_TOOL_ERROR_RECOVERY_RATE,
        "tool-error recovery rate {recovery_rate:.2} is below {MIN_TOOL_ERROR_RECOVERY_RATE:.2}"
    );
    assert!(
        max_turns <= MAX_TURNS_PER_TASK,
        "a task used {max_turns} turns, above the {MAX_TURNS_PER_TASK} limit"
    );
    assert!(
        max_first_output <= MAX_TIME_TO_FIRST_OUTPUT,
        "time to first streamed output {max_first_output:?} is above {MAX_TIME_TO_FIRST_OUTPUT:?}"
    );
    assert!(
        search_p50 <= MAX_SEARCH_LATENCY_P50,
        "search p50 latency {search_p50:?} is above {MAX_SEARCH_LATENCY_P50:?}"
    );
}

/// The search fixture must actually find the needle, so a silently empty search
/// cannot make the latency gate pass vacuously.
#[test]
fn search_fixture_finds_the_needle() {
    let root = workspace("search-content");
    for file in 0..3 {
        fs::write(
            root.join(format!("file-{file}.rs")),
            if file == 1 {
                "let needle = 1;\n"
            } else {
                "let value = 0;\n"
            },
        )
        .unwrap();
    }
    let executor = ToolExecutor::new(&root).unwrap();
    let call = ToolCall {
        id: loom_core::ToolCallId::new(),
        name: "search_text".to_owned(),
        arguments: serde_json::json!({"query": "needle"}),
    };
    let result: ToolResult =
        executor.execute_with_cancel(&call, &loom_model::CancellationToken::new());
    assert!(result.success);
    assert!(
        result.output.contains("needle"),
        "output was {:?}",
        result.output
    );
    let _ = fs::remove_dir_all(&root);
}

/// A definition guard so the fixture list stays aligned with the tool schema
/// the guidance is generated from.
#[test]
fn fixture_tools_are_advertised() {
    let root = workspace("schema");
    let executor = ToolExecutor::new(&root).unwrap();
    let names = executor
        .definitions()
        .into_iter()
        .map(|definition: ToolDefinition| definition.name)
        .collect::<Vec<_>>();
    for expected in [
        "list_files",
        "read_file",
        "search_text",
        "apply_patch",
        "run_command",
    ] {
        assert!(
            names.iter().any(|name| name == expected),
            "{expected} must remain an advertised tool"
        );
    }
    let _ = fs::remove_dir_all(&root);
}
