#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use app_root::AppRoot;
use gpui::{App, Bounds, Focusable, WindowBounds, WindowOptions, prelude::*, px, size};
#[cfg(target_os = "macos")]
use gpui::{TitlebarOptions, point};
use gpui_component::Root;
#[cfg(target_os = "macos")]
use gpui_component::TitleBar;
use reqwest_client::ReqwestClient;
use std::sync::{Arc, mpsc};
use ui::AppAssets;
#[cfg(target_os = "macos")]
use ui::PAGE_HEADER_HEIGHT;
use workspace::{AppProfile, WorkspaceView, build_app_menus, install_app_key_bindings};

const DARCULA_THEME_JSON: &str = include_str!("darcula_theme.json");

/// Register the bundled Darcula theme and make it the active dark theme.
fn install_darcula_theme(cx: &mut App) {
  use gpui_component::{Theme, ThemeRegistry};
  if let Err(error) = ThemeRegistry::global_mut(cx).load_themes_from_str(DARCULA_THEME_JSON) {
    log::error!("failed to load Darcula theme: {error:?}");
    return;
  }
  let Some(darcula) = ThemeRegistry::global(cx).themes().get("Darcula").cloned() else {
    return;
  };
  let theme = Theme::global_mut(cx);
  theme.dark_theme = darcula;
  // Re-apply the current mode so a dark session picks up Darcula immediately;
  // later `Theme::change` calls keep it because `dark_theme` now points here.
  let mode = theme.mode;
  Theme::change(mode, None, cx);
}

#[cfg(target_os = "linux")]
mod linux_single_instance {
  use std::io;
  use std::os::unix::net::UnixDatagram;
  use std::path::PathBuf;
  use std::sync::mpsc;
  use std::thread;
  use workspace::AppProfile;

  fn sock_path() -> PathBuf {
    let data_dir = std::env::var_os("XDG_DATA_HOME")
      .map(PathBuf::from)
      .unwrap_or_else(|| {
        let home = std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into());
        PathBuf::from(home).join(".local/share")
      })
      .join(AppProfile::current().storage_dir_name());
    std::fs::create_dir_all(&data_dir).ok();
    data_dir.join("reviu.sock")
  }

  /// If another instance is already listening, forward the URL and return true.
  pub fn try_forward_url(url: &str) -> bool {
    let path = sock_path();
    let Ok(sock) = UnixDatagram::unbound() else {
      return false;
    };
    if sock.connect(&path).is_ok() {
      let _ = sock.send(url.as_bytes());
      return true;
    }
    false
  }

  /// Start listening for URLs from other instances.
  /// Sends received URLs into the provided channel.
  pub fn listen(tx: mpsc::Sender<Vec<String>>) {
    let path = sock_path();
    // Clean up stale socket: try to connect, if refused then the listener is dead.
    if let Ok(probe) = UnixDatagram::unbound() {
      if let Err(e) = probe.connect(&path) {
        if e.kind() == io::ErrorKind::ConnectionRefused {
          std::fs::remove_file(&path).ok();
        }
      } else {
        // Another instance is already listening, should not happen since
        // try_forward_url would have caught this, but be safe.
        return;
      }
    }
    // Remove leftover socket file that has no listener.
    // bind() fails if the file already exists.
    let _ = std::fs::remove_file(&path);
    let Ok(listener) = UnixDatagram::bind(&path) else {
      return;
    };
    thread::spawn(move || {
      let mut buf = [0u8; 2048];
      while let Ok(len) = listener.recv(&mut buf) {
        let url = String::from_utf8_lossy(&buf[..len]).to_string();
        let _ = tx.send(vec![url]);
      }
    });
  }
}

#[cfg(target_os = "windows")]
mod windows_single_instance {
  use std::ffi::OsStr;
  use std::os::windows::ffi::OsStrExt;
  use std::ptr;
  use std::sync::mpsc;
  use std::thread;
  use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_PIPE_CONNECTED, GENERIC_WRITE, GetLastError, INVALID_HANDLE_VALUE,
  };
  use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, OPEN_EXISTING, PIPE_ACCESS_INBOUND, ReadFile, WriteFile,
  };
  use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_MESSAGE,
    PIPE_TYPE_MESSAGE, PIPE_WAIT,
  };
  use workspace::AppProfile;

  const PIPE_BUFFER_SIZE: usize = 4096;

  fn pipe_name() -> Vec<u16> {
    let name = format!(r"\\.\pipe\{}-deeplink", AppProfile::current().url_scheme());
    OsStr::new(&name).encode_wide().chain(Some(0)).collect()
  }

  pub fn try_forward_url(url: &str) -> bool {
    let pipe_name = pipe_name();
    let handle = unsafe {
      CreateFileW(
        pipe_name.as_ptr(),
        GENERIC_WRITE,
        0,
        ptr::null(),
        OPEN_EXISTING,
        FILE_ATTRIBUTE_NORMAL,
        ptr::null_mut(),
      )
    };
    if handle == INVALID_HANDLE_VALUE {
      return false;
    }

    let bytes = url.as_bytes();
    let mut bytes_written = 0;
    let write_result = unsafe {
      WriteFile(
        handle,
        bytes.as_ptr(),
        bytes.len() as u32,
        &mut bytes_written,
        ptr::null_mut(),
      )
    };
    unsafe {
      CloseHandle(handle);
    }

    write_result != 0 && bytes_written as usize == bytes.len()
  }

  pub fn listen(tx: mpsc::Sender<Vec<String>>) {
    let _ = thread::Builder::new()
      .name("WindowsDeeplinkListener".to_string())
      .spawn(move || {
        let pipe_name = pipe_name();
        loop {
          let pipe = unsafe {
            CreateNamedPipeW(
              pipe_name.as_ptr(),
              PIPE_ACCESS_INBOUND,
              PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
              1,
              PIPE_BUFFER_SIZE as u32,
              PIPE_BUFFER_SIZE as u32,
              0,
              ptr::null(),
            )
          };
          if pipe == INVALID_HANDLE_VALUE {
            return;
          }

          let connected = unsafe {
            ConnectNamedPipe(pipe, ptr::null_mut()) != 0 || GetLastError() == ERROR_PIPE_CONNECTED
          };
          if connected {
            let mut buffer = [0_u8; PIPE_BUFFER_SIZE];
            let mut bytes_read = 0;
            let read_result = unsafe {
              ReadFile(
                pipe,
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                &mut bytes_read,
                ptr::null_mut(),
              )
            };
            if read_result != 0 && bytes_read > 0 {
              let url = String::from_utf8_lossy(&buffer[..bytes_read as usize]).to_string();
              let _ = tx.send(vec![url]);
            }
          }

          unsafe {
            DisconnectNamedPipe(pipe);
            CloseHandle(pipe);
          }
        }
      });
  }
}

mod app_root;
const INITIAL_WINDOW_WIDTH: f32 = 1200.0;
const INITIAL_WINDOW_HEIGHT: f32 = 800.0;
#[cfg(target_os = "macos")]
const MACOS_TRAFFIC_LIGHT_X: f32 = 9.0;
#[cfg(target_os = "macos")]
const MACOS_TRAFFIC_LIGHT_Y: f32 = PAGE_HEADER_HEIGHT / 2.0 - 12.0;

#[cfg(target_os = "macos")]
fn macos_titlebar_options() -> TitlebarOptions {
  let mut options = TitleBar::title_bar_options();

  options.title = Some("Reviu".into());
  // Align traffic lights vertically with the 50px global top bar.
  options.traffic_light_position =
    Some(point(px(MACOS_TRAFFIC_LIGHT_X), px(MACOS_TRAFFIC_LIGHT_Y)));
  options
}

#[cfg(unix)]
mod shell_env;

fn main() {
  if workspace::handle_askpass_invocation() {
    return;
  }

  #[cfg(unix)]
  shell_env::handle_printenv_flag();
  #[cfg(unix)]
  shell_env::load();

  let app_profile = AppProfile::current();
  workspace::app_log_sink::install(app_profile);
  // Used only on Linux/Windows (deeplink forwarding); unused on macOS.
  #[allow(unused_variables)]
  let startup_deeplink_url = startup_deeplink_url(app_profile);

  // On Linux, the .desktop file launches a new process for deeplinks.
  // If an instance is already running, forward the URL via Unix socket and exit.
  #[cfg(target_os = "linux")]
  {
    if let Some(url) = startup_deeplink_url.as_deref()
      && linux_single_instance::try_forward_url(url)
    {
      std::process::exit(0);
    }
  }

  // On Windows, protocol handlers launch a new process for deeplinks.
  // Forward the URL to the running instance so the existing window handles it.
  #[cfg(target_os = "windows")]
  {
    if let Some(url) = startup_deeplink_url.as_deref()
      && windows_single_instance::try_forward_url(url)
    {
      std::process::exit(0);
    }
  }

  let (open_url_tx, _open_url_rx) = mpsc::channel::<Vec<String>>();

  // On Linux, listen for deeplinks forwarded from other instances via Unix socket.
  #[cfg(target_os = "linux")]
  linux_single_instance::listen(open_url_tx.clone());

  // On Windows, listen for deeplinks forwarded from other instances via named pipe.
  #[cfg(target_os = "windows")]
  windows_single_instance::listen(open_url_tx.clone());

  // If this instance was launched with a deeplink URL arg (first launch),
  // inject it into the channel so it gets processed once the app is ready.
  #[cfg(any(target_os = "linux", target_os = "windows"))]
  if let Some(url) = startup_deeplink_url.clone() {
    let _ = open_url_tx.send(vec![url]);
  }

  let app = gpui_platform::application().with_assets(AppAssets);
  app.on_open_urls({
    let open_url_tx = open_url_tx.clone();
    move |urls| {
      let _ = open_url_tx.send(urls);
    }
  });

  app.run(move |cx: &mut App| {
    gpui_component::init(cx);
    ui::init(cx);
    install_darcula_theme(cx);
    if let Some(dir) = agent_registry::icon_cache_dir() {
      ui::set_runtime_asset_dir(dir);
    }
    // gpui-component defaults the UI font to `.SystemUIFont`, which only
    // resolves on macOS; elsewhere it falls back to a monospace face. Point
    // the theme at the fonts we bundle (see ui::init) so Linux and Windows
    // render with the same proportional/monospace pair as macOS.
    #[cfg(not(target_os = "macos"))]
    {
      let theme = gpui_component::Theme::global_mut(cx);
      theme.font_family = "Inter".into();
      theme.mono_font_family = "Lilex".into();
    }
    let http_client = ReqwestClient::user_agent("reviu").expect("Failed to create HTTP client");
    cx.set_http_client(Arc::new(http_client));

    let bounds = Bounds::centered(
      None,
      size(px(INITIAL_WINDOW_WIDTH), px(INITIAL_WINDOW_HEIGHT)),
      cx,
    );

    install_app_key_bindings(cx);

    cx.set_menus(build_app_menus());

    let window_options = WindowOptions {
      window_bounds: Some(WindowBounds::Windowed(bounds)),
      #[cfg(target_os = "macos")]
      titlebar: Some(macos_titlebar_options()),
      ..Default::default()
    };

    let window = cx
      .open_window(window_options, |window, cx| {
        let view = cx.new(|cx| WorkspaceView::new(window, cx));
        let app_root = cx.new(|cx| AppRoot::new(view, window, cx));
        window.focus(&app_root.focus_handle(cx), cx);
        cx.new(|cx| Root::new(app_root, window, cx))
      })
      .unwrap();

    window
      .update(cx, move |_, window, cx| {
        window.activate_window();
        cx.activate(true);
      })
      .unwrap();

    // Refresh the agent registry off the UI thread; the cached or embedded
    // copy already backs the picker, so a slow or failed fetch changes nothing.
    cx.background_executor()
      .spawn(async {
        // Icons only need refetching when the document itself moved: their
        // URLs sit under /latest/, so the same URL can serve new artwork.
        let force_icons = match agent_registry::refresh_global_blocking() {
          Ok(outcome) => {
            log::info!("[agent-registry] {outcome:?}");
            outcome == agent_registry::RefreshOutcome::Updated
          }
          Err(err) => {
            log::warn!("[agent-registry] refresh failed, keeping the cached list: {err}");
            false
          }
        };
        let fetched =
          agent_registry::download_icons_blocking(&agent_registry::global(), force_icons);
        if fetched > 0 {
          log::info!("[agent-registry] cached {fetched} agent icons");
        }
      })
      .detach();
  });
}

fn startup_deeplink_url(app_profile: AppProfile) -> Option<String> {
  startup_deeplink_url_from_args(std::env::args(), app_profile.url_scheme())
}

fn startup_deeplink_url_from_args<I, S>(args: I, scheme: &str) -> Option<String>
where
  I: IntoIterator<Item = S>,
  S: AsRef<str>,
{
  let url = args.into_iter().nth(1)?.as_ref().to_string();
  if url
    .strip_prefix(scheme)
    .and_then(|tail| tail.strip_prefix("://"))
    .is_some()
  {
    Some(url)
  } else {
    None
  }
}

#[cfg(test)]
mod tests {
  use super::startup_deeplink_url_from_args;

  #[test]
  fn startup_deeplink_url_reads_current_profile_url_arg() {
    let url = startup_deeplink_url_from_args(
      [
        "reviu".to_string(),
        "reviu://open?url=https%3A%2F%2Fexample.com".to_string(),
      ],
      "reviu",
    );

    assert_eq!(
      url.as_deref(),
      Some("reviu://open?url=https%3A%2F%2Fexample.com")
    );
  }

  #[test]
  fn startup_deeplink_url_rejects_other_args() {
    assert_eq!(
      startup_deeplink_url_from_args(["reviu", "--help"], "reviu"),
      None
    );
    assert_eq!(
      startup_deeplink_url_from_args(["reviu", "reviu-dev://open"], "reviu"),
      None
    );
  }
}
