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
            cx,
        );
        assert_eq!(view.collaboration.identity.as_ref(), Some(&octocat));
        assert!(!view.awaits_sign_in());
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
