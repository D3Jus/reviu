//! Palette actions every page answers the same way.

use gpui::{App, SharedString, Window};
use ui::CommandPaletteAction;

use crate::{git_config_page::open_git_config_dialog, settings_page::open_settings_dialog};

/// Handles the navigation and dialog actions shared by every page.
/// Pages match their own actions first and route the rest here.
pub(crate) fn handle_global_command_palette_action(
  action: CommandPaletteAction,
  window: &mut Window,
  cx: &mut App,
) -> Result<(), SharedString> {
  match action {
    CommandPaletteAction::OpenGitConfigPage => {
      open_git_config_dialog(window, cx);
      Ok(())
    }
    CommandPaletteAction::OpenSettingsPage => {
      open_settings_dialog(window, cx);
      Ok(())
    }
    CommandPaletteAction::OpenSettingsFile => {
      cx.reveal_path(&crate::settings_file::settings_file_path());
      Ok(())
    }
    CommandPaletteAction::OpenKeybindingsFile => {
      cx.reveal_path(&crate::keybindings_file::keybindings_file_path());
      Ok(())
    }
    CommandPaletteAction::OpenAboutPage => {
      crate::about_dialog::open_about_dialog(window, cx);
      Ok(())
    }
    CommandPaletteAction::OpenLogs => {
      if crate::app_log_sink::open_active_log(cx) {
        Ok(())
      } else {
        Err("Logs are disabled for this Reviu session.".into())
      }
    }
    CommandPaletteAction::RevealLogs => {
      if crate::app_log_sink::reveal_active_log(cx) {
        Ok(())
      } else {
        Err("Logs are disabled for this Reviu session.".into())
      }
    }
    _ => Err("Command not available.".into()),
  }
}
