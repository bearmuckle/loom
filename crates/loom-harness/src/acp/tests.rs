use std::{ffi::OsString, fs};

use loom_core::{
    ANTIGRAVITY_AUTH_GEMINI_API_KEY, ANTIGRAVITY_AUTH_OAUTH_PERSONAL, ANTIGRAVITY_HARNESS_ID,
    ErrorCode,
};
use serde_json::json;

use super::{
    AcpAuthMethod, AcpEvent, AcpPermissionOption, AcpPermissionRequest, AntigravityRunContext,
    LOOM_ACP_CLIENT_NAME, LOOM_ACP_CLIENT_VERSION, antigravity_notices,
    check_interactive_only_with_env, client, discover_antigravity_command, redact_acp_text,
    validate_auth_method,
};

#[test]
fn client_info_is_looms_own_identity_and_not_t3_code() {
    assert_eq!(LOOM_ACP_CLIENT_NAME, "loom");
    assert!(!LOOM_ACP_CLIENT_VERSION.is_empty());
    let lower = LOOM_ACP_CLIENT_NAME.to_ascii_lowercase();
    assert!(!lower.contains("t3"));
    assert!(!lower.contains("ping"));
    assert!(!lower.contains("antigravity"));
}

#[test]
fn discovery_prefers_manual_override_then_env_then_path() {
    let dir =
        std::env::temp_dir().join(format!("loom-acp-discover-{}", loom_core::RequestId::new()));
    fs::create_dir_all(&dir).unwrap();
    let agy_path = dir.join("agy");
    fs::write(&agy_path, "#!/bin/sh\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&agy_path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let path_env = OsString::from(dir.as_os_str());
    let from_path = discover_antigravity_command(None, None, Some(&path_env)).unwrap();
    assert_eq!(from_path.program, agy_path);
    assert_eq!(from_path.args, vec!["acp", "--stdio"]);
    assert!(from_path.supports_agy_profile_flags);

    let from_env =
        discover_antigravity_command(None, Some("/custom/bin/antigravity-acp"), Some(&path_env))
            .unwrap();
    assert_eq!(
        from_env.program.to_string_lossy(),
        "/custom/bin/antigravity-acp"
    );
    assert!(from_env.args.is_empty());
    assert!(!from_env.supports_agy_profile_flags);

    let from_override =
        discover_antigravity_command(Some("/opt/agy"), Some("/custom/bin/other"), Some(&path_env))
            .unwrap();
    assert_eq!(from_override.program.to_string_lossy(), "/opt/agy");
    assert_eq!(from_override.args, vec!["acp", "--stdio"]);
    assert!(from_override.supports_agy_profile_flags);

    let missing =
        discover_antigravity_command(None, None, Some(std::ffi::OsStr::new(""))).unwrap_err();
    assert_eq!(missing.code, ErrorCode::ProviderUnavailable);
    assert!(missing.message.contains("agy"));
    assert!(missing.message.contains("gemini-api-key"));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn discovery_falls_back_to_antigravity_acp_binary_on_path() {
    let dir =
        std::env::temp_dir().join(format!("loom-acp-fallback-{}", loom_core::RequestId::new()));
    fs::create_dir_all(&dir).unwrap();
    let bin_path = dir.join("antigravity-acp");
    fs::write(&bin_path, "#!/bin/sh\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&bin_path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let spec = discover_antigravity_command(None, None, Some(dir.as_os_str())).unwrap();
    assert_eq!(spec.program, bin_path);
    assert_eq!(spec.args, vec!["--stdio"]);
    assert_eq!(spec.display, "antigravity-acp");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn auth_method_validation_defaults_to_oauth_personal_and_rejects_unadvertised() {
    let advertised = vec![
        AcpAuthMethod {
            id: ANTIGRAVITY_AUTH_OAUTH_PERSONAL.to_owned(),
            name: "Google Sign-In".to_owned(),
            description: None,
        },
        AcpAuthMethod {
            id: ANTIGRAVITY_AUTH_GEMINI_API_KEY.to_owned(),
            name: "Gemini API Key".to_owned(),
            description: None,
        },
    ];
    assert_eq!(
        validate_auth_method(None, &advertised).unwrap(),
        ANTIGRAVITY_AUTH_OAUTH_PERSONAL
    );
    assert_eq!(
        validate_auth_method(Some(ANTIGRAVITY_AUTH_GEMINI_API_KEY), &advertised).unwrap(),
        ANTIGRAVITY_AUTH_GEMINI_API_KEY
    );

    let err = validate_auth_method(Some("oauth-business"), &advertised).unwrap_err();
    assert_eq!(err.code, ErrorCode::ProviderAuthentication);
    assert!(err.message.contains("oauth-business"));
    assert!(err.message.contains("gemini-api-key"));
}

#[test]
fn interactive_only_guard_refuses_headless_auto_approve_delegated_and_ci() {
    let interactive_ok = AntigravityRunContext {
        interactive: true,
        auto_approve_actions: false,
        delegated_or_scheduled: false,
    };
    assert!(
        check_interactive_only_with_env(ANTIGRAVITY_HARNESS_ID, interactive_ok, |_| None).is_ok()
    );

    let headless = AntigravityRunContext {
        interactive: false,
        ..interactive_ok
    };
    let err =
        check_interactive_only_with_env(ANTIGRAVITY_HARNESS_ID, headless, |_| None).unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidRequest);
    assert!(err.message.contains("interactive-only"));

    let auto_approve = AntigravityRunContext {
        auto_approve_actions: true,
        ..interactive_ok
    };
    let err = check_interactive_only_with_env(ANTIGRAVITY_HARNESS_ID, auto_approve, |_| None)
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidRequest);
    assert!(err.message.contains("auto-approve"));

    let delegated = AntigravityRunContext {
        delegated_or_scheduled: true,
        ..interactive_ok
    };
    let err =
        check_interactive_only_with_env(ANTIGRAVITY_HARNESS_ID, delegated, |_| None).unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidRequest);

    for env_key in ["CI", "LOOM_UNATTENDED", "LOOM_HEADLESS", "LOOM_SCHEDULED"] {
        let err = check_interactive_only_with_env(ANTIGRAVITY_HARNESS_ID, interactive_ok, |k| {
            (k == env_key).then(|| "true".to_owned())
        })
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest, "env {env_key}");
        assert!(err.message.contains(env_key), "env {env_key}");
    }
}

#[test]
fn policy_notices_include_terms_interactions_and_unregistered_client() {
    let joined = antigravity_notices().join("\n").to_ascii_lowercase();
    assert!(joined.contains("terms"));
    assert!(joined.contains("interactions"));
    assert!(joined.contains("not a google-registered acp client"));
    assert!(joined.contains("interactive-only"));
}

#[test]
fn initialize_and_session_catalog_parsing_handles_models_and_config_options() {
    let init = client::parse_initialize_result(&json!({
        "protocolVersion": 1,
        "agentInfo": { "name": "antigravity-acp", "version": "1.2.3" },
        "agentCapabilities": {
            "loadSession": false,
            "sessionCapabilities": { "resume": {} },
            "promptCapabilities": { "image": true }
        },
        "authMethods": [
            { "id": "oauth-personal", "name": "Google Account" },
            { "id": "gemini-api-key", "name": "Gemini API Key" }
        ]
    }))
    .unwrap();
    assert_eq!(init.protocol_version, 1);
    assert_eq!(init.agent_name.as_deref(), Some("antigravity-acp"));
    assert!(!init.capabilities.load_session);
    assert!(init.capabilities.resume_session);
    assert!(init.capabilities.supports_resume());
    assert!(init.capabilities.prompt_image);
    assert_eq!(init.auth_methods.len(), 2);

    let from_config = client::parse_session_info(
        &json!({
            "sessionId": "s-cfg",
            "configOptions": [
                {
                    "id": "model",
                    "category": "model",
                    "currentValue": "gemini-2.5-pro",
                    "options": [
                        { "value": "gemini-2.5-pro", "name": "Gemini 2.5 Pro" },
                        { "value": "gemini-2.5-flash", "name": "Gemini 2.5 Flash" }
                    ]
                }
            ]
        }),
        None,
    )
    .unwrap();
    assert_eq!(from_config.session_id, "s-cfg");
    assert_eq!(
        from_config.current_model_id.as_deref(),
        Some("gemini-2.5-pro")
    );
    assert_eq!(from_config.model_config_option_id.as_deref(), Some("model"));
    assert_eq!(from_config.models.len(), 2);
}

#[test]
fn permission_request_selects_allow_and_reject_option_ids() {
    let request = AcpPermissionRequest {
        request_id: json!(7),
        session_id: Some("s-1".to_owned()),
        tool_call_id: Some("tc-1".to_owned()),
        title: "Run test".to_owned(),
        kind: Some("execute".to_owned()),
        raw_input: json!({ "command": "cargo test" }),
        options: vec![
            AcpPermissionOption {
                option_id: "allow-1".to_owned(),
                name: "Allow once".to_owned(),
                kind: "allow_once".to_owned(),
            },
            AcpPermissionOption {
                option_id: "deny-1".to_owned(),
                name: "Reject once".to_owned(),
                kind: "reject_once".to_owned(),
            },
        ],
    };
    assert_eq!(request.allow_option_id(), Some("allow-1"));
    assert_eq!(request.reject_option_id(), Some("deny-1"));

    // R6: allow_option_id must NEVER pick allow_always or an arbitrary first option.
    let allow_always_only = AcpPermissionRequest {
        request_id: json!(2),
        session_id: None,
        tool_call_id: None,
        title: "Run test".to_owned(),
        kind: None,
        raw_input: json!({}),
        options: vec![
            AcpPermissionOption {
                option_id: "always-1".to_owned(),
                name: "Always allow".to_owned(),
                kind: "allow_always".to_owned(),
            },
            AcpPermissionOption {
                option_id: "deny-1".to_owned(),
                name: "Reject".to_owned(),
                kind: "reject_once".to_owned(),
            },
        ],
    };
    assert_eq!(allow_always_only.allow_option_id(), None);
    assert_eq!(allow_always_only.reject_option_id(), Some("deny-1"));

    let reject_only = AcpPermissionRequest {
        request_id: json!(3),
        session_id: None,
        tool_call_id: None,
        title: "Run test".to_owned(),
        kind: None,
        raw_input: json!({}),
        options: vec![AcpPermissionOption {
            option_id: "deny-1".to_owned(),
            name: "Reject".to_owned(),
            kind: "reject_once".to_owned(),
        }],
    };
    assert_eq!(reject_only.allow_option_id(), None);
}

#[test]
fn command_override_display_never_includes_raw_args_or_env_assignments() {
    let spec = discover_antigravity_command(
        Some("env SECRET_KEY=topsecret /opt/tools/agy-wrapper --flag=value"),
        None,
        None,
    )
    .unwrap();
    assert_eq!(spec.display, "env");
    assert!(!spec.display.contains("SECRET_KEY"));
    assert!(!spec.display.contains("topsecret"));
    assert!(!spec.display.contains("--flag"));

    let agy_spec =
        discover_antigravity_command(Some("/usr/local/bin/agy acp --custom"), None, None).unwrap();
    assert_eq!(agy_spec.display, "agy acp");
    assert!(!agy_spec.display.contains("--custom"));
}

#[test]
fn stderr_redactor_masks_google_keys_oauth_tokens_and_query_secrets() {
    let raw = "Error: Bearer ya29.a0AfH6SM_secret_tok key=AIzaSyD1234567890abcdef1234567890 \
               https://accounts.google.com/o/oauth2/v2/auth?client_id=public123&code=4/0AX_secret&access_token=tok_abc&refresh_token=ref_def \
               GEMINI_API_KEY=AIzaSyAnotherKey9876543210";
    let redacted = redact_acp_text(raw);
    assert!(!redacted.contains("ya29.a0AfH6SM_secret_tok"));
    assert!(!redacted.contains("AIzaSyD1234567890abcdef1234567890"));
    assert!(!redacted.contains("4/0AX_secret"));
    assert!(!redacted.contains("tok_abc"));
    assert!(!redacted.contains("ref_def"));
    assert!(!redacted.contains("AIzaSyAnotherKey9876543210"));
    // Non-secret login URL prefix and client_id stay readable for the operator.
    assert!(redacted.contains("https://accounts.google.com/o/oauth2/v2/auth?client_id=public123"));
    assert!(redacted.contains("Bearer [REDACTED]"));
    assert!(redacted.contains("code=[REDACTED]"));
    assert!(redacted.contains("access_token=[REDACTED]"));
    assert!(redacted.contains("refresh_token=[REDACTED]"));
    assert!(redacted.contains("GEMINI_API_KEY=[REDACTED]"));
}

#[test]
fn session_update_parsing_maps_chunks_tools_plan_and_usage() {
    let chunk = client::parse_session_update(&json!({
        "update": {
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": "hi" }
        }
    }));
    assert_eq!(chunk, Some(AcpEvent::MessageChunk("hi".to_owned())));

    let thought = client::parse_session_update(&json!({
        "update": {
            "sessionUpdate": "agent_thought_chunk",
            "content": { "type": "text", "text": "hmm" }
        }
    }));
    assert_eq!(thought, Some(AcpEvent::ThoughtChunk("hmm".to_owned())));

    let plan = client::parse_session_update(&json!({
        "update": {
            "sessionUpdate": "plan",
            "entries": [{ "content": "Step one", "status": "pending" }]
        }
    }));
    assert!(matches!(plan, Some(AcpEvent::Plan(entries)) if entries.len() == 1));
}

#[test]
fn acp_host_sources_never_read_google_credentials_or_gemini_api_key() {
    let antigravity_src = include_str!("antigravity.rs");
    let client_src = include_str!("client.rs");
    let mod_src = include_str!("mod.rs");
    for (name, src) in [
        ("antigravity.rs", antigravity_src),
        ("client.rs", client_src),
        ("mod.rs", mod_src),
    ] {
        for forbidden in [
            "var(\"GEMINI_API_KEY\")",
            "var_os(\"GEMINI_API_KEY\")",
            "read_to_string",
            "File::open",
            "read_dir",
            "keyring",
            ".antigravity",
        ] {
            assert!(
                !src.contains(forbidden),
                "ACP source {name} must not contain credential-reading pattern '{forbidden}'"
            );
        }
    }
}
