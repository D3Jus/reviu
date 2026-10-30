use gpui::actions;

actions!(
  workspace,
  [
    CloseCenterPane,
    CloseCenterTab,
    NextCenterTab,
    PreviousCenterTab,
    ToggleTabSwitcher,
    ToggleTabSwitcherBackward,
    MoveCenterTabLeft,
    MoveCenterTabRight,
    SplitPaneRight,
    SplitPaneDown,
    MoveCenterPaneLeft,
    MoveCenterPaneRight,
    MoveCenterPaneUp,
    MoveCenterPaneDown,
    FocusCenterPaneLeft,
    FocusCenterPaneRight,
    FocusCenterPaneUp,
    FocusCenterPaneDown,
    OpenTerminalInSplitRight,
    OpenTerminalInSplitDown,
    OpenFileInSplitRight,
    OpenFileInSplitDown,
    OpenDiffInSplitRight,
    OpenDiffInSplitDown,
    ShowBranchSwitcher,
    OpenGitHistorySidebar,
    OpenGitChangesSidebar,
    ReturnFocusToEditor,
    OpenFilesSidebar,
    OpenReviewSidebar,
    PullChanges,
    PushChanges,
    ForcePushChanges,
    ToggleDiffView,
    ToggleFileDiff,
    ToggleHideWhitespace,
    PreviousAnnotation,
    NextAnnotation,
    CommentHunk,
    SendReviewCommentsToAgent,
    AddSelectionToAgent,
    JumpToLatestMessage,
    NewAgentSession,
    NewAgentWorktreeSession,
    NewFile,
    NewFileInFilesPanel,
    ToggleHunkStage,
    RestoreHunk,
    ToggleFileStage,
    RestoreFile,
    SaveFileAs,
    RenameSelectedFileItem,
    DeleteSelectedFileItem,
    AcceptBothConflict,
    OpenGitConfigPage,
    OpenSettingsPage,
    OpenAboutPage,
  ]
);

mod about_dialog;
mod actions;
mod agent_chat_state;
mod agent_notification;
mod agent_review;
mod agent_review_store;
mod agent_settings;
mod annotations;
pub mod app_log_sink;
mod app_profile;
mod app_update;
mod askpass;
mod command_usage;

mod center_file_drag;
mod center_pane_controls;
mod changes_list;
mod config;
mod config_file;
mod config_reload;
mod conversation_hub;
mod date_format;
mod diff_toolbar;
mod diff_view_policy;
mod dock_panel;
#[cfg(any(test, feature = "test-support"))]
mod driver_support;
mod file_preview;
mod file_search_palette;
mod file_tree;
mod file_view;
mod git_config_page;
mod history_list;
mod hunk_actions;
mod interactive_rebase;
mod interactive_rebase_todo_view;
mod keybindings_file;
mod open_intent;
mod palette_actions;
mod palette_branches;
mod presentation;
mod project_files;
mod project_search_view;
mod repo_command;
mod repo_snapshot;
mod repo_state;
mod review_destination;
mod review_list;
mod session_list;
mod session_page;
mod settings_file;
mod settings_page;
mod shortcut_bindings;
mod shortcuts;
mod status_poll;
mod svg_preview;
#[cfg(test)]
mod test_support;
mod workspace;
mod workspace_onboarding;
mod workspace_window;

pub use actions::{
  CommitChanges, OpenProject, SaveFile, ShowCommandPalette, ShowFileSearch, ShowGlobalSearch,
};
pub use app_profile::{AppProfile, URL_SCHEME_PROD};
pub use askpass::handle_askpass_invocation;
#[cfg(any(test, feature = "test-support"))]
pub use driver_support::{
  DriverBranchKind, DriverBranchRef, DriverGitAction, DriverInteractiveRebaseAction,
  DriverInteractiveRebaseTarget, DriverNotification, DriverNotificationKind, DriverTerminalState,
};
#[cfg(any(test, feature = "test-support"))]
pub use presentation::DriverPresentation;
pub use shortcuts::{SHOW_COMMAND_PALETTE_SHORTCUT, install_app_key_bindings};
pub use workspace::{WorkspaceView, build_app_menus};
