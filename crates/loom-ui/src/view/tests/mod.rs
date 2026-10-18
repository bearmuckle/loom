//! View tests, one file per focused area.
//!
//! # Leave no active run behind
//!
//! A `#[gpui_kit::test]` that ends while `active_run_id` is set and `run_state`
//! is still Planning, Executing, Evaluating, or Paused hangs: `run_should_poll`
//! stays true, so the test app keeps polling the run after the test body
//! returns. Clear the run before the test ends (`view.active_run_id = None;`
//! `view.run_state = None;`) or finish it with a terminal state. The workaround
//! for an interruptible run is in `loom_view_render_tests.rs`.

use super::*;

mod agent_question_tests;
mod archived_session_tests;
mod collaboration_tests;
mod connectivity_tests;
mod covering_surface_tests;
mod destructive_approval_tests;
mod disconnected_screen_render_tests;
mod display_helper_tests;
mod event_resync_tests;
mod github_sign_in_tests;
mod lifecycle_source_action_tests;
mod local_source_path_tests;
mod loom_mark_tests;
mod loom_view_render_tests;
mod new_project_pane_tests;
mod policy_decision_tests;
mod project_action_tests;
mod project_activity_tests;
mod project_list_tests;
mod provider_control_tests;
mod provider_error_tests;
mod provider_mode_tests;
mod render_state_tests;
mod responsive_layout_tests;
mod review_path_tests;
mod review_project_action_tests;
mod session_header_render_tests;
mod session_load_tests;
mod session_name_tests;
mod session_run_action_tests;
mod sidebar_navigation_tests;
mod startup_tests;
mod state_toggle_tests;
mod transcript_action_tests;
mod transcript_paging_tests;
#[cfg(feature = "visual-tests")]
mod visual_render_tests;
mod worker_failure_tests;
mod worker_node_tests;
mod worker_project_tests;
mod worker_resume_tests;
