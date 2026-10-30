//! The right dock of the shell: changes, files, history, pull request.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use git::{
  HeadCommitStatus, RepoStage, RepoStatusEntry, RepoStatusKind, commit_changes,
  head_commit_message, head_commit_status, is_merge_in_progress, is_rebase_in_progress,
  list_repo_status, stage_all,
};
use gpui::{
  Anchor, AnyElement, AnyWindowHandle, App, Context, Entity, FocusHandle, Focusable, Render,
  SharedString, Task, WeakEntity, Window, div, img, prelude::*, px,
};
use gpui_component::{
  ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, h_flex,
  input::{self, Input, InputState},
  menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem},
  tree::{TreeEvent, TreeItem, TreeState, tree},
  v_flex,
};

use crate::center_file_drag::{CenterFileDrag, CenterFileDragMode};
use crate::changes_list::{
  ChangesList, ChangesListEvent, status_color, status_tooltip, status_uses_warning_icon,
};
use crate::config::AppSettings;
use crate::file_tree::{build_project_tree_items_with_expansion, is_empty_folder_placeholder_id};
use crate::history_list::{HistoryList, HistoryListEvent};
use crate::project_files::{
  ProjectEntry, ProjectEntryKind, ProjectScanOptions, list_project_entries_with_options,
};
use crate::repo_state::{PaletteCommand, RepoState, push_flags};
use crate::review_list::{ReviewList, ReviewListEvent, ReviewSection};

pub(crate) const DOCK_PANEL_HISTORY_DEBUG_SELECTOR: &str = "dock-panel-history";
pub(crate) const DOCK_PANEL_REVIEW_DEBUG_SELECTOR: &str = "dock-panel-review";
const DOCK_PANEL_COMMIT_DEBUG_SELECTOR: &str = "dock-panel-commit";
const DOCK_PANEL_COMMIT_MENU_DEBUG_SELECTOR: &str = "dock-panel-commit-menu";
const DOCK_PANEL_CHANGES_SUMMARY_DEBUG_SELECTOR: &str = "dock-panel-changes-summary";
const DOCK_PANEL_CHANGES_ACTION_DEBUG_SELECTOR: &str = "dock-panel-changes-action";
const DOCK_PANEL_CHANGES_ACTION_MENU_DEBUG_SELECTOR: &str = "dock-panel-changes-action-menu";
const DOCK_PANEL_CHANGES_EMPTY_DEBUG_SELECTOR: &str = "dock-panel-changes-empty-state";
const DOCK_PANEL_FILES_ACTION_MENU_DEBUG_SELECTOR: &str = "dock-panel-files-action-menu";
const DOCK_PANEL_HEADER_HEIGHT_PX: f32 = 36.0;
const DOCK_PANEL_REFRESH_DEBUG_SELECTOR: &str = "dock-panel-refresh";
const FILE_TREE_SUBDUED_ENTRY_OPACITY: f32 = 0.62;
use std::rc::Rc;

use crate::open_intent::OpenIntent;
use gpui_component::notification::Notification;
use ui::{
  Button, ButtonVariants as _, ConfirmDialog, StatusThemeExt as _, Textarea, TextareaState,
  UiIconName, WindowExt as _,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DockPanelOpenFileMode {
  File,
  Diff,
}

#[derive(Clone, Debug)]
pub enum DockPanelEvent {
  OpenFile {
    path: PathBuf,
    intent: OpenIntent,
    mode: DockPanelOpenFileMode,
  },
  FileRenamed {
    old_path: PathBuf,
    new_path: PathBuf,
  },
  FileDeleted {
    path: PathBuf,
  },
  /// A file as it was in a commit, read-only.
  OpenCommitFile {
    commit_oid: String,
    path: PathBuf,
    intent: OpenIntent,
  },
  CheckoutCommit {
    commit_oid: String,
  },
  CreateBranchFromCommit {
    commit_oid: String,
    name: String,
  },
  /// A commit landed: whoever shows the branch state has to refresh it.
  Committed,
  /// The rebase can move on: the host runs it, it owns the conflict flow.
  ContinueRebase,
  /// A commit option changed; the host owns window-bound work.
  RunCommand(CommitMenuCommand),
  /// The commit button should amend the pending commit.
  AmendCommit,
  /// A command picked in the Changes actions menu; the host owns running it.
  RunChangesAction(ChangesActionCommand),
  /// The working tree was re-read: whoever shows a file has to look again.
  StatusRefreshed,
  /// A row of the review panel: the host owns the diff and the batch.
  OpenReviewComment {
    path: PathBuf,
    line: usize,
    intent: OpenIntent,
  },
  DeleteReviewComment {
    id: u64,
  },
  SendReviewComment {
    id: u64,
  },
  SendReview,
  DiscardReview,
}

impl gpui::EventEmitter<DockPanelEvent> for DockPanel {}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FilesContextTarget {
  relative_path: Option<PathBuf>,
  is_folder: bool,
}

impl FilesContextTarget {
  fn root() -> Self {
    Self {
      relative_path: None,
      is_folder: true,
    }
  }

  fn entry(relative_path: PathBuf, is_folder: bool) -> Self {
    Self {
      relative_path: Some(relative_path),
      is_folder,
    }
  }
}

enum FilesInlineOperation {
  Rename,
  Create {
    directory: Option<PathBuf>,
    is_folder: bool,
  },
}

struct FilesInlineRename {
  relative_path: PathBuf,
  input: Entity<InputState>,
  operation: FilesInlineOperation,
}

fn child_relative_path(base: Option<&Path>, input: &str) -> anyhow::Result<PathBuf> {
  let child = validate_relative_input(input)?;
  Ok(match base {
    Some(base) => base.join(child),
    None => child,
  })
}

fn validate_relative_input(input: &str) -> anyhow::Result<PathBuf> {
  let trimmed = input.trim();
  if trimmed.is_empty() {
    return Err(anyhow::anyhow!("Enter a name"));
  }
  let path = Path::new(trimmed);
  if path.is_absolute() {
    return Err(anyhow::anyhow!("Use a relative path"));
  }
  let mut clean = PathBuf::new();
  for component in path.components() {
    match component {
      std::path::Component::Normal(part) => clean.push(part),
      std::path::Component::CurDir => {}
      _ => return Err(anyhow::anyhow!("Path cannot leave the project")),
    }
  }
  if clean.as_os_str().is_empty() {
    return Err(anyhow::anyhow!("Enter a name"));
  }
  Ok(clean)
}

fn validate_file_name(input: &str) -> anyhow::Result<String> {
  let path = validate_relative_input(input)?;
  if path.components().count() != 1 {
    return Err(anyhow::anyhow!("Enter a name, not a path"));
  }
  Ok(path.to_string_lossy().into_owned())
}

fn next_duplicate_path(root: &Path, relative_path: &Path) -> Option<PathBuf> {
  let parent = relative_path
    .parent()
    .filter(|path| !path.as_os_str().is_empty());
  let file_name = relative_path.file_name()?.to_string_lossy();
  let (stem, extension) = match relative_path.extension() {
    Some(extension) => {
      let stem = relative_path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| file_name.to_string());
      (stem, Some(extension.to_string_lossy().into_owned()))
    }
    None => (file_name.to_string(), None),
  };

  for suffix in 1.. {
    let name = match (&extension, suffix) {
      (Some(extension), 1) => format!("{stem} copy.{extension}"),
      (Some(extension), suffix) => format!("{stem} copy {suffix}.{extension}"),
      (None, 1) => format!("{stem} copy"),
      (None, suffix) => format!("{stem} copy {suffix}"),
    };
    let candidate = parent
      .map(|parent| parent.join(&name))
      .unwrap_or_else(|| PathBuf::from(&name));
    if !root.join(&candidate).exists() {
      return Some(root.join(candidate));
    }
  }
  None
}

/// What the menu next to the commit button offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitMenuCommand {
  Amend,
}

impl CommitMenuCommand {
  fn label(self, amend_pending: bool) -> &'static str {
    match self {
      Self::Amend if amend_pending => "Cancel Amend",
      Self::Amend => "Amend",
    }
  }

  fn icon(self, amend_pending: bool) -> Icon {
    match self {
      Self::Amend if amend_pending => Icon::new(IconName::Close),
      Self::Amend => Icon::new(IconName::Replace),
    }
  }

  fn rule(self) -> PaletteCommand {
    match self {
      Self::Amend => PaletteCommand::Amend,
    }
  }
}

/// Repository-wide actions shown above the Changes list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangesActionCommand {
  StageAll,
  UnstageAll,
  RestoreAll,
  Stash,
  StashWithUntracked,
  Push,
  ForcePush,
  UndoLastCommit,
}

impl ChangesActionCommand {
  const ALL: [Self; 8] = [
    Self::StageAll,
    Self::UnstageAll,
    Self::RestoreAll,
    Self::Stash,
    Self::StashWithUntracked,
    Self::Push,
    Self::ForcePush,
    Self::UndoLastCommit,
  ];

  fn label(self) -> &'static str {
    match self {
      Self::StageAll => "Stage All",
      Self::UnstageAll => "Unstage All",
      Self::RestoreAll => "Restore All Changes",
      Self::Stash => "Stash",
      Self::StashWithUntracked => "Stash With Untracked",
      Self::Push => "Push",
      Self::ForcePush => "Force Push (with Lease)",
      Self::UndoLastCommit => "Undo Last Commit",
    }
  }

  fn icon(self) -> Icon {
    match self {
      Self::StageAll => Icon::new(IconName::Plus),
      Self::UnstageAll => Icon::new(UiIconName::ArrowUpFromLine),
      Self::RestoreAll => Icon::new(IconName::Undo),
      Self::Stash => Icon::new(UiIconName::ArrowDownFromLine),
      Self::StashWithUntracked => Icon::new(UiIconName::ArrowDownFromLine),
      Self::Push | Self::ForcePush => Icon::new(IconName::ArrowUp),
      Self::UndoLastCommit => Icon::new(IconName::Undo),
    }
  }

  fn rule(self) -> PaletteCommand {
    match self {
      Self::StageAll => PaletteCommand::StageAll,
      Self::UnstageAll => PaletteCommand::UnstageAll,
      Self::RestoreAll => PaletteCommand::RestoreAll,
      Self::Stash => PaletteCommand::Stash,
      Self::StashWithUntracked => PaletteCommand::StashWithUntracked,
      Self::Push => PaletteCommand::Push,
      Self::ForcePush => PaletteCommand::ForcePush,
      Self::UndoLastCommit => PaletteCommand::UndoLastCommit,
    }
  }
}

fn changes_primary_command(state: &RepoState<'_>) -> Option<ChangesActionCommand> {
  if state.allows(PaletteCommand::StageAll) {
    Some(ChangesActionCommand::StageAll)
  } else if state.allows(PaletteCommand::UnstageAll) {
    Some(ChangesActionCommand::UnstageAll)
  } else if state.status_entries.is_empty() && state.allows(PaletteCommand::Push) {
    Some(ChangesActionCommand::Push)
  } else if state.status_entries.is_empty() && state.allows(PaletteCommand::ForcePush) {
    Some(ChangesActionCommand::ForcePush)
  } else {
    None
  }
}

fn changes_action_menu_items(state: &RepoState<'_>) -> Vec<ChangesActionCommand> {
  ChangesActionCommand::ALL
    .into_iter()
    .filter(|command| state.allows(command.rule()))
    .collect()
}

fn build_changes_action_menu(
  menu: PopupMenu,
  menu_items: Vec<ChangesActionCommand>,
  view: Entity<DockPanel>,
  changes_action_in_flight: Option<ChangesActionCommand>,
) -> PopupMenu {
  menu_items.into_iter().fold(menu, |menu, command| {
    let view = view.clone();
    menu.item(
      PopupMenuItem::new(command.label())
        .icon(command.icon())
        .disabled(changes_action_in_flight.is_some())
        .on_click(move |_, _, cx| {
          view.update(cx, |_, cx| {
            cx.emit(DockPanelEvent::RunChangesAction(command));
          });
        }),
    )
  })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DockPanelTab {
  Changes,
  Review,
  Files,
  History,
}

pub struct DockPanel {
  focus_handle: FocusHandle,
  window_handle: AnyWindowHandle,
  project_root: Option<PathBuf>,
  repo_root: Option<PathBuf>,
  status_entries: Vec<RepoStatusEntry>,
  /// The repository `status_entries` was read from: until the first read for
  /// the current one lands, an absent file is unknown rather than clean.
  status_repo_root: Option<PathBuf>,
  merge_in_progress: bool,
  rebase_in_progress: bool,
  head_status: HeadCommitStatus,
  branch_status: Option<git::BranchStatus>,
  default_branch: Option<git::BranchRef>,
  pub(crate) commit_input: Entity<TextareaState>,
  commit_message_drafts: HashMap<PathBuf, String>,
  amend_pending: bool,
  pre_amend_commit_message: Option<String>,
  committing: bool,
  last_error: Option<SharedString>,
  active_tab: DockPanelTab,
  changes_list: Entity<ChangesList>,
  pub(crate) review_list: Entity<ReviewList>,
  pub(crate) history_list: Entity<HistoryList>,
  files_tree_state: Entity<TreeState>,
  files_context_menu_target: Option<FilesContextTarget>,
  files_inline_rename: Option<FilesInlineRename>,
  files_inline_create_placeholder_to_ignore: Option<PathBuf>,
  files_loaded: bool,
  files_file_count: usize,
  files_entry_visibility: HashMap<String, FileTreeEntryVisibility>,
  files_include_gitignored: bool,
  files_include_hidden: bool,
  files_reveal_path_when_loaded: Option<PathBuf>,
  /// The tab was opened before its tree existed: focus it as soon as it does.
  focus_files_tree_when_loaded: bool,
  files_loading: bool,
  files_load_generation: u64,
  changes_action_in_flight: Option<ChangesActionCommand>,
  pub(crate) _refresh_task: Option<Task<()>>,
  _commit_task: Option<Task<()>>,
  _amend_message_task: Option<Task<()>>,
  _files_task: Option<Task<()>>,
  _file_operation_task: Option<Task<()>>,
}

impl DockPanel {
  pub fn new(repo_root: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
    let commit_input = cx.new(|cx| {
      TextareaState::new(window, cx)
        .auto_grow(1, 5)
        .placeholder("Commit message...")
    });
    let _ = commit_input
      .read(cx)
      .focus_handle(cx)
      .tab_stop(true)
      .tab_index(1);
    // cmd-enter from inside the input commits.
    cx.subscribe_in(
      &commit_input,
      window,
      |this, _state, event: &gpui_component::input::InputEvent, _window, cx| {
        if let gpui_component::input::InputEvent::PressEnter {
          secondary: true, ..
        } = event
        {
          this.commit(cx);
        }
      },
    )
    .detach();

    let settings = AppSettings::get(cx);
    let split_sections = !settings.git_unified_file_view;
    let changes_list = cx.new(|cx| ChangesList::new(repo_root.clone(), split_sections, window, cx));
    cx.subscribe_in(
      &changes_list,
      window,
      |this, _list, event: &ChangesListEvent, _window, cx| match event {
        ChangesListEvent::OpenFile { path, intent } => {
          cx.emit(DockPanelEvent::OpenFile {
            path: path.clone(),
            intent: *intent,
            mode: DockPanelOpenFileMode::Diff,
          });
        }
        ChangesListEvent::Changed => this.refresh(cx),
      },
    )
    .detach();

    cx.observe_global::<AppSettings>(|this, cx| {
      let settings = AppSettings::get(cx);
      let split_sections = !settings.git_unified_file_view;
      this
        .changes_list
        .update(cx, |list, cx| list.set_split_sections(split_sections, cx));
      this.sync_files_scan_options_with_settings(cx);
    })
    .detach();

    // The worktree tree says where the user is and what they chose: a file row
    // shows as they walk it, and opens when they pick it.
    let files_tree_state = cx.new(|cx| TreeState::new(cx));
    let _ = files_tree_state
      .read(cx)
      .focus_handle(cx)
      .tab_stop(true)
      .tab_index(0);
    cx.subscribe(&files_tree_state, |this, tree, event: &TreeEvent, cx| {
      let (id, intent) = match event {
        TreeEvent::Selected(id) => (id.clone(), OpenIntent::Browse),
        TreeEvent::Confirmed(id) => (id.clone(), OpenIntent::Open),
        TreeEvent::Expanded(_) | TreeEvent::Collapsed(_) => return,
      };
      let is_inline_create_placeholder = this.files_inline_rename.as_ref().is_some_and(|rename| {
        matches!(rename.operation, FilesInlineOperation::Create { .. })
          && file_tree_path_id(&rename.relative_path) == id.as_ref()
      }) || this
        .files_inline_create_placeholder_to_ignore
        .as_ref()
        .is_some_and(|path| file_tree_path_id(path) == id.as_ref());
      if is_empty_folder_placeholder_id(id.as_ref()) || is_inline_create_placeholder {
        return;
      }
      // Walking onto a folder moves the selection and nothing else: a folder
      // has no contents to show.
      let state = tree.read(cx);
      let is_folder = state
        .index_of(&id)
        .and_then(|ix| state.entry(ix))
        .is_some_and(|entry| entry.is_folder());
      if is_folder {
        return;
      }
      cx.emit(DockPanelEvent::OpenFile {
        path: PathBuf::from(id.as_ref()),
        intent,
        mode: DockPanelOpenFileMode::File,
      });
    })
    .detach();

    let review_list = cx.new(|cx| ReviewList::new(window, cx));
    cx.subscribe(
      &review_list,
      |_this, _list, event: &ReviewListEvent, cx| match event {
        ReviewListEvent::OpenComment {
          section,
          path,
          line,
          intent,
        } => match section {
          ReviewSection::Agent => cx.emit(DockPanelEvent::OpenReviewComment {
            path: path.clone(),
            line: *line,
            intent: *intent,
          }),
          // The pull request section is never populated now that GitHub is gone.
          ReviewSection::PullRequest => {}
        },
        ReviewListEvent::DeleteComment { section, id } => match section {
          ReviewSection::Agent => cx.emit(DockPanelEvent::DeleteReviewComment { id: *id }),
          ReviewSection::PullRequest => {}
        },
        ReviewListEvent::SendComment { id } => {
          cx.emit(DockPanelEvent::SendReviewComment { id: *id });
        }
        ReviewListEvent::SubmitReview | ReviewListEvent::DiscardPullRequestReview => {}
        ReviewListEvent::SendReview => cx.emit(DockPanelEvent::SendReview),
        ReviewListEvent::DiscardReview => cx.emit(DockPanelEvent::DiscardReview),
      },
    )
    .detach();

    let history_list = cx.new(HistoryList::new);
    cx.subscribe(
      &history_list,
      |_this, _list, event: &HistoryListEvent, cx| match event {
        HistoryListEvent::OpenCommitFile {
          commit_oid,
          path,
          intent,
        } => {
          cx.emit(DockPanelEvent::OpenCommitFile {
            commit_oid: commit_oid.clone(),
            path: path.clone(),
            intent: *intent,
          });
        }
        HistoryListEvent::CheckoutCommit { commit_oid } => {
          cx.emit(DockPanelEvent::CheckoutCommit {
            commit_oid: commit_oid.clone(),
          });
        }
        HistoryListEvent::CreateBranchFromCommit { commit_oid, name } => {
          cx.emit(DockPanelEvent::CreateBranchFromCommit {
            commit_oid: commit_oid.clone(),
            name: name.clone(),
          });
        }
      },
    )
    .detach();

    let mut panel = Self {
      review_list,
      focus_handle: cx.focus_handle(),
      window_handle: window.window_handle(),
      project_root: repo_root.clone(),
      repo_root,
      status_entries: Vec::new(),
      status_repo_root: None,
      merge_in_progress: false,
      rebase_in_progress: false,
      head_status: HeadCommitStatus::default(),
      branch_status: None,
      default_branch: None,
      commit_input,
      commit_message_drafts: HashMap::new(),
      amend_pending: false,
      pre_amend_commit_message: None,
      committing: false,
      last_error: None,
      active_tab: DockPanelTab::Changes,
      changes_list,
      history_list,
      files_tree_state,
      files_context_menu_target: None,
      files_inline_rename: None,
      files_inline_create_placeholder_to_ignore: None,
      files_loaded: false,
      files_file_count: 0,
      files_entry_visibility: HashMap::new(),
      files_include_gitignored: settings.files_show_gitignored,
      files_include_hidden: settings.files_show_hidden,
      files_reveal_path_when_loaded: None,
      focus_files_tree_when_loaded: false,
      files_loading: false,
      files_load_generation: 0,
      changes_action_in_flight: None,
      _refresh_task: None,
      _commit_task: None,
      _amend_message_task: None,
      _files_task: None,
      _file_operation_task: None,
    };
    panel.refresh(cx);
    panel
  }

  fn load_project_files(&mut self, cx: &mut Context<Self>) {
    self.load_project_files_with_expansion(None, cx);
  }

  fn load_project_files_with_expansion(
    &mut self,
    expanded_folder_paths: Option<HashSet<String>>,
    cx: &mut Context<Self>,
  ) {
    let Some(project_root) = self.project_root.clone() else {
      return;
    };
    if self.files_loading {
      return;
    }
    self.files_loading = true;
    self.files_load_generation = self.files_load_generation.wrapping_add(1);
    let load_generation = self.files_load_generation;
    let repo_root = self.repo_root.clone();
    let load_project_root = project_root.clone();
    let scan_options = ProjectScanOptions {
      include_gitignored: self.files_include_gitignored,
      include_hidden: self.files_include_hidden,
      ..ProjectScanOptions::default()
    };

    let task = cx.spawn(async move |this, cx| {
      let files = cx
        .background_spawn(async move {
          list_project_entries_with_options(&load_project_root, &scan_options)
        })
        .await;
      let _ = this.update(cx, |this, cx| {
        if this.project_root.as_deref() != Some(project_root.as_path())
          || this.repo_root != repo_root
          || this.files_load_generation != load_generation
        {
          return;
        }
        this.files_loading = false;
        if let Ok(files) = files {
          this.files_file_count = files.iter().filter(|entry| entry.is_file()).count();
          let entries = files.into_iter().map(Rc::new).collect::<Vec<_>>();
          let entry_visibility = file_tree_entry_visibility_map(&entries);
          let expanded = expanded_folder_paths.unwrap_or_else(|| {
            if this.files_loaded {
              this.current_files_expanded_paths(cx)
            } else {
              HashSet::new()
            }
          });
          let (items, _, _, _) = build_project_tree_items_with_expansion(&entries, Some(&expanded));
          this.files_tree_state.update(cx, |state, cx| {
            state.set_items(items, cx);
          });
          this.files_entry_visibility = entry_visibility;
          this.files_inline_create_placeholder_to_ignore = None;
          this.files_loaded = true;
          if this.active_tab == DockPanelTab::Files
            && let Some(path) = this.files_reveal_path_when_loaded.take()
          {
            this.reveal_loaded_file_path(&path, cx);
          }
        }
        cx.notify();
      });
    });
    self._files_task = Some(task);
  }

  fn build_files_context_menu(
    menu: PopupMenu,
    panel: WeakEntity<Self>,
    target: FilesContextTarget,
  ) -> PopupMenu {
    let new_file_target = target.clone();
    let new_file_panel = panel.clone();
    let new_folder_target = target.clone();
    let new_folder_panel = panel.clone();
    let reveal_target = target.clone();
    let reveal_panel = panel.clone();
    let copy_path_target = target.clone();
    let copy_path_panel = panel.clone();
    let copy_relative_target = target.clone();
    let copy_relative_panel = panel.clone();
    let duplicate_target = target.clone();
    let duplicate_panel = panel.clone();
    let rename_target = target.clone();
    let rename_panel = panel.clone();
    let delete_target = target;
    let delete_panel = panel;

    menu
      .item(
        PopupMenuItem::new("New File").on_click(move |_, window, cx| {
          let target = new_file_target.clone();
          let _ = new_file_panel.update(cx, |panel, cx| {
            panel.open_new_file_dialog(target, window, cx);
          });
        }),
      )
      .item(
        PopupMenuItem::new("New Folder").on_click(move |_, window, cx| {
          let target = new_folder_target.clone();
          let _ = new_folder_panel.update(cx, |panel, cx| {
            panel.open_new_folder_dialog(target, window, cx);
          });
        }),
      )
      .separator()
      .item(
        PopupMenuItem::new("Reveal in Finder").on_click(move |_, _, cx| {
          let target = reveal_target.clone();
          let _ = reveal_panel.update(cx, |panel, cx| {
            panel.reveal_files_context_target(&target, cx);
          });
        }),
      )
      .item(PopupMenuItem::new("Copy Path").on_click(move |_, _, cx| {
        let target = copy_path_target.clone();
        let _ = copy_path_panel.update(cx, |panel, cx| {
          panel.copy_files_context_path(&target, false, cx);
        });
      }))
      .item(
        PopupMenuItem::new("Copy Relative Path").on_click(move |_, _, cx| {
          let target = copy_relative_target.clone();
          let _ = copy_relative_panel.update(cx, |panel, cx| {
            panel.copy_files_context_path(&target, true, cx);
          });
        }),
      )
      .when(
        duplicate_target.relative_path.is_some() && !duplicate_target.is_folder,
        |menu| {
          menu
            .separator()
            .item(PopupMenuItem::new("Duplicate").on_click(move |_, _, cx| {
              let target = duplicate_target.clone();
              let _ = duplicate_panel.update(cx, |panel, cx| {
                panel.duplicate_file_from_context(target, cx);
              });
            }))
        },
      )
      .when(rename_target.relative_path.is_some(), |menu| {
        menu
          .separator()
          .item(PopupMenuItem::new("Rename").on_click(move |_, window, cx| {
            let target = rename_target.clone();
            let _ = rename_panel.update(cx, |panel, cx| {
              panel.start_inline_rename_from_context(target, window, cx);
            });
          }))
          .item(PopupMenuItem::new("Delete").on_click(move |_, window, cx| {
            let target = delete_target.clone();
            let _ = delete_panel.update(cx, |panel, cx| {
              panel.confirm_delete_file_context_target(target, window, cx);
            });
          }))
      })
  }

  pub(crate) fn reveal_file_path(&mut self, path: &Path, cx: &mut Context<Self>) {
    if self.active_tab != DockPanelTab::Files {
      return;
    }
    if !self.files_loaded {
      self.files_reveal_path_when_loaded = Some(path.to_path_buf());
      return;
    }
    self.reveal_loaded_file_path(path, cx);
  }

  fn reveal_loaded_file_path(&mut self, path: &Path, cx: &mut Context<Self>) {
    let id: SharedString = file_tree_path_id(path).into();
    self.files_tree_state.update(cx, |tree, cx| {
      tree.reveal_item(&id, gpui::ScrollStrategy::Center, cx);
      if let Some(index) = tree.index_of(&id) {
        tree.set_selected_index(Some(index), cx);
      }
    });
  }

  fn current_files_expanded_paths(&self, cx: &App) -> HashSet<String> {
    let tree = self.files_tree_state.read(cx);
    let mut expanded = HashSet::new();
    let mut index = 0;
    while let Some(entry) = tree.entry(index) {
      if entry.is_folder() && entry.is_expanded() {
        expanded.insert(entry.item().id.to_string());
      }
      index += 1;
    }
    expanded
  }

  fn apply_files_rename_to_tree(
    &mut self,
    old_relative_path: &Path,
    new_relative_path: &Path,
    cx: &mut Context<Self>,
  ) {
    let old_id = file_tree_path_id(old_relative_path);
    let new_id = file_tree_path_id(new_relative_path);
    let mut entries = self.current_project_tree_entries(cx);
    if entries.is_empty() {
      return;
    }

    let mut renamed_any = false;
    for entry in &mut entries {
      let old_path = entry.path_id();
      let renamed = renamed_file_tree_path_id(&old_path, &old_id, &new_id);
      if renamed != old_path {
        Rc::make_mut(entry).path = PathBuf::from(renamed);
        renamed_any = true;
      }
    }
    if !renamed_any {
      return;
    }

    let expanded = self
      .current_files_expanded_paths(cx)
      .into_iter()
      .map(|path| renamed_file_tree_path_id(&path, &old_id, &new_id))
      .collect::<HashSet<_>>();
    let entry_visibility = file_tree_entry_visibility_map(&entries);
    let (items, _, _, _) = build_project_tree_items_with_expansion(&entries, Some(&expanded));
    self.files_tree_state.update(cx, |tree, cx| {
      tree.set_items(items, cx);
    });
    self.files_entry_visibility = entry_visibility;
    cx.notify();
  }

  fn reload_project_files_after_operation(&mut self, cx: &mut Context<Self>) {
    let mut expanded = if self.files_loaded {
      self.current_files_expanded_paths(cx)
    } else {
      HashSet::new()
    };
    if let Some(path) = &self.files_reveal_path_when_loaded
      && let Some(parent) = path.parent()
    {
      add_path_and_ancestors_to_expanded(parent, &mut expanded);
    }
    self.files_loading = false;
    self._files_task = None;
    self.load_project_files_with_expansion(Some(expanded), cx);
    self.refresh_status(cx);
    cx.notify();
  }

  fn run_file_operation<F>(
    &mut self,
    failure_label: &'static str,
    operation: F,
    cx: &mut Context<Self>,
  ) where
    F: FnOnce() -> anyhow::Result<()> + Send + 'static,
  {
    self.run_file_operation_after_success(failure_label, operation, |_, _| {}, cx);
  }

  fn run_file_operation_after_success<F, S>(
    &mut self,
    failure_label: &'static str,
    operation: F,
    after_success: S,
    cx: &mut Context<Self>,
  ) where
    F: FnOnce() -> anyhow::Result<()> + Send + 'static,
    S: FnOnce(&mut Self, &mut Context<Self>) + 'static,
  {
    let window_handle = self.window_handle;
    let task = cx.spawn(async move |this, cx| {
      let result = cx.background_spawn(async move { operation() }).await;
      let _ = this.update(cx, |this, cx| match result {
        Ok(()) => {
          after_success(this, cx);
          this.reload_project_files_after_operation(cx);
        }
        Err(error) => {
          let _ = cx.update_window(window_handle, |_, window, cx| {
            window.push_notification(Notification::error(format!("{failure_label}: {error}")), cx);
          });
        }
      });
    });
    self._file_operation_task = Some(task);
  }

  fn absolute_files_context_path(&self, target: &FilesContextTarget) -> Option<PathBuf> {
    let root = self.project_root.as_ref()?;
    Some(match target.relative_path.as_ref() {
      Some(relative_path) => root.join(relative_path),
      None => root.clone(),
    })
  }

  fn files_context_directory(&self, target: &FilesContextTarget) -> Option<PathBuf> {
    let mut directory = target.relative_path.clone().filter(|_| target.is_folder);
    if directory.is_none() {
      directory = target
        .relative_path
        .as_ref()
        .and_then(|path| path.parent().map(Path::to_path_buf));
    }
    directory.filter(|path| !path.as_os_str().is_empty())
  }

  fn selected_files_context_target(&self, cx: &App) -> Option<FilesContextTarget> {
    if self.active_tab != DockPanelTab::Files {
      return None;
    }
    let tree = self.files_tree_state.read(cx);
    let entry = tree.selected_entry()?;
    let id = entry.item().id.as_ref();
    if is_empty_folder_placeholder_id(id) {
      return None;
    }
    Some(FilesContextTarget::entry(
      PathBuf::from(id),
      entry.is_folder(),
    ))
  }

  fn reveal_files_context_target(&self, target: &FilesContextTarget, cx: &mut Context<Self>) {
    let Some(path) = self.absolute_files_context_path(target) else {
      return;
    };
    cx.reveal_path(path.as_path());
  }

  fn copy_files_context_path(
    &self,
    target: &FilesContextTarget,
    relative: bool,
    cx: &mut Context<Self>,
  ) {
    let path = if relative {
      target
        .relative_path
        .clone()
        .unwrap_or_else(|| PathBuf::from("."))
    } else {
      let Some(path) = self.absolute_files_context_path(target) else {
        return;
      };
      path
    };
    cx.write_to_clipboard(gpui::ClipboardItem::new_string(
      path.to_string_lossy().into_owned(),
    ));
  }

  fn open_new_file_dialog(
    &mut self,
    target: FilesContextTarget,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    self.start_inline_create(target, false, window, cx);
  }

  fn open_new_folder_dialog(
    &mut self,
    target: FilesContextTarget,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    self.start_inline_create(target, true, window, cx);
  }

  fn start_inline_create(
    &mut self,
    target: FilesContextTarget,
    is_folder: bool,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    self.cancel_inline_rename(cx);
    self.files_inline_create_placeholder_to_ignore = None;
    let directory = self.files_context_directory(&target);
    let placeholder = self.next_inline_create_placeholder(directory.as_deref(), is_folder, cx);
    let initial_value = if is_folder {
      "untitled folder"
    } else {
      "untitled"
    };
    let input = self.create_files_inline_input(initial_value, window, cx);
    let mut expanded = self.current_files_expanded_paths(cx);
    if let Some(directory) = &directory {
      add_path_and_ancestors_to_expanded(directory, &mut expanded);
    }
    let mut entries = self.current_project_tree_entries(cx);
    entries.push(Rc::new(ProjectEntry {
      path: placeholder.clone(),
      kind: if is_folder {
        ProjectEntryKind::Directory
      } else {
        ProjectEntryKind::File
      },
      is_gitignored: false,
      is_hidden: false,
    }));
    let entry_visibility = file_tree_entry_visibility_map(&entries);
    let (items, _, _, _) = build_project_tree_items_with_expansion(&entries, Some(&expanded));
    self.files_tree_state.update(cx, |tree, cx| {
      tree.set_items(items, cx);
      if let Some(index) = tree.index_of(&file_tree_path_id(&placeholder).into()) {
        tree.set_selected_index(Some(index), cx);
      }
    });
    self.files_entry_visibility = entry_visibility;
    self.files_inline_rename = Some(FilesInlineRename {
      relative_path: placeholder,
      input: input.clone(),
      operation: FilesInlineOperation::Create {
        directory,
        is_folder,
      },
    });
    window.on_next_frame(move |window, cx| {
      input.update(cx, |input, cx| {
        input.focus(window, cx);
        input.select_all(window, cx);
      });
    });
    cx.notify();
  }

  fn next_inline_create_placeholder(
    &self,
    directory: Option<&Path>,
    is_folder: bool,
    cx: &App,
  ) -> PathBuf {
    let base_name = if is_folder {
      ".reviu-new-folder"
    } else {
      ".reviu-new-file"
    };
    for index in 0.. {
      let name = if index == 0 {
        base_name.to_string()
      } else {
        format!("{base_name}-{index}")
      };
      let path = directory
        .map(|dir| dir.join(&name))
        .unwrap_or_else(|| name.into());
      let id: SharedString = file_tree_path_id(&path).into();
      if self.files_tree_state.read(cx).index_of(&id).is_none() {
        return path;
      }
    }
    PathBuf::from(base_name)
  }

  fn current_project_tree_entries(&self, cx: &App) -> Vec<Rc<ProjectEntry>> {
    let tree = self.files_tree_state.read(cx);
    let mut entries = Vec::new();
    let mut index = 0;
    while let Some(entry) = tree.entry(index) {
      if entry.is_root() {
        collect_project_tree_entries(entry.item(), &self.files_entry_visibility, &mut entries);
      }
      index += 1;
    }
    entries
  }

  fn files_selection_after_delete(&self, relative_path: &Path, cx: &App) -> Option<PathBuf> {
    let target_id = file_tree_path_id(relative_path);
    let tree = self.files_tree_state.read(cx);
    let mut visible_ids = Vec::new();
    let mut index = 0;
    while let Some(entry) = tree.entry(index) {
      let id = entry.item().id.as_ref();
      if !is_empty_folder_placeholder_id(id) {
        visible_ids.push(id.to_string());
      }
      index += 1;
    }
    let target_index = visible_ids.iter().position(|id| id == &target_id)?;
    visible_ids
      .iter()
      .skip(target_index + 1)
      .find(|id| !file_tree_path_is_self_or_descendant(id, &target_id))
      .or_else(|| {
        visible_ids
          .iter()
          .take(target_index)
          .rev()
          .find(|id| !file_tree_path_is_self_or_descendant(id, &target_id))
      })
      .map(PathBuf::from)
  }

  fn remove_inline_create_placeholder(&mut self, placeholder: &Path, cx: &mut Context<Self>) {
    let placeholder_id = file_tree_path_id(placeholder);
    let entries = self
      .current_project_tree_entries(cx)
      .into_iter()
      .filter(|entry| entry.path_id() != placeholder_id)
      .collect::<Vec<_>>();
    let expanded = self.current_files_expanded_paths(cx);
    let entry_visibility = file_tree_entry_visibility_map(&entries);
    let (items, _, _, _) = build_project_tree_items_with_expansion(&entries, Some(&expanded));
    self.files_tree_state.update(cx, |tree, cx| {
      tree.set_items(items, cx);
    });
    self.files_entry_visibility = entry_visibility;
  }

  fn create_file_entry(
    &mut self,
    directory: Option<PathBuf>,
    name: String,
    is_folder: bool,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> bool {
    let Some(root) = self.project_root.clone() else {
      return false;
    };
    let result = child_relative_path(directory.as_deref(), &name);
    let relative_path = match result {
      Ok(path) => path,
      Err(error) => {
        window.push_notification(Notification::error(error.to_string()), cx);
        return false;
      }
    };
    let created_relative_path = relative_path.clone();
    let path = root.join(relative_path);
    if path.exists() {
      window.push_notification(
        Notification::error(format!("{} already exists", path.display())),
        cx,
      );
      return false;
    }
    self.run_file_operation_after_success(
      if is_folder {
        "Creating folder failed"
      } else {
        "Creating file failed"
      },
      move || {
        if is_folder {
          std::fs::create_dir_all(&path)?;
        } else {
          if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
          }
          std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        }
        Ok(())
      },
      move |this, cx| {
        this.files_reveal_path_when_loaded = Some(created_relative_path.clone());
        if !is_folder {
          cx.emit(DockPanelEvent::OpenFile {
            path: created_relative_path,
            intent: OpenIntent::Open,
            mode: DockPanelOpenFileMode::File,
          });
        }
      },
      cx,
    );
    true
  }

  fn start_inline_rename_from_context(
    &mut self,
    target: FilesContextTarget,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let Some(relative_path) = target.relative_path else {
      return;
    };
    self.start_inline_rename(relative_path, window, cx);
  }

  fn start_inline_rename(
    &mut self,
    relative_path: PathBuf,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    self.cancel_inline_rename(cx);
    let Some(initial_value) = relative_path
      .file_name()
      .map(|name| name.to_string_lossy().into_owned())
    else {
      return;
    };
    let input = self.create_files_inline_input(&initial_value, window, cx);

    self.files_tree_state.update(cx, |tree, cx| {
      tree.set_selected_index(None, cx);
    });
    self.files_inline_rename = Some(FilesInlineRename {
      relative_path,
      input: input.clone(),
      operation: FilesInlineOperation::Rename,
    });
    window.on_next_frame(move |window, cx| {
      input.update(cx, |input, cx| {
        input.focus(window, cx);
        input.select_all(window, cx);
      });
    });
    cx.notify();
  }

  fn create_files_inline_input(
    &self,
    initial_value: &str,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Entity<InputState> {
    let input = cx.new(|cx| InputState::new(window, cx));
    input.update(cx, |input, cx| {
      input.set_value(initial_value, window, cx);
      input.select_all(window, cx);
    });
    cx.subscribe_in(
      &input,
      window,
      |this, _input, event: &gpui_component::input::InputEvent, window, cx| match event {
        gpui_component::input::InputEvent::PressEnter { .. } => {
          this.confirm_inline_rename(window, cx);
          cx.stop_propagation();
        }
        gpui_component::input::InputEvent::Blur => this.cancel_inline_rename(cx),
        gpui_component::input::InputEvent::Change | gpui_component::input::InputEvent::Focus => {}
      },
    )
    .detach();
    input
  }

  fn cancel_inline_rename(&mut self, cx: &mut Context<Self>) {
    let Some(rename) = self.files_inline_rename.take() else {
      return;
    };
    if matches!(rename.operation, FilesInlineOperation::Create { .. }) {
      self.remove_inline_create_placeholder(&rename.relative_path, cx);
    }
    cx.notify();
  }

  fn confirm_inline_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    let Some(rename) = &self.files_inline_rename else {
      return;
    };
    let relative_path = rename.relative_path.clone();
    let name = rename.input.read(cx).value().to_string();
    let remove_placeholder = matches!(&rename.operation, FilesInlineOperation::Create { .. });
    let confirmed = match &rename.operation {
      FilesInlineOperation::Rename => {
        if relative_path
          .file_name()
          .is_some_and(|current| current.to_string_lossy() == name)
        {
          self.cancel_inline_rename(cx);
          return;
        }
        self.rename_file_entry(relative_path.clone(), name, window, cx)
      }
      FilesInlineOperation::Create {
        directory,
        is_folder,
      } => self.create_file_entry(directory.clone(), name, *is_folder, window, cx),
    };
    if confirmed {
      if remove_placeholder {
        self.files_inline_create_placeholder_to_ignore = Some(relative_path.clone());
      }
      self.files_inline_rename = None;
      if remove_placeholder {
        self.remove_inline_create_placeholder(&relative_path, cx);
      }
      cx.notify();
    }
  }

  fn on_dock_key_down(
    &mut self,
    event: &gpui::KeyDownEvent,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    if event.keystroke.key != "escape" || event.keystroke.modifiers.modified() {
      return;
    }
    let rename_focused = self
      .files_inline_rename
      .as_ref()
      .is_some_and(|rename| rename.input.read(cx).focus_handle(cx).is_focused(window));
    if rename_focused {
      self.cancel_inline_rename(cx);
      cx.stop_propagation();
    }
  }

  fn new_file_in_files_panel_action(
    &mut self,
    _: &crate::NewFileInFilesPanel,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    if self.active_tab != DockPanelTab::Files {
      cx.propagate();
      return;
    }
    let target = self
      .selected_files_context_target(cx)
      .unwrap_or_else(FilesContextTarget::root);
    self.start_inline_create(target, false, window, cx);
  }

  fn rename_selected_file_item_action(
    &mut self,
    _: &crate::RenameSelectedFileItem,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let Some(target) = self.selected_files_context_target(cx) else {
      cx.propagate();
      return;
    };
    self.start_inline_rename_from_context(target, window, cx);
  }

  fn delete_selected_file_item_action(
    &mut self,
    _: &crate::DeleteSelectedFileItem,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let Some(target) = self.selected_files_context_target(cx) else {
      cx.propagate();
      return;
    };
    self.confirm_delete_file_context_target(target, window, cx);
  }

  fn rename_file_entry(
    &mut self,
    relative_path: PathBuf,
    name: String,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> bool {
    let Some(root) = self.project_root.clone() else {
      return false;
    };
    let new_name = match validate_file_name(&name) {
      Ok(name) => name,
      Err(error) => {
        window.push_notification(Notification::error(error.to_string()), cx);
        return false;
      }
    };
    let new_relative_path = relative_path
      .parent()
      .filter(|path| !path.as_os_str().is_empty())
      .map(|parent| parent.join(&new_name))
      .unwrap_or_else(|| PathBuf::from(&new_name));
    let previous_relative_path = relative_path.clone();
    let renamed_relative_path = new_relative_path.clone();
    let from = root.join(relative_path);
    let to = root.join(new_relative_path);
    self.run_file_operation_after_success(
      "Renaming failed",
      move || {
        if !from.exists() {
          if to.exists() {
            return Ok(());
          }
          return Err(anyhow::anyhow!("{} no longer exists", from.display()));
        }
        if to.exists() {
          return Err(anyhow::anyhow!("{} already exists", to.display()));
        }
        std::fs::rename(&from, &to)?;
        Ok(())
      },
      move |this, cx| {
        this.files_reveal_path_when_loaded = Some(renamed_relative_path.clone());
        this.apply_files_rename_to_tree(&previous_relative_path, &renamed_relative_path, cx);
        cx.emit(DockPanelEvent::FileRenamed {
          old_path: previous_relative_path,
          new_path: renamed_relative_path,
        });
      },
      cx,
    );
    true
  }

  fn duplicate_file_from_context(&mut self, target: FilesContextTarget, cx: &mut Context<Self>) {
    let (Some(root), Some(relative_path)) = (self.project_root.clone(), target.relative_path)
    else {
      return;
    };
    let from = root.join(&relative_path);
    let Some(to) = next_duplicate_path(&root, &relative_path) else {
      return;
    };
    self.run_file_operation(
      "Duplicating failed",
      move || {
        std::fs::copy(&from, &to)?;
        Ok(())
      },
      cx,
    );
  }

  fn confirm_delete_file_context_target(
    &mut self,
    target: FilesContextTarget,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let (Some(path), Some(relative_path)) = (
      self.absolute_files_context_path(&target),
      target.relative_path.clone(),
    ) else {
      return;
    };
    let view = cx.entity();
    let message: SharedString = format!(
      "Delete {} permanently? This cannot be undone.",
      relative_path.to_string_lossy()
    )
    .into();
    window.open_alert_dialog(cx, move |alert, _, _| {
      let view = view.clone();
      let path = path.clone();
      ConfirmDialog::new("Delete item?", div().child(message.clone()))
        .confirm_text("Delete")
        .cancel_text("Cancel")
        .destructive()
        .on_confirm(move |_, _, cx| {
          let path = path.clone();
          view.update(cx, |panel, cx| {
            panel.delete_file_entry(path, cx);
          });
          true
        })
        .build(alert)
    });
  }

  fn delete_file_entry(&mut self, path: PathBuf, cx: &mut Context<Self>) {
    let deleted_path = self
      .project_root
      .as_ref()
      .and_then(|root| path.strip_prefix(root).ok())
      .map(Path::to_path_buf);
    let select_after_delete = deleted_path
      .as_deref()
      .and_then(|relative_path| self.files_selection_after_delete(relative_path, cx));
    self.run_file_operation_after_success(
      "Deleting failed",
      move || {
        let metadata = std::fs::metadata(&path)?;
        if metadata.is_dir() {
          std::fs::remove_dir_all(&path)?;
        } else {
          std::fs::remove_file(&path)?;
        }
        Ok(())
      },
      move |this, cx| {
        this.files_reveal_path_when_loaded = select_after_delete;
        if let Some(path) = deleted_path {
          cx.emit(DockPanelEvent::FileDeleted { path });
        }
      },
      cx,
    );
  }

  pub fn refresh(&mut self, cx: &mut Context<Self>) {
    self.refresh_all(false, cx);
  }

  /// The refresh button: being asked is reason enough to re-read everything.
  pub(crate) fn refresh_requested(&mut self, cx: &mut Context<Self>) {
    self.refresh_all(true, cx);
    cx.notify();
  }

  fn refresh_all(&mut self, force: bool, cx: &mut Context<Self>) {
    self.refresh_status(cx);
    if self.files_loaded {
      self.load_project_files(cx);
    }
    // A closed history tab catches up when it is opened again.
    if self.active_tab == DockPanelTab::History {
      self.history_list.update(cx, |list, cx| {
        if force {
          list.refresh(cx);
        } else {
          list.refresh_if_repository_moved(cx);
        }
      });
    }
  }

  /// The working tree alone: what a poll can afford to re-read, with no request
  /// to GitHub and no reload of the file tree.
  pub(crate) fn refresh_status(&mut self, cx: &mut Context<Self>) {
    let Some(repo_root) = self.repo_root.clone() else {
      self.status_entries.clear();
      self.merge_in_progress = false;
      self.rebase_in_progress = false;
      self.head_status = HeadCommitStatus::default();
      self.branch_status = None;
      self.default_branch = None;
      cx.notify();
      return;
    };

    let status_repo_root = repo_root.clone();
    let task = cx.spawn(async move |this, cx| {
      let (result, merge_in_progress, rebase_in_progress, head_status) = cx
        .background_spawn(async move {
          (
            list_repo_status(&repo_root),
            is_merge_in_progress(&repo_root).unwrap_or(false),
            is_rebase_in_progress(&repo_root).unwrap_or(false),
            head_commit_status(&repo_root).unwrap_or_default(),
          )
        })
        .await;
      let _ = this.update(cx, |this, cx| {
        this.merge_in_progress = merge_in_progress;
        this.rebase_in_progress = rebase_in_progress;
        this.head_status = head_status;
        match result {
          Ok(entries) => {
            this.changes_list.update(cx, |list, cx| {
              list.set_entries(entries.clone(), cx);
            });
            this.status_entries = entries;
            this.status_repo_root = Some(status_repo_root);
            this.last_error = None;
            cx.emit(DockPanelEvent::StatusRefreshed);
          }
          Err(error) => this.last_error = Some(format!("{error}").into()),
        }
        cx.notify();
      });
    });
    self._refresh_task = Some(task);
  }

  /// One poll tick: the working tree, plus the history when its tab is open and
  /// the repository actually moved.
  pub(crate) fn poll(&mut self, cx: &mut Context<Self>) {
    self.refresh_status(cx);
    if self.active_tab == DockPanelTab::History {
      self.history_list.update(cx, |list, cx| {
        list.refresh_if_repository_moved(cx);
      });
    }
  }

  /// Git prepared a message for the operation in progress (merge, rebase).
  pub(crate) fn set_commit_message(
    &mut self,
    message: &str,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    self
      .commit_input
      .update(cx, |input, cx| input.set_value(message, window, cx));
  }

  pub(crate) fn commit_message(&self, cx: &App) -> String {
    self.commit_input.read(cx).value().to_string()
  }

  fn remember_commit_message_draft(&mut self, cx: &App) {
    let Some(repo_root) = self.repo_root.clone() else {
      return;
    };
    let draft = if self.amend_pending {
      self
        .pre_amend_commit_message
        .clone()
        .unwrap_or_else(|| self.commit_input.read(cx).value().to_string())
    } else {
      self.commit_input.read(cx).value().to_string()
    };
    if draft.is_empty() {
      self.commit_message_drafts.remove(&repo_root);
    } else {
      self.commit_message_drafts.insert(repo_root, draft);
    }
  }

  fn current_commit_message_draft(&self) -> String {
    self
      .repo_root
      .as_ref()
      .and_then(|repo_root| self.commit_message_drafts.get(repo_root))
      .cloned()
      .unwrap_or_default()
  }

  #[cfg(test)]
  pub(crate) fn amend_pending(&self) -> bool {
    self.amend_pending
  }

  pub(crate) fn toggle_amend_pending(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    if self.amend_pending {
      self.finish_amend(window, cx);
      return;
    }

    let Some(repo_root) = self.repo_root.clone() else {
      return;
    };
    if !self.head_status.has_head_commit {
      return;
    }

    let draft = self.commit_input.read(cx).value().to_string();
    self.pre_amend_commit_message = (!draft.trim().is_empty()).then_some(draft);
    self.amend_pending = true;
    self.last_error = None;
    cx.notify();

    let window_handle = self.window_handle;
    let commit_input = self.commit_input.clone();
    let task = cx.spawn(async move |this, cx| {
      let message = cx
        .background_spawn(async move { head_commit_message(&repo_root) })
        .await;
      let _ = cx.update_window(window_handle, |_, window, cx| {
        let _ = this.update(cx, |this, cx| {
          match message {
            Ok(Some(message)) => {
              commit_input.update(cx, |input, cx| input.set_value(&message, window, cx));
            }
            Ok(None) => this.last_error = Some("No commit to amend.".into()),
            Err(error) => this.last_error = Some(format!("{error}").into()),
          }
          cx.notify();
        });
      });
    });
    self._amend_message_task = Some(task);
  }

  pub(crate) fn finish_amend(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    self.amend_pending = false;
    self._amend_message_task = None;
    let draft = self.pre_amend_commit_message.take().unwrap_or_default();
    self
      .commit_input
      .update(cx, |input, cx| input.set_value(&draft, window, cx));
    cx.notify();
  }

  pub(crate) fn set_branch_status(
    &mut self,
    branch_status: Option<git::BranchStatus>,
    cx: &mut Context<Self>,
  ) {
    self.branch_status = branch_status;
    cx.notify();
  }

  pub(crate) fn set_default_branch(
    &mut self,
    default_branch: Option<git::BranchRef>,
    cx: &mut Context<Self>,
  ) {
    self.default_branch = default_branch;
    cx.notify();
  }

  fn repo_state<'a>(&'a self, commit_message: &'a str) -> RepoState<'a> {
    let branch_status = self.branch_status.as_ref();
    let (can_push, can_force_push) =
      push_flags(branch_status, self.head_status.has_head_commit, false);
    RepoState {
      has_repo: self.repo_root.is_some(),
      merge_in_progress: self.merge_in_progress,
      rebase_in_progress: self.rebase_in_progress,
      has_head_commit: self.head_status.has_head_commit,
      can_push,
      can_force_push,
      can_undo_last_commit: self.head_status.can_undo_last_commit,
      branch_status,
      status_entries: &self.status_entries,
      selected_entry: None,
      commit_message,
    }
  }

  pub(crate) fn changes_list(&self) -> Entity<ChangesList> {
    self.changes_list.clone()
  }

  pub(crate) fn head_status(&self) -> HeadCommitStatus {
    self.head_status
  }

  pub(crate) fn merge_in_progress(&self) -> bool {
    self.merge_in_progress
  }

  pub(crate) fn rebase_in_progress(&self) -> bool {
    self.rebase_in_progress
  }

  pub(crate) fn status_entries(&self) -> &[RepoStatusEntry] {
    &self.status_entries
  }

  /// Whether `status_entries` describes the repository shown now.
  pub(crate) fn status_is_current(&self) -> bool {
    self.repo_root.is_some() && self.status_repo_root == self.repo_root
  }

  pub(crate) fn set_changes_action_in_flight(
    &mut self,
    command: Option<ChangesActionCommand>,
    cx: &mut Context<Self>,
  ) {
    if self.changes_action_in_flight == command {
      return;
    }
    self.changes_action_in_flight = command;
    cx.notify();
  }

  #[cfg(test)]
  pub(crate) fn changes_action_in_flight(&self) -> Option<ChangesActionCommand> {
    self.changes_action_in_flight
  }

  #[cfg(test)]
  pub(crate) fn project_root(&self) -> Option<&Path> {
    self.project_root.as_deref()
  }

  pub(crate) fn repo_root(&self) -> Option<&Path> {
    self.repo_root.as_deref()
  }

  pub(crate) fn set_project_and_repo_roots(
    &mut self,
    project_root: Option<PathBuf>,
    repo_root: Option<PathBuf>,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let draft = self.set_project_and_repo_roots_state(project_root, repo_root, cx);
    if let Some(draft) = draft {
      self
        .commit_input
        .update(cx, |input, cx| input.set_value(draft, window, cx));
    }
  }

  fn set_project_and_repo_roots_state(
    &mut self,
    project_root: Option<PathBuf>,
    repo_root: Option<PathBuf>,
    cx: &mut Context<Self>,
  ) -> Option<String> {
    let project_changed = self.project_root != project_root;
    let repo_changed = self.repo_root != repo_root;
    if repo_changed {
      self.remember_commit_message_draft(cx);
      self.default_branch = None;
    }
    self.project_root = project_root.clone();
    self.repo_root = repo_root.clone();
    let commit_message_draft = repo_changed.then(|| self.current_commit_message_draft());
    if project_changed {
      self.files_loaded = false;
      self.files_file_count = 0;
      self.files_entry_visibility.clear();
      self.files_reveal_path_when_loaded = None;
      self.files_loading = false;
      self.files_load_generation = self.files_load_generation.wrapping_add(1);
      self._files_task = None;
      self.files_tree_state.update(cx, |state, cx| {
        state.set_items(Vec::new(), cx);
      });
    }
    self.status_entries.clear();
    self.changes_action_in_flight = None;
    self.amend_pending = false;
    self.pre_amend_commit_message = None;
    self._amend_message_task = None;
    self.last_error = None;
    self.changes_list.update(cx, |list, cx| {
      list.set_repo_root(repo_root.clone(), cx);
    });
    self.history_list.update(cx, |list, cx| {
      list.set_repo_root(repo_root.clone(), cx);
    });
    commit_message_draft
  }

  #[cfg(test)]
  pub(crate) fn set_repo_root(&mut self, repo_root: Option<PathBuf>, cx: &mut Context<Self>) {
    let draft = self.set_project_and_repo_roots_state(repo_root.clone(), repo_root, cx);
    if let Some(draft) = draft {
      let commit_input = self.commit_input.clone();
      let _ = cx.update_window(self.window_handle, |_, window, cx| {
        commit_input.update(cx, |input, cx| input.set_value(draft, window, cx));
      });
    }
  }

  /// The history is only worth loading once its tab is opened, and it may have
  /// missed commits made while another tab was showing.
  fn refresh_history(&mut self, cx: &mut Context<Self>) {
    let repo_root = self.repo_root.clone();
    self.history_list.update(cx, |list, cx| {
      list.set_repo_root(repo_root, cx);
      if list.is_empty() {
        list.refresh(cx);
      } else {
        list.refresh_if_repository_moved(cx);
      }
    });
  }

  fn render_review_tab(&self) -> AnyElement {
    // No padding here: the list's own header and footer rules run edge to edge,
    // like the commit zone's does on the Changes tab.
    div()
      .id("dock-panel-review")
      .debug_selector(|| DOCK_PANEL_REVIEW_DEBUG_SELECTOR.to_string())
      .flex_1()
      .min_h_0()
      .min_w(px(0.0))
      .child(self.review_list.clone())
      .into_any_element()
  }

  fn render_history_tab(&self) -> AnyElement {
    div()
      .id("dock-panel-history")
      .debug_selector(|| DOCK_PANEL_HISTORY_DEBUG_SELECTOR.to_string())
      .flex_1()
      .min_h_0()
      .min_w(px(0.0))
      .px_1()
      .py_1()
      .child(self.history_list.clone())
      .into_any_element()
  }

  fn render_files_loading_state() -> AnyElement {
    v_flex()
      .debug_selector(|| "dock-panel-files-loading".to_string())
      .flex_1()
      .min_h_0()
      .gap_1()
      .px_2()
      .py_2()
      .children((0..10).map(Self::render_files_loading_row))
      .into_any_element()
  }

  fn render_files_loading_row(index: usize) -> AnyElement {
    let indent = match index % 5 {
      0 => 0.0,
      1 | 2 => 16.0,
      _ => 32.0,
    };

    h_flex()
      .w_full()
      .items_center()
      .gap_2()
      .pr_2()
      .pl(px(8.0 + indent))
      .py_1p5()
      .child(
        gpui_component::skeleton::Skeleton::new()
          .size(px(16.0))
          .rounded(px(4.0)),
      )
      .child(
        gpui_component::skeleton::Skeleton::new()
          .h(px(14.0))
          .flex_1()
          .rounded(px(999.0)),
      )
      .into_any_element()
  }

  fn has_staged_changes(&self) -> bool {
    self
      .status_entries
      .iter()
      .any(|entry| !matches!(entry.stage, RepoStage::Unstaged))
  }

  pub(crate) fn commit(&mut self, cx: &mut Context<Self>) {
    if self.committing {
      return;
    }
    if self.rebase_in_progress {
      if !crate::changes_list::has_conflicted_entries(&self.status_entries) {
        cx.emit(DockPanelEvent::ContinueRebase);
      }
      return;
    }
    let message = self.commit_input.read(cx).value().to_string();
    if self.amend_pending {
      if self.head_status.has_head_commit
        && !message.trim().is_empty()
        && !crate::changes_list::has_conflicted_entries(&self.status_entries)
      {
        cx.emit(DockPanelEvent::AmendCommit);
      }
      return;
    }
    let Some(repo_root) = self.repo_root.clone() else {
      return;
    };
    if message.trim().is_empty() || self.status_entries.is_empty() {
      return;
    }
    let stage_all_needed = !self.has_staged_changes();
    self.committing = true;
    self.last_error = None;
    cx.notify();

    let window_handle = self.window_handle;
    let commit_input = self.commit_input.clone();
    let committed_repo_root = repo_root.clone();
    let task = cx.spawn(async move |this, cx| {
      let result = cx
        .background_spawn(async move {
          if stage_all_needed {
            stage_all(&repo_root)?;
          }
          commit_changes(&repo_root, &message)
        })
        .await;

      let _ = this.update(cx, |this, cx| {
        this.committing = false;
        match result {
          Ok(()) => {
            this.commit_message_drafts.remove(&committed_repo_root);
            if this.repo_root.as_deref() == Some(committed_repo_root.as_path()) {
              let _ = cx.update_window(window_handle, |_, window, cx| {
                commit_input.update(cx, |input, cx| input.set_value("", window, cx));
              });
            }
            cx.emit(DockPanelEvent::Committed);
          }
          Err(error) => this.last_error = Some(format!("{error}").into()),
        }
        this.refresh(cx);
      });
    });
    self._commit_task = Some(task);
  }

  fn render_commit_zone(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
    let theme = cx.theme().clone();
    // A rebase ends by continuing it, not by writing another commit message.
    let continuing_rebase = self.rebase_in_progress;
    let message = self.commit_input.read(cx).value();
    let has_conflicts = crate::changes_list::has_conflicted_entries(&self.status_entries);
    let can_commit = if continuing_rebase {
      !has_conflicts
    } else if self.amend_pending {
      !self.committing
        && self.head_status.has_head_commit
        && !message.trim().is_empty()
        && !has_conflicts
    } else {
      !self.committing && !self.status_entries.is_empty() && !message.trim().is_empty()
    };
    let commit_shortcut = crate::shortcuts::resolved_display_shortcut_keystroke_in(
      cx,
      window,
      crate::shortcuts::ShortcutId::CommitChanges,
    );

    v_flex()
      .gap_2()
      .p_2()
      .border_t_1()
      .border_color(theme.border)
      .when_some(self.last_error.clone(), |this, error| {
        this.child(div().text_xs().text_color(theme.status_red()).child(error))
      })
      .child(div().w_full().min_w_0().key_context("CommitInput").child({
        let commit_box = Textarea::new(&self.commit_input).w_full();
        commit_box.into_any_element()
      }))
      .child(h_flex().w_full().child(self.render_commit_button(
        continuing_rebase,
        can_commit,
        commit_shortcut,
        cx,
      )))
      .into_any_element()
  }

  /// The primary action, plus the menu of what else can be done to the last
  /// commit or the branch.
  fn render_commit_button(
    &self,
    continuing_rebase: bool,
    can_commit: bool,
    commit_shortcut: gpui::Keystroke,
    cx: &mut Context<Self>,
  ) -> AnyElement {
    let commit_message = self.commit_input.read(cx).value().to_string();
    let state = self.repo_state(&commit_message);
    let menu_items =
      [CommitMenuCommand::Amend].map(|command| (command, state.allows(command.rule())));
    let menu_enabled = menu_items.iter().any(|(_, allowed)| *allowed);
    let amend_pending = self.amend_pending;
    let view = cx.entity();

    h_flex()
      .w_full()
      .child(
        Button::new("dock-panel-commit")
          .tab_index(2)
          .label(if continuing_rebase {
            "Continue rebase"
          } else if self.amend_pending {
            "Amend"
          } else {
            "Commit"
          })
          .debug_selector(|| DOCK_PANEL_COMMIT_DEBUG_SELECTOR.to_string())
          .with_variant(gpui_component::button::ButtonVariant::Secondary)
          .outline()
          .small()
          .w_full()
          .child(gpui_component::kbd::Kbd::new(commit_shortcut).ml_1())
          .loading(self.committing)
          .disabled(!can_commit)
          .on_click(cx.listener(|this, _, _, cx| {
            if this.rebase_in_progress {
              cx.emit(DockPanelEvent::ContinueRebase);
              return;
            }
            this.commit(cx)
          }))
          .flex_1()
          .rounded_r_none(),
      )
      .child(
        Button::new("dock-panel-commit-menu")
          .tab_index(3)
          .icon(IconName::ChevronDown)
          .with_variant(gpui_component::button::ButtonVariant::Secondary)
          .outline()
          .small()
          .rounded_l_none()
          .border_l_0()
          .debug_selector(|| DOCK_PANEL_COMMIT_MENU_DEBUG_SELECTOR.to_string())
          .disabled(!menu_enabled)
          .dropdown_menu_with_anchor(Anchor::BottomRight, move |menu, _, _| {
            menu_items.iter().fold(menu, |menu, (command, allowed)| {
              let view = view.clone();
              let command = *command;
              menu.item(
                PopupMenuItem::new(command.label(amend_pending))
                  .icon(command.icon(amend_pending))
                  .disabled(!allowed)
                  .on_click(move |_, _, cx| {
                    view.update(cx, |_, cx| cx.emit(DockPanelEvent::RunCommand(command)));
                  }),
              )
            })
          }),
      )
      .into_any_element()
  }

  fn render_changes_actions(&self, cx: &mut Context<Self>) -> AnyElement {
    let theme = cx.theme().clone();
    let commit_message = self.commit_input.read(cx).value().to_string();
    let state = self.repo_state(&commit_message);
    let menu_items = changes_action_menu_items(&state);
    let primary_command = changes_primary_command(&state);
    let changes_action_in_flight = self.changes_action_in_flight;
    let file_count = self.status_entries.len();
    let summary = match file_count {
      0 => "No changes".to_string(),
      1 => "1 file".to_string(),
      count => format!("{count} files"),
    };
    let view = cx.entity();

    h_flex()
      .h(px(DOCK_PANEL_HEADER_HEIGHT_PX))
      .min_h(px(DOCK_PANEL_HEADER_HEIGHT_PX))
      .max_h(px(DOCK_PANEL_HEADER_HEIGHT_PX))
      .flex_shrink_0()
      .items_center()
      .justify_between()
      .gap_2()
      .text_xs()
      .border_b_1()
      .border_color(theme.border)
      .px_2()
      .child(
        div()
          .debug_selector(|| DOCK_PANEL_CHANGES_SUMMARY_DEBUG_SELECTOR.to_string())
          .min_w_0()
          .truncate()
          .text_xs()
          .text_color(theme.muted_foreground)
          .child(summary),
      )
      .child(
        h_flex()
          .items_center()
          .flex_shrink_0()
          .when_some(primary_command, |this, command| {
            this.child(
              Button::new("dock-panel-changes-action")
                .label(command.label())
                .icon(command.icon())
                .debug_selector(|| DOCK_PANEL_CHANGES_ACTION_DEBUG_SELECTOR.to_string())
                .with_variant(gpui_component::button::ButtonVariant::Secondary)
                .outline()
                .compact()
                .xsmall()
                .rounded_r_none()
                .loading(changes_action_in_flight == Some(command))
                .disabled(changes_action_in_flight.is_some())
                .on_click(cx.listener(move |_, _, _, cx| {
                  cx.emit(DockPanelEvent::RunChangesAction(command));
                })),
            )
          })
          .when(!menu_items.is_empty(), |this| {
            let menu_view = view.clone();
            let menu_button = Button::new("dock-panel-changes-action-menu")
              .icon(IconName::ChevronDown)
              .debug_selector(|| DOCK_PANEL_CHANGES_ACTION_MENU_DEBUG_SELECTOR.to_string())
              .with_variant(gpui_component::button::ButtonVariant::Secondary)
              .outline()
              .compact()
              .xsmall()
              .tooltip("Changes actions")
              .when(primary_command.is_some(), |button| {
                button.rounded_l_none().border_l_0()
              })
              .when(primary_command.is_none(), |button| button.label("Actions"))
              .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                build_changes_action_menu(
                  menu,
                  menu_items.clone(),
                  menu_view.clone(),
                  changes_action_in_flight,
                )
              });
            this.child(menu_button)
          }),
      )
      .into_any_element()
  }

  fn set_files_include_gitignored(&mut self, include: bool, cx: &mut Context<Self>) {
    if self.files_include_gitignored == include {
      return;
    }
    self.files_include_gitignored = include;
    AppSettings::update(cx, |settings| settings.files_show_gitignored = include);
    self.reload_project_files_for_scan_options(cx);
  }

  fn set_files_include_hidden(&mut self, include: bool, cx: &mut Context<Self>) {
    if self.files_include_hidden == include {
      return;
    }
    self.files_include_hidden = include;
    AppSettings::update(cx, |settings| settings.files_show_hidden = include);
    self.reload_project_files_for_scan_options(cx);
  }

  fn sync_files_scan_options_with_settings(&mut self, cx: &mut Context<Self>) -> bool {
    let settings = AppSettings::get(cx);
    if self.files_include_gitignored == settings.files_show_gitignored
      && self.files_include_hidden == settings.files_show_hidden
    {
      return false;
    }
    self.files_include_gitignored = settings.files_show_gitignored;
    self.files_include_hidden = settings.files_show_hidden;
    self.reload_project_files_for_scan_options(cx);
    true
  }

  fn reload_project_files_for_scan_options(&mut self, cx: &mut Context<Self>) {
    let expanded = self
      .files_loaded
      .then(|| self.current_files_expanded_paths(cx));
    self.files_loaded = false;
    self.files_file_count = 0;
    self.files_loading = false;
    self._files_task = None;
    self.load_project_files_with_expansion(expanded, cx);
    cx.notify();
  }

  fn render_files_action_menu(&self, cx: &mut Context<Self>) -> AnyElement {
    let include_gitignored = self.files_include_gitignored;
    let include_hidden = self.files_include_hidden;
    let view = cx.entity();

    Button::new("dock-panel-files-action-menu")
      .icon(IconName::EllipsisVertical)
      .debug_selector(|| DOCK_PANEL_FILES_ACTION_MENU_DEBUG_SELECTOR.to_string())
      .ghost()
      .compact()
      .xsmall()
      .tooltip("Files actions")
      .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
        let gitignored_view = view.clone();
        let hidden_view = view.clone();
        menu
          .item(
            PopupMenuItem::new(if include_gitignored {
              "Hide Gitignored Files"
            } else {
              "Show Gitignored Files"
            })
            .icon(if include_gitignored {
              IconName::EyeOff
            } else {
              IconName::Eye
            })
            .on_click(move |_, _, cx| {
              gitignored_view.update(cx, |panel, cx| {
                panel.set_files_include_gitignored(!include_gitignored, cx);
              });
            }),
          )
          .item(
            PopupMenuItem::new(if include_hidden {
              "Hide Hidden Files"
            } else {
              "Show Hidden Files"
            })
            .icon(if include_hidden {
              IconName::EyeOff
            } else {
              IconName::Eye
            })
            .on_click(move |_, _, cx| {
              hidden_view.update(cx, |panel, cx| {
                panel.set_files_include_hidden(!include_hidden, cx);
              });
            }),
          )
      })
      .into_any_element()
  }

  /// Opens a tab and gives it focus, loading what that tab needs the first time.
  pub(crate) fn open_tab(
    &mut self,
    target: DockPanelTab,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    if self.active_tab != target {
      self.active_tab = target;
      match target {
        DockPanelTab::History => self.refresh_history(cx),
        DockPanelTab::Changes | DockPanelTab::Files | DockPanelTab::Review => {}
      }
    }
    match target {
      DockPanelTab::Changes => {
        if self.status_entries.is_empty() {
          // The empty state mounts no list; its handle would drop the focus.
          window.focus(&self.focus_handle, cx);
        } else {
          let list = self.changes_list.clone();
          list.update(cx, |list, cx| list.focus(window, cx));
        }
      }
      DockPanelTab::History => {
        let history = self.history_list.clone();
        history.update(cx, |history, cx| history.focus(window, cx));
      }
      DockPanelTab::Files => {
        if self.files_loaded {
          self
            .files_tree_state
            .update(cx, |tree, cx| tree.focus(window, cx));
        } else {
          self.focus_files_tree_when_loaded = true;
          window.focus(&self.focus_handle, cx);
        }
      }
      DockPanelTab::Review => {
        if self.review_list.read(cx).is_empty() {
          // The empty state mounts no list; its handle would drop the focus.
          window.focus(&self.focus_handle, cx);
        } else {
          let review = self.review_list.clone();
          review.update(cx, |review, cx| review.focus(window, cx));
        }
      }
    }
    cx.notify();
  }

  /// Whether the keyboard is already in the surface a tab's shortcut focuses.
  /// Being in the panel is not enough: the commit box is in the Changes tab, and
  /// its shortcut should reach the file list from there.
  #[cfg(test)]
  pub(crate) fn review_list(&self) -> &Entity<ReviewList> {
    &self.review_list
  }

  pub(crate) fn tab_has_focus(&self, tab: DockPanelTab, window: &Window, cx: &App) -> bool {
    // An empty surface mounts nothing to focus and the panel holds it instead;
    // the shortcut still has to be able to send the dock away.
    if self.focus_handle.is_focused(window) {
      return true;
    }
    match tab {
      DockPanelTab::Changes => self.changes_list.read(cx).is_focused(window, cx),
      DockPanelTab::Review => self.review_list.read(cx).is_focused(window, cx),
      DockPanelTab::Files => self
        .files_tree_state
        .read(cx)
        .focus_handle(cx)
        .contains_focused(window, cx),
      DockPanelTab::History => self.history_list.read(cx).tree_has_focus(window, cx),
    }
  }

  /// Switches tab without taking the focus: for the panel following what the page
  /// is doing, rather than the user asking for it.
  pub(crate) fn select_tab(&mut self, target: DockPanelTab, cx: &mut Context<Self>) {
    if self.active_tab == target {
      return;
    }
    self.active_tab = target;
    cx.notify();
  }

  pub(crate) fn active_tab(&self) -> DockPanelTab {
    self.active_tab
  }

  fn render_files_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
    // Rendering is the first moment the tree exists to take the focus the tab
    // was opened for.
    if self.focus_files_tree_when_loaded && self.files_loaded {
      self.focus_files_tree_when_loaded = false;
      self
        .files_tree_state
        .update(cx, |tree, cx| tree.focus(window, cx));
    }

    if self.project_root.is_none() {
      return self.render_empty_state(cx);
    }

    if self.sync_files_scan_options_with_settings(cx) {
      return Self::render_files_loading_state();
    }

    if !self.files_loaded {
      if !self.files_loading {
        self.load_project_files(cx);
      }
      return Self::render_files_loading_state();
    }

    let (file_statuses, folder_statuses) = file_tree_status_maps(&self.status_entries);
    let entry_visibility = self.files_entry_visibility.clone();

    let panel = cx.entity().downgrade();
    let inline_rename = self.files_inline_rename.as_ref().map(|rename| {
      (
        rename.relative_path.clone(),
        rename.input.clone(),
        matches!(rename.operation, FilesInlineOperation::Create { .. }),
      )
    });
    let root_menu_panel = panel.clone();
    let clear_context_target_panel = panel.clone();
    let files_tree_state = self.files_tree_state.clone();

    v_flex()
      .debug_selector(|| "dock-panel-files-empty-space".to_string())
      .flex_1()
      .min_h_0()
      .py_1()
      .px_1()
      .capture_any_mouse_down(move |event, _, cx| {
        if event.button == gpui::MouseButton::Right {
          let _ = clear_context_target_panel.update(cx, |panel, _| {
            panel.files_context_menu_target = None;
          });
        }
      })
      .on_mouse_down(gpui::MouseButton::Left, {
        let panel = panel.clone();
        move |_, _, cx| {
          let _ = panel.update(cx, |panel, cx| panel.cancel_inline_rename(cx));
        }
      })
      .context_menu(move |menu, _, cx| {
        let target = root_menu_panel
          .update(cx, |panel, _| {
            panel
              .files_context_menu_target
              .take()
              .unwrap_or_else(FilesContextTarget::root)
          })
          .unwrap_or_else(|_| FilesContextTarget::root());
        Self::build_files_context_menu(menu, root_menu_panel.clone(), target)
      })
      .child(tree(
        &self.files_tree_state,
        move |ix, entry, selected, window, cx| {
          let theme = cx.theme().clone();
          let item = entry.item();
          let is_folder = entry.is_folder();
          let visibility = entry_visibility
            .get(item.id.as_ref())
            .copied()
            .unwrap_or_default();
          let is_subdued_entry = visibility.is_gitignored;
          let icon: AnyElement = if is_folder {
            Icon::new(if entry.is_expanded() {
              IconName::FolderOpen
            } else {
              IconName::Folder
            })
            .size_3()
            .text_color(theme.muted_foreground)
            .into_any_element()
          } else {
            ui::file_icon_path_for_name_with_theme(item.label.as_ref(), &theme)
              .map(|path| img(path).size(px(ui::FILE_ICON_SIZE_PX)).into_any_element())
              .unwrap_or_else(|| {
                Icon::new(IconName::File)
                  .size_3()
                  .text_color(theme.muted_foreground)
                  .into_any_element()
              })
          };
          let is_empty_placeholder = is_empty_folder_placeholder_id(item.id.as_ref());
          let relative_path = PathBuf::from(item.id.as_ref());
          let is_inline_create_placeholder = inline_rename
            .as_ref()
            .is_some_and(|(path, _, is_create)| *is_create && path == &relative_path);
          let is_internal_placeholder = is_empty_placeholder || is_inline_create_placeholder;
          let rename_input = inline_rename
            .as_ref()
            .filter(|(path, _, _)| path == &relative_path)
            .map(|(_, input, _)| input.clone());
          let status = if is_folder {
            folder_statuses.get(&relative_path).copied()
          } else {
            file_statuses.get(&relative_path).copied()
          };
          let context_target = FilesContextTarget::entry(relative_path.clone(), is_folder);
          let context_target_panel = panel.clone();
          let drag_path = relative_path.clone();
          let click_path = relative_path.clone();
          let click_panel = panel.clone();
          let click_tree_state = files_tree_state.clone();
          let is_draggable_file = !is_folder && !is_internal_placeholder && rename_input.is_none();

          let label: AnyElement = if let Some(input) = rename_input {
            if !input.read(cx).focus_handle(cx).is_focused(window) {
              input.update(cx, |input, cx| {
                input.focus(window, cx);
                input.select_all(window, cx);
              });
            }
            div()
              .debug_selector({
                let id = item.id.clone();
                move || format!("dock-panel-file-rename-input-{id}")
              })
              .capture_action({
                let panel = panel.clone();
                move |_: &input::Escape, _, cx| {
                  let _ = panel.update(cx, |panel, cx| panel.cancel_inline_rename(cx));
                  cx.stop_propagation();
                }
              })
              .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
              .h(px(19.5))
              .min_w_0()
              .flex_1()
              .child(
                Input::new(&input)
                  .xsmall()
                  .appearance(false)
                  .bordered(false)
                  .focus_bordered(false)
                  .w_full()
                  .h(px(19.5))
                  .px_0()
                  .py_0(),
              )
              .into_any_element()
          } else {
            div()
              .flex_1()
              .overflow_hidden()
              .text_ellipsis()
              .text_xs()
              .when(is_empty_placeholder, |this| {
                this.text_color(theme.muted_foreground).italic()
              })
              .when(is_subdued_entry && !is_empty_placeholder, |this| {
                this.opacity(FILE_TREE_SUBDUED_ENTRY_OPACITY)
              })
              .child(item.label.clone())
              .into_any_element()
          };

          let indent = px(8.) + px(14.) * entry.depth();
          ui::selectable_list_item(ix, selected, ui::SelectableRowStyle::Inset, &theme)
            .when(!is_internal_placeholder, |this| {
              this.on_mouse_down(gpui::MouseButton::Right, move |_, _, cx| {
                let target = context_target.clone();
                let _ = context_target_panel.update(cx, |panel, _| {
                  panel.files_context_menu_target = Some(target);
                });
              })
            })
            .when(is_draggable_file, |this| {
              this
                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(move |_, window, cx| {
                  cx.stop_propagation();
                  click_tree_state.update(cx, |tree, cx| {
                    tree.set_selected_index(Some(ix), cx);
                    tree.focus(window, cx);
                  });
                  let path = click_path.clone();
                  let _ = click_panel.update(cx, |panel, cx| {
                    panel.cancel_inline_rename(cx);
                    cx.emit(DockPanelEvent::OpenFile {
                      path,
                      intent: OpenIntent::Open,
                      mode: DockPanelOpenFileMode::File,
                    });
                  });
                })
                .on_drag(
                  CenterFileDrag {
                    path: drag_path,
                    mode: CenterFileDragMode::File,
                  },
                  |drag, _, _, cx| {
                    cx.stop_propagation();
                    cx.new(|_| drag.clone())
                  },
                )
            })
            .w_full()
            .px_2()
            .pl(indent)
            .child(
              h_flex()
                .id(format!("dock-panel-file-context-{}", item.id.as_ref()))
                .debug_selector({
                  let id = item.id.clone();
                  move || format!("dock-panel-file-{id}")
                })
                .w_full()
                .items_center()
                .gap_2()
                .child(
                  div()
                    .flex_none()
                    .when(is_subdued_entry, |this| {
                      this.opacity(FILE_TREE_SUBDUED_ENTRY_OPACITY)
                    })
                    .child(icon),
                )
                .child(label)
                .when_some(status, |this, status| {
                  this.child(render_file_tree_status_badge(
                    status,
                    &theme,
                    item.id.clone(),
                  ))
                }),
            )
        },
      ))
      .into_any_element()
  }

  fn render_changes_empty_state(&self, cx: &mut Context<Self>) -> AnyElement {
    if self.repo_root.is_none() {
      return self.render_empty_state(cx);
    }

    v_flex()
      .debug_selector(|| DOCK_PANEL_CHANGES_EMPTY_DEBUG_SELECTOR.to_string())
      .flex_1()
      .min_h_0()
      .items_center()
      .justify_center()
      .px_4()
      .child(
        div()
          .text_xs()
          .text_color(cx.theme().muted_foreground)
          .child("No changes to commit"),
      )
      .into_any_element()
  }

  fn render_empty_state(&self, cx: &mut Context<Self>) -> AnyElement {
    let theme = cx.theme().clone();
    let (icon, title, description, first_hint, second_hint) = if self.project_root.is_some() {
      (
        UiIconName::CircleSlash,
        "No Git repository",
        "This project is a plain folder, so Reviu cannot show changes or history here.",
        (UiIconName::FolderPlus, "Open a repository"),
        (UiIconName::FileCode, "Browse files from the Files tab"),
      )
    } else {
      (
        UiIconName::FolderPlus,
        "No project open",
        "Add a project to browse files, track changes, and review an agent's work.",
        (UiIconName::FolderPlus, "Open a folder or repository"),
        (UiIconName::MessageCircle, "Sessions will follow it here"),
      )
    };

    v_flex()
      .debug_selector(|| "dock-panel-empty-state".to_string())
      .flex_1()
      .min_h_0()
      .items_center()
      .justify_start()
      .px_4()
      .pt(px(96.0))
      .child(
        v_flex()
          .w_full()
          .max_w(px(320.0))
          .gap_3()
          .rounded(px(16.0))
          .border_1()
          .border_color(theme.border.opacity(0.75))
          .bg(theme.secondary.opacity(0.45))
          .p_4()
          .child(
            h_flex()
              .items_center()
              .gap_3()
              .child(
                h_flex()
                  .size(px(54.0))
                  .flex_shrink_0()
                  .items_center()
                  .justify_center()
                  .rounded(px(16.0))
                  .border_1()
                  .border_color(theme.primary.opacity(0.22))
                  .bg(theme.primary.opacity(0.10))
                  .child(Icon::new(icon).size_6().text_color(theme.primary)),
              )
              .child(
                v_flex()
                  .min_w_0()
                  .gap_1()
                  .child(
                    div()
                      .text_sm()
                      .font_weight(gpui::FontWeight::SEMIBOLD)
                      .text_color(theme.foreground)
                      .child(title),
                  )
                  .child(
                    div()
                      .text_xs()
                      .line_height(px(17.0))
                      .text_color(theme.muted_foreground)
                      .child(description),
                  ),
              ),
          )
          .child(
            v_flex()
              .w_full()
              .gap_2()
              .child(Self::render_empty_hint(first_hint, &theme))
              .child(Self::render_empty_hint(second_hint, &theme)),
          ),
      )
      .into_any_element()
  }

  fn render_empty_hint(
    hint: (UiIconName, &'static str),
    theme: &gpui_component::Theme,
  ) -> AnyElement {
    h_flex()
      .w_full()
      .min_w_0()
      .items_center()
      .gap_2()
      .rounded(px(10.0))
      .border_1()
      .border_color(theme.border.opacity(0.65))
      .bg(theme.background.opacity(0.7))
      .px_2()
      .py_2()
      .child(Icon::new(hint.0).size_3().text_color(theme.primary))
      .child(
        div()
          .min_w_0()
          .text_xs()
          .text_color(theme.muted_foreground)
          .child(hint.1),
      )
      .into_any_element()
  }
}

impl Render for DockPanel {
  fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    let theme = cx.theme().clone();

    let header = h_flex()
      .debug_selector(|| "dock-panel-header".to_string())
      .h(px(DOCK_PANEL_HEADER_HEIGHT_PX))
      .min_h(px(DOCK_PANEL_HEADER_HEIGHT_PX))
      .max_h(px(DOCK_PANEL_HEADER_HEIGHT_PX))
      .flex_shrink_0()
      .items_center()
      .justify_between()
      .text_xs()
      .px_2()
      .border_b_1()
      .border_color(theme.border)
      .child(
        div()
          .flex_1()
          .min_w(px(0.0))
          .overflow_hidden()
          .truncate()
          .text_xs()
          .font_weight(gpui::FontWeight::SEMIBOLD)
          .text_color(theme.muted_foreground)
          .child(match self.active_tab {
            DockPanelTab::Changes => "Changes",
            DockPanelTab::Review => "Review",
            DockPanelTab::Files => "Files",
            DockPanelTab::History => "History",
          }),
      )
      .child(
        h_flex()
          .min_w(px(0.0))
          .items_center()
          .gap_1()
          .child(
            Button::new("dock-panel-refresh")
              .debug_selector(|| DOCK_PANEL_REFRESH_DEBUG_SELECTOR.to_string())
              .icon(UiIconName::RefreshCw)
              .ghost()
              .compact()
              .xsmall()
              .tooltip("Refresh")
              .on_click(cx.listener(|this, _, _, cx| this.refresh_requested(cx))),
          )
          .when(self.active_tab == DockPanelTab::Files, |this| {
            this.child(self.render_files_action_menu(cx))
          }),
      );

    let body = match self.active_tab {
      DockPanelTab::Files => self.render_files_tab(_window, cx),
      DockPanelTab::Changes => {
        let content = if self.status_entries.is_empty() {
          self.render_changes_empty_state(cx)
        } else {
          let commit_message = self.commit_input.read(cx).value().to_string();
          let state = self.repo_state(&commit_message);
          let menu_items = changes_action_menu_items(&state);
          let menu_view = cx.entity();
          let changes_action_in_flight = self.changes_action_in_flight;
          let context_menu_changes_list = self.changes_list.clone();
          let clear_context_menu_changes_list = self.changes_list.clone();
          div()
            .id("dock-panel-file-list")
            .key_context("ChangesList")
            .debug_selector(|| "dock-panel-file-list".to_string())
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .py_1()
            .px_1()
            .capture_any_mouse_down(move |event, _, cx| {
              if event.button == gpui::MouseButton::Right {
                clear_context_menu_changes_list.update(cx, |list, _| {
                  list.clear_context_menu_target();
                });
              }
            })
            .child(self.changes_list.clone())
            .context_menu(move |menu, _, cx| {
              if context_menu_changes_list.update(cx, |list, _| list.consume_context_menu_target())
              {
                menu
              } else {
                build_changes_action_menu(
                  menu,
                  menu_items.clone(),
                  menu_view.clone(),
                  changes_action_in_flight,
                )
              }
            })
            .into_any_element()
        };
        v_flex()
          .flex_1()
          .min_h_0()
          .when(self.repo_root.is_some(), |this| {
            this.child(self.render_changes_actions(cx))
          })
          .child(content)
          .into_any_element()
      }
      DockPanelTab::Review => self.render_review_tab(),
      DockPanelTab::History => self.render_history_tab(),
    };

    let mut panel = v_flex()
      .size_full()
      .min_w(px(0.0))
      .min_h_0()
      .bg(theme.sidebar)
      .track_focus(&self.focus_handle)
      // One stop of the window, and its own order inside: the surface first,
      // then what the tab holds besides it.
      .tab_group()
      .key_context(crate::shortcuts::DOCK_PANEL_CONTEXT)
      .on_key_down(cx.listener(Self::on_dock_key_down))
      .on_action(cx.listener(|this, _: &crate::CommitChanges, _, cx| this.commit(cx)))
      .on_action(cx.listener(Self::new_file_in_files_panel_action))
      .on_action(cx.listener(Self::rename_selected_file_item_action))
      .on_action(cx.listener(Self::delete_selected_file_item_action))
      .child(header)
      .child(body);
    if self.active_tab == DockPanelTab::Changes {
      panel = panel.child(self.render_commit_zone(_window, cx));
    }
    panel
  }
}

impl Focusable for DockPanel {
  fn focus_handle(&self, _cx: &App) -> FocusHandle {
    self.focus_handle.clone()
  }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FileTreeEntryVisibility {
  is_gitignored: bool,
  is_hidden: bool,
}

impl From<&ProjectEntry> for FileTreeEntryVisibility {
  fn from(entry: &ProjectEntry) -> Self {
    Self {
      is_gitignored: entry.is_gitignored,
      is_hidden: entry.is_hidden,
    }
  }
}

fn file_tree_entry_visibility_map(
  entries: &[Rc<ProjectEntry>],
) -> HashMap<String, FileTreeEntryVisibility> {
  entries
    .iter()
    .map(|entry| {
      (
        entry.path_id(),
        FileTreeEntryVisibility::from(entry.as_ref()),
      )
    })
    .collect()
}

fn collect_project_tree_entries(
  item: &TreeItem,
  visibility: &HashMap<String, FileTreeEntryVisibility>,
  entries: &mut Vec<Rc<ProjectEntry>>,
) {
  if is_empty_folder_placeholder_id(item.id.as_ref()) {
    return;
  }
  let path = PathBuf::from(item.id.as_ref());
  let entry_visibility = visibility
    .get(item.id.as_ref())
    .copied()
    .unwrap_or_default();
  let mut entry = if item.is_folder() {
    ProjectEntry {
      path,
      kind: ProjectEntryKind::Directory,
      is_gitignored: false,
      is_hidden: false,
    }
  } else {
    ProjectEntry::file(path)
  };
  entry.is_gitignored = entry_visibility.is_gitignored;
  entry.is_hidden = entry_visibility.is_hidden;
  entries.push(Rc::new(entry));
  for child in &item.children {
    collect_project_tree_entries(child, visibility, entries);
  }
}

fn add_path_and_ancestors_to_expanded(path: &Path, expanded: &mut HashSet<String>) {
  let mut current = Some(path);
  while let Some(path) = current {
    if path.as_os_str().is_empty() {
      break;
    }
    expanded.insert(file_tree_path_id(path));
    current = path.parent();
  }
}

fn file_tree_path_id(path: &Path) -> String {
  path
    .to_string_lossy()
    .replace(std::path::MAIN_SEPARATOR, "/")
}

fn render_file_tree_status_badge(
  status: RepoStatusKind,
  theme: &gpui_component::Theme,
  id: SharedString,
) -> AnyElement {
  let color = status_color(status, theme);
  let content = if status_uses_warning_icon(status) {
    Icon::new(IconName::TriangleAlert)
      .size_3()
      .text_color(color)
      .into_any_element()
  } else {
    div()
      .text_xs()
      .font_weight(gpui::FontWeight::BOLD)
      .text_color(color)
      .child(status.short_code())
      .into_any_element()
  };
  let tooltip = status_tooltip(status);
  let element_id = format!("dock-panel-file-status-{id}");

  div()
    .id(element_id.clone())
    .debug_selector(move || element_id.clone())
    .w(px(15.))
    .min_w(px(15.))
    .flex()
    .items_center()
    .justify_end()
    .tooltip(move |window, cx| {
      gpui_component::tooltip::Tooltip::new(tooltip.clone()).build(window, cx)
    })
    .child(content)
    .into_any_element()
}

fn file_tree_status_maps(
  entries: &[RepoStatusEntry],
) -> (
  HashMap<PathBuf, RepoStatusKind>,
  HashMap<PathBuf, RepoStatusKind>,
) {
  let mut file_statuses = HashMap::new();
  let mut folder_statuses = HashMap::new();
  for entry in entries {
    set_preferred_file_tree_status(&mut file_statuses, entry.path.clone(), entry.status);

    let mut parent = entry.path.parent();
    while let Some(path) = parent {
      if path.as_os_str().is_empty() {
        break;
      }
      set_preferred_file_tree_status(&mut folder_statuses, path.to_path_buf(), entry.status);
      parent = path.parent();
    }
  }
  (file_statuses, folder_statuses)
}

fn set_preferred_file_tree_status(
  statuses: &mut HashMap<PathBuf, RepoStatusKind>,
  path: PathBuf,
  status: RepoStatusKind,
) {
  statuses
    .entry(path)
    .and_modify(|existing| {
      if file_tree_status_priority(status) > file_tree_status_priority(*existing) {
        *existing = status;
      }
    })
    .or_insert(status);
}

fn file_tree_status_priority(status: RepoStatusKind) -> u8 {
  match status {
    RepoStatusKind::Modified => 1,
    RepoStatusKind::Renamed | RepoStatusKind::TypeChange => 2,
    RepoStatusKind::Added | RepoStatusKind::Untracked => 3,
    RepoStatusKind::Deleted => 4,
    RepoStatusKind::Conflicted => 5,
  }
}

fn file_tree_path_is_self_or_descendant(path: &str, ancestor: &str) -> bool {
  path == ancestor
    || path
      .strip_prefix(ancestor)
      .is_some_and(|suffix| suffix.starts_with('/'))
}

fn renamed_file_tree_path_id(path: &str, old_id: &str, new_id: &str) -> String {
  if path == old_id {
    return new_id.to_string();
  }
  if let Some(suffix) = path
    .strip_prefix(old_id)
    .and_then(|path| path.strip_prefix('/'))
  {
    return format!("{new_id}/{suffix}");
  }
  path.to_string()
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::test_support::{TempRepo, commit_text_file};
  use git::{RepoStage, RepoStatusKind};
  use git2::Repository;
  use gpui::TestAppContext;
  use std::path::Path;
  use std::sync::Arc;
  use std::sync::atomic::{AtomicBool, Ordering};

  #[test]
  fn file_tree_status_maps_keep_file_statuses_and_aggregate_folders() {
    let entries = vec![
      RepoStatusEntry {
        path: PathBuf::from("src/main.rs"),
        old_path: None,
        status: RepoStatusKind::Modified,
        stage: RepoStage::Unstaged,
      },
      RepoStatusEntry {
        path: PathBuf::from("src/nested/lib.rs"),
        old_path: None,
        status: RepoStatusKind::Conflicted,
        stage: RepoStage::Unstaged,
      },
      RepoStatusEntry {
        path: PathBuf::from("assets/icon.svg"),
        old_path: None,
        status: RepoStatusKind::Untracked,
        stage: RepoStage::Unstaged,
      },
      RepoStatusEntry {
        path: PathBuf::from("docs/gone.md"),
        old_path: None,
        status: RepoStatusKind::Deleted,
        stage: RepoStage::Unstaged,
      },
    ];

    let (file_statuses, folder_statuses) = file_tree_status_maps(&entries);

    assert_eq!(
      file_statuses[Path::new("src/main.rs")],
      RepoStatusKind::Modified
    );
    assert_eq!(
      file_statuses[Path::new("src/nested/lib.rs")],
      RepoStatusKind::Conflicted
    );
    assert_eq!(
      file_statuses[Path::new("assets/icon.svg")],
      RepoStatusKind::Untracked
    );
    assert_eq!(
      file_statuses[Path::new("docs/gone.md")],
      RepoStatusKind::Deleted
    );
    assert_eq!(
      folder_statuses[Path::new("src")],
      RepoStatusKind::Conflicted
    );
    assert_eq!(
      folder_statuses[Path::new("src/nested")],
      RepoStatusKind::Conflicted
    );
    assert_eq!(
      folder_statuses[Path::new("assets")],
      RepoStatusKind::Untracked
    );
    assert_eq!(folder_statuses[Path::new("docs")], RepoStatusKind::Deleted);
  }

  #[gpui::test]
  async fn opening_the_files_tab_hands_the_keyboard_to_its_tree(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-files-keyboard");
    commit_text_file(&repo.path, Path::new("a.txt"), "v1\n", "first");
    commit_text_file(&repo.path, Path::new("b.txt"), "v1\n", "second");

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;

    // The tab is asked for before its tree exists: the files load off the main
    // thread and the focus follows on the render that mounts them.
    panel.update_in(cx, |panel, window, cx| {
      panel.open_tab(DockPanelTab::Files, window, cx)
    });
    let files = panel.update(cx, |panel, _| panel._files_task.take());
    if let Some(files) = files {
      files.await;
    }
    cx.run_until_parked();

    let before = panel.read_with(cx, |panel, cx| {
      panel.files_tree_state.read(cx).selected_index()
    });
    cx.simulate_keystrokes("down");
    let after = panel.read_with(cx, |panel, cx| {
      panel.files_tree_state.read(cx).selected_index()
    });
    assert_ne!(before, after, "the arrow keys reach the file tree");
  }

  #[gpui::test]
  async fn opening_the_review_tab_hands_the_keyboard_to_its_rows(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-review-keyboard");
    commit_text_file(&repo.path, Path::new("a.txt"), "v1\n", "first");

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;

    let comments =
      crate::review_list::review_panel_comments(&[crate::agent_review::LocalAgentReviewComment {
        id: 1,
        in_reply_to_id: None,
        path: PathBuf::from("a.txt"),
        line: 1,
        side: editor::ReviewCommentSide::Right,
        start_line: None,
        start_side: None,
        body: std::sync::Arc::from("look here"),
        original_start_line: Some(2),
        original_lines: Vec::new(),
        state: crate::agent_review::LocalAgentReviewCommentState::Draft,
      }]);
    panel.update(cx, |panel, cx| {
      panel.review_list.update(cx, |list, cx| {
        list.set_comments(crate::review_list::ReviewSection::Agent, comments, cx)
      });
    });
    cx.run_until_parked();

    panel.update_in(cx, |panel, window, cx| {
      panel.open_tab(DockPanelTab::Review, window, cx)
    });
    cx.run_until_parked();

    cx.simulate_keystrokes("down");
    panel.read_with(cx, |panel, cx| {
      assert!(
        panel
          .review_list
          .read(cx)
          .keyboard_selected_row(cx)
          .is_some(),
        "the arrow keys reach the review rows"
      );
    });
  }

  #[gpui::test]
  async fn walking_the_file_tree_shows_files_and_enter_opens_them(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-files-intent");
    commit_text_file(&repo.path, Path::new("a.txt"), "v1\n", "first");
    commit_text_file(&repo.path, Path::new("b.txt"), "v1\n", "second");

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;

    panel.update_in(cx, |panel, window, cx| {
      panel.open_tab(DockPanelTab::Files, window, cx)
    });
    let files = panel.update(cx, |panel, _| panel._files_task.take());
    if let Some(files) = files {
      files.await;
    }
    cx.run_until_parked();

    let opened = Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = opened.clone();
    cx.update(|_, cx| {
      cx.subscribe(&panel, move |_panel, event: &DockPanelEvent, _cx| {
        if let DockPanelEvent::OpenFile { path, intent, mode } = event {
          seen.borrow_mut().push((path.clone(), *intent, *mode));
        }
      })
      .detach();
    });

    let first_file = cx.debug_bounds("dock-panel-file-a.txt").expect("file row");
    cx.simulate_event(gpui::MouseDownEvent {
      button: gpui::MouseButton::Left,
      position: first_file.center(),
      modifiers: gpui::Modifiers::default(),
      click_count: 1,
      first_mouse: false,
    });
    cx.run_until_parked();
    assert!(
      opened.borrow().is_empty(),
      "holding a file row for drag does not open it"
    );
    cx.simulate_event(gpui::MouseUpEvent {
      button: gpui::MouseButton::Left,
      position: gpui::point(first_file.right() + px(20.0), first_file.center().y),
      modifiers: gpui::Modifiers::default(),
      click_count: 1,
    });
    cx.run_until_parked();
    assert!(opened.borrow().is_empty());

    // The first down lands on the second row: the tree starts with nothing
    // selected and counts from there.
    cx.simulate_keystrokes("down");
    cx.run_until_parked();
    assert_eq!(
      opened.borrow().as_slice(),
      &[(
        PathBuf::from("b.txt"),
        OpenIntent::Browse,
        DockPanelOpenFileMode::File,
      )],
      "walking the tree shows the file, it does not choose it"
    );

    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(
      opened.borrow().last(),
      Some(&(
        PathBuf::from("b.txt"),
        OpenIntent::Open,
        DockPanelOpenFileMode::File,
      )),
      "Enter chooses it"
    );
  }

  fn open_files_context_menu(cx: &mut gpui::VisualTestContext, selector: &'static str) {
    let row = cx.debug_bounds(selector).expect("file row bounds");
    cx.simulate_event(gpui::MouseDownEvent {
      button: gpui::MouseButton::Right,
      position: row.center(),
      modifiers: gpui::Modifiers::default(),
      click_count: 1,
      first_mouse: false,
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
      let _ = window.draw(cx);
    });
  }

  fn open_changes_list_empty_context_menu(cx: &mut gpui::VisualTestContext) {
    let list = cx
      .debug_bounds("dock-panel-file-list")
      .expect("changes list bounds");
    let row = cx
      .debug_bounds("changes-row-0-0")
      .expect("changes row bounds");
    let position = gpui::point(list.center().x, row.bottom() + px(8.0));
    assert!(position.y < list.bottom());
    cx.simulate_event(gpui::MouseDownEvent {
      button: gpui::MouseButton::Right,
      position,
      modifiers: gpui::Modifiers::default(),
      click_count: 1,
      first_mouse: false,
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
      let _ = window.draw(cx);
    });
  }

  async fn await_files_loaded(panel: &Entity<DockPanel>, cx: &mut gpui::VisualTestContext) {
    let files = panel.update(cx, |panel, _| panel._files_task.take());
    if let Some(files) = files {
      files.await;
    }
    cx.run_until_parked();
  }

  async fn await_file_operation_task(panel: &Entity<DockPanel>, cx: &mut gpui::VisualTestContext) {
    let operation = panel.update(cx, |panel, _| panel._file_operation_task.take());
    if let Some(operation) = operation {
      operation.await;
    }
    cx.run_until_parked();
  }

  async fn await_file_operation(panel: &Entity<DockPanel>, cx: &mut gpui::VisualTestContext) {
    await_file_operation_task(panel, cx).await;
    await_refresh(panel, cx).await;
    await_files_loaded(panel, cx).await;
  }

  async fn open_files_tab_and_wait(panel: &Entity<DockPanel>, cx: &mut gpui::VisualTestContext) {
    panel.update_in(cx, |panel, window, cx| {
      panel.open_tab(DockPanelTab::Files, window, cx)
    });
    await_files_loaded(panel, cx).await;
  }

  #[gpui::test]
  async fn files_panel_f2_starts_inline_rename_for_selected_item(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-files-keyboard-rename");
    commit_text_file(&repo.path, Path::new("a.txt"), "a\n", "a");
    commit_text_file(&repo.path, Path::new("b.txt"), "b\n", "b");

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;
    open_files_tab_and_wait(&panel, cx).await;

    cx.simulate_keystrokes("down");
    cx.run_until_parked();
    cx.simulate_keystrokes("f2");
    cx.run_until_parked();
    cx.update(|window, cx| {
      let _ = window.draw(cx);
    });

    assert!(
      cx.debug_bounds("dock-panel-file-rename-input-b.txt")
        .is_some(),
      "F2 starts inline rename for the selected file"
    );
  }

  #[gpui::test]
  async fn files_panel_delete_asks_before_deleting_selected_item(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-files-keyboard-delete");
    commit_text_file(&repo.path, Path::new("a.txt"), "a\n", "a");
    commit_text_file(&repo.path, Path::new("b.txt"), "b\n", "b");

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;
    open_files_tab_and_wait(&panel, cx).await;

    cx.simulate_keystrokes("down delete");
    cx.run_until_parked();
    cx.update(|window, cx| {
      let _ = window.draw(cx);
    });
    assert!(cx.update(|window, cx| window.has_active_dialog(cx)));
    assert!(repo.path.join("b.txt").is_file());
  }

  #[gpui::test]
  async fn files_panel_delete_edits_inline_input_instead_of_deleting_item(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-files-keyboard-delete-inline");
    commit_text_file(&repo.path, Path::new("a.txt"), "a\n", "a");
    commit_text_file(&repo.path, Path::new("b.txt"), "b\n", "b");

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;
    open_files_tab_and_wait(&panel, cx).await;

    cx.simulate_keystrokes("down f2 delete");
    cx.run_until_parked();

    assert!(!cx.update(|window, cx| window.has_active_dialog(cx)));
    assert!(repo.path.join("b.txt").is_file());
    panel.read_with(cx, |panel, _| {
      assert!(panel.files_inline_rename.is_some());
    });
  }

  #[gpui::test]
  async fn file_context_menu_copies_relative_path(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-files-context-copy-relative");
    commit_text_file(&repo.path, Path::new("README.md"), "v1\n", "first");

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;
    open_files_tab_and_wait(&panel, cx).await;

    open_files_context_menu(cx, "dock-panel-file-README.md");
    cx.simulate_keystrokes("down down down down down enter");
    cx.run_until_parked();

    let copied = cx
      .update(|_, cx| cx.read_from_clipboard())
      .and_then(|item| item.text());
    assert_eq!(copied.as_deref(), Some("README.md"));
  }

  #[gpui::test]
  async fn file_context_menu_escape_closes_the_only_open_menu(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-files-context-single-menu");
    commit_text_file(&repo.path, Path::new("README.md"), "v1\n", "first");

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;
    open_files_tab_and_wait(&panel, cx).await;

    cx.update(|_, cx| {
      cx.write_to_clipboard(gpui::ClipboardItem::new_string("unchanged".to_string()))
    });
    open_files_context_menu(cx, "dock-panel-file-README.md");
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    cx.simulate_keystrokes("down down down down down enter");
    cx.run_until_parked();

    let copied = cx
      .update(|_, cx| cx.read_from_clipboard())
      .and_then(|item| item.text());
    assert_eq!(copied.as_deref(), Some("unchanged"));
  }

  #[gpui::test]
  async fn file_context_rename_edits_inline_without_moving_the_row(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-files-inline-rename");
    commit_text_file(&repo.path, Path::new("README.md"), "v1\n", "first");

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;
    open_files_tab_and_wait(&panel, cx).await;

    let row_before = cx
      .debug_bounds("dock-panel-file-README.md")
      .expect("file row before rename");
    panel.update_in(cx, |panel, window, cx| {
      panel.start_inline_rename_from_context(
        FilesContextTarget::entry(PathBuf::from("README.md"), false),
        window,
        cx,
      );
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
      let _ = window.draw(cx);
    });
    cx.run_until_parked();

    assert!(
      cx.debug_bounds("dock-panel-file-rename-input-README.md")
        .is_some(),
      "the file row becomes an inline input"
    );
    let row_during = cx
      .debug_bounds("dock-panel-file-README.md")
      .expect("file row during rename");
    assert_eq!(row_during.origin.y, row_before.origin.y);
    assert_eq!(row_during.size.height, row_before.size.height);

    panel.update_in(cx, |panel, window, cx| {
      let input = panel
        .files_inline_rename
        .as_ref()
        .expect("inline rename")
        .input
        .clone();
      input.update(cx, |input, cx| input.set_value("NOTES.md", window, cx));
    });
    cx.simulate_keystrokes("enter");
    await_file_operation(&panel, cx).await;

    assert!(repo.path.join("NOTES.md").is_file());
    assert!(!repo.path.join("README.md").exists());
    assert!(panel.read_with(cx, |panel, _| panel.files_inline_rename.is_none()));
  }

  #[gpui::test]
  async fn folder_context_rename_edits_inline(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-files-inline-folder-rename");
    commit_text_file(&repo.path, Path::new("docs/a.txt"), "v1\n", "first");

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;
    open_files_tab_and_wait(&panel, cx).await;

    open_files_context_menu(cx, "dock-panel-file-docs");
    cx.simulate_keystrokes("down down down down down down enter");
    cx.run_until_parked();
    cx.update(|window, cx| {
      let _ = window.draw(cx);
    });
    cx.run_until_parked();

    assert!(
      cx.debug_bounds("dock-panel-file-rename-input-docs")
        .is_some(),
      "the folder row becomes an inline input"
    );
    panel.update_in(cx, |panel, window, cx| {
      let input = panel
        .files_inline_rename
        .as_ref()
        .expect("inline rename")
        .input
        .clone();
      input.update(cx, |input, cx| input.set_value("src", window, cx));
    });
    cx.simulate_keystrokes("enter");
    await_file_operation(&panel, cx).await;

    assert!(repo.path.join("src/a.txt").is_file());
    assert!(!repo.path.join("docs").exists());
  }

  #[gpui::test]
  async fn inline_rename_preserves_current_folder_expansion(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-files-inline-rename-preserve-expansion");
    commit_text_file(&repo.path, Path::new("data.json"), "{}\n", "data");
    commit_text_file(&repo.path, Path::new("src/changed.ts"), "v1\n", "src");
    std::fs::write(repo.path.join("src/changed.ts"), "v2\n").expect("dirty src");

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;
    open_files_tab_and_wait(&panel, cx).await;

    let src_expanded = panel.read_with(cx, |panel, cx| {
      let tree = panel.files_tree_state.read(cx);
      tree
        .index_of(&"src".into())
        .and_then(|index| tree.entry(index))
        .is_some_and(|entry| entry.is_expanded())
    });
    if src_expanded {
      let src = cx.debug_bounds("dock-panel-file-src").expect("src row");
      cx.simulate_click(src.center(), gpui::Modifiers::default());
      cx.run_until_parked();
    }
    panel.read_with(cx, |panel, cx| {
      let tree = panel.files_tree_state.read(cx);
      let src = tree
        .index_of(&"src".into())
        .and_then(|index| tree.entry(index))
        .expect("src stays visible before rename");
      assert!(!src.is_expanded());
    });

    panel.update_in(cx, |panel, window, cx| {
      panel.start_inline_rename_from_context(
        FilesContextTarget::entry(PathBuf::from("data.json"), false),
        window,
        cx,
      );
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
      let _ = window.draw(cx);
    });
    cx.run_until_parked();
    panel.update_in(cx, |panel, window, cx| {
      let input = panel
        .files_inline_rename
        .as_ref()
        .expect("inline rename")
        .input
        .clone();
      input.update(cx, |input, cx| input.set_value("renamed.json", window, cx));
    });
    cx.simulate_keystrokes("enter");
    await_file_operation(&panel, cx).await;

    assert!(repo.path.join("renamed.json").is_file());
    panel.read_with(cx, |panel, cx| {
      let tree = panel.files_tree_state.read(cx);
      let src = tree
        .index_of(&"src".into())
        .and_then(|index| tree.entry(index))
        .expect("src stays visible");
      assert!(!src.is_expanded());
    });
  }

  #[gpui::test]
  async fn inline_rename_round_trip_refreshes_file_row(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-files-inline-rename-round-trip");
    commit_text_file(&repo.path, Path::new("env.d.ts"), "v1\n", "first");

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;
    open_files_tab_and_wait(&panel, cx).await;

    panel.update_in(cx, |panel, window, cx| {
      panel.start_inline_rename(PathBuf::from("env.d.ts"), window, cx);
    });
    panel.update_in(cx, |panel, window, cx| {
      let input = panel
        .files_inline_rename
        .as_ref()
        .expect("inline rename")
        .input
        .clone();
      input.update(cx, |input, cx| input.set_value("env.d.t", window, cx));
    });
    cx.simulate_keystrokes("enter");
    await_file_operation(&panel, cx).await;

    assert!(repo.path.join("env.d.t").is_file());
    panel.read_with(cx, |panel, cx| {
      let tree = panel.files_tree_state.read(cx);
      assert!(tree.index_of(&"env.d.t".into()).is_some());
      assert!(tree.index_of(&"env.d.ts".into()).is_none());
    });

    panel.update_in(cx, |panel, window, cx| {
      panel.start_inline_rename(PathBuf::from("env.d.t"), window, cx);
    });
    panel.update_in(cx, |panel, window, cx| {
      let input = panel
        .files_inline_rename
        .as_ref()
        .expect("inline rename")
        .input
        .clone();
      input.update(cx, |input, cx| input.set_value("env.d.ts", window, cx));
    });
    cx.simulate_keystrokes("enter");
    await_file_operation_task(&panel, cx).await;

    assert!(repo.path.join("env.d.ts").is_file());
    assert!(!repo.path.join("env.d.t").exists());
    cx.update(|window, cx| {
      let _ = window.draw(cx);
    });
    assert!(cx.debug_bounds("dock-panel-file-env.d.ts").is_some());
    assert!(cx.debug_bounds("dock-panel-file-env.d.t").is_none());
    await_refresh(&panel, cx).await;
    await_files_loaded(&panel, cx).await;
    panel.read_with(cx, |panel, cx| {
      let tree = panel.files_tree_state.read(cx);
      assert!(tree.index_of(&"env.d.ts".into()).is_some());
      assert!(tree.index_of(&"env.d.t".into()).is_none());
    });
  }

  #[gpui::test]
  async fn inline_rename_stale_row_heals_when_target_already_exists(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-files-inline-rename-stale-row");
    commit_text_file(&repo.path, Path::new("env.d.ts"), "v1\n", "first");

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;
    open_files_tab_and_wait(&panel, cx).await;

    panel.update_in(cx, |panel, window, cx| {
      panel.rename_file_entry(PathBuf::from("env.d.ts"), "env.d.t".to_string(), window, cx);
    });
    await_file_operation(&panel, cx).await;
    std::fs::rename(repo.path.join("env.d.t"), repo.path.join("env.d.ts"))
      .expect("external rename back");

    panel.update_in(cx, |panel, window, cx| {
      assert!(panel.rename_file_entry(
        PathBuf::from("env.d.t"),
        "env.d.ts".to_string(),
        window,
        cx,
      ));
    });
    await_file_operation(&panel, cx).await;

    panel.read_with(cx, |panel, cx| {
      let tree = panel.files_tree_state.read(cx);
      assert!(tree.index_of(&"env.d.ts".into()).is_some());
      assert!(tree.index_of(&"env.d.t".into()).is_none());
    });
  }

  #[gpui::test]
  async fn file_context_inline_rename_escape_cancels(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-files-inline-rename-cancel");
    commit_text_file(&repo.path, Path::new("README.md"), "v1\n", "first");

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;
    open_files_tab_and_wait(&panel, cx).await;

    panel.update_in(cx, |panel, window, cx| {
      panel.start_inline_rename_from_context(
        FilesContextTarget::entry(PathBuf::from("README.md"), false),
        window,
        cx,
      );
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
      let _ = window.draw(cx);
    });
    cx.run_until_parked();
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();

    assert!(repo.path.join("README.md").is_file());
    assert!(panel.read_with(cx, |panel, _| panel.files_inline_rename.is_none()));
  }

  #[gpui::test]
  async fn file_context_inline_rename_clicking_another_file_cancels(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-files-inline-rename-click-cancel");
    commit_text_file(&repo.path, Path::new("README.md"), "v1\n", "first");
    commit_text_file(&repo.path, Path::new("other.txt"), "v1\n", "second");

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;
    open_files_tab_and_wait(&panel, cx).await;

    panel.update_in(cx, |panel, window, cx| {
      panel.start_inline_rename_from_context(
        FilesContextTarget::entry(PathBuf::from("README.md"), false),
        window,
        cx,
      );
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
      let _ = window.draw(cx);
    });
    panel.update_in(cx, |panel, window, cx| {
      let input = panel
        .files_inline_rename
        .as_ref()
        .expect("inline rename")
        .input
        .clone();
      input.update(cx, |input, cx| input.set_value("NOTES.md", window, cx));
    });

    let other = cx
      .debug_bounds("dock-panel-file-other.txt")
      .expect("other file row");
    cx.simulate_click(other.center(), gpui::Modifiers::default());
    cx.run_until_parked();

    assert!(repo.path.join("README.md").is_file());
    assert!(!repo.path.join("NOTES.md").exists());
    assert!(panel.read_with(cx, |panel, _| panel.files_inline_rename.is_none()));
  }

  #[gpui::test]
  async fn empty_files_panel_context_menu_copies_project_relative_path(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-files-context-empty-copy-relative");
    commit_text_file(&repo.path, Path::new("README.md"), "v1\n", "first");

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;
    open_files_tab_and_wait(&panel, cx).await;

    open_files_context_menu(cx, "dock-panel-files-empty-space");
    cx.simulate_keystrokes("down down down down down enter");
    cx.run_until_parked();

    let copied = cx
      .update(|_, cx| cx.read_from_clipboard())
      .and_then(|item| item.text());
    assert_eq!(copied.as_deref(), Some("."));
  }

  #[gpui::test]
  async fn file_context_new_file_uses_inline_input(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-files-inline-new-file");
    commit_text_file(&repo.path, Path::new("src/existing.rs"), "v1\n", "first");

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;
    open_files_tab_and_wait(&panel, cx).await;
    let opened = Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = opened.clone();
    cx.update(|_, cx| {
      cx.subscribe(&panel, move |_panel, event: &DockPanelEvent, _cx| {
        if let DockPanelEvent::OpenFile { path, intent, mode } = event {
          seen.borrow_mut().push((path.clone(), *intent, *mode));
        }
      })
      .detach();
    });

    panel.update_in(cx, |panel, window, cx| {
      panel.start_inline_create(
        FilesContextTarget::entry(PathBuf::from("src"), true),
        false,
        window,
        cx,
      );
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
      let _ = window.draw(cx);
    });
    cx.run_until_parked();

    assert!(
      cx.debug_bounds("dock-panel-file-rename-input-src/.reviu-new-file")
        .is_some(),
      "the new file row is an inline input"
    );
    panel.update_in(cx, |panel, window, cx| {
      let input = panel
        .files_inline_rename
        .as_ref()
        .expect("inline create")
        .input
        .clone();
      input.update(cx, |input, cx| input.set_value("main.rs", window, cx));
    });
    cx.simulate_keystrokes("enter");
    await_file_operation(&panel, cx).await;

    assert!(repo.path.join("src/main.rs").is_file());
    assert_eq!(
      opened.borrow().as_slice(),
      &[(
        PathBuf::from("src/main.rs"),
        OpenIntent::Open,
        DockPanelOpenFileMode::File,
      )],
      "created files open in the editor"
    );
    panel.read_with(cx, |panel, cx| {
      let tree = panel.files_tree_state.read(cx);
      let selected = tree.selected_entry().expect("created file is selected");
      assert_eq!(selected.item().id.as_ref(), "src/main.rs");
      assert!(tree.index_of(&"src/main.rs".into()).is_some());
      assert!(tree.index_of(&"src/.reviu-new-file".into()).is_none());
    });
  }

  #[gpui::test]
  async fn file_context_new_folder_uses_inline_input_and_keeps_empty_folder_visible(
    cx: &mut TestAppContext,
  ) {
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-files-inline-new-folder");
    commit_text_file(&repo.path, Path::new("README.md"), "v1\n", "first");

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;
    open_files_tab_and_wait(&panel, cx).await;

    panel.update_in(cx, |panel, window, cx| {
      panel.start_inline_create(FilesContextTarget::root(), true, window, cx);
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
      let _ = window.draw(cx);
    });
    cx.run_until_parked();

    assert!(
      cx.debug_bounds("dock-panel-file-rename-input-.reviu-new-folder")
        .is_some(),
      "the new folder row is an inline input"
    );
    panel.update_in(cx, |panel, window, cx| {
      let input = panel
        .files_inline_rename
        .as_ref()
        .expect("inline create")
        .input
        .clone();
      input.update(cx, |input, cx| input.set_value("assets", window, cx));
    });
    cx.simulate_keystrokes("enter");
    await_file_operation(&panel, cx).await;

    assert!(repo.path.join("assets").is_dir());
    panel.read_with(cx, |panel, cx| {
      let tree = panel.files_tree_state.read(cx);
      let assets = tree
        .index_of(&"assets".into())
        .and_then(|index| tree.entry(index))
        .expect("empty folder stays visible");
      let selected = tree.selected_entry().expect("created folder is selected");
      assert!(assets.is_folder());
      assert_eq!(selected.item().id.as_ref(), "assets");
      assert!(tree.index_of(&".reviu-new-folder".into()).is_none());
    });
  }

  #[gpui::test]
  async fn file_context_delete_selects_the_next_visible_entry(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-files-delete-select-next");
    commit_text_file(&repo.path, Path::new("a.txt"), "a\n", "a");
    commit_text_file(&repo.path, Path::new("b.txt"), "b\n", "b");
    commit_text_file(&repo.path, Path::new("c.txt"), "c\n", "c");

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;
    open_files_tab_and_wait(&panel, cx).await;

    panel.update(cx, |panel, cx| {
      panel.delete_file_entry(repo.path.join("b.txt"), cx);
    });
    await_file_operation(&panel, cx).await;

    assert!(!repo.path.join("b.txt").exists());
    panel.read_with(cx, |panel, cx| {
      let tree = panel.files_tree_state.read(cx);
      let selected = tree.selected_entry().expect("next file is selected");
      assert_eq!(selected.item().id.as_ref(), "c.txt");
    });
  }

  #[gpui::test]
  async fn file_context_actions_create_duplicate_rename_and_delete(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-files-context-actions");
    commit_text_file(&repo.path, Path::new("README.md"), "v1\n", "first");
    std::fs::create_dir_all(repo.path.join("src")).expect("create src");

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;
    open_files_tab_and_wait(&panel, cx).await;

    panel.update_in(cx, |panel, window, cx| {
      assert!(panel.create_file_entry(
        Some(PathBuf::from("src")),
        "main.rs".to_string(),
        false,
        window,
        cx,
      ));
    });
    await_file_operation(&panel, cx).await;
    assert!(repo.path.join("src/main.rs").is_file());

    panel.update_in(cx, |panel, window, cx| {
      assert!(panel.create_file_entry(None, "assets".to_string(), true, window, cx));
    });
    await_file_operation(&panel, cx).await;
    assert!(repo.path.join("assets").is_dir());

    panel.update(cx, |panel, cx| {
      panel.duplicate_file_from_context(
        FilesContextTarget::entry(PathBuf::from("README.md"), false),
        cx,
      );
    });
    await_file_operation(&panel, cx).await;
    assert!(repo.path.join("README copy.md").is_file());

    panel.update_in(cx, |panel, window, cx| {
      assert!(panel.rename_file_entry(
        PathBuf::from("README copy.md"),
        "COPY.md".to_string(),
        window,
        cx,
      ));
    });
    await_file_operation(&panel, cx).await;
    assert!(repo.path.join("COPY.md").is_file());
    assert!(!repo.path.join("README copy.md").exists());

    panel.update_in(cx, |panel, window, cx| {
      assert!(panel.rename_file_entry(PathBuf::from("assets"), "static".to_string(), window, cx,));
    });
    await_file_operation(&panel, cx).await;
    assert!(repo.path.join("static").is_dir());
    assert!(!repo.path.join("assets").exists());

    panel.update(cx, |panel, cx| {
      panel.delete_file_entry(repo.path.join("static"), cx);
    });
    await_file_operation(&panel, cx).await;
    assert!(!repo.path.join("static").exists());
  }

  #[gpui::test]
  async fn non_git_project_files_use_the_project_root(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let project = crate::test_support::temp_path("dock-files-non-git");
    std::fs::create_dir_all(&project).expect("create project");
    std::fs::write(project.join("main.rs"), "fn main() {}\n").expect("write file");

    let (panel, cx) = add_dock_panel_window(None, cx);
    panel.update_in(cx, |panel, window, cx| {
      panel.set_project_and_repo_roots(Some(project.clone()), None, window, cx)
    });
    await_refresh(&panel, cx).await;

    panel.read_with(cx, |panel, _| {
      assert_eq!(panel.project_root(), Some(project.as_path()));
      assert_eq!(panel.repo_root(), None);
      assert!(panel.status_entries().is_empty());
    });

    panel.update_in(cx, |panel, window, cx| {
      panel.open_tab(DockPanelTab::Files, window, cx)
    });
    let files = panel.update(cx, |panel, _| panel._files_task.take());
    if let Some(files) = files {
      files.await;
    }
    cx.run_until_parked();

    let opened = Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = opened.clone();
    cx.update(|_, cx| {
      cx.subscribe(&panel, move |_panel, event: &DockPanelEvent, _cx| {
        if let DockPanelEvent::OpenFile { path, intent, mode } = event {
          seen.borrow_mut().push((path.clone(), *intent, *mode));
        }
      })
      .detach();
    });
    cx.simulate_keystrokes("down");
    cx.run_until_parked();
    assert_eq!(
      opened.borrow().as_slice(),
      &[(
        PathBuf::from("main.rs"),
        OpenIntent::Browse,
        DockPanelOpenFileMode::File,
      )]
    );

    let _ = std::fs::remove_dir_all(&project);
  }

  #[gpui::test]
  async fn files_tab_starts_with_changed_folders_collapsed(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-files-changed-collapsed");
    commit_text_file(&repo.path, Path::new("src/changed.ts"), "v1\n", "src");
    std::fs::write(repo.path.join("src/changed.ts"), "v2\n").expect("dirty src");

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;
    open_files_tab_and_wait(&panel, cx).await;

    panel.read_with(cx, |panel, cx| {
      let tree = panel.files_tree_state.read(cx);
      let src = tree
        .index_of(&"src".into())
        .and_then(|index| tree.entry(index))
        .expect("src folder is visible");
      assert!(src.is_folder());
      assert!(!src.is_expanded());
      assert!(tree.index_of(&"src/changed.ts".into()).is_none());
    });
  }

  #[gpui::test]
  async fn files_tab_shows_gitignored_entries(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-files-gitignored");
    commit_text_file(
      &repo.path,
      Path::new(".gitignore"),
      "ignored/\n",
      "ignore rules",
    );
    std::fs::create_dir_all(repo.path.join("ignored")).expect("create ignored dir");
    std::fs::write(repo.path.join("ignored/secret.txt"), "ignored\n").expect("write ignored file");

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;
    open_files_tab_and_wait(&panel, cx).await;

    panel.read_with(cx, |panel, cx| {
      let tree = panel.files_tree_state.read(cx);
      let ignored = tree
        .index_of(&"ignored".into())
        .and_then(|index| tree.entry(index))
        .expect("ignored folder is visible");
      assert!(ignored.is_folder());
      assert_eq!(ignored.item().children[0].id.as_ref(), "ignored/secret.txt");
      assert!(
        panel
          .files_entry_visibility
          .get("ignored")
          .is_some_and(|visibility| visibility.is_gitignored)
      );
      assert!(
        panel
          .files_entry_visibility
          .get("ignored/secret.txt")
          .is_some_and(|visibility| visibility.is_gitignored)
      );
    });
  }

  #[gpui::test]
  async fn files_panel_renders_status_badges_for_changed_files_and_folders(
    cx: &mut TestAppContext,
  ) {
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-files-status-badges");
    commit_text_file(
      &repo.path,
      Path::new("modified.txt"),
      "before\n",
      "modified",
    );
    commit_text_file(&repo.path, Path::new("src/main.rs"), "before\n", "nested");
    std::fs::write(repo.path.join("modified.txt"), "after\n").expect("modify file");
    std::fs::write(repo.path.join("untracked.txt"), "new\n").expect("write untracked");
    std::fs::write(repo.path.join("src/main.rs"), "after\n").expect("modify nested file");

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;
    open_files_tab_and_wait(&panel, cx).await;
    cx.run_until_parked();

    assert!(
      cx.debug_bounds("dock-panel-file-status-modified.txt")
        .is_some()
    );
    assert!(
      cx.debug_bounds("dock-panel-file-status-untracked.txt")
        .is_some()
    );
    assert!(cx.debug_bounds("dock-panel-file-status-src").is_some());
  }

  #[gpui::test]
  async fn files_panel_reveals_a_path_without_taking_focus(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-files-reveal-path");
    commit_text_file(
      &repo.path,
      Path::new("src/nested/main.rs"),
      "main\n",
      "main",
    );
    commit_text_file(&repo.path, Path::new("README.md"), "readme\n", "readme");

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;
    open_files_tab_and_wait(&panel, cx).await;

    panel.read_with(cx, |panel, cx| {
      let tree = panel.files_tree_state.read(cx);
      let src = tree
        .index_of(&"src".into())
        .and_then(|index| tree.entry(index))
        .expect("src folder is visible");
      assert!(!src.is_expanded());
    });

    panel.update(cx, |panel, cx| {
      panel.reveal_file_path(Path::new("src/nested/main.rs"), cx);
    });

    panel.read_with(cx, |panel, cx| {
      let tree = panel.files_tree_state.read(cx);
      let src = tree
        .index_of(&"src".into())
        .and_then(|index| tree.entry(index))
        .expect("src folder is visible");
      let nested = tree
        .index_of(&"src/nested".into())
        .and_then(|index| tree.entry(index))
        .expect("nested folder is visible");
      let selected = tree.selected_entry().expect("selected entry");
      assert!(src.is_expanded());
      assert!(nested.is_expanded());
      assert_eq!(selected.item().id.as_ref(), "src/nested/main.rs");
    });
  }

  #[gpui::test]
  async fn files_panel_reveals_a_path_after_loading(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-files-pending-reveal-path");
    commit_text_file(
      &repo.path,
      Path::new("src/nested/main.rs"),
      "main\n",
      "main",
    );

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;
    panel.update_in(cx, |panel, window, cx| {
      panel.open_tab(DockPanelTab::Files, window, cx);
      panel.reveal_file_path(Path::new("src/nested/main.rs"), cx);
      panel.load_project_files(cx);
    });
    await_files_loaded(&panel, cx).await;

    panel.read_with(cx, |panel, cx| {
      let tree = panel.files_tree_state.read(cx);
      let selected = tree.selected_entry().expect("selected entry");
      assert_eq!(selected.item().id.as_ref(), "src/nested/main.rs");
    });
  }

  #[gpui::test]
  async fn files_actions_can_hide_gitignored_entries(cx: &mut TestAppContext) {
    crate::config::ConfigStore::set_test_db_path(Some(
      crate::test_support::temp_path("dock-files-toggle-gitignored-settings")
        .with_extension("sqlite"),
    ));
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-files-toggle-gitignored");
    commit_text_file(
      &repo.path,
      Path::new(".gitignore"),
      "ignored/\n",
      "ignore rules",
    );
    std::fs::write(repo.path.join("visible.txt"), "visible\n").expect("write visible");
    std::fs::create_dir_all(repo.path.join("ignored")).expect("create ignored dir");
    std::fs::write(repo.path.join("ignored/secret.txt"), "ignored\n").expect("write ignored");

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;
    open_files_tab_and_wait(&panel, cx).await;

    panel.read_with(cx, |panel, cx| {
      let tree = panel.files_tree_state.read(cx);
      assert_eq!(panel.files_file_count, 3);
      assert!(tree.index_of(&"ignored".into()).is_some());
      assert!(tree.index_of(&"visible.txt".into()).is_some());
    });

    panel.update(cx, |panel, cx| {
      panel.set_files_include_gitignored(false, cx);
    });
    await_files_loaded(&panel, cx).await;

    panel.read_with(cx, |panel, cx| {
      let tree = panel.files_tree_state.read(cx);
      assert_eq!(panel.files_file_count, 2);
      assert!(tree.index_of(&"ignored".into()).is_none());
      assert!(tree.index_of(&"ignored/secret.txt".into()).is_none());
      assert!(tree.index_of(&"visible.txt".into()).is_some());
    });
    assert!(!cx.update(|_, cx| AppSettings::get(cx).files_show_gitignored));
    crate::config::ConfigStore::set_test_db_path(None);
  }

  #[gpui::test]
  async fn files_actions_can_hide_hidden_entries(cx: &mut TestAppContext) {
    crate::config::ConfigStore::set_test_db_path(Some(
      crate::test_support::temp_path("dock-files-toggle-hidden-settings").with_extension("sqlite"),
    ));
    cx.update(gpui_component::init);
    let project = crate::test_support::temp_path("dock-files-toggle-hidden");
    std::fs::create_dir_all(project.join(".hidden")).expect("create hidden dir");
    std::fs::write(project.join(".hidden/secret.txt"), "hidden\n").expect("write hidden");
    std::fs::write(project.join("visible.txt"), "visible\n").expect("write visible");

    let (panel, cx) = add_dock_panel_window(None, cx);
    panel.update_in(cx, |panel, window, cx| {
      panel.set_project_and_repo_roots(Some(project.clone()), None, window, cx)
    });
    await_refresh(&panel, cx).await;
    open_files_tab_and_wait(&panel, cx).await;

    panel.read_with(cx, |panel, cx| {
      let tree = panel.files_tree_state.read(cx);
      assert!(tree.index_of(&".hidden".into()).is_some());
      assert!(
        panel
          .files_entry_visibility
          .get(".hidden")
          .is_some_and(|visibility| visibility.is_hidden)
      );
      assert!(
        panel
          .files_entry_visibility
          .get(".hidden/secret.txt")
          .is_some_and(|visibility| visibility.is_hidden)
      );
    });

    panel.update(cx, |panel, cx| {
      panel.set_files_include_hidden(false, cx);
    });
    await_files_loaded(&panel, cx).await;

    panel.read_with(cx, |panel, cx| {
      let tree = panel.files_tree_state.read(cx);
      assert_eq!(panel.files_file_count, 1);
      assert!(tree.index_of(&".hidden".into()).is_none());
      assert!(tree.index_of(&".hidden/secret.txt".into()).is_none());
      assert!(tree.index_of(&"visible.txt".into()).is_some());
    });
    assert!(!cx.update(|_, cx| AppSettings::get(cx).files_show_hidden));

    let _ = std::fs::remove_dir_all(&project);
    crate::config::ConfigStore::set_test_db_path(None);
  }

  #[gpui::test]
  async fn walking_onto_a_folder_opens_no_editor(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-files-folder");
    commit_text_file(&repo.path, Path::new("one/a.txt"), "v1\n", "first");
    commit_text_file(&repo.path, Path::new("two/b.txt"), "v1\n", "second");

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;

    panel.update_in(cx, |panel, window, cx| {
      panel.open_tab(DockPanelTab::Files, window, cx)
    });
    let files = panel.update(cx, |panel, _| panel._files_task.take());
    if let Some(files) = files {
      files.await;
    }
    cx.run_until_parked();

    let opened = Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = opened.clone();
    cx.update(|_, cx| {
      cx.subscribe(&panel, move |_panel, event: &DockPanelEvent, _cx| {
        if let DockPanelEvent::OpenFile { path, intent, mode } = event {
          seen.borrow_mut().push((path.clone(), *intent, *mode));
        }
      })
      .detach();
    });

    // Both rows are folders, and the first down lands on the second of them.
    cx.simulate_keystrokes("down");
    cx.run_until_parked();
    assert!(
      opened.borrow().is_empty(),
      "a folder has no contents to show, got {:?}",
      opened.borrow()
    );

    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(
      opened.borrow().is_empty(),
      "Enter on a folder unfolds it, it does not open an editor"
    );

    cx.simulate_keystrokes("down");
    cx.run_until_parked();
    assert_eq!(
      opened.borrow().as_slice(),
      &[(
        PathBuf::from("two/b.txt"),
        OpenIntent::Browse,
        DockPanelOpenFileMode::File,
      )],
      "the file inside it still shows"
    );
  }

  async fn await_refresh(panel: &Entity<DockPanel>, cx: &mut gpui::VisualTestContext) {
    let task = panel.update(cx, |panel, _| panel._refresh_task.take());
    if let Some(task) = task {
      task.await;
    }
    cx.run_until_parked();
  }

  fn add_dock_panel_window(
    repo_root: Option<PathBuf>,
    cx: &mut TestAppContext,
  ) -> (Entity<DockPanel>, &mut gpui::VisualTestContext) {
    cx.update(|cx| {
      // Painting a panel needs the theme, and some of these tests paint.
      gpui_component::init(cx);
      if !cx.has_global::<crate::config::AppSettings>() {
        cx.set_global(crate::config::AppSettings::default());
      }
      crate::install_app_key_bindings(cx);
    });
    let mut mounted: Option<Entity<DockPanel>> = None;
    let (_root, cx) = cx.add_window_view(|window, cx| {
      let panel = cx.new(|cx| DockPanel::new(repo_root.clone(), window, cx));
      mounted = Some(panel.clone());
      gpui_component::Root::new(panel, window, cx)
    });
    (mounted.expect("dock panel"), cx)
  }

  #[gpui::test]
  async fn commit_message_drafts_are_scoped_to_the_checkout(cx: &mut TestAppContext) {
    let (panel, cx) = add_dock_panel_window(Some(PathBuf::from("/repo")), cx);

    panel.update_in(cx, |panel, window, cx| {
      panel.set_commit_message("repo draft", window, cx)
    });
    panel.update(cx, |panel, cx| {
      panel.set_repo_root(Some(PathBuf::from("/other")), cx)
    });
    panel.read_with(cx, |panel, cx| {
      assert_eq!(panel.commit_message(cx), "");
    });

    panel.update_in(cx, |panel, window, cx| {
      panel.set_commit_message("other draft", window, cx)
    });
    panel.update(cx, |panel, cx| {
      panel.set_repo_root(Some(PathBuf::from("/repo")), cx)
    });
    panel.read_with(cx, |panel, cx| {
      assert_eq!(panel.commit_message(cx), "repo draft");
    });

    panel.update(cx, |panel, cx| {
      panel.set_repo_root(Some(PathBuf::from("/other")), cx)
    });
    panel.read_with(cx, |panel, cx| {
      assert_eq!(panel.commit_message(cx), "other draft");
    });
  }

  #[gpui::test]
  async fn refresh_lists_working_tree_changes(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-panel-refresh");
    commit_text_file(&repo.path, Path::new("README.md"), "v1\n", "initial");
    std::fs::write(repo.path.join("README.md"), "v2\n").expect("update file");

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;

    panel.read_with(cx, |panel, _| {
      assert_eq!(panel.status_entries.len(), 1);
      assert_eq!(panel.status_entries[0].path, PathBuf::from("README.md"));
      assert_eq!(panel.status_entries[0].status, RepoStatusKind::Modified);
    });
  }

  #[gpui::test]
  async fn a_merge_in_progress_still_ends_with_a_commit(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-panel-merge");
    commit_text_file(&repo.path, Path::new("a.txt"), "base\n", "initial");
    let base = git::BranchRef {
      name: git::current_branch_status(&repo.path)
        .expect("branch status")
        .name,
      kind: git::BranchKind::Local,
    };
    let feature = git::BranchRef {
      name: "feature".to_string(),
      kind: git::BranchKind::Local,
    };
    git::create_branch(&repo.path, &feature.name).expect("create branch");
    git::switch_branch(&repo.path, &feature).expect("switch to feature");
    commit_text_file(&repo.path, Path::new("a.txt"), "feature\n", "feature work");
    git::switch_branch(&repo.path, &base).expect("switch back");
    commit_text_file(&repo.path, Path::new("a.txt"), "main\n", "main work");
    let _ = git::merge_branch(&repo.path, &feature);
    std::fs::write(repo.path.join("a.txt"), "resolved\n").expect("resolve conflict");

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;
    panel.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();

    panel.read_with(cx, |panel, _| {
      assert!(panel.merge_in_progress());
      assert!(!panel.rebase_in_progress(), "a merge is not a rebase");
    });
    assert!(cx.debug_bounds("dock-panel-operation").is_none());

    // Unlike a rebase, the button still commits: that is how a merge ends.
    panel.update_in(cx, |panel, window, cx| {
      panel.set_commit_message("Merge branch 'feature'", window, cx)
    });
    let button = cx
      .debug_bounds(DOCK_PANEL_COMMIT_DEBUG_SELECTOR)
      .expect("commit button bounds");
    cx.simulate_click(button.center(), gpui::Modifiers::default());
    let commit = panel.update(cx, |panel, _| panel._commit_task.take());
    if let Some(task) = commit {
      task.await;
    }
    cx.run_until_parked();

    assert!(!git::is_merge_in_progress(&repo.path).expect("merge state"));
  }

  #[test]
  fn changes_primary_command_promotes_push_only_when_the_tree_is_clean() {
    let push_branch = git::BranchStatus {
      name: "feature".to_string(),
      ahead: 1,
      behind: 0,
      has_upstream: true,
    };
    let force_push_branch = git::BranchStatus {
      name: "feature".to_string(),
      ahead: 1,
      behind: 1,
      has_upstream: true,
    };
    let status_entries = [git::RepoStatusEntry {
      path: PathBuf::from("a.txt"),
      old_path: None,
      status: git::RepoStatusKind::Modified,
      stage: git::RepoStage::Unstaged,
    }];

    assert_eq!(
      changes_primary_command(&RepoState {
        has_repo: true,
        merge_in_progress: false,
        rebase_in_progress: false,
        has_head_commit: true,
        can_push: true,
        can_force_push: false,
        can_undo_last_commit: true,
        branch_status: Some(&push_branch),
        status_entries: &[],
        selected_entry: None,
        commit_message: "",
      }),
      Some(ChangesActionCommand::Push)
    );
    assert_eq!(
      changes_primary_command(&RepoState {
        has_repo: true,
        merge_in_progress: false,
        rebase_in_progress: false,
        has_head_commit: true,
        can_push: false,
        can_force_push: true,
        can_undo_last_commit: true,
        branch_status: Some(&force_push_branch),
        status_entries: &[],
        selected_entry: None,
        commit_message: "",
      }),
      Some(ChangesActionCommand::ForcePush)
    );
    assert_eq!(
      changes_primary_command(&RepoState {
        has_repo: true,
        merge_in_progress: false,
        rebase_in_progress: false,
        has_head_commit: true,
        can_push: true,
        can_force_push: false,
        can_undo_last_commit: true,
        branch_status: Some(&push_branch),
        status_entries: &status_entries,
        selected_entry: None,
        commit_message: "",
      }),
      Some(ChangesActionCommand::StageAll)
    );
  }

  #[test]
  fn amend_menu_item_names_the_action_it_will_take() {
    assert_eq!(CommitMenuCommand::Amend.label(false), "Amend");
    assert_eq!(CommitMenuCommand::Amend.label(true), "Cancel Amend");
  }

  #[gpui::test]
  async fn the_commit_menu_offers_what_the_repository_allows(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-panel-commit-menu");
    commit_text_file(&repo.path, Path::new("a.txt"), "v1\n", "initial");
    commit_text_file(&repo.path, Path::new("b.txt"), "v1\n", "second");

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;
    panel.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();

    assert!(
      cx.debug_bounds(DOCK_PANEL_COMMIT_MENU_DEBUG_SELECTOR)
        .is_some(),
      "the commit button carries its menu"
    );
    assert!(
      cx.debug_bounds(DOCK_PANEL_CHANGES_ACTION_MENU_DEBUG_SELECTOR)
        .is_some(),
      "the Changes panel carries repository actions"
    );
    assert!(
      cx.debug_bounds(DOCK_PANEL_CHANGES_ACTION_DEBUG_SELECTOR)
        .is_none(),
      "a clean tree has no Stage All or Unstage All primary action"
    );
    assert!(
      cx.debug_bounds(DOCK_PANEL_CHANGES_EMPTY_DEBUG_SELECTOR)
        .is_some(),
      "a clean tree uses the lightweight Changes empty state"
    );
    assert!(
      cx.debug_bounds("dock-panel-empty-state").is_none(),
      "a clean tree does not use the setup empty-state card"
    );

    // The menus share the same rules: commit-specific amend, repository-level undo and push.
    panel.read_with(cx, |panel, _| {
      let state = panel.repo_state("");
      assert!(state.allows(PaletteCommand::Amend));
      assert!(state.allows(PaletteCommand::UndoLastCommit));
      assert!(
        !state.allows(PaletteCommand::Push),
        "the panel knows no branch yet"
      );
    });

    // The host hands the branch over: publishing becomes possible.
    panel.update(cx, |panel, cx| {
      panel.set_branch_status(
        Some(git::BranchStatus {
          name: "main".to_string(),
          ahead: 0,
          behind: 0,
          has_upstream: false,
        }),
        cx,
      )
    });
    panel.read_with(cx, |panel, _| {
      assert!(
        panel.repo_state("").allows(PaletteCommand::Push),
        "an unpublished branch is pushed to publish it"
      );
    });

    // The menu asks the host to run, it never runs the command itself.
    let asked = Arc::new(AtomicBool::new(false));
    let observer = {
      let asked = asked.clone();
      cx.update(|_, cx| {
        cx.subscribe(&panel, move |_panel, event: &DockPanelEvent, _cx| {
          if matches!(event, DockPanelEvent::RunCommand(CommitMenuCommand::Amend)) {
            asked.store(true, Ordering::SeqCst);
          }
        })
      })
    };
    panel.update(cx, |_, cx| {
      cx.emit(DockPanelEvent::RunCommand(CommitMenuCommand::Amend))
    });
    cx.run_until_parked();
    assert!(asked.load(Ordering::SeqCst));
    drop(observer);
  }

  #[gpui::test]
  async fn the_changes_actions_primary_follows_the_worktree(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-panel-changes-actions");
    commit_text_file(&repo.path, Path::new("a.txt"), "v1\n", "initial");
    std::fs::write(repo.path.join("a.txt"), "v2\n").expect("dirty the worktree");

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;
    panel.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();

    let action_bounds = cx
      .debug_bounds(DOCK_PANEL_CHANGES_ACTION_DEBUG_SELECTOR)
      .expect("unstaged work gets a Stage All primary action");
    assert!(
      action_bounds.size.height <= px(21.0),
      "the changes action button stays compact"
    );

    git::stage_all(&repo.path).expect("stage all");
    await_refresh(&panel, cx).await;
    panel.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();

    assert!(
      cx.debug_bounds(DOCK_PANEL_CHANGES_ACTION_DEBUG_SELECTOR)
        .is_some(),
      "staged work gets an Unstage All primary action"
    );

    let asked = Arc::new(AtomicBool::new(false));
    let observer = {
      let asked = asked.clone();
      cx.update(|_, cx| {
        cx.subscribe(&panel, move |_panel, event: &DockPanelEvent, _cx| {
          if matches!(
            event,
            DockPanelEvent::RunChangesAction(ChangesActionCommand::UnstageAll)
          ) {
            asked.store(true, Ordering::SeqCst);
          }
        })
      })
    };
    panel.update(cx, |_, cx| {
      cx.emit(DockPanelEvent::RunChangesAction(
        ChangesActionCommand::UnstageAll,
      ))
    });
    cx.run_until_parked();
    assert!(asked.load(Ordering::SeqCst));
    drop(observer);
  }

  #[gpui::test]
  async fn changes_row_context_menu_does_not_leave_empty_space_menu_behind(
    cx: &mut TestAppContext,
  ) {
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-panel-changes-row-single-menu");
    commit_text_file(&repo.path, Path::new("a.txt"), "v1\n", "initial");
    std::fs::write(repo.path.join("a.txt"), "v2\n").expect("dirty the worktree");

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;
    panel.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();

    let asked = Arc::new(AtomicBool::new(false));
    let observer = {
      let asked = asked.clone();
      cx.update(|_, cx| {
        cx.subscribe(&panel, move |_panel, event: &DockPanelEvent, _cx| {
          if matches!(
            event,
            DockPanelEvent::RunChangesAction(ChangesActionCommand::StageAll)
          ) {
            asked.store(true, Ordering::SeqCst);
          }
        })
      })
    };

    open_files_context_menu(cx, "changes-row-0-0");
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    cx.simulate_keystrokes("down enter");
    cx.run_until_parked();

    assert!(!asked.load(Ordering::SeqCst));
    drop(observer);
  }

  #[gpui::test]
  async fn the_changes_list_empty_space_context_menu_runs_changes_actions(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-panel-changes-list-empty-menu");
    commit_text_file(&repo.path, Path::new("a.txt"), "v1\n", "initial");
    std::fs::write(repo.path.join("a.txt"), "v2\n").expect("dirty the worktree");

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;
    panel.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();

    let asked = Arc::new(AtomicBool::new(false));
    let observer = {
      let asked = asked.clone();
      cx.update(|_, cx| {
        cx.subscribe(&panel, move |_panel, event: &DockPanelEvent, _cx| {
          if matches!(
            event,
            DockPanelEvent::RunChangesAction(ChangesActionCommand::StageAll)
          ) {
            asked.store(true, Ordering::SeqCst);
          }
        })
      })
    };

    open_changes_list_empty_context_menu(cx);
    cx.simulate_keystrokes("down enter");
    cx.run_until_parked();
    assert!(asked.load(Ordering::SeqCst));
    drop(observer);
  }

  #[gpui::test]
  async fn the_changes_actions_primary_pushes_a_clean_ahead_branch(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-panel-changes-push-primary");
    commit_text_file(&repo.path, Path::new("a.txt"), "v1\n", "initial");

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;
    panel.update(cx, |panel, cx| {
      panel.set_branch_status(
        Some(git::BranchStatus {
          name: "feature".to_string(),
          ahead: 1,
          behind: 0,
          has_upstream: false,
        }),
        cx,
      )
    });
    cx.run_until_parked();

    assert!(
      cx.debug_bounds(DOCK_PANEL_CHANGES_ACTION_DEBUG_SELECTOR)
        .is_some(),
      "a clean branch with commits to push gets Push as its primary action"
    );

    let asked = Arc::new(AtomicBool::new(false));
    let observer = {
      let asked = asked.clone();
      cx.update(|_, cx| {
        cx.subscribe(&panel, move |_panel, event: &DockPanelEvent, _cx| {
          if matches!(
            event,
            DockPanelEvent::RunChangesAction(ChangesActionCommand::Push)
          ) {
            asked.store(true, Ordering::SeqCst);
          }
        })
      })
    };
    let button = cx
      .debug_bounds(DOCK_PANEL_CHANGES_ACTION_DEBUG_SELECTOR)
      .expect("changes primary action bounds");
    cx.simulate_click(button.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    assert!(asked.load(Ordering::SeqCst));
    drop(observer);
  }

  #[gpui::test]
  async fn a_rebase_in_progress_replaces_the_commit_button(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-panel-rebase");
    commit_text_file(&repo.path, Path::new("a.txt"), "base\n", "initial");
    let base = git::BranchRef {
      name: git::current_branch_status(&repo.path)
        .expect("branch status")
        .name,
      kind: git::BranchKind::Local,
    };
    let feature = git::BranchRef {
      name: "feature".to_string(),
      kind: git::BranchKind::Local,
    };
    git::create_branch(&repo.path, &feature.name).expect("create branch");
    git::switch_branch(&repo.path, &feature).expect("switch to feature");
    commit_text_file(&repo.path, Path::new("a.txt"), "feature\n", "feature work");
    git::switch_branch(&repo.path, &base).expect("switch back");
    commit_text_file(&repo.path, Path::new("a.txt"), "main\n", "main work");
    git::switch_branch(&repo.path, &feature).expect("switch to feature");
    let _ = git::rebase_branch(&repo.path, &base);

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;
    panel.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();

    panel.read_with(cx, |panel, _| assert!(panel.rebase_in_progress()));
    assert!(cx.debug_bounds("dock-panel-operation").is_none());

    // The button now continues the rebase; the host runs it.
    let asked = Arc::new(AtomicBool::new(false));
    let observer = {
      let asked = asked.clone();
      cx.update(|_, cx| {
        cx.subscribe(&panel, move |_panel, event: &DockPanelEvent, _cx| {
          if matches!(event, DockPanelEvent::ContinueRebase) {
            asked.store(true, Ordering::SeqCst);
          }
        })
      })
    };

    // Conflicted: the button is there but does nothing yet.
    let button = cx
      .debug_bounds(DOCK_PANEL_COMMIT_DEBUG_SELECTOR)
      .expect("commit button bounds");
    cx.simulate_click(button.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    assert!(!asked.load(Ordering::SeqCst));

    // Resolved and staged: the same button asks to continue.
    std::fs::write(repo.path.join("a.txt"), "resolved\n").expect("resolve conflict");
    git::stage_all(&repo.path).expect("stage the resolution");
    await_refresh(&panel, cx).await;
    panel.update(cx, |panel, cx| {
      panel.refresh(cx);
    });
    await_refresh(&panel, cx).await;
    let button = cx
      .debug_bounds(DOCK_PANEL_COMMIT_DEBUG_SELECTOR)
      .expect("commit button bounds");
    cx.simulate_click(button.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    assert!(asked.load(Ordering::SeqCst));

    // Nothing was committed behind our back.
    assert!(git::is_rebase_in_progress(&repo.path).expect("rebase state"));
    drop(observer);
  }

  #[gpui::test]
  async fn commit_stages_and_commits_all_changes(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-panel-commit");
    commit_text_file(&repo.path, Path::new("README.md"), "v1\n", "initial");
    std::fs::write(repo.path.join("README.md"), "v2\n").expect("update file");

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;

    panel.update_in(cx, |panel, window, cx| {
      panel.commit_input.update(cx, |input, cx| {
        input.set_value("feat: update readme", window, cx)
      });
    });
    // The shell refreshes its branch counters on this event.
    let committed = Arc::new(AtomicBool::new(false));
    let observer = {
      let committed = committed.clone();
      cx.update(|_, cx| {
        cx.subscribe(&panel, move |_, event: &DockPanelEvent, _| {
          if matches!(event, DockPanelEvent::Committed) {
            committed.store(true, Ordering::Relaxed);
          }
        })
      })
    };

    panel.update(cx, |panel, cx| panel.commit(cx));

    let commit_task = panel.update(cx, |panel, _| {
      panel._commit_task.take().expect("commit task")
    });
    commit_task.await;
    await_refresh(&panel, cx).await;
    drop(observer);
    assert!(
      committed.load(Ordering::Relaxed),
      "expected a Committed event"
    );

    let repo_handle = Repository::open(&repo.path).expect("open repo");
    let head = repo_handle
      .head()
      .and_then(|head| head.peel_to_commit())
      .expect("read head");
    assert_eq!(
      head.summary().expect("read summary"),
      Some("feat: update readme")
    );
    panel.read_with(cx, |panel, cx| {
      assert!(panel.status_entries.is_empty());
      assert!(panel.commit_input.read(cx).value().is_empty());
    });
  }

  #[gpui::test]
  async fn commit_requires_message_and_changes(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-panel-commit-guards");
    commit_text_file(&repo.path, Path::new("README.md"), "v1\n", "initial");

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;

    // Clean tree: commit is a no-op even with a message.
    panel.update_in(cx, |panel, window, cx| {
      panel
        .commit_input
        .update(cx, |input, cx| input.set_value("feat: nothing", window, cx));
    });
    panel.update(cx, |panel, cx| panel.commit(cx));
    panel.read_with(cx, |panel, _| assert!(panel._commit_task.is_none()));

    // Dirty tree but empty message: also a no-op.
    std::fs::write(repo.path.join("README.md"), "v2\n").expect("update file");
    panel.update(cx, |panel, cx| panel.refresh(cx));
    await_refresh(&panel, cx).await;
    panel.update_in(cx, |panel, window, cx| {
      panel
        .commit_input
        .update(cx, |input, cx| input.set_value("   ", window, cx));
    });
    panel.update(cx, |panel, cx| panel.commit(cx));
    panel.read_with(cx, |panel, _| assert!(panel._commit_task.is_none()));
  }

  #[gpui::test]
  async fn the_history_tab_lists_the_commits_and_opens_one_of_their_files(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-panel-history-tab");
    commit_text_file(&repo.path, Path::new("README.md"), "v1\n", "initial");
    commit_text_file(&repo.path, Path::new("README.md"), "v2\n", "second");

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;
    panel.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();

    // The tabs live in the page's rail now; the panel exposes open_tab.
    panel.update_in(cx, |panel, window, cx| {
      panel.open_tab(DockPanelTab::History, window, cx)
    });
    let history = panel.read_with(cx, |panel, _| panel.history_list.clone());
    let load = history.update(cx, |list, _| list._history_task.take());
    if let Some(task) = load {
      task.await;
    }
    cx.run_until_parked();

    panel.read_with(cx, |panel, _| {
      assert_eq!(panel.active_tab, DockPanelTab::History);
    });
    assert!(
      cx.debug_bounds(crate::history_list::HISTORY_LIST_DEBUG_SELECTOR)
        .is_some(),
      "the history tab shows the commit tree"
    );

    // The panel forwards the file of a commit to whoever hosts it.
    let opened = Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = opened.clone();
    cx.update(|_, cx| {
      cx.subscribe(&panel, move |_panel, event: &DockPanelEvent, _cx| {
        if let DockPanelEvent::OpenCommitFile {
          commit_oid, path, ..
        } = event
        {
          seen.borrow_mut().push((commit_oid.clone(), path.clone()));
        }
      })
      .detach();
    });

    let head = git::current_head_sha(&repo.path)
      .expect("head sha")
      .expect("head sha");
    history.update(cx, |list, cx| {
      list.open_commit_file(
        head.clone(),
        PathBuf::from("README.md"),
        OpenIntent::Open,
        cx,
      )
    });
    cx.run_until_parked();

    assert_eq!(
      opened.borrow().as_slice(),
      &[(head, PathBuf::from("README.md"))]
    );
  }

  #[gpui::test]
  async fn staging_from_the_changes_list_refreshes_the_panel(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let repo = TempRepo::init("dock-panel-changes-list");
    commit_text_file(&repo.path, Path::new("README.md"), "v1\n", "initial");
    std::fs::write(repo.path.join("README.md"), "v2\n").expect("update file");

    let (panel, cx) = add_dock_panel_window(Some(repo.path.clone()), cx);
    await_refresh(&panel, cx).await;
    panel.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();

    panel.read_with(cx, |panel, cx| {
      assert_eq!(panel.status_entries.len(), 1);
      assert_eq!(panel.status_entries[0].stage, RepoStage::Unstaged);
      // The panel feeds the shared list.
      assert_eq!(panel.changes_list.read(cx).entries().len(), 1);
    });

    let button = cx
      .debug_bounds("changes-stage-0-0")
      .expect("stage button bounds");
    cx.simulate_click(button.center(), gpui::Modifiers::default());
    let action = panel.update(cx, |panel, cx| {
      panel
        .changes_list
        .update(cx, |list, _| list._action_task.take().expect("stage task"))
    });
    action.await;
    cx.run_until_parked();
    await_refresh(&panel, cx).await;

    panel.read_with(cx, |panel, _| {
      assert!(
        panel
          .status_entries
          .iter()
          .all(|entry| entry.stage != RepoStage::Unstaged),
        "the panel should have refreshed after the staging action"
      );
    });
  }
}
