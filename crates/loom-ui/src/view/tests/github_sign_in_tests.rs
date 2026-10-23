//! GitHub sign-in from the client: an unsigned connection waits for a sign-in,
//! a signed-in person binds the identity, and signing out returns a connection
//! without an operator token to the prompt.

use super::*;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{TestAppContext, px, size};
use loom_core::{GitHubSignInStatus, ParticipantId, ParticipantIdentity};
use loom_protocol::AccessResponse;

fn person(login: &str) -> ParticipantIdentity {
    ParticipantIdentity {
        id: ParticipantId::new(),
        display_name: login.to_owned(),
    }
}

fn signed_out() -> ResponseEnvelope {
    ResponseEnvelope::success(
        loom_core::RequestId::new(),
        ServerResponse::Access(AccessResponse::SignedOutGitHub),
    )
}

#[gpui_kit::test]
fn signed_in_binds_the_identity_and_ends_the_wait(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        view.github_sign_in_available = true;
        let node_id = view.default_backend_node_id.clone();
        view.mark_node_pre_login(node_id.clone());
        view.collaboration.identity_loaded = true;
        assert!(view.awaits_sign_in());
        assert!(!view.session_permissions().can_act);

        let octocat = person("octocat");
        view.apply_github_sign_in_status(
            &node_id,
            GitHubSignInStatus::SignedIn {
                participant: octocat.clone(),
            },
            None,
            cx,
        );
        assert!(!view.awaits_sign_in(), "a bound person ends the wait");
        assert_eq!(view.collaboration.identity.as_ref(), Some(&octocat));
        assert!(view.collaboration.identity_loaded);
        assert!(
            view.github_login.is_none(),
            "the pane closes once the person is bound"
        );
        // A signed-in person starts read-only: the worker grants roles.
        assert!(!view.session_permissions().can_act);
        assert!(
            !view
                .awaiting_sign_in_nodes
                .contains(&view.default_backend_node_id),
            "a signed-in node is no longer pre-login"
        );
        view
    });
}

#[gpui_kit::test]
fn a_failed_sign_in_stays_in_the_pane(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.github_sign_in_available = true;
        let node_id = view.default_backend_node_id.clone();
        view.mark_node_pre_login(node_id.clone());
        view.show_github_login(GitHubLoginState::Starting);
        view.apply_github_sign_in_status(
            &node_id,
            GitHubSignInStatus::Failed {
                message: "GitHub account 'someone' is not allowed to sign in to this worker"
                    .to_owned(),
            },
            None,
            cx,
        );
        assert!(
            matches!(
                view.github_login,
                Some(GitHubLoginState::Error(ref message)) if message.contains("not allowed")
            ),
            "the worker's reason is shown in the pane: {:?}",
            view.github_login
        );
        assert!(view.awaits_sign_in(), "a failed sign-in binds nobody");
        assert!(view.collaboration.identity.is_none());
        view
    });
}

#[gpui_kit::test]
fn signing_out_clears_the_identity_and_permissions(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        view.github_sign_in_available = true;
        let node_id = view.default_backend_node_id.clone();
        view.mark_node_pre_login(node_id.clone());
        view.collaboration.identity_loaded = true;
        let octocat = person("octocat");
        view.apply_github_sign_in_status(
            &node_id,
            GitHubSignInStatus::SignedIn {
                participant: octocat.clone(),
            },
            None,
            cx,
        );
        view.collaboration.directory = vec![octocat.clone()];
        view.collaboration.replace_roster(
            view.active_session.id,
            vec![loom_core::RosterEntry {
                participant: octocat.clone(),
                role: loom_core::SessionRole::Owner,
                presence: loom_core::PresenceStatus::Online,
                connections: 1,
                last_active_at: None,
            }],
        );
        view.collaboration.credentials = vec![loom_core::ProviderCredentialMetadata {
            id: loom_core::CredentialId::new(),
            provider: "github-copilot".to_owned(),
            owner: None,
            owner_label: "octocat".to_owned(),
            version: 1,
            created_at: loom_core::Timestamp::now(),
            updated_at: loom_core::Timestamp::now(),
            revoked: false,
        }];
        assert!(view.session_permissions().can_act);

        view.finish_sign_out_github(&node_id, signed_out(), cx);
        assert!(view.collaboration.identity.is_none());
        assert!(view.collaboration.roster.is_empty());
        assert!(view.collaboration.directory.is_empty());
        assert!(view.collaboration.credentials.is_empty());
        assert!(view.collaboration.conflict.is_none());
        assert!(view.collaboration.revision.is_none());
        assert!(!view.session_permissions().can_act);
        assert!(!view.session_permissions().can_manage);
        assert!(
            view.awaits_sign_in(),
            "an unsigned connection returns to the sign-in prompt"
        );
        view
    });
}

#[gpui_kit::test]
fn an_operator_connection_keeps_its_authority(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        // An operator token binds no person and needs no GitHub sign-in.
        view.collaboration.identity_loaded = true;
        view.collaboration.support = crate::collaboration::CollaborationSupport::Supported;
        assert!(!view.awaits_sign_in());
        assert!(view.session_permissions().can_act);
        assert!(view.session_permissions().can_manage);
        view.show_github_login(GitHubLoginState::Starting);
        let node_id = view.default_backend_node_id.clone();
        let octocat = person("octocat");
        view.apply_github_sign_in_status(
            &node_id,
            GitHubSignInStatus::SignedIn {
                participant: octocat.clone(),
            },
            None,
            cx,
        );
        assert_eq!(view.collaboration.identity.as_ref(), Some(&octocat));
        assert!(!view.awaits_sign_in());
        // The token still decides what the connection may do; the identity is
        // attribution, not a narrower role.
        assert!(view.session_permissions().can_act);
        assert!(view.session_permissions().can_manage);
        assert!(view.node_uses_operator_token(&node_id));
        // Signing out keeps the operator's unrestricted authority.
        view.finish_sign_out_github(&node_id, signed_out(), cx);
        assert!(view.collaboration.identity.is_none());
        assert!(!view.awaits_sign_in());
        assert!(view.session_permissions().can_act);
        assert!(view.session_permissions().can_manage);
        view
    });
}

#[gpui_kit::test]
fn a_remembered_person_is_named_in_the_sign_in_prompt(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        view.github_sign_in_available = true;
        view.mark_node_pre_login(view.default_backend_node_id.clone());
        view.collaboration.identity_loaded = true;
        view
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("composer-sign-in-message").label(),
            Some("Sign in to this worker to send messages and start runs."),
            "a connection with no remembered person keeps the generic prompt"
        );
        assert_eq!(
            window.find("composer-sign-in-github").label(),
            Some("Sign in with GitHub")
        );
    })
    .unwrap();
    cx.update_window(handle.into(), |_, window, cx| {
        let view = window.root::<LoomView>().unwrap().unwrap();
        view.update(cx, |view, _| {
            let node_id = view.active_node_id().to_owned();
            view.remember_identity(node_id, person("octocat"));
        });
    })
    .unwrap();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        // The stored person is the sign-in prompt's hint: the credential the
        // client keeps is what signs that person in again on a later
        // connection, so the prompt names them instead of a fresh account.
        let prompt = window
            .find("composer-sign-in-message")
            .label()
            .unwrap_or_default()
            .to_owned();
        assert_eq!(prompt, "Sign in again as octocat");
        assert_eq!(
            window.find("composer-sign-in-github").label(),
            Some("Sign in with GitHub")
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn a_sign_in_that_carries_a_credential_is_kept_for_the_worker(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        view.github_sign_in_available = true;
        // A node of this test's own, so the entry it stores cannot be mistaken
        // for another test's: the store a test build uses is shared in memory.
        let node_id = format!("kept-sign-in-{}", ParticipantId::new());
        let (backend, _jobs) = BackendWorker::deferred_for_test();
        view.node_backends.insert(node_id.clone(), backend);
        view.mark_node_pre_login(node_id.clone());
        view.collaboration.identity_loaded = true;

        // A worker that sends no credential keeps only the person.
        view.apply_github_sign_in_status(
            &node_id,
            GitHubSignInStatus::SignedIn {
                participant: person("octocat"),
            },
            None,
            cx,
        );
        assert_eq!(crate::credential_store::load(&node_id), None);

        // The poll that reports the sign-in can carry the person's own
        // credential, which is what signs a later connection in as them.
        let hubot = person("hubot");
        view.apply_github_sign_in_status(
            &node_id,
            GitHubSignInStatus::SignedIn {
                participant: hubot.clone(),
            },
            Some("github-sign-in-credential".to_owned()),
            cx,
        );
        assert_eq!(
            crate::credential_store::load(&node_id).as_deref(),
            Some("github-sign-in-credential")
        );
        assert_eq!(view.remembered_identity(&node_id), Some(&hubot));
        view
    });
}

#[gpui_kit::test]
fn signing_out_forgets_the_kept_credential(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        view.github_sign_in_available = true;
        let node_id = format!("sign-out-{}", ParticipantId::new());
        let (backend, _jobs) = BackendWorker::deferred_for_test();
        view.node_backends.insert(node_id.clone(), backend);
        view.mark_node_pre_login(node_id.clone());
        view.collaboration.identity_loaded = true;
        view.apply_github_sign_in_status(
            &node_id,
            GitHubSignInStatus::SignedIn {
                participant: person("octocat"),
            },
            Some("github-sign-in-credential".to_owned()),
            cx,
        );
        assert!(crate::credential_store::load(&node_id).is_some());

        // Signing out revokes the credential on the worker, so the client must
        // not present it on the next connection.
        assert!(view.finish_sign_out_github(&node_id, signed_out(), cx));
        assert_eq!(crate::credential_store::load(&node_id), None);
        view
    });
}

#[gpui_kit::test]
fn removing_a_worker_node_forgets_the_kept_credential(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        let url = format!("ws://worker-{}.example:8765", ParticipantId::new());
        let node_id = format!("removed-{}", ParticipantId::new());
        let mut node =
            connection_placeholder(1, url.clone(), WorkerConnectionState::Connected, None);
        node.status.node_id = node_id.clone();
        view.worker_nodes.push(node);
        let identity_key = crate::identity_store::worker_key(Some(url.as_str()));
        crate::credential_store::save(&identity_key, "github-sign-in-credential");

        view.remove_worker_node(1, cx);
        assert_eq!(crate::credential_store::load(&identity_key), None);
        view
    });
}

#[gpui_kit::test]
fn a_kept_credential_does_not_make_a_connection_an_operator(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        view.github_sign_in_available = true;
        let node_id = view.default_backend_node_id.clone();
        // The connect paths mark every connection opened without an operator
        // token, including one the kept credential signs in straight away.
        view.mark_node_unsigned(node_id.clone());
        view.collaboration.identity_loaded = true;
        let octocat = person("octocat");
        view.apply_github_sign_in_status(
            &node_id,
            GitHubSignInStatus::SignedIn {
                participant: octocat.clone(),
            },
            Some("github-sign-in-credential".to_owned()),
            cx,
        );
        assert!(
            !view.node_uses_operator_token(&node_id),
            "a person credential grants no operator authority"
        );
        // The worker's roles decide what the connection may do.
        assert!(!view.session_permissions().can_act);
        assert!(!view.session_permissions().can_manage);

        view.collaboration.replace_roster(
            view.active_session.id,
            vec![loom_core::RosterEntry {
                participant: octocat.clone(),
                role: loom_core::SessionRole::Owner,
                presence: loom_core::PresenceStatus::Online,
                connections: 1,
                last_active_at: None,
            }],
        );
        assert!(
            view.session_permissions().can_manage,
            "a granted role is what widens the connection"
        );
        view
    });
}

#[gpui_kit::test]
fn the_sign_in_prompt_says_the_sign_in_is_remembered(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        view.github_sign_in_available = true;
        view.mark_node_pre_login(view.default_backend_node_id.clone());
        view.collaboration.identity_loaded = true;
        // A connection whose transport is not secure for secrets cannot keep
        // the credential, so its prompt must not promise a remembered sign-in.
        assert_eq!(
            view.sign_in_retention_note("a-worker-with-no-connection"),
            crate::view::collaboration::UNSAVED_SIGN_IN_NOTE
        );
        view
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("composer-sign-in-note").label(),
            Some(crate::view::collaboration::KEPT_SIGN_IN_NOTE),
            "the prompt says the sign-in outlives the connection"
        );
        assert!(!crate::view::collaboration::KEPT_SIGN_IN_NOTE.contains("for the connection"));
        assert!(crate::view::collaboration::KEPT_SIGN_IN_NOTE.contains("signing out"));
    })
    .unwrap();
}

#[gpui_kit::test]
fn an_unsigned_connection_shows_the_sign_in_prompt(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        view.github_sign_in_available = true;
        view.mark_node_pre_login(view.default_backend_node_id.clone());
        view.collaboration.identity_loaded = true;
        assert!(!view.send_composer_text("hello".to_owned(), cx));
        assert!(view.timeline.is_empty());
        assert!(!view.can_manage_sources());
        view
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.find("composer-sign-in-prompt").visible(),
            "the composer's place is held by the sign-in prompt"
        );
        assert!(window.try_find("composer-input-box").is_none());
        assert!(window.try_find("send-message").is_none());
    })
    .unwrap();
}
