use super::*;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{ScrollDelta, TestAppContext, point, px, size};

#[test]
fn model_choice_labels_preserve_overlapping_provider_models_and_fallbacks() {
    let openai = ModelId::new("openai/gpt-5");
    let copilot = ModelId::new("github-copilot/gpt-5");
    let names = BTreeMap::from([
        (
            openai.clone(),
            crate::connection::provider_name_for_id("openai"),
        ),
        (
            copilot.clone(),
            crate::connection::provider_name_for_id("github-copilot"),
        ),
    ]);
    let models = vec![openai.clone(), copilot.clone()];
    let choices = model_choice_labels(&models, Some(&names));
    assert_eq!(choices.len(), 2);
    assert_eq!(choices["OpenAI · openai/gpt-5"], openai);
    assert_eq!(choices["GitHub Copilot · github-copilot/gpt-5"], copilot);
    assert_eq!(
        choices,
        model_choice_labels(&[copilot.clone(), openai.clone()], Some(&names))
    );
    assert_eq!(model_choice_label(&openai, None), "Provider · openai/gpt-5");
    assert_eq!(
        model_choice_label(&copilot, Some(&BTreeMap::new())),
        "Provider · github-copilot/gpt-5"
    );
    assert!(model_choice_labels(&[], None).is_empty());
}

#[test]
fn qualified_model_choice_labels_disambiguate_the_same_raw_model() {
    let raw = ModelId::new("gpt-5");
    let openai = ModelId::for_provider(&loom_model::ProviderId::new("openai"), &raw);
    let copilot = ModelId::for_provider(&loom_model::ProviderId::new("github-copilot"), &raw);
    let choices = model_choice_labels(&[openai.clone(), copilot.clone()], None);
    assert_eq!(choices["OpenAI · gpt-5"], openai);
    assert_eq!(choices["GitHub Copilot · gpt-5"], copilot);
    let custom = ModelId::for_provider(&loom_model::ProviderId::new("my_custom-provider"), &raw);
    assert_eq!(
        model_choice_label(&custom, None),
        "My Custom Provider · gpt-5"
    );
}

#[test]
fn legacy_model_choices_require_a_unique_provider() {
    let raw = ModelId::new("gpt-5");
    let openai = ModelId::for_provider(&loom_model::ProviderId::new("openai"), &raw);
    let copilot = ModelId::for_provider(&loom_model::ProviderId::new("github-copilot"), &raw);
    let unique = vec![openai.clone()];
    let ambiguous = vec![openai.clone(), copilot.clone()];
    assert_eq!(resolve_model_choice(&unique, &raw), Some(openai.clone()));
    assert_eq!(preferred_model_choice(&unique, &raw), openai);
    assert_eq!(resolve_model_choice(&ambiguous, &raw), None);
    assert_eq!(preferred_model_choice(&ambiguous, &raw), raw);
    assert_eq!(resolve_model_choice(&unique, &copilot), None);
    assert_eq!(preferred_model_choice(&unique, &copilot), copilot);
    assert_eq!(preferred_model_choice(&[], &raw), raw);
    assert_eq!(
        preferred_model_choice(&unique, &ModelId::new("missing")),
        unique[0]
    );
    assert!(
        validate_model_for_node(
            &BTreeMap::from([("worker".to_owned(), unique)]),
            "worker",
            &raw
        )
        .is_ok()
    );
    assert!(
        validate_model_for_node(
            &BTreeMap::from([("worker".to_owned(), ambiguous)]),
            "worker",
            &raw
        )
        .is_err()
    );
}

#[gpui_kit::test]
fn model_select_labels_use_worker_names_for_options_and_selected_values(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    cx.open_window(size(px(1280.), px(800.)), |window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        let openai = ModelId::new("openai/gpt-5");
        let copilot = ModelId::new("github-copilot/gpt-5");
        view.models = vec![openai.clone(), copilot.clone()];
        view.default_models = view.models.clone();
        view.model = copilot.clone();
        view.default_model = openai.clone();
        view.node_model_provider_names.insert(
            view.default_backend_node_id.clone(),
            BTreeMap::from([
                (openai.clone(), "OpenAI".to_owned()),
                (copilot.clone(), "GitHub Copilot".to_owned()),
            ]),
        );
        view.sync_model_select_states(window, cx);
        assert_eq!(
            view.model_select_items,
            vec![
                "GitHub Copilot · github-copilot/gpt-5",
                "OpenAI · openai/gpt-5"
            ]
        );
        assert_eq!(view.default_model_select_items, view.model_select_items);
        assert_eq!(
            view.model_select_value.as_deref(),
            Some("GitHub Copilot · github-copilot/gpt-5")
        );
        assert_eq!(
            view.default_model_select_value.as_deref(),
            Some("OpenAI · openai/gpt-5")
        );
        assert_eq!(
            view.model_select_choices["GitHub Copilot · github-copilot/gpt-5"],
            copilot
        );
        assert_eq!(
            view.default_model_select_choices["OpenAI · openai/gpt-5"],
            openai
        );

        view.session_node_ids
            .insert(view.active_session.id, "worker".to_owned());
        view.node_model_provider_names.insert(
            "worker".to_owned(),
            BTreeMap::from([(copilot.clone(), "Worker Copilot".to_owned())]),
        );
        view.sync_model_select_states(window, cx);
        assert_eq!(
            view.model_select_value.as_deref(),
            Some("Worker Copilot · github-copilot/gpt-5")
        );
        assert_eq!(
            view.model_select_choices["Worker Copilot · github-copilot/gpt-5"],
            copilot
        );
        assert_eq!(
            view.default_model_select_value.as_deref(),
            Some("OpenAI · openai/gpt-5")
        );

        // A selected model can outlive its catalog during a refresh.
        view.models.clear();
        view.sync_model_select_states(window, cx);
        assert_eq!(
            view.model_select_value.as_deref(),
            Some("Worker Copilot · github-copilot/gpt-5")
        );
        view.node_model_provider_names.remove("worker");
        view.sync_model_select_states(window, cx);
        assert_eq!(
            view.model_select_value.as_deref(),
            Some("Provider · github-copilot/gpt-5")
        );
        let raw = ModelId::new("gpt-5");
        let openai = ModelId::for_provider(&loom_model::ProviderId::new("openai"), &raw);
        let copilot = ModelId::for_provider(&loom_model::ProviderId::new("github-copilot"), &raw);
        view.models = vec![openai.clone(), copilot.clone()];
        view.default_models = view.models.clone();
        view.model = copilot.clone();
        view.default_model = openai.clone();
        view.sync_model_select_states(window, cx);
        assert_eq!(
            view.model_select_items,
            vec!["GitHub Copilot · gpt-5", "OpenAI · gpt-5"]
        );
        assert_eq!(view.default_model_select_items, view.model_select_items);
        assert_eq!(
            view.model_select_value.as_deref(),
            Some("GitHub Copilot · gpt-5")
        );
        assert_eq!(
            view.default_model_select_value.as_deref(),
            Some("OpenAI · gpt-5")
        );
        assert_eq!(view.model_select_choices["GitHub Copilot · gpt-5"], copilot);
        assert_eq!(view.default_model_select_choices["OpenAI · gpt-5"], openai);
        view.model = raw.clone();
        view.default_model = raw.clone();
        view.sync_model_select_states(window, cx);
        assert_eq!(view.model, raw);
        assert_eq!(view.default_model, raw);
        assert_eq!(view.model_select_value.as_deref(), Some("Provider · gpt-5"));
        view.models = vec![openai.clone()];
        view.default_models = view.models.clone();
        view.node_model_catalogs
            .insert("worker".to_owned(), view.models.clone());
        view.model_catalog_node_id = Some("worker".to_owned());
        view.sync_model_select_states(window, cx);
        assert_eq!(view.model, openai);
        assert_eq!(view.default_model, openai);
        assert_eq!(view.session_models[&view.active_session.id], openai);
        view.select_model(raw.clone(), cx);
        view.select_default_model(raw, cx);
        assert_eq!(view.model, openai);
        assert_eq!(view.default_model, openai);
        view
    });
}

#[gpui_kit::test]
fn provider_model_and_mode_controls_update_state(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sync_model_select_states(window, cx);
        assert!(view.model_select.is_some());

        view.select_model(ModelId::new("deterministic/demo"), cx);
        assert_eq!(view.model, ModelId::new("deterministic/demo"));
        view.select_model(ModelId::new("missing/model"), cx);

        view.select_default_model(ModelId::new("deterministic/demo"), cx);
        assert_eq!(view.default_model, ModelId::new("deterministic/demo"));
        view.select_default_model(ModelId::new("missing/model"), cx);

        view.toggle_auto_approve_actions(cx);
        view.observe_system_appearance(window, cx);
        view.observe_system_appearance(window, cx);

        view.open_settings_from_menu(cx);
        assert!(view.settings_open());
        view.open_providers_for_node("test-node".to_owned(), cx);
        assert!(view.settings_open());
        assert_eq!(view.settings_section(), Some(SettingsSection::Providers));

        view.handle_github_provider_configured("test-node".to_owned(), cx);
        assert!(view.github_connected);

        let appearance = window.appearance();
        view.apply_appearance(appearance, window, cx);
        view
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

#[gpui_kit::test]
fn github_write_access_toggle_round_trips(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        crate::connection::negotiate(&view.connection).unwrap();
        assert!(!view.github_write_access);
        view.github_repository_connected = true;
        view.show_surface(CoveringSurface::Settings(SettingsSection::Providers));
        view
    });
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window
            .within("settings-dialog")
            .click("github-write-access-toggle", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        let view = window.root::<LoomView>().unwrap().unwrap();
        view.update(cx, |view, cx| {
            assert!(view.github_write_access);
            view.toggle_github_write_access(false, cx);
        });
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        let view = window.root::<LoomView>().unwrap().unwrap();
        view.update(cx, |view, cx| {
            assert!(!view.github_write_access);
            view.refresh_github_write_access("missing-node".to_owned(), cx);
        });
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

#[gpui_kit::test]
fn reasoning_visibility_toggle_round_trips(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        assert!(!view.show_reasoning);
        view.show_surface(CoveringSurface::Settings(SettingsSection::Agents));
        view
    });
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window
            .within("settings-dialog")
            .click("show-reasoning-toggle", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        let view = window.root::<LoomView>().unwrap().unwrap();
        view.update(cx, |view, cx| {
            assert!(view.show_reasoning);
            view.toggle_show_reasoning(cx);
            assert!(!view.show_reasoning);
        });
    })
    .unwrap();
}

#[gpui_kit::test]
fn output_detail_choices_round_trip(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        assert_eq!(view.output_detail, crate::state::OutputDetail::Medium);
        view.show_surface(CoveringSurface::Settings(SettingsSection::Agents));
        view
    });
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        // The Agents card carries the display preferences above the output
        // detail row, so the row is below the fold at this window size.
        window.scroll(
            "settings-content",
            ScrollDelta::Pixels(point(px(0.), px(-160.))),
            cx,
        );
        window
            .within("settings-dialog")
            .click(("output-detail", 0usize), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        let view = window.root::<LoomView>().unwrap().unwrap();
        view.update(cx, |view, _| {
            assert_eq!(view.output_detail, crate::state::OutputDetail::Low);
        });
        window
            .within("settings-dialog")
            .click(("output-detail", 2usize), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        let view = window.root::<LoomView>().unwrap().unwrap();
        view.update(cx, |view, cx| {
            assert_eq!(view.output_detail, crate::state::OutputDetail::High);
            view.set_output_detail(crate::state::OutputDetail::Low, cx);
        });
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        let view = window.root::<LoomView>().unwrap().unwrap();
        view.update(cx, |view, _| {
            assert_eq!(view.output_detail, crate::state::OutputDetail::Low);
        });
    })
    .unwrap();
}

/// The settings that describe how much of the agent's own output the
/// transcript shows belong to the Agents section, not to Appearance, which is
/// about the window's look.
#[gpui_kit::test]
fn agent_output_settings_live_in_the_agents_section(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.show_surface(CoveringSurface::Settings(SettingsSection::Agents));
        view
    });
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("show-reasoning-toggle").is_some());
        assert!(window.try_find(("output-detail", 0usize)).is_some());
        assert!(window.try_find(("output-detail", 2usize)).is_some());
        let view = window.root::<LoomView>().unwrap().unwrap();
        view.update(cx, |view, _| {
            view.show_surface(CoveringSurface::Settings(SettingsSection::Appearance));
        });
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        // Appearance keeps only the look of the window: theme and font size.
        assert!(window.try_find(("theme-choice", 0usize)).is_some());
        assert!(window.try_find("show-reasoning-toggle").is_none());
        assert!(window.try_find(("output-detail", 0usize)).is_none());
    })
    .unwrap();
}

#[test]
fn github_login_kinds_name_the_flow_each_one_runs() {
    use crate::state::GitHubLoginKind;

    // Each flow acquires something different, so neither the heading nor the
    // explanation may read like one generic GitHub login.
    assert_eq!(
        GitHubLoginKind::Copilot.pane_title(),
        "Sign in to GitHub Copilot"
    );
    assert_eq!(
        GitHubLoginKind::Repository.pane_title(),
        "Authorize GitHub repository access"
    );
    assert_eq!(
        GitHubLoginKind::Identity.pane_title(),
        "Sign in to this worker"
    );

    let copilot = GitHubLoginKind::Copilot.intro();
    assert!(copilot.contains("model provider"), "{copilot}");
    assert!(!copilot.contains("repository"), "{copilot}");
    let repository = GitHubLoginKind::Repository.intro();
    assert!(repository.contains("clone"), "{repository}");
    assert!(!repository.contains("model provider"), "{repository}");
    let identity = GitHubLoginKind::Identity.intro();
    assert!(identity.contains("this device"), "{identity}");
    assert!(identity.contains("Signing out"), "{identity}");
    assert!(identity.contains("no repository"), "{identity}");

    // The two credential flows say they are not the worker sign-in, and the
    // worker sign-in says it grants no credential.
    assert!(copilot.contains("does not sign you in"), "{copilot}");
    assert!(repository.contains("does not sign you in"), "{repository}");

    assert_ne!(copilot, repository);
    assert_ne!(repository, identity);

    let copilot_success = GitHubLoginKind::Copilot.success_message();
    assert!(
        copilot_success.contains("GitHub Copilot"),
        "{copilot_success}"
    );
    let repository_success = GitHubLoginKind::Repository.success_message();
    assert!(
        repository_success.contains("Repository"),
        "{repository_success}"
    );
    let identity_success = GitHubLoginKind::Identity.success_message();
    assert!(identity_success.contains("signed in"), "{identity_success}");
    assert_ne!(copilot_success, repository_success);
    assert_ne!(repository_success, identity_success);
}

#[gpui_kit::test]
fn the_providers_section_separates_copilot_from_repository_access(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        crate::connection::negotiate(&view.connection).unwrap();
        view.login_enabled = true;
        view.show_surface(CoveringSurface::Settings(SettingsSection::Providers));
        view
    });
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("settings-github-copilot-title").label(),
            Some("GitHub Copilot")
        );
        assert_eq!(
            window.find("connect-github-account").label(),
            Some("Sign in to Copilot")
        );
        assert_eq!(
            window.find("connect-github-repository").label(),
            Some("Authorize repository")
        );
        assert_ne!(
            window.find("connect-github-account").label(),
            Some("Connect GitHub"),
            "the Copilot card must not read as a general GitHub login"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn the_login_pane_explains_which_flow_it_runs(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.github_login_kind = crate::state::GitHubLoginKind::Identity;
        view.show_github_login(GitHubLoginState::Awaiting {
            verification_uri: "https://github.com/login/device".to_owned(),
            user_code: "ABCD-1234".to_owned(),
            expires_in: 900,
        });
        view
    });
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("github-login-intro").label(),
            Some(crate::state::GitHubLoginKind::Identity.intro())
        );
    })
    .unwrap();
    cx.update_window(handle.into(), |_, window, cx| {
        let view = window.root::<LoomView>().unwrap().unwrap();
        view.update(cx, |view, _| {
            view.github_login_kind = crate::state::GitHubLoginKind::Copilot;
        });
    })
    .unwrap();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("github-login-intro").label(),
            Some(crate::state::GitHubLoginKind::Copilot.intro())
        );
    })
    .unwrap();
}

#[test]
fn github_login_kind_secure_messages_differ() {
    assert!(
        crate::state::GitHubLoginKind::Copilot
            .secure_connection_message()
            .contains("Copilot")
    );
    assert!(
        crate::state::GitHubLoginKind::Repository
            .secure_connection_message()
            .contains("repository")
    );
}

#[gpui_kit::test]
fn begin_github_login_clears_finished_states(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.show_github_login(GitHubLoginState::Success);
        view.begin_github_login(crate::state::GitHubLoginKind::Repository, cx);
        assert!(view.github_login.is_none());
        assert!(!view.github_login_open());
        view.show_github_login(GitHubLoginState::Error("failed".to_owned()));
        view.begin_github_login(crate::state::GitHubLoginKind::Copilot, cx);
        assert!(view.github_login.is_none());
        assert!(!view.github_login_open());
        view
    });
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

#[gpui_kit::test]
fn github_repository_access_row_renders_when_disconnected(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        crate::connection::negotiate(&view.connection).unwrap();
        view.login_enabled = true;
        view.github_repository_connected = false;
        view.show_surface(CoveringSurface::Settings(SettingsSection::Providers));
        view
    });
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

#[gpui_kit::test]
fn a_signed_in_worker_offers_reauthentication_and_sign_out(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        view.login_enabled = true;
        view.github_sign_in_available = true;
        view.collaboration.identity = Some(loom_core::ParticipantIdentity {
            id: loom_core::ParticipantId::new(),
            display_name: "octocat".to_owned(),
        });
        view.show_surface(CoveringSurface::Settings(SettingsSection::Workers));
        view
    });
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("worker-reauth-github").is_some());
        assert!(window.try_find("worker-sign-out-github").is_some());
    })
    .unwrap();
}

#[gpui_kit::test]
fn an_operator_without_an_identity_can_link_one(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.login_enabled = true;
        view.github_sign_in_available = true;
        view.show_surface(CoveringSurface::Settings(SettingsSection::Workers));
        view
    });
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("worker-sign-in-github").is_some());
    })
    .unwrap();
}

#[gpui_kit::test]
fn an_api_key_can_be_removed_from_its_provider_card(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        crate::connection::negotiate(&view.connection).unwrap();
        let provider_id = loom_model::ProviderId::new("openai-compatible");
        let configured = view
            .connection
            .request(RequestEnvelope::new(ClientRequest::Provider(
                ProviderRequest::ConfigureApiKeyProvider {
                    provider_id: provider_id.clone(),
                    api_key: "key-to-drop".to_owned(),
                },
            )));
        assert!(configured.result.is_ok(), "{:?}", configured.result);
        view.open_providers_for_node(view.default_backend_node_id.clone(), cx);
        view
    });
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window
                .try_find("remove-api-key-openai-compatible")
                .is_some()
        );
    })
    .unwrap();

    cx.update_window(handle.into(), |_, window, cx| {
        window
            .root::<LoomView>()
            .unwrap()
            .unwrap()
            .update(cx, |view, cx| {
                view.remove_api_key_provider(loom_model::ProviderId::new("openai-compatible"), cx);
            });
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window
                .try_find("remove-api-key-openai-compatible")
                .is_none()
        );
        let view = window.root::<LoomView>().unwrap().unwrap();
        view.update(cx, |view, _| {
            let providers = view
                .connection
                .request(RequestEnvelope::new(ClientRequest::Provider(
                    ProviderRequest::ListProviders,
                )));
            let Ok(ServerResponse::Provider(ProviderResponse::Providers { providers })) =
                providers.result
            else {
                panic!("unexpected provider response");
            };
            let provider = providers
                .into_iter()
                .find(|provider| provider.id.as_str() == "openai-compatible")
                .unwrap();
            assert!(provider.credential_id.is_none());
        });
    })
    .unwrap();
}

#[gpui_kit::test]
fn github_copilot_login_finishes_by_registering_the_provider(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        crate::connection::negotiate(&view.connection).unwrap();
        view.github_login_kind = crate::state::GitHubLoginKind::Copilot;
        // The worker stores the credential when its device flow completes;
        // here the test stores one directly, as that flow would.
        let configured = view
            .connection
            .request(RequestEnvelope::new(ClientRequest::Provider(
                ProviderRequest::ConfigureGitHubCopilot {
                    access_token: "ghu_copilot_token".to_owned(),
                },
            )));
        assert!(configured.result.is_ok(), "{:?}", configured.result);
        let node_id = view.default_backend_node_id.clone();
        view.handle_github_provider_configured(node_id, cx);
        view
    });
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        let view = window.root::<LoomView>().unwrap().unwrap();
        view.update(cx, |view, _| {
            assert!(view.github_connected);
            assert!(matches!(view.github_login, Some(GitHubLoginState::Success)));
        });
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}
