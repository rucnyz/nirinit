use std::{
   collections::HashMap,
   fs,
   io,
   path::{
      Path,
      PathBuf,
   },
   sync::{
      Arc,
      atomic::{
         AtomicBool,
         Ordering,
      },
   },
   thread,
   time::{
      Duration,
      Instant,
   },
};

use anstyle::{
   AnsiColor,
   Color,
   Style,
};
use clap::{
   Parser,
   builder,
   crate_authors,
};
use color_eyre::eyre;
use eyre::{
   Context as _,
   OptionExt as _,
};
use log::{
   debug,
   error,
   info,
   warn,
};
use niri_ipc::{
   Action,
   Reply,
   Request,
   Response,
   SizeChange,
   Window,
   Workspace,
   WorkspaceReferenceArg,
   socket::Socket,
};
use serde::{
   Deserialize,
   Serialize,
};
use signal_hook::{
   consts::TERM_SIGNALS,
   flag,
};
use thiserror::Error;

mod logger;

const APP_NAME: &str = env!("CARGO_PKG_NAME");

const WINDOW_POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, Error)]
pub enum NiriError {
   #[error("Failed to communicate with Niri via IPC: {0}")]
   Reply(String),
   #[error("Failed to connect to Niri's IPC socket: {0}")]
   Connect(io::Error),
   #[error("Failed to send data to Niri's IPC socket: {0}")]
   Send(io::Error),
}

type NiriResult<T> = Result<T, NiriError>;

/// Window data for session persistence
#[derive(Serialize, Deserialize)]
struct SessionWindow<'niri> {
   id:               u64,
   /// The application id of the window, see <https://wayland-book.com/xdg-shell-basics/xdg-toplevel.html>
   app_id:           Option<String>,
   /// The window title (used for extracting project paths for IDEs like PyCharm)
   #[serde(default)]
   title:            Option<String>,
   /// The launch command to spawn this window (mapped from `app_id` via config,
   /// otherwise `app_id` if no mapping exists)
   launch_command:   Option<String>,
   /// Index of the workspace on the corresponding monitor
   workspace_idx:    Option<u8>,
   /// Name of the workspace, in case of a named workspace
   workspace_name:   Option<&'niri str>,
   /// Output the workspace is on
   workspace_output: Option<&'niri str>,
   /// Whether the window is focused or not
   is_focused:       bool,
   /// Window size (width, height) in logical pixels
   /// TODO: Remove [`Option`] in a month
   #[serde(default)]
   window_size:      Option<(i32, i32)>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
   #[serde(default)]
   skip:   Skip,
   /// Map `app_id` to actual launch command (e.g.,
   /// "thorium-discord.com__app-Default" -> "discord-web-app")
   #[serde(default)]
   launch: HashMap<String, String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Skip {
   #[serde(default)]
   apps: Vec<String>,
}

#[derive(Parser)]
#[command(
    author=crate_authors!("\n"),
    styles=get_styles(),
    version,
    about,
    long_about = None,
    help_template = concat!(
        "\n",
        "{before-help}{name} {version}\n",
        "{author-with-newline}\n",
        "{about-with-newline}\n",
        "{usage-heading} {usage}\n",
        "\n",
        "{all-args}{after-help}\n",
        "\n"
    )
)]
struct Args {
   /// Save interval in seconds
   #[arg(long, default_value = "300")]
   save_interval: u64,

   /// Enable debug output
   #[arg(long, short)]
   debug: bool,
}

fn load_config() -> eyre::Result<Config> {
   let config_path = config_file()?;

   let config = fs::read_to_string(&config_path)
      .wrap_err_with(|| format!("The config file doesn't exist at {}", config_path.display()))?;

   Ok(toml::from_str(&config)?)
}

fn niri_windows() -> NiriResult<Vec<Window>> {
   let mut socket = Socket::connect().map_err(NiriError::Connect)?;
   match socket
      .send(Request::Windows)
      .map_err(NiriError::Send)?
      .map_err(NiriError::Reply)?
   {
      Response::Windows(windows) => Ok(windows),
      other => {
         Err(NiriError::Reply(format!(
            "Unexpected response from Niri: {other:?}"
         )))
      },
   }
}

fn niri_workspaces() -> NiriResult<Vec<Workspace>> {
   let mut socket = Socket::connect().map_err(NiriError::Connect)?;
   match socket
      .send(Request::Workspaces)
      .map_err(NiriError::Send)?
      .map_err(NiriError::Reply)?
   {
      Response::Workspaces(workspaces) => Ok(workspaces),
      other => {
         Err(NiriError::Reply(format!(
            "Unexpected response from Niri: {other:?}"
         )))
      },
   }
}

fn data_file() -> eyre::Result<PathBuf> {
   let data_dir = dirs::data_dir()
      .ok_or_eyre("Failed to locate the data directory ($XDG_DATA_HOME)")?
      .join(APP_NAME);
   fs::create_dir_all(&data_dir)
      .wrap_err_with(|| format!("Failed to create data directory: {}", data_dir.display()))?;
   Ok(data_dir.join("session.json"))
}

fn config_file() -> eyre::Result<PathBuf> {
   let config_dir = dirs::config_dir()
      .ok_or_eyre("Failed to locate the config directory ($XDG_CONFIG_HOME)")?
      .join(APP_NAME);
   fs::create_dir_all(&config_dir).wrap_err_with(|| {
      format!(
         "Failed to create config directory: {}",
         config_dir.display()
      )
   })?;
   Ok(config_dir.join("config.toml"))
}

fn find_workspace_for_window<'niri>(
   window: &Window,
   workspaces: &'niri [Workspace],
) -> Option<&'niri Workspace> {
   workspaces
      .iter()
      .find(|w| window.workspace_id == Some(w.id))
}

/// Save the session
fn save_session(file_path: &Path, config: &Config) -> eyre::Result<()> {
   let windows = niri_windows()?;
   let workspaces = niri_workspaces()?;

   let session_windows = windows
      .into_iter()
      .map(|window| {
         let workspace = find_workspace_for_window(&window, &workspaces);

         // Map app_id to launch command if it exists in the config
         let launch_command = window.app_id.as_ref().and_then(|app_id| {
            config
               .launch
               .get(app_id)
               .cloned()
               .or_else(|| Some(app_id.clone()))
         });

         SessionWindow {
            id: window.id,
            app_id: window.app_id,
            title: window.title,
            launch_command,
            workspace_idx: workspace.map(|w| w.idx),
            workspace_name: workspace.and_then(|w| w.name.as_deref()),
            workspace_output: workspace.and_then(|w| w.output.as_deref()),
            is_focused: window.is_focused,
            window_size: Some(window.layout.window_size),
         }
      })
      .collect::<Vec<_>>();

   let json_data = serde_json::to_string_pretty(&session_windows)
      .wrap_err("Failed to serialize session data")?;

   fs::write(file_path, json_data)
      .wrap_err_with(|| format!("Failed to write to session file: {}", file_path.display()))?;
   debug!("saved session to {}", file_path.display());
   Ok(())
}

/// Extract project path from JetBrains IDE window title.
///
/// JetBrains IDEs (PyCharm, IntelliJ, etc.) display the project path in the window title
/// in the format: "project_name [/path/to/project] – current_file.ext"
///
/// This function extracts the path between square brackets and expands `~` to the home directory.
///
/// # Arguments
/// * `title` - The window title from niri IPC
///
/// # Returns
/// * `Some(path)` - The expanded absolute path to the project
/// * `None` - If the title doesn't contain a valid path in brackets
///
/// # Example
/// ```
/// // Input:  "camel [~/projects/camel] – main.py"
/// // Output: Some("/home/user/projects/camel")
/// ```
fn extract_jetbrains_project_path(title: &str) -> Option<String> {
   // Find the path between square brackets: [path]
   let start = title.find('[')?;
   let end = title.find(']')?;

   // Sanity check: start must be before end
   if start >= end {
      return None;
   }

   // Extract the path (excluding the brackets themselves)
   let path = &title[start + 1..end];

   // Expand ~ to the user's home directory
   // e.g., "~/projects" -> "/home/username/projects"
   if path.starts_with('~') {
      dirs::home_dir()
         .map(|home| path.replacen('~', home.to_str().unwrap_or(""), 1))
   } else {
      Some(path.to_string())
   }
}

/// Information extracted from a tmux window title
struct TmuxInfo {
   /// The hostname where tmux is running (could be local or remote)
   hostname: String,
   /// The tmux session name
   session: String,
}

/// Extract hostname and tmux session name from kitty window title.
///
/// When running tmux inside kitty, the window title follows the format set by oh-my-tmux:
/// `hostname ❐ session_name ● window_index program_name`
///
/// For example:
/// - Local:  "YuzhouArch ❐ main ● 1 claude"
/// - Remote: "surfi1 ❐ dt-agent ● 2 zsh"
///
/// The `❐` (U+2750) marks the boundary between hostname and session name.
/// The `●` (U+25CF) marks the end of session name.
///
/// # Arguments
/// * `title` - The window title from niri IPC
///
/// # Returns
/// * `Some(TmuxInfo)` - The hostname and tmux session name
/// * `None` - If the title doesn't match the expected format
///
/// # Example
/// ```
/// // Input:  "surfi1 ❐ dt-agent ● 2 zsh"
/// // Output: Some(TmuxInfo { hostname: "surfi1", session: "dt-agent" })
/// ```
fn extract_tmux_info(title: &str) -> Option<TmuxInfo> {
   // Find the markers: hostname ❐ session ● ...
   let start_marker = '❐';
   let end_marker = '●';

   let start = title.find(start_marker)?;
   let end = title.find(end_marker)?;

   if start >= end {
      return None;
   }

   // Extract hostname (everything before ❐)
   let hostname = title[..start].trim();
   if hostname.is_empty() {
      return None;
   }

   // Extract session name (between ❐ and ●)
   let session = title[start + start_marker.len_utf8()..end].trim();
   if session.is_empty() {
      return None;
   }

   Some(TmuxInfo {
      hostname: hostname.to_string(),
      session: session.to_string(),
   })
}

/// Get the local machine's hostname
fn get_local_hostname() -> Option<String> {
   std::fs::read_to_string("/etc/hostname")
      .ok()
      .map(|s| s.trim().to_string())
}

/// Get Microsoft Edge workspace ID from workspace name.
///
/// Edge stores workspace information in a JSON cache file at:
/// `~/.config/microsoft-edge/Default/Workspaces/WorkspacesCache`
///
/// The window title of an Edge workspace window is the workspace name itself,
/// so we can use it to look up the corresponding workspace ID.
///
/// Edge can then be launched with `--launch-workspace=<ID>` to open that workspace.
///
/// # Arguments
/// * `workspace_name` - The workspace name (from window title)
///
/// # Returns
/// * `Some(id)` - The UUID of the workspace
/// * `None` - If the cache file doesn't exist or workspace not found
///
/// # Example
/// ```
/// // Workspace name: "vllm"
/// // Returns: Some("bbb831b3-9481-4fa9-872e-2b7344417362")
/// ```
fn get_edge_workspace_id(workspace_name: &str) -> Option<String> {
   // Build path to Edge's workspace cache file
   // Location: ~/.config/microsoft-edge/Default/Workspaces/WorkspacesCache
   let cache_path = dirs::config_dir()?
      .join("microsoft-edge/Default/Workspaces/WorkspacesCache");

   // Read and parse the JSON cache file
   let cache_content = fs::read_to_string(&cache_path).ok()?;
   let cache: serde_json::Value = serde_json::from_str(&cache_content).ok()?;

   // Search for a workspace with matching name and return its ID
   // Cache structure: { "workspaces": [{ "name": "...", "id": "..." }, ...] }
   cache["workspaces"]
      .as_array()?
      .iter()
      .find(|ws| ws["name"].as_str() == Some(workspace_name))
      .and_then(|ws| ws["id"].as_str())
      .map(|id| id.to_string())
}

fn spawn_and_move_window<'niri>(
   launch_command: &str,
   app_id: &str,
   title: Option<&str>,
   workspace_idx: Option<u8>,
   workspace_name: Option<&'niri str>,
   workspace_output: Option<&'niri str>,
   window_size: Option<(i32, i32)>,
) -> eyre::Result<()> {
   // Build the launch command with app-specific arguments.
   //
   // Different applications need different handling to restore their state:
   // - JetBrains IDEs: Pass project path as argument (e.g., `pycharm /path/to/project`)
   // - Microsoft Edge: Pass workspace ID (e.g., `edge --launch-workspace=<uuid>`)
   // - Other apps: Just launch without special arguments
   let command = if app_id.starts_with("jetbrains-") {
      // JetBrains IDEs (PyCharm, IntelliJ, WebStorm, etc.)
      // Window title format: "project_name [/path/to/project] – filename"
      // We extract the path and pass it as an argument to open the correct project
      if let Some(title) = title {
         if let Some(project_path) = extract_jetbrains_project_path(title) {
            debug!("extracted project path for {app_id}: {project_path}");
            // Launch with project path: `pycharm /home/user/projects/myproject`
            vec![launch_command.to_owned(), project_path]
         } else {
            // Fallback: launch without project path (will open last project or welcome screen)
            vec![launch_command.to_owned()]
         }
      } else {
         vec![launch_command.to_owned()]
      }
   } else if app_id == "microsoft-edge" {
      // Microsoft Edge with Workspaces feature
      // Window title IS the workspace name (e.g., "vllm", "work", "personal")
      // We look up the workspace ID from Edge's cache and launch with --launch-workspace
      if let Some(workspace_name) = title {
         if let Some(workspace_id) = get_edge_workspace_id(workspace_name) {
            debug!("found Edge workspace ID for '{workspace_name}': {workspace_id}");
            // Launch with workspace: `microsoft-edge-stable --launch-workspace=<uuid>`
            vec![
               launch_command.to_owned(),
               format!("--launch-workspace={}", workspace_id),
            ]
         } else {
            // Workspace not found in cache (maybe deleted or new profile)
            debug!("no Edge workspace found for '{workspace_name}'");
            vec![launch_command.to_owned()]
         }
      } else {
         vec![launch_command.to_owned()]
      }
   } else if app_id == "kitty" {
      // Kitty terminal with tmux session (local or remote via SSH)
      // Window title format: "hostname ❐ session_name ● window_index program_name"
      //
      // Examples:
      // - Local:  "YuzhouArch ❐ main ● 1 zsh"     -> tmux attach -t main
      // - Remote: "surfi1 ❐ dt-agent ● 2 zsh"    -> ssh surfi1 -t tmux attach -t dt-agent
      if let Some(title) = title {
         if let Some(tmux_info) = extract_tmux_info(title) {
            let local_hostname = get_local_hostname();
            let is_local = local_hostname
               .as_ref()
               .map(|h| h.eq_ignore_ascii_case(&tmux_info.hostname))
               .unwrap_or(false);

            if is_local {
               // Local tmux session
               // Wait for session to exist (tmux-resurrect may still be restoring),
               // then attach. If timeout, fall back to choose-tree.
               debug!("found local tmux session: {}", tmux_info.session);
               vec![
                  launch_command.to_owned(),
                  "-e".to_owned(),
                  "sh".to_owned(),
                  "-c".to_owned(),
                  format!(
                     "for i in $(seq 1 20); do tmux has-session -t {} 2>/dev/null && break; sleep 0.5; done; \
                      tmux attach -t {} || tmux new-session \\; choose-tree -s",
                     tmux_info.session, tmux_info.session
                  ),
               ]
            } else {
               // Remote tmux session via SSH
               // Same logic but over SSH
               debug!(
                  "found remote tmux session: {} on host {}",
                  tmux_info.session, tmux_info.hostname
               );
               vec![
                  launch_command.to_owned(),
                  "-e".to_owned(),
                  "ssh".to_owned(),
                  tmux_info.hostname,
                  "-t".to_owned(),
                  format!(
                     "for i in $(seq 1 20); do tmux has-session -t {} 2>/dev/null && break; sleep 0.5; done; \
                      tmux attach -t {} || tmux new-session \\; choose-tree -s",
                     tmux_info.session, tmux_info.session
                  ),
               ]
            }
         } else {
            // No tmux session detected, just launch kitty normally
            vec![launch_command.to_owned()]
         }
      } else {
         vec![launch_command.to_owned()]
      }
   } else {
      // All other applications: just launch with the configured command
      vec![launch_command.to_owned()]
   };

   let mut socket = Socket::connect().wrap_err("Failed to connect to Niri IPC socket")?;

   // Record existing window IDs before spawning, so we can find the NEW window after spawn
   let existing_window_ids: std::collections::HashSet<u64> = niri_windows()?
      .iter()
      .map(|w| w.id)
      .collect();

   let reply = socket
      .send(Request::Action(Action::Spawn { command }))
      .map_err(NiriError::Send)?;

   let Reply::Ok(Response::Handled) = reply else {
      error!("failed to spawn command `{launch_command}`");
      return Ok(());
   };

   // Prioritize named workspaces
   let workspace_reference = if let Some(name) = workspace_name {
      WorkspaceReferenceArg::Name(name.to_owned())
   } else if let Some(idx) = workspace_idx {
      WorkspaceReferenceArg::Index(idx)
   } else {
      return Ok(());
   };

   for _ in 0..20 {
      thread::sleep(WINDOW_POLL_INTERVAL);

      let windows = niri_windows()?;

      // Find a NEW window with matching app_id (not in existing_window_ids)
      let Some(new_window) = windows.iter().find(|w| {
         w.app_id.as_deref() == Some(app_id) && !existing_window_ids.contains(&w.id)
      }) else {
         continue;
      };

      if let Some(output) = workspace_output
         && let Err(err) = socket.send(Request::Action(Action::MoveWindowToMonitor {
            id:     Some(new_window.id),
            output: output.to_owned(),
         }))
      {
         warn!(
            "failed to move window {}: {err}",
            new_window
               .app_id
               .as_ref()
               .map_or_else(String::new, |app_id| format!("(app_id: {app_id})")),
         );
      }

      // Move window to the correct workspace
      // This will automatically create the workspace if it doesn't exist
      socket
         .send(Request::Action(Action::MoveWindowToWorkspace {
            window_id: Some(new_window.id),
            reference: workspace_reference,
            focus:     false,
         }))
         .map_err(NiriError::Send)?
         .map_err(NiriError::Reply)?;

      if let Some((width, height)) = window_size {
         if let Err(err) = socket.send(Request::Action(Action::SetWindowWidth {
            id:     Some(new_window.id),
            change: SizeChange::SetFixed(width),
         })) {
            warn!(
               "failed to restore window width for {}: {err}",
               new_window.app_id.as_deref().unwrap_or("unknown")
            );
         }

         if let Err(err) = socket.send(Request::Action(Action::SetWindowHeight {
            id:     Some(new_window.id),
            change: SizeChange::SetFixed(height),
         })) {
            warn!(
               "failed to restore window height for {}: {err}",
               new_window.app_id.as_deref().unwrap_or("unknown")
            );
         }
      }

      return Ok(());
   }

   warn!("window for `{launch_command}` did not appear within 5s");

   Ok(())
}

fn restore_session(config: &Config, session_path: &Path) -> eyre::Result<()> {
   if !session_path.exists() {
      save_session(session_path, config)?;
      return Ok(());
   }

   info!("restoring previous session");

   let session_data = fs::read_to_string(session_path).wrap_err("Failed to read session file")?;
   if session_data.is_empty() {
      info!("session file at {} is empty", session_path.display());
      return Ok(());
   }

   let windows = serde_json::from_str::<Vec<SessionWindow>>(&session_data)
      .wrap_err("Failed to load session data")?;

   // Sort windows by workspace index to ensure lower-indexed workspaces get
   // created first
   let mut sorted_windows = windows;
   sorted_windows.sort_by_key(|w| (w.workspace_output, w.workspace_idx));

   for window in sorted_windows {
      // Check if the launch command should be skipped
      if let Some(ref launch_command) = window.launch_command {
         if config.skip.apps.contains(launch_command) {
            info!("skipping command: {launch_command}");
            continue;
         }

         if let Some(ref app_id) = window.app_id {
            spawn_and_move_window(
               launch_command,
               app_id,
               window.title.as_deref(),
               window.workspace_idx,
               window.workspace_name,
               window.workspace_output,
               window.window_size,
            )?;
         }
      }
   }

   info!("restored session");
   Ok(())
}

#[must_use]
const fn get_styles() -> builder::Styles {
   builder::Styles::styled()
      .usage(
         Style::new()
            .bold()
            .fg_color(Some(Color::Ansi(AnsiColor::Yellow))),
      )
      .header(
         Style::new()
            .bold()
            .fg_color(Some(Color::Ansi(AnsiColor::Yellow))),
      )
      .literal(Style::new().fg_color(Some(Color::Ansi(AnsiColor::Green))))
      .invalid(
         Style::new()
            .bold()
            .fg_color(Some(Color::Ansi(AnsiColor::Red))),
      )
      .error(
         Style::new()
            .bold()
            .fg_color(Some(Color::Ansi(AnsiColor::Red))),
      )
      .valid(
         Style::new()
            .bold()
            .fg_color(Some(Color::Ansi(AnsiColor::Green))),
      )
      .placeholder(Style::new().fg_color(Some(Color::Ansi(AnsiColor::White))))
}

fn main() -> eyre::Result<()> {
   logger::init();
   color_eyre::install()?;

   let args = Args::parse();

   if args.debug {
      logger::enable_debug();
   }

   let config = load_config().unwrap_or_else(|err| {
      warn!("failed to load config, using default values (reason: {err})");
      Config::default()
   });

   let session_path = data_file()?;
   let term = Arc::new(AtomicBool::new(false));

   for sig in TERM_SIGNALS {
      flag::register(*sig, Arc::clone(&term))?;
   }

   info!("starting nirinit-manager");
   restore_session(&config, &session_path)?;

   info!("starting periodic save (interval: {}s)", args.save_interval);
   let mut last_save = Instant::now();

   while !term.load(Ordering::Relaxed) {
      thread::sleep(Duration::from_millis(100));

      if last_save.elapsed() >= Duration::from_secs(args.save_interval) {
         if let Err(report) = save_session(&session_path, &config) {
            error!("failed to save session: {report}");
         }
         last_save = Instant::now();
      }
   }

   info!("shutting down...");
   if let Err(report) = save_session(&session_path, &config) {
      error!("error saving final session: {report}");
   }
   info!("shutdown complete");
   Ok(())
}
