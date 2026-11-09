#[macro_use]
extern crate rust_i18n;

mod audio;
mod autostart;
mod bridge;
mod capture;
mod config;
mod doctor;
mod i18n;
mod inhibit;
mod input;
mod launcher;
mod protocol;
mod proxy;
mod session;
mod terminal;
mod tray;
mod uninstall;
mod upstream;
mod video;
mod wallpaper;
mod wine;
mod wol;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};

// Locale catalogs are embedded from `locales/` at the crate root.
i18n!("locales", fallback = "en");

/// Native Linux companion for NetEase UU Remote.
///
/// uur runs the official Windows client inside a managed Wine prefix and
/// bridges its input and screen capture onto the real Linux desktop through
/// X11 XTest or the Wayland RemoteDesktop portal.
#[derive(Parser)]
#[command(name = "uur", version, about, arg_required_else_help = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// One-time provisioning: EULA gate, Wine prefix, official client download.
    Setup {
        /// Accept the NetEase UU Remote EULA non-interactively.
        #[arg(long)]
        accept_eula: bool,
        /// Update the existing Wine prefix, retry Wine components, and repair an incomplete client installation.
        #[arg(long)]
        repair: bool,
    },
    /// Print the live environment report (sessions, wine, portals, backends).
    Doctor,
    /// Print the current X11 pointer position (verification helper).
    Pointer,
    /// Capture the desktop through the ScreenCast portal (video path).
    Capture,
    /// Start a full session: capture + bridge + the managed client.
    Run,
    /// Stop every uur-owned process and the managed Wine prefix.
    Stop,
    /// Remove this user's integration and source installation; optionally erase all managed data.
    Uninstall {
        /// Delete the managed Wine prefix, account data, installer cache, configuration and logs.
        #[arg(long)]
        purge: bool,
        /// Show the exact removal plan without stopping processes or changing files.
        #[arg(long)]
        dry_run: bool,
        /// Confirm removal without an interactive prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Manage optional XDG login autostart.
    Autostart {
        #[command(subcommand)]
        action: AutostartAction,
    },
    /// Inspect or configure host Wake-on-LAN.
    Wol {
        #[command(subcommand)]
        action: WolAction,
    },
    /// Select the portal capture source used on Wayland.
    Display {
        #[command(subcommand)]
        action: DisplayAction,
    },
    /// Internal input bridge process. Not part of the public CLI.
    #[command(hide = true, name = "__bridge")]
    InternalBridge,
    /// Internal capture process. Not part of the public CLI.
    #[command(hide = true, name = "__capture")]
    InternalCapture,
    /// Internal native PTY bridge. Not part of the public CLI.
    #[command(hide = true, name = "__terminal")]
    InternalTerminal,
    /// Internal Linux wallpaper synchronizer. Not part of the public CLI.
    #[command(hide = true, name = "__wallpaper")]
    InternalWallpaper,
    /// Internal desktop suspend/idle inhibitor. Not part of the public CLI.
    #[command(hide = true, name = "__inhibit")]
    InternalInhibit,
    /// Internal native desktop tray. Not part of the public CLI.
    #[command(hide = true, name = "__tray")]
    InternalTray,
    /// Internal authenticated XDG application launcher.
    #[command(hide = true, name = "__launcher")]
    InternalLauncher,
    /// Query NetEase's official release feed.
    Upstream {
        #[command(subcommand)]
        action: UpstreamAction,
    },
}

#[derive(Subcommand)]
enum UpstreamAction {
    /// Print the latest official UU Remote release known to NetEase.
    Check,
}

#[derive(Subcommand)]
enum AutostartAction {
    /// Start the managed session after graphical login.
    Enable,
    /// Remove the login autostart entry.
    Disable,
    /// Report whether login autostart is enabled.
    Status,
}

#[derive(Subcommand)]
enum WolAction {
    /// Report the selected physical wired adapter and Magic Packet state.
    Status {
        #[arg(long)]
        interface: Option<String>,
    },
    /// Enable Magic Packet wake for the adapter and its NetworkManager profile.
    Enable {
        #[arg(long)]
        interface: Option<String>,
    },
    /// Disable Magic Packet wake for the adapter and its NetworkManager profile.
    Disable {
        #[arg(long)]
        interface: Option<String>,
    },
}

#[derive(Subcommand)]
enum DisplayAction {
    /// Show the configured source type.
    Status,
    /// Use a monitor, one window, or a portal-provided virtual display.
    Source {
        source: DisplaySource,
        /// Allow selecting several monitors in the next portal consent dialog.
        #[arg(long)]
        multiple: bool,
    },
    /// Choose portal capture, or explicitly enable direct Wayland monitor capture.
    Backend {
        backend: CaptureBackend,
        /// Name from `uur display outputs`; required if more than one output is present.
        #[arg(long)]
        output: Option<String>,
    },
    /// List native Wayland outputs without capturing their content.
    Outputs,
}

#[derive(Clone, ValueEnum)]
enum CaptureBackend {
    Portal,
    Wayland,
}

#[derive(Clone, ValueEnum)]
enum DisplaySource {
    Monitor,
    Window,
    Virtual,
}

impl DisplaySource {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Monitor => "monitor",
            Self::Window => "window",
            Self::Virtual => "virtual",
        }
    }
}

fn main() -> Result<()> {
    // CLI output is routinely piped; a closed pipe is not an error.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    i18n::init();
    let cli = Cli::parse();
    match cli.command {
        Command::Setup {
            accept_eula,
            repair,
        } => wine::setup(accept_eula, repair),
        Command::Doctor => doctor::report(),
        Command::Pointer => bridge::print_pointer(),
        Command::Capture => capture::run_standalone(),
        Command::Run => session::start(),
        Command::Stop => session::stop(),
        Command::Uninstall {
            purge,
            dry_run,
            yes,
        } => uninstall::run(purge, dry_run, yes),
        Command::Autostart { action } => match action {
            AutostartAction::Enable => autostart::enable(),
            AutostartAction::Disable => autostart::disable(),
            AutostartAction::Status => autostart::status(),
        },
        Command::Wol { action } => match action {
            WolAction::Status { interface } => wol::status(interface.as_deref()),
            WolAction::Enable { interface } => wol::configure(interface.as_deref(), true),
            WolAction::Disable { interface } => wol::configure(interface.as_deref(), false),
        },
        Command::Display { action } => {
            if matches!(action, DisplayAction::Outputs) {
                return capture::list_wayland_outputs();
            }
            let config = config::Config::load()?;
            match action {
                DisplayAction::Status => {
                    println!("{}", t!("display.status", source = config.capture_source));
                    println!("capture backend: {}", config.capture_backend);
                    println!("multiple monitors: {}", config.capture_multiple);
                    if let Some(output) = &config.capture_output {
                        println!("output: {output}");
                    }
                    Ok(())
                }
                DisplayAction::Backend { backend, output } => {
                    let backend = match backend {
                        CaptureBackend::Portal => "portal",
                        CaptureBackend::Wayland => "wayland",
                    };
                    let config = config::Config::update(|stored| {
                        stored.capture_backend = backend.into();
                        stored.capture_output = output;
                        if backend == "wayland" {
                            stored.capture_source = "monitor".into();
                            stored.capture_multiple = false;
                        }
                        stored.capture_restore_token = None;
                        stored.remote_desktop_restore_token = None;
                    })?;
                    println!(
                        "{}",
                        if config.capture_backend == "wayland" {
                            t!("capture.direct_enabled").to_string()
                        } else {
                            t!("capture.portal_enabled").to_string()
                        }
                    );
                    Ok(())
                }
                DisplayAction::Outputs => unreachable!(),
                DisplayAction::Source { source, multiple } => {
                    anyhow::ensure!(
                        !multiple || matches!(&source, DisplaySource::Monitor),
                        "--multiple is supported only for monitor sources"
                    );
                    anyhow::ensure!(
                        !multiple || config.capture_backend == "portal",
                        "multi-monitor selection requires the portal capture backend"
                    );
                    config::Config::update(|stored| {
                        stored.capture_source = source.as_str().to_string();
                        stored.capture_multiple = multiple;
                        stored.capture_restore_token = None;
                        stored.remote_desktop_restore_token = None;
                    })?;
                    println!("{}", t!("display.changed", source = source.as_str()));
                    println!(
                        "multiple monitors: {multiple}; selection applies on the next session"
                    );
                    Ok(())
                }
            }
        }
        Command::InternalBridge => bridge::serve(),
        Command::InternalCapture => capture::run(),
        Command::InternalTerminal => terminal::serve(),
        Command::InternalWallpaper => wallpaper::serve(),
        Command::InternalInhibit => inhibit::serve(),
        Command::InternalTray => tray::serve(),
        Command::InternalLauncher => launcher::serve(),
        Command::Upstream {
            action: UpstreamAction::Check,
        } => upstream::check(),
    }
}
