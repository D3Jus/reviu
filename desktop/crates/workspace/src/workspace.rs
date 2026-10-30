use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use editor::{Copy, Cut, Paste, Quit, Redo, Save, SelectAll, Undo};
#[cfg(test)]
use gpui::Keystroke;
use gpui::{
  App, Context, Decorations, Entity, FocusHandle, Focusable, Menu, MenuItem, Render, Subscription,
  Task, Window, WindowButton, div, prelude::*, px,
};
use gpui_component::{
  ActiveTheme as _, Disableable, Icon, IconName, Sizable as _, Theme, h_flex, kbd::Kbd, tag::Tag,
};

use crate::AppProfile;
use crate::about_dialog::open_about_dialog;
use crate::app_update::{
  AppUpdateNotificationId, AppUpdateState, AppUpdateStore, start_update_download,
  update_action_label,
};
use crate::config::{AppSettings as PersistedSettings, ConfigStore};
use crate::git_config_page::open_git_config_dialog;
use crate::session_page::SessionPage;
use crate::settings_page::open_settings_dialog;
use crate::shortcuts::{self, ShortcutId};
use crate::workspace_onboarding::WorkspaceOnboarding;
use crate::workspace_window::WorkspaceWindow;
use crate::{ShowCommandPalette, ShowFileSearch};
use ui::{
  Button, ButtonVariants as _, GLOBAL_BAR_HEIGHT, UiIconName, UserMenuConfig, UserMenuPage,
  UserMenuState, WindowExt, user_menu,
};

type NavigateFn = dyn Fn(&mut Window, &mut App);

const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);

fn should_run_scheduled_update_check(state: Option<AppUpdateState>) -> bool {
  !matches!(
    state,
    Some(AppUpdateState::Available(_))
      | Some(AppUpdateState::Downloading(_))
      | Some(AppUpdateState::ReadyToInstall(_))
      | Some(AppUpdateState::Error {
        update: Some(_),
        ..
      })
  )
}

pub fn build_app_menus() -> Vec<Menu> {
  vec![
    Menu {
      name: "Reviu".into(),
      disabled: false,
      items: vec![
        MenuItem::action("About Reviu", crate::OpenAboutPage),
        MenuItem::separator(),
        MenuItem::action("Settings...", crate::OpenSettingsPage),
        MenuItem::action("Git Config...", crate::OpenGitConfigPage),
        MenuItem::separator(),
        MenuItem::action("Quit Reviu", Quit),
      ],
    },
    Menu {
      name: "File".into(),
      disabled: false,
      items: vec![
        MenuItem::action("New File", crate::NewFile),
        MenuItem::action("Open Project...", crate::OpenProject),
        MenuItem::separator(),
        MenuItem::action("Save", Save),
        MenuItem::action("Save As...", crate::SaveFileAs),
        MenuItem::separator(),
        MenuItem::action("Close Tab", crate::CloseCenterTab),
      ],
    },
    Menu {
      name: "Edit".into(),
      disabled: false,
      items: vec![
        MenuItem::os_action("Undo", Undo, gpui::OsAction::Undo),
        MenuItem::os_action("Redo", Redo, gpui::OsAction::Redo),
        MenuItem::separator(),
        MenuItem::os_action("Cut", Cut, gpui::OsAction::Cut),
        MenuItem::os_action("Copy", Copy, gpui::OsAction::Copy),
        MenuItem::os_action("Paste", Paste, gpui::OsAction::Paste),
        MenuItem::separator(),
        MenuItem::os_action("Select All", SelectAll, gpui::OsAction::SelectAll),
      ],
    },
  ]
}

pub struct WorkspaceView {
  session_page: Entity<SessionPage>,
  onboarding_page: Entity<WorkspaceOnboarding>,
  _update_check_task: Option<Task<()>>,
  _periodic_update_check_task: Option<Task<()>>,
  _config_watch_task: Option<Task<()>>,
  _subscriptions: Vec<Subscription>,
}

impl WorkspaceView {
  const GLOBAL_BAR_MACOS_LEFT_PADDING: f32 = 85.0;

  #[cfg(test)]
  fn command_palette_shortcut() -> Keystroke {
    shortcuts::shortcut_keystroke(ShortcutId::ShowCommandPalette)
  }

  fn command_palette_kbd(window: &Window, cx: &App) -> Kbd {
    Kbd::new(shortcuts::resolved_shortcut_keystroke_in(
      cx,
      window,
      ShortcutId::ShowCommandPalette,
      shortcuts::WORKSPACE_SESSION_CONTEXT,
    ))
  }

  fn file_search_kbd(window: &Window, cx: &App) -> Kbd {
    Kbd::new(shortcuts::resolved_shortcut_keystroke_in(
      cx,
      window,
      ShortcutId::ShowFileSearch,
      shortcuts::WORKSPACE_SESSION_CONTEXT,
    ))
  }

  fn sync_app_menus(cx: &mut App) {
    cx.set_menus(build_app_menus());
  }

  fn quit_action(&mut self, _: &Quit, window: &mut Window, cx: &mut Context<Self>) {
    self
      .session_page
      .update(cx, |page, cx| page.request_quit(window, cx));
    cx.stop_propagation();
  }

  pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
    if let Some(dir) = AppProfile::current().config_dir() {
      agent_chat_panel::set_settings_dir(dir);
    }

    let settings = ConfigStore::load_app_settings();
    let settings_error = crate::settings_file::take_startup_error();

    cx.set_global(AppUpdateStore::default());

    crate::config_reload::apply_settings(settings, Some(window), cx);
    cx.set_global(shortcuts::load_shortcut_overrides());
    let keybindings_error = crate::keybindings_file::take_startup_error();
    cx.set_global(crate::command_usage::CommandUsageStore::load());
    crate::command_usage::install_palette_usage_recorder(cx);
    crate::install_app_key_bindings(cx);

    // Deep links, pro prompts, and git askpass requests land without their own window.
    WorkspaceWindow::register(window.window_handle(), cx);
    crate::askpass::install(cx);
    for (kind, error) in [
      (crate::config_reload::ConfigKind::Settings, settings_error),
      (
        crate::config_reload::ConfigKind::Keybindings,
        keybindings_error,
      ),
    ] {
      if let Some(error) = error {
        crate::config_reload::save_failed(kind, anyhow::anyhow!(error), cx);
      }
    }

    let session_page = cx.new(|cx| SessionPage::new(window, cx));
    let onboarding_page = cx.new(|cx| WorkspaceOnboarding::new(session_page.clone(), window, cx));
    session_page.update(cx, |page, cx| page.activate(window, cx));
    let focus_handle = if settings.onboarding_done {
      session_page.read(cx).focus_handle(cx)
    } else {
      onboarding_page.read(cx).focus_handle(cx)
    };
    window.focus(&focus_handle, cx);

    let workspace = cx.entity().downgrade();
    window.on_window_should_close(cx, move |window, cx| {
      workspace
        .update(cx, |workspace, cx| {
          workspace
            .session_page
            .update(cx, |page, cx| page.window_can_close(window, cx))
        })
        .unwrap_or(true)
    });

    let view = Self {
      session_page,
      onboarding_page,
      _update_check_task: None,
      _periodic_update_check_task: None,
      _config_watch_task: Some(crate::config_reload::watch(cx)),
      _subscriptions: Vec::new(),
    };

    let mut view = view;
    let subscription = cx.observe_window_appearance(window, |this, window, cx| {
      this.on_window_appearance_changed(window, cx);
    });
    view._subscriptions.push(subscription);
    let subscription = cx.observe_global::<shortcuts::ShortcutOverrides>(|_, cx| {
      Self::sync_app_menus(cx);
      cx.notify();
    });
    view._subscriptions.push(subscription);
    Self::sync_app_menus(cx);
    view.check_for_updates(cx);
    view.start_periodic_update_checks(cx);

    view
  }

  #[doc(hidden)]
  pub fn open_project_for_driver(
    &mut self,
    project_root: PathBuf,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Result<(), gpui::SharedString> {
    self.session_page.update(cx, |page, cx| {
      page.open_project_for_driver(project_root, window, cx)
    })
  }

  #[cfg(any(test, feature = "test-support"))]
  #[doc(hidden)]
  pub fn open_file_for_driver(
    &mut self,
    rel_path: PathBuf,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Result<(), gpui::SharedString> {
    self.session_page.update(cx, |page, cx| {
      page.open_file_for_driver(rel_path, window, cx)
    })
  }

  #[cfg(any(test, feature = "test-support"))]
  #[doc(hidden)]
  pub fn open_code_file_for_driver(
    &mut self,
    rel_path: PathBuf,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Result<(), gpui::SharedString> {
    self.session_page.update(cx, |page, cx| {
      page.open_code_file_for_driver(rel_path, window, cx)
    })
  }

  #[cfg(any(test, feature = "test-support"))]
  #[doc(hidden)]
  pub fn git_state_for_driver(&self, cx: &App) -> serde_json::Value {
    self
      .session_page
      .read_with(cx, |page, cx| page.git_state_for_driver(cx))
  }

  #[cfg(any(test, feature = "test-support"))]
  #[doc(hidden)]
  pub fn notification_log_for_driver(&self, cx: &App) -> serde_json::Value {
    self
      .session_page
      .read_with(cx, |page, _| page.notification_log_for_driver())
  }

  #[cfg(any(test, feature = "test-support"))]
  #[doc(hidden)]
  pub fn run_git_action_for_driver(
    &mut self,
    action: crate::DriverGitAction,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Result<(), gpui::SharedString> {
    self.session_page.update(cx, |page, cx| {
      page.run_git_action_for_driver(action, window, cx)
    })
  }

  #[cfg(any(test, feature = "test-support"))]
  #[doc(hidden)]
  pub fn show_changes_for_driver(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    self
      .session_page
      .update(cx, |page, cx| page.show_changes_for_driver(window, cx));
  }

  #[cfg(any(test, feature = "test-support"))]
  #[doc(hidden)]
  pub fn resize_dock_for_driver(&mut self, width: f32, cx: &mut Context<Self>) {
    self
      .session_page
      .update(cx, |page, cx| page.resize_dock_for_driver(width, cx));
  }

  #[cfg(any(test, feature = "test-support"))]
  #[doc(hidden)]
  pub fn expand_sidebar_projects_for_driver(&mut self, cx: &mut Context<Self>) {
    self
      .session_page
      .update(cx, |page, cx| page.expand_sidebar_projects_for_driver(cx));
  }

  #[cfg(any(test, feature = "test-support"))]
  #[doc(hidden)]
  pub fn set_editor_scroll_for_driver(
    &mut self,
    scroll_offset_y: f32,
    cx: &mut Context<Self>,
  ) -> Result<(), gpui::SharedString> {
    self.session_page.update(cx, |page, cx| {
      page.set_editor_scroll_for_driver(scroll_offset_y, cx)
    })
  }

  #[cfg(any(test, feature = "test-support"))]
  #[doc(hidden)]
  pub fn resize_sidebar_for_driver(&mut self, width: f32, cx: &mut Context<Self>) {
    self
      .session_page
      .update(cx, |page, cx| page.resize_sidebar_for_driver(width, cx));
  }

  #[cfg(any(test, feature = "test-support"))]
  #[doc(hidden)]
  pub fn open_terminal_for_driver(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    self
      .session_page
      .update(cx, |page, cx| page.open_terminal_for_driver(window, cx));
  }

  #[cfg(any(test, feature = "test-support"))]
  #[doc(hidden)]
  pub fn split_center_with_previous_for_driver(
    &mut self,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Result<(), gpui::SharedString> {
    self.session_page.update(cx, |page, cx| {
      page.split_center_with_previous_for_driver(window, cx)
    })
  }

  #[cfg(any(test, feature = "test-support"))]
  #[doc(hidden)]
  pub fn terminal_state_for_driver(
    &self,
    cx: &App,
  ) -> Result<crate::DriverTerminalState, gpui::SharedString> {
    self.session_page.read(cx).terminal_state_for_driver(cx)
  }

  #[cfg(any(test, feature = "test-support"))]
  #[doc(hidden)]
  pub fn open_first_terminal_file_link_for_driver(
    &mut self,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Result<(), gpui::SharedString> {
    self.session_page.update(cx, |page, cx| {
      page.open_first_terminal_file_link_for_driver(window, cx)
    })
  }

  #[cfg(any(test, feature = "test-support"))]
  #[doc(hidden)]
  pub fn open_agent_diff_snapshot_for_driver(
    &mut self,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Result<(), gpui::SharedString> {
    self.session_page.update(cx, |page, cx| {
      page.open_agent_diff_snapshot_for_driver(window, cx)
    })
  }

  #[cfg(any(test, feature = "test-support"))]
  #[doc(hidden)]
  pub fn focus_agent_chat_for_driver(
    &mut self,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Result<(), gpui::SharedString> {
    self
      .session_page
      .update(cx, |page, cx| page.focus_agent_chat_for_driver(window, cx))
  }

  #[cfg(any(test, feature = "test-support"))]
  #[doc(hidden)]
  pub fn focus_agent_chat_by_title_for_driver(
    &mut self,
    title: String,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Result<(), gpui::SharedString> {
    self.session_page.update(cx, |page, cx| {
      page.focus_agent_chat_by_title_for_driver(title, window, cx)
    })
  }

  #[cfg(any(test, feature = "test-support"))]
  #[doc(hidden)]
  pub fn split_center_with_chat_for_driver(
    &mut self,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Result<(), gpui::SharedString> {
    self.session_page.update(cx, |page, cx| {
      page.split_center_with_chat_for_driver(window, cx)
    })
  }

  #[cfg(any(test, feature = "test-support"))]
  #[doc(hidden)]
  pub fn prepare_screenshot_workspace_for_driver(
    &mut self,
    active_path: Option<PathBuf>,
    dock_width: Option<f32>,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Result<(), gpui::SharedString> {
    self.session_page.update(cx, |page, cx| {
      page.prepare_screenshot_workspace_for_driver(active_path, dock_width, window, cx)
    })
  }

  #[cfg(any(test, feature = "test-support"))]
  #[doc(hidden)]
  pub fn show_review_for_driver(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    self
      .session_page
      .update(cx, |page, cx| page.show_review_for_driver(window, cx));
  }

  #[cfg(any(test, feature = "test-support"))]
  #[doc(hidden)]
  pub fn hide_dock_for_driver(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    self
      .session_page
      .update(cx, |page, cx| page.hide_dock_for_driver(window, cx));
  }

  #[cfg(any(test, feature = "test-support"))]
  #[doc(hidden)]
  pub fn submit_agent_prompt_for_driver(
    &mut self,
    text: String,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Result<(), gpui::SharedString> {
    self.session_page.update(cx, |page, cx| {
      page.submit_agent_prompt_for_driver(text, window, cx)
    })
  }

  #[cfg(any(test, feature = "test-support"))]
  #[doc(hidden)]
  pub fn new_agent_session_for_driver(
    &mut self,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Result<(), gpui::SharedString> {
    self
      .session_page
      .update(cx, |page, cx| page.new_agent_session_for_driver(window, cx))
  }

  #[cfg(any(test, feature = "test-support"))]
  #[doc(hidden)]
  pub fn seed_agent_message_for_driver(
    &mut self,
    text: String,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Result<(), gpui::SharedString> {
    self.session_page.update(cx, |page, cx| {
      page.seed_agent_message_for_driver(text, window, cx)
    })
  }

  #[cfg(any(test, feature = "test-support"))]
  #[doc(hidden)]
  pub fn agent_stats_for_driver(&self, cx: &App) -> serde_json::Value {
    self.session_page.read(cx).agent_stats_for_driver(cx)
  }

  #[cfg(any(test, feature = "test-support"))]
  #[doc(hidden)]
  pub fn editor_stats_for_driver(&self, cx: &App) -> serde_json::Value {
    self.session_page.read(cx).editor_stats_for_driver(cx)
  }

  fn on_window_appearance_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    if !PersistedSettings::get(cx).auto_switch_theme {
      return;
    }

    Theme::sync_system_appearance(Some(window), cx);
  }

  // ponytail: the auto-update check went through the (now removed) Pro backend
  // client, so there is no update source. Left inert; wire a new transport here
  // if the desktop updater comes back.
  fn check_for_updates(&mut self, _cx: &mut Context<Self>) {}

  fn start_periodic_update_checks(&mut self, cx: &mut Context<Self>) {
    let task = cx.spawn(async move |this, cx| {
      loop {
        cx.background_executor().timer(UPDATE_CHECK_INTERVAL).await;

        let should_check = this
          .update(cx, |_, cx| {
            should_run_scheduled_update_check(AppUpdateStore::try_state(cx))
          })
          .unwrap_or(false);

        if !should_check {
          continue;
        }

        let _ = this.update(cx, |this, cx| {
          this.check_for_updates(cx);
        });
      }
    });

    self._periodic_update_check_task = Some(task);
  }

  fn global_update_download_action(
    &mut self,
    _: &gpui::ClickEvent,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    start_update_download(cx);
    window.on_next_frame(|window, cx| {
      window.remove_notification::<AppUpdateNotificationId>(cx);
    });
  }

  fn render_linux_window_controls(window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
    let theme = cx.theme().clone();
    let controls = window.window_controls();
    let button_layout = cx.button_layout();

    let render_button = |icon: IconName, action: WindowButton, theme: &Theme| -> gpui::AnyElement {
      let is_close = matches!(action, WindowButton::Close);
      div()
        .id(match action {
          WindowButton::Minimize => "linux-minimize",
          WindowButton::Maximize => "linux-maximize",
          WindowButton::Close => "linux-close",
        })
        .flex()
        .items_center()
        .justify_center()
        .w(px(28.0))
        .h(px(28.0))
        .rounded(px(6.0))
        .cursor_pointer()
        .hover(|s| {
          if is_close {
            s.bg(gpui::red())
          } else {
            s.bg(theme.secondary_hover)
          }
        })
        .active(|s| {
          if is_close {
            s.bg(gpui::red())
          } else {
            s.bg(theme.secondary_active)
          }
        })
        .child(Icon::new(icon).size_4().text_color(theme.muted_foreground))
        .on_click(move |_, window, _cx| match action {
          WindowButton::Minimize => window.minimize_window(),
          WindowButton::Maximize => window.zoom_window(),
          WindowButton::Close => std::process::exit(0),
        })
        .into_any_element()
    };

    let mut buttons: Vec<gpui::AnyElement> = Vec::new();

    let ordered_buttons: Vec<WindowButton> = if let Some(layout) = button_layout {
      layout
        .right
        .iter()
        .chain(layout.left.iter())
        .filter_map(|b| *b)
        .collect()
    } else {
      vec![
        WindowButton::Minimize,
        WindowButton::Maximize,
        WindowButton::Close,
      ]
    };

    for button in ordered_buttons {
      let (icon, supported) = match button {
        WindowButton::Minimize => (IconName::WindowMinimize, controls.minimize),
        WindowButton::Maximize => (IconName::WindowMaximize, controls.maximize),
        WindowButton::Close => (IconName::WindowClose, true),
      };
      if supported {
        buttons.push(render_button(icon, button, &theme));
      }
    }

    h_flex().items_center().gap_1().ml_2().children(buttons)
  }

  fn render_onboarding_bar(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
    let theme = cx.theme().clone();
    let use_client_decorations = cfg!(target_os = "linux")
      && matches!(window.window_decorations(), Decorations::Client { .. });

    let bar = div()
      .h(px(GLOBAL_BAR_HEIGHT))
      .max_h(px(GLOBAL_BAR_HEIGHT))
      .w_full()
      .flex()
      .items_center()
      .justify_between()
      .bg(theme.sidebar)
      .border_b_1()
      .border_color(theme.title_bar_border);
    let bar = if cfg!(target_os = "macos") {
      bar.pl(px(Self::GLOBAL_BAR_MACOS_LEFT_PADDING)).pr_3()
    } else {
      bar.px_3()
    };

    let left = h_flex().items_center().gap_2().when_some(
      crate::presentation::profile_badge_label(AppProfile::current(), cx),
      |this, label| this.child(Tag::secondary().small().rounded_full().child(label)),
    );

    let drag_area = div()
      .id("onboarding-titlebar-drag")
      .flex_1()
      .h_full()
      .on_mouse_down(gpui::MouseButton::Left, |ev, window, _cx| {
        if ev.click_count >= 2 {
          window.zoom_window();
        } else if cfg!(target_os = "linux") {
          window.start_window_move();
        }
      })
      .on_mouse_down(gpui::MouseButton::Right, |ev, window, _cx| {
        window.show_window_menu(ev.position);
      });

    let right = h_flex()
      .items_center()
      .gap_2()
      .when(use_client_decorations, |this| {
        this.child(Self::render_linux_window_controls(window, cx))
      });

    bar.child(left).child(drag_area).child(right)
  }

  fn render_global_bar(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
    let theme = cx.theme().clone();
    let update_state = AppUpdateStore::try_state(cx);
    let show_update_button = AppUpdateStore::try_available_update(cx).is_some();
    let update_download_in_progress = AppUpdateStore::is_downloading(cx);
    let show_download_icon = matches!(
      update_state.as_ref(),
      Some(AppUpdateState::Available(_))
        | Some(AppUpdateState::Error {
          update: Some(_),
          ..
        })
    );
    let current_page = UserMenuPage::Session;

    let open_git_config: Rc<NavigateFn> = Rc::new(|window: &mut Window, cx: &mut App| {
      open_git_config_dialog(window, cx);
    });
    let open_settings = Rc::new(|window: &mut Window, cx: &mut App| {
      open_settings_dialog(window, cx);
    });
    let open_about = Rc::new(|window: &mut Window, cx: &mut App| {
      open_about_dialog(window, cx);
    });

    let auth_control = user_menu(UserMenuConfig {
      id: "workspace-auth-menu".into(),
      state: UserMenuState::Unauthenticated,
      current_page,
      on_open_billing: None,
      on_open_git_config: Some(open_git_config),
      on_open_settings: Some(open_settings),
      on_open_about: Some(open_about),
      on_open_browser_extensions: None,
      on_sign_in: None,
      on_sign_out: None,
    });

    let update_button = Button::new("workspace-global-update-download")
      .label(update_action_label(update_state.clone()))
      .primary()
      .compact()
      .xsmall()
      .loading(update_download_in_progress)
      .disabled(update_download_in_progress)
      .on_click(cx.listener(Self::global_update_download_action));
    let update_button = if show_download_icon {
      update_button.child(Icon::new(UiIconName::Download).size_3())
    } else {
      update_button
    };

    let file_search_button = Button::new("workspace-global-file-search")
      .label("File search")
      .ghost()
      .compact()
      .xsmall()
      .child(Self::file_search_kbd(window, cx).ml_1())
      .on_click(|_, window, cx| {
        window.dispatch_action(Box::new(ShowFileSearch), cx);
      });

    let command_palette_button = Button::new("workspace-global-command-palette")
      .label("Command palette")
      .ghost()
      .compact()
      .xsmall()
      .child(Self::command_palette_kbd(window, cx).ml_1())
      .on_click(|_, window, cx| {
        window.dispatch_action(Box::new(ShowCommandPalette), cx);
      });

    let sidebar_open = self.session_page.read(cx).sidebar_open();
    let sidebar_toggle_button = Button::new("workspace-global-sidebar-toggle")
      .debug_selector(|| "workspace-global-sidebar-toggle".to_string())
      .icon(IconName::PanelLeft)
      .ghost()
      .compact()
      .small()
      .tooltip(if sidebar_open {
        "Collapse sidebar"
      } else {
        "Open sidebar"
      })
      .on_click({
        let session_page = self.session_page.clone();
        move |_, _, cx| {
          session_page.update(cx, |page, cx| page.toggle_sidebar(cx));
        }
      });

    let bar = div()
      .h(px(GLOBAL_BAR_HEIGHT))
      .max_h(px(GLOBAL_BAR_HEIGHT))
      .w_full()
      .flex()
      .items_center()
      .justify_between()
      .bg(theme.sidebar)
      .border_b_1()
      .border_color(theme.title_bar_border);
    let bar = if cfg!(target_os = "macos") {
      bar.pl(px(Self::GLOBAL_BAR_MACOS_LEFT_PADDING)).pr_3()
    } else {
      bar.px_3()
    };

    let mut right = div().flex().items_center().gap_1();
    if show_update_button {
      right = right.child(update_button);
    }
    right = right.child(file_search_button);
    right = right.child(command_palette_button);
    if let Some(auth_control) = auth_control {
      right = right.child(auth_control);
    }

    let use_client_decorations = cfg!(target_os = "linux")
      && matches!(window.window_decorations(), Decorations::Client { .. });

    if use_client_decorations {
      right = right.child(Self::render_linux_window_controls(window, cx));
    }

    let left = h_flex()
      .items_center()
      .gap_2()
      .child(sidebar_toggle_button)
      .when_some(
        crate::presentation::profile_badge_label(AppProfile::current(), cx),
        |this, label| this.child(Tag::secondary().small().rounded_full().child(label)),
      );

    if use_client_decorations {
      let drag_area = div()
        .id("linux-titlebar-drag")
        .flex_1()
        .h_full()
        .on_mouse_down(gpui::MouseButton::Left, |ev, window, _cx| {
          if ev.click_count >= 2 {
            window.zoom_window();
          } else {
            window.start_window_move();
          }
        })
        .on_mouse_down(gpui::MouseButton::Right, |ev, window, _cx| {
          window.show_window_menu(ev.position);
        });
      bar.child(left).child(drag_area).child(right)
    } else {
      let zoom_area = div().id("titlebar-zoom").flex_1().h_full().on_mouse_down(
        gpui::MouseButton::Left,
        |ev, window, _cx| {
          if ev.click_count >= 2 {
            window.zoom_window();
          }
        },
      );
      bar.child(left).child(zoom_area).child(right)
    }
  }
}

impl Render for WorkspaceView {
  fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    let show_onboarding = !PersistedSettings::get(cx).onboarding_done;
    let session_page = self.session_page.clone();
    let onboarding_page = self.onboarding_page.clone();
    let key_context = shortcuts::current_workspace_key_context(cx);

    div()
      .size_full()
      .flex()
      .flex_col()
      .key_context(key_context.as_str())
      .on_action(cx.listener(Self::quit_action))
      .on_action(cx.listener(|_, _: &crate::OpenGitConfigPage, window, cx| {
        open_git_config_dialog(window, cx);
      }))
      .on_action(cx.listener(|_, _: &crate::OpenSettingsPage, window, cx| {
        open_settings_dialog(window, cx);
      }))
      .on_action(cx.listener(|_, _: &crate::OpenAboutPage, window, cx| {
        open_about_dialog(window, cx);
      }))
      .child(ui::scroll_dispatcher())
      .when(show_onboarding, |this| {
        this
          .child(self.render_onboarding_bar(window, cx))
          .child(div().flex_1().min_h_0().child(onboarding_page))
      })
      .when(!show_onboarding, |this| {
        this
          .child(self.render_global_bar(window, cx))
          .child(div().flex_1().min_h_0().child(session_page))
      })
      .into_any_element()
  }
}

impl Focusable for WorkspaceView {
  fn focus_handle(&self, cx: &App) -> FocusHandle {
    if !PersistedSettings::get(cx).onboarding_done {
      return self.onboarding_page.read(cx).focus_handle(cx);
    }
    self.session_page.read(cx).focus_handle(cx)
  }
}

#[cfg(test)]
mod tests {
  use super::{WorkspaceView, build_app_menus, should_run_scheduled_update_check};
  use crate::app_update::{
    AppUpdateState, AppUpdateStore, AvailableAppUpdate, ReadyToInstallAppUpdate, UpdateArtifact,
  };
  use crate::session_page::SessionPage;
  use crate::shortcuts::{self, ShortcutId};
  use crate::workspace_onboarding::WorkspaceOnboarding;
  use gpui::{AppContext as _, Focusable, Menu, MenuItem, TestAppContext};
  use std::path::PathBuf;

  fn action_menu_item_names(menu: &Menu) -> Vec<String> {
    menu
      .items
      .iter()
      .filter_map(|item| match item {
        MenuItem::Action { name, .. } => Some(name.to_string()),
        _ => None,
      })
      .collect()
  }

  fn make_available_update() -> AvailableAppUpdate {
    AvailableAppUpdate {
      latest_version: "0.2.0".to_string(),
      minimum_supported_version: "0.1.0".to_string(),
      release_notes_url: "https://reviu.dev/changelog".to_string(),
      force_update: false,
      artifact: UpdateArtifact {
        url: "https://reviu.dev/downloads/reviu-0.2.0.dmg".to_string(),
        sha256: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string(),
        size: 123,
      },
    }
  }

  #[gpui::test]
  async fn fresh_settings_show_onboarding_without_workspace_chrome(cx: &mut TestAppContext) {
    cx.update(|cx| {
      gpui_component::init(cx);
      cx.set_global(crate::config::AppSettings::default());
      cx.set_global(AppUpdateStore::default());
      cx.set_global(shortcuts::ShortcutOverrides::default());
    });

    let (_workspace, cx) = cx.add_window_view(|window, cx| {
      let page = cx.new(|cx| SessionPage::new(window, cx));
      let onboarding_page = cx.new(|cx| WorkspaceOnboarding::new(page.clone(), window, cx));
      WorkspaceView {
        session_page: page,
        onboarding_page,
        _config_watch_task: None,
        _update_check_task: None,
        _periodic_update_check_task: None,
        _subscriptions: Vec::new(),
      }
    });
    cx.run_until_parked();

    assert!(cx.debug_bounds("workspace-onboarding").is_some());
    assert!(cx.debug_bounds("workspace-global-sidebar-toggle").is_none());
  }

  #[gpui::test]
  async fn app_bar_sidebar_toggle_hides_and_shows_the_sidebar(cx: &mut TestAppContext) {
    cx.update(|cx| {
      gpui_component::init(cx);
      cx.set_global(crate::config::AppSettings {
        onboarding_done: true,
        ..Default::default()
      });
      cx.set_global(AppUpdateStore::default());
      cx.set_global(shortcuts::ShortcutOverrides::default());
    });

    let mut session_page = None;
    let (_workspace, cx) = cx.add_window_view(|window, cx| {
      let page = cx.new(|cx| SessionPage::new(window, cx));
      let onboarding_page = cx.new(|cx| WorkspaceOnboarding::new(page.clone(), window, cx));
      session_page = Some(page.clone());
      WorkspaceView {
        session_page: page,
        onboarding_page,
        _config_watch_task: None,
        _update_check_task: None,
        _periodic_update_check_task: None,
        _subscriptions: Vec::new(),
      }
    });
    let page = session_page.expect("session page");
    cx.run_until_parked();

    assert!(cx.debug_bounds("session-sidebar-collapse").is_none());
    assert!(cx.debug_bounds("sidebar-rail-open").is_none());

    let toggle = cx
      .debug_bounds("workspace-global-sidebar-toggle")
      .expect("sidebar toggle");
    cx.simulate_click(toggle.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    page.read_with(cx, |page, _| assert!(!page.sidebar_open()));

    assert!(cx.debug_bounds("sidebar-rail-open").is_none());

    let toggle = cx
      .debug_bounds("workspace-global-sidebar-toggle")
      .expect("sidebar toggle");
    cx.simulate_click(toggle.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    page.read_with(cx, |page, _| assert!(page.sidebar_open()));
  }

  #[gpui::test]
  async fn every_settings_shortcut_has_a_rendered_action_handler(cx: &mut TestAppContext) {
    cx.update(|cx| {
      gpui_component::init(cx);
      cx.set_global(crate::config::AppSettings {
        onboarding_done: true,
        ..Default::default()
      });
      cx.set_global(AppUpdateStore::default());
      cx.set_global(shortcuts::ShortcutOverrides::default());
    });

    let mut session_page = None;
    let (_workspace, cx) = cx.add_window_view(|window, cx| {
      let page = cx.new(|cx| SessionPage::new(window, cx));
      let onboarding_page = cx.new(|cx| WorkspaceOnboarding::new(page.clone(), window, cx));
      session_page = Some(page.clone());
      WorkspaceView {
        session_page: page,
        onboarding_page,
        _config_watch_task: None,
        _update_check_task: None,
        _periodic_update_check_task: None,
        _subscriptions: Vec::new(),
      }
    });
    let page = session_page.expect("session page");
    cx.run_until_parked();

    let session_focus = page.read_with(cx, |page, cx| page.focus_handle(cx));
    cx.update(|window, cx| window.focus(&session_focus, cx));
    cx.run_until_parked();

    let unavailable = cx.update(|window, cx| {
      shortcuts::shortcut_definitions()
        .iter()
        .filter(|definition| {
          !matches!(
            definition.id,
            ShortcutId::CommitChanges | ShortcutId::ToggleSoftWrap | ShortcutId::ToggleHunkExpanded
          )
        })
        .filter_map(|definition| {
          let available = shortcuts::with_shortcut_action(definition.id, |action| {
            window.is_action_available(action, cx)
          });
          (!available).then(|| format!("{} ({})", definition.title, definition.id.storage_key()))
        })
        .collect::<Vec<_>>()
    });
    assert!(
      unavailable.is_empty(),
      "shortcuts without action handlers: {unavailable:?}"
    );

    cx.update(|window, cx| window.dispatch_action(Box::new(crate::OpenGitChangesSidebar), cx));
    cx.run_until_parked();
    let commit_available = cx.update(|window, cx| {
      shortcuts::with_shortcut_action(ShortcutId::CommitChanges, |action| {
        window.is_action_available(action, cx)
      })
    });
    assert!(
      commit_available,
      "Commit Changes is handled only from the changes panel focus path"
    );
  }

  #[test]
  fn build_app_menus_include_file_actions_without_cluttering_the_app_menu() {
    let menus = build_app_menus();
    let app_menu = menus
      .iter()
      .find(|menu| menu.name == "Reviu")
      .expect("app menu");

    assert_eq!(
      action_menu_item_names(app_menu),
      vec!["About Reviu", "Settings...", "Git Config...", "Quit Reviu"]
    );
    let file_menu = menus
      .iter()
      .find(|menu| menu.name == "File")
      .expect("file menu");
    assert_eq!(
      action_menu_item_names(file_menu),
      vec![
        "New File",
        "Open Project...",
        "Save",
        "Save As...",
        "Close Tab"
      ]
    );
  }

  #[test]
  fn scheduled_update_check_runs_without_known_update() {
    assert!(should_run_scheduled_update_check(None));
    assert!(should_run_scheduled_update_check(Some(
      AppUpdateState::Error {
        update: None,
        message: "network timeout".to_string(),
      }
    )));
  }

  #[test]
  fn scheduled_update_check_skips_when_update_is_already_known() {
    let update = make_available_update();
    let ready = ReadyToInstallAppUpdate {
      update: update.clone(),
      artifact_path: PathBuf::from("/tmp/reviu.dmg"),
      restart_binary_path: None,
    };

    assert!(!should_run_scheduled_update_check(Some(
      AppUpdateState::Available(update.clone())
    )));
    assert!(!should_run_scheduled_update_check(Some(
      AppUpdateState::Downloading(update.clone())
    )));
    assert!(!should_run_scheduled_update_check(Some(
      AppUpdateState::ReadyToInstall(ready)
    )));
    assert!(!should_run_scheduled_update_check(Some(
      AppUpdateState::Error {
        update: Some(update),
        message: "checksum mismatch".to_string(),
      }
    )));
  }

  #[test]
  fn workspace_command_palette_shortcut_matches_global_binding() {
    assert_eq!(
      WorkspaceView::command_palette_shortcut(),
      shortcuts::shortcut_keystroke(ShortcutId::ShowCommandPalette)
    );
  }
}
