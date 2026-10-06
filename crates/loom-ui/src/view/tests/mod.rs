//! View tests, one file per focused area.

use super::*;

mod display_helper_tests;
mod lifecycle_source_action_tests;
mod local_source_path_tests;
mod loom_view_render_tests;
mod project_action_tests;
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
mod state_toggle_tests;
mod transcript_action_tests;
mod transcript_paging_tests;
#[cfg(feature = "visual-tests")]
mod visual_render_tests;
mod worker_failure_tests;
mod worker_node_tests;
