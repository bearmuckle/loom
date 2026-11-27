//! End-to-end ACP host tests using a real compiled stdio JSON-RPC stub harness.
//!
//! When invoked with `LOOM_ACP_STUB_MODE=1`, this test binary acts as the stub
//! ACP agent over stdin/stdout; otherwise it runs the test suite against itself.

use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use loom_core::{ANTIGRAVITY_AUTH_GEMINI_API_KEY, ANTIGRAVITY_AUTH_OAUTH_PERSONAL, ErrorCode};
use loom_harness::acp::{
    AcpClient, AcpCommandSpec, AcpEvent, AcpNext, AcpOptions, AcpPermissionOutcome,
    LOOM_ACP_CLIENT_NAME, LOOM_ACP_CLIENT_VERSION,
};
use serde_json::Value;

fn stub_command() -> AcpCommandSpec {
    let exe = std::env::current_exe().expect("test executable path");
    AcpCommandSpec {
        program: exe,
        args: Vec::new(),
        display: "acp-stub".to_owned(),
        supports_agy_profile_flags: false,
    }
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "loom-acp-host-{label}-{}",
        loom_core::RequestId::new()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn spawn_stub(cwd: &Path, profile_dir: Option<PathBuf>, extra_env: &[(&str, &str)]) -> AcpClient {
    let mut env = vec![("LOOM_ACP_STUB_MODE".to_owned(), "1".to_owned())];
    for (k, v) in extra_env {
        env.push(((*k).to_owned(), (*v).to_owned()));
    }
    AcpClient::spawn(&AcpOptions {
        command: stub_command(),
        cwd: cwd.to_path_buf(),
        profile_dir,
        env,
    })
    .expect("spawn stub ACP harness")
}

fn test_initialize_client_info_and_auth_methods() {
    let dir = temp_dir("init-auth");
    let record = dir.join("record.jsonl");
    let profile = dir.join("profile");

    // 1. Default auth method is oauth-personal, and clientInfo is Loom's own.
    {
        let mut client = spawn_stub(
            &dir,
            Some(profile.clone()),
            &[("LOOM_ACP_STUB_RECORD", record.to_str().unwrap())],
        );
        let init = client.initialize(Duration::from_secs(5)).unwrap();
        assert_eq!(init.protocol_version, 1);
        assert_eq!(init.agent_name.as_deref(), Some("antigravity-stub"));
        let chosen = client
            .authenticate(
                None,
                &init.auth_methods,
                Duration::from_secs(5),
                None,
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(chosen, ANTIGRAVITY_AUTH_OAUTH_PERSONAL);

        let session = client.session_new(&dir, Duration::from_secs(5)).unwrap();
        assert_eq!(session.session_id, "acp-stub-sess-1");
        assert_eq!(session.models.len(), 2);
        assert_eq!(session.models[0].id, "gemini-catalog-pro");
        client.kill();
    }

    let lines = fs::read_to_string(&record)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .collect::<Vec<_>>();
    let init_req = lines
        .iter()
        .find(|v| v.get("method").and_then(Value::as_str) == Some("initialize"))
        .expect("initialize recorded");
    assert_eq!(
        init_req
            .pointer("/params/clientInfo/name")
            .and_then(Value::as_str),
        Some(LOOM_ACP_CLIENT_NAME)
    );
    assert_eq!(
        init_req
            .pointer("/params/clientInfo/version")
            .and_then(Value::as_str),
        Some(LOOM_ACP_CLIENT_VERSION)
    );
    let init_text = init_req.to_string().to_ascii_lowercase();
    assert!(!init_text.contains("t3code"));
    assert!(!init_text.contains("t3-code"));
    assert!(profile.is_dir(), "isolated profile directory was created");

    // 2. Explicit gemini-api-key auth method works without Loom reading GEMINI_API_KEY.
    {
        let mut client = spawn_stub(&dir, None, &[]);
        let init = client.initialize(Duration::from_secs(5)).unwrap();
        let chosen = client
            .authenticate(
                Some(ANTIGRAVITY_AUTH_GEMINI_API_KEY),
                &init.auth_methods,
                Duration::from_secs(5),
                None,
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(chosen, ANTIGRAVITY_AUTH_GEMINI_API_KEY);
        client.kill();
    }

    // 3. Unadvertised auth method fails clearly with hint to use gemini-api-key.
    {
        let mut client = spawn_stub(
            &dir,
            None,
            &[("LOOM_ACP_STUB_AUTH_METHODS", "gemini-api-key")],
        );
        let init = client.initialize(Duration::from_secs(5)).unwrap();
        let err = client
            .authenticate(
                None,
                &init.auth_methods,
                Duration::from_secs(5),
                None,
                &mut |_| {},
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::ProviderAuthentication);
        assert!(err.message.contains("oauth-personal"));
        assert!(err.message.contains("gemini-api-key"));
        client.kill();
    }

    // 4. Sign-in failure surfaces clearly and never hangs.
    {
        let mut client = spawn_stub(&dir, None, &[("LOOM_ACP_STUB_FAIL_AUTH", "browser closed")]);
        let init = client.initialize(Duration::from_secs(5)).unwrap();
        let err = client
            .authenticate(
                None,
                &init.auth_methods,
                Duration::from_secs(5),
                None,
                &mut |_| {},
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::ProviderAuthentication);
        assert!(err.message.contains("browser closed"));
        assert!(err.message.contains("gemini-api-key"));
        client.kill();
    }

    let _ = fs::remove_dir_all(&dir);
}

fn test_prompt_chunks_tool_permission_allow_and_deny() {
    let dir = temp_dir("permission");

    // Allow path
    {
        let mut client = spawn_stub(&dir, None, &[("LOOM_ACP_STUB_SCENARIO", "permission")]);
        let init = client.initialize(Duration::from_secs(5)).unwrap();
        client
            .authenticate(
                None,
                &init.auth_methods,
                Duration::from_secs(5),
                None,
                &mut |_| {},
            )
            .unwrap();
        let session = client.session_new(&dir, Duration::from_secs(5)).unwrap();
        let req_id = client
            .start_prompt(&session.session_id, "Run tests")
            .unwrap();

        let mut saw_tool_call = false;
        let mut saw_permission = false;
        let mut text = String::new();
        let mut finished = false;
        for _ in 0..50 {
            match client.next_event(Duration::from_millis(200)) {
                AcpNext::Event(event) => match *event {
                    AcpEvent::ToolCall(_) => saw_tool_call = true,
                    AcpEvent::PermissionRequest(req) => {
                        saw_permission = true;
                        let allow = req.allow_option_id().unwrap().to_owned();
                        assert_eq!(allow, "opt-allow-once");
                        client
                            .respond_permission(
                                &req.request_id,
                                &AcpPermissionOutcome::Selected { option_id: allow },
                            )
                            .unwrap();
                    }
                    AcpEvent::MessageChunk(chunk) => text.push_str(&chunk),
                    AcpEvent::PromptFinished(res) => {
                        assert_eq!(res.request_id, req_id);
                        assert_eq!(res.stop_reason, "end_turn");
                        finished = true;
                        break;
                    }
                    _ => {}
                },
                AcpNext::Idle => continue,
                AcpNext::Exited { .. } => panic!("unexpected exit"),
            }
        }
        assert!(saw_tool_call && saw_permission && finished);
        assert_eq!(text, "command allowed and executed");
        client.kill();
    }

    // Deny path
    {
        let mut client = spawn_stub(&dir, None, &[("LOOM_ACP_STUB_SCENARIO", "permission")]);
        let init = client.initialize(Duration::from_secs(5)).unwrap();
        client
            .authenticate(
                None,
                &init.auth_methods,
                Duration::from_secs(5),
                None,
                &mut |_| {},
            )
            .unwrap();
        let session = client.session_new(&dir, Duration::from_secs(5)).unwrap();
        let _ = client
            .start_prompt(&session.session_id, "Run tests")
            .unwrap();

        let mut text = String::new();
        let mut finished = false;
        for _ in 0..50 {
            match client.next_event(Duration::from_millis(200)) {
                AcpNext::Event(event) => match *event {
                    AcpEvent::PermissionRequest(req) => {
                        let reject = req.reject_option_id().unwrap().to_owned();
                        assert_eq!(reject, "opt-reject-once");
                        client
                            .respond_permission(
                                &req.request_id,
                                &AcpPermissionOutcome::Selected { option_id: reject },
                            )
                            .unwrap();
                    }
                    AcpEvent::MessageChunk(chunk) => text.push_str(&chunk),
                    AcpEvent::PromptFinished(_) => {
                        finished = true;
                        break;
                    }
                    _ => {}
                },
                AcpNext::Idle => continue,
                AcpNext::Exited { .. } => panic!("unexpected exit"),
            }
        }
        assert!(finished);
        assert_eq!(text, "command rejected by operator");
        client.kill();
    }

    let _ = fs::remove_dir_all(&dir);
}

fn test_cancel_mid_turn_and_harness_crash_and_missing_binary() {
    let dir = temp_dir("cancel-crash");

    // Cancel mid-turn
    {
        let mut client = spawn_stub(&dir, None, &[("LOOM_ACP_STUB_SCENARIO", "cancel")]);
        let init = client.initialize(Duration::from_secs(5)).unwrap();
        client
            .authenticate(
                None,
                &init.auth_methods,
                Duration::from_secs(5),
                None,
                &mut |_| {},
            )
            .unwrap();
        let session = client.session_new(&dir, Duration::from_secs(5)).unwrap();
        let _ = client.start_prompt(&session.session_id, "Work").unwrap();

        let mut cancelled = false;
        for _ in 0..50 {
            match client.next_event(Duration::from_millis(200)) {
                AcpNext::Event(event) => match *event {
                    AcpEvent::MessageChunk(_) => {
                        client.cancel_session(&session.session_id).unwrap();
                    }
                    AcpEvent::PromptFinished(res) => {
                        assert_eq!(res.stop_reason, "cancelled");
                        cancelled = true;
                        break;
                    }
                    other => panic!("unexpected event during cancel: {other:?}"),
                },
                AcpNext::Idle => continue,
                other => panic!("unexpected event during cancel: {other:?}"),
            }
        }
        assert!(cancelled);
        client.kill();
    }

    // Harness exits mid-run
    {
        let mut client = spawn_stub(&dir, None, &[("LOOM_ACP_STUB_SCENARIO", "crash")]);
        let init = client.initialize(Duration::from_secs(5)).unwrap();
        client
            .authenticate(
                None,
                &init.auth_methods,
                Duration::from_secs(5),
                None,
                &mut |_| {},
            )
            .unwrap();
        let session = client.session_new(&dir, Duration::from_secs(5)).unwrap();
        let _ = client.start_prompt(&session.session_id, "Crash").unwrap();

        let mut saw_exit = false;
        for _ in 0..50 {
            if let AcpNext::Exited {
                exit_code,
                stderr_tail,
            } = client.next_event(Duration::from_millis(200))
            {
                assert_eq!(exit_code, Some(42));
                assert!(stderr_tail.contains("fatal harness crash"));
                saw_exit = true;
                break;
            }
        }
        assert!(saw_exit);
    }

    // Missing binary fails immediately with ProviderUnavailable
    {
        let err = AcpClient::spawn(&AcpOptions {
            command: AcpCommandSpec {
                program: PathBuf::from("/nonexistent/loom-missing-agy"),
                args: vec!["acp".to_owned()],
                display: "/nonexistent/loom-missing-agy acp".to_owned(),
                supports_agy_profile_flags: false,
            },
            cwd: dir.clone(),
            profile_dir: None,
            env: Vec::new(),
        })
        .err()
        .expect("missing binary should fail");
        assert_eq!(err.code, ErrorCode::ProviderUnavailable);
        assert!(err.message.contains("gemini-api-key"));
    }

    let _ = fs::remove_dir_all(&dir);
}

fn main() {
    if std::env::var_os("LOOM_ACP_STUB_MODE").is_some() {
        loom_harness::acp::stub::run_stub_stdio();
        return;
    }
    let tests: &[(&str, fn())] = &[
        (
            "initialize_client_info_and_auth_methods",
            test_initialize_client_info_and_auth_methods,
        ),
        (
            "prompt_chunks_tool_permission_allow_and_deny",
            test_prompt_chunks_tool_permission_allow_and_deny,
        ),
        (
            "cancel_mid_turn_and_harness_crash_and_missing_binary",
            test_cancel_mid_turn_and_harness_crash_and_missing_binary,
        ),
    ];
    if std::env::args().any(|arg| arg == "--list") {
        for (name, _) in tests {
            println!("{name}: test");
        }
        return;
    }
    for (name, test_fn) in tests {
        test_fn();
        println!("test {name} ... ok");
    }
}
