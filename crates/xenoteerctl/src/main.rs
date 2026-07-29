//! Scriptable diagnostic CLI over the public Rust SDK.

use std::{
    env, fmt, fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::{Serialize, de::DeserializeOwned};
use tokio::io::AsyncWriteExt;
use xenoteer_sdk::{
    ApplicationArgument, ApplicationId, ArtifactContentType, ArtifactRef, ClipboardReadRequest,
    Command, CommandId, ControlLeaseId, Desktop, ElementQueryRequest, ElementSnapshotRequest,
    ElementWaitRequest, ErrorCode, EventStreamItem, EventTopic, KeyboardKeyIdentifier,
    KeyboardNamedKey, Point, PointerClickTarget, PointerLogicalButton, ProcessRef,
    ScreenshotDelivery, ScreenshotRequest, SdkError, ViewerMode, ViewerTicketRequest,
    WindowListRequest, WindowQueryRequest, WindowSnapshotRequest, WindowSnapshotTarget,
    WindowWaitRequest, XenoteerClient,
};

const MAX_INPUT_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Parser)]
#[command(name = "xenoteerctl", version, about)]
struct Cli {
    /// Xenoteer HTTPS origin, or numeric-loopback HTTP for local development.
    #[arg(long, env = "XENOTEER_URL", default_value = "http://127.0.0.1:8080")]
    base_url: String,
    /// Read the bearer from this private regular file.
    #[arg(long, env = "XENOTEER_TOKEN_FILE", conflicts_with = "token_stdin")]
    token_file: Option<PathBuf>,
    /// Read the bearer from stdin; cannot be combined with `--input -`.
    #[arg(long, conflicts_with = "token_file")]
    token_stdin: bool,
    /// Environment variable containing the bearer when no other source is selected.
    #[arg(long, default_value = "XENOTEER_TOKEN")]
    token_env: String,
    /// Machine-readable JSON or readable pretty JSON.
    #[arg(long, value_enum, default_value_t = OutputMode::Human)]
    output: OutputMode,
    /// Emit one compact machine-readable JSON object (alias for `--output json`).
    #[arg(long)]
    json: bool,
    #[command(subcommand)]
    command: TopLevel,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum OutputMode {
    Human,
    Json,
}

#[derive(Debug, Subcommand)]
enum TopLevel {
    /// Print authenticated server, desktop, and capability status.
    Status,
    /// Print the current live capability report.
    Capabilities,
    /// Diagnose connection, protocol, readiness, and required capabilities.
    Doctor(DoctorArgs),
    /// Manage the exclusive controller lease.
    Lease {
        #[command(subcommand)]
        command: LeaseCommand,
    },
    /// Submit, inspect, wait for, or cancel exact command IDs.
    Command {
        #[command(subcommand)]
        command: CommandCommand,
    },
    /// Physical pointer operations requiring an existing lease ID.
    Mouse {
        #[command(subcommand)]
        command: MouseCommand,
    },
    /// Physical keyboard operations requiring an existing lease ID.
    #[command(name = "key", visible_alias = "keyboard")]
    Keyboard {
        #[command(subcommand)]
        command: KeyboardCommand,
    },
    /// Query or wait for observed windows using a typed request JSON document.
    #[command(name = "window", visible_alias = "windows")]
    Windows {
        #[command(subcommand)]
        command: WindowsCommand,
    },
    /// Query or wait for accessible elements using typed request JSON.
    #[command(name = "element", visible_alias = "elements")]
    Elements {
        #[command(subcommand)]
        command: ElementsCommand,
    },
    /// Read the clipboard with an explicit typed request.
    Clipboard {
        #[command(subcommand)]
        command: ClipboardCommand,
    },
    /// Launch, inspect, or terminate registered applications.
    App {
        #[command(subcommand)]
        command: AppCommand,
    },
    /// Capture a screenshot to a private artifact.
    Screenshot(ScreenshotArgs),
    /// Subscribe to filtered events and emit one compact JSON object per line.
    Events {
        #[command(subcommand)]
        command: EventsCommand,
    },
    /// Transfer or delete immutable private artifacts.
    Artifact {
        #[command(subcommand)]
        command: ArtifactCommand,
    },
    /// Issue a one-time view-only ticket bound to an Origin.
    #[command(name = "viewer", visible_alias = "viewer-ticket")]
    ViewerTicket {
        #[command(subcommand)]
        command: ViewerCommand,
    },
}

#[derive(Debug, Args)]
struct DoctorArgs {
    /// Include a one-time viewer ticket issuance probe.
    #[arg(long)]
    viewer: bool,
    /// Exact allowlisted Origin used by the `--viewer` one-time ticket probe.
    #[arg(long, requires = "viewer", default_value = "https://viewer.example")]
    viewer_origin: String,
    /// Require `application.browser.registered`; current base images do not advertise it.
    #[arg(long)]
    browser: bool,
    /// Explicitly perform an owned-input reset probe.
    #[arg(long, requires = "lease_id")]
    input: bool,
    /// Existing caller-owned lease for `--input`; doctor never steals control.
    #[arg(long)]
    lease_id: Option<String>,
}

#[derive(Debug, Subcommand)]
enum LeaseCommand {
    #[command(name = "show", alias = "state")]
    State,
    Acquire {
        #[arg(long)]
        ttl_ms: Option<u32>,
    },
    Renew {
        #[arg(long)]
        lease_id: String,
        #[arg(long)]
        ttl_ms: Option<u32>,
    },
    Release {
        #[arg(long)]
        lease_id: String,
    },
}

#[derive(Debug, Subcommand)]
enum CommandCommand {
    Submit {
        #[command(flatten)]
        input: InputDocument,
        #[command(flatten)]
        mutation: MutationOptions,
    },
    #[command(name = "show", alias = "get")]
    Get(CommandIdentity),
    Wait {
        #[command(flatten)]
        identity: CommandIdentity,
        #[arg(long, default_value_t = 30_000)]
        timeout_ms: u64,
    },
    Cancel(CommandIdentity),
}

#[derive(Debug, Args)]
struct CommandIdentity {
    #[arg(long)]
    command_id: String,
}

#[derive(Debug, Subcommand)]
enum MouseCommand {
    Move {
        #[command(flatten)]
        mutation: MutationOptions,
        #[arg(long)]
        x: i32,
        #[arg(long)]
        y: i32,
        #[arg(long, default_value_t = 250)]
        duration_ms: u32,
    },
    Click {
        #[command(flatten)]
        mutation: MutationOptions,
        #[arg(long)]
        x: i32,
        #[arg(long)]
        y: i32,
        #[arg(long, value_enum, default_value_t = CliButton::Left)]
        button: CliButton,
        #[arg(long, default_value_t = 1)]
        count: u8,
        #[arg(long, default_value_t = 250)]
        duration_ms: u32,
    },
    /// Submit typed `pointer_drag` Command JSON.
    Drag(MutationDocument),
    /// Submit typed `pointer_scroll` Command JSON.
    Scroll(MutationDocument),
    /// Reserved command name; v1 returns an explicit unsupported-capability error.
    Position,
    /// Submit typed `input_reset` Command JSON.
    Reset(MutationDocument),
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliButton {
    Left,
    Middle,
    Right,
    Back,
    Forward,
}

impl From<CliButton> for PointerLogicalButton {
    fn from(value: CliButton) -> Self {
        match value {
            CliButton::Left => Self::Left,
            CliButton::Middle => Self::Middle,
            CliButton::Right => Self::Right,
            CliButton::Back => Self::Back,
            CliButton::Forward => Self::Forward,
        }
    }
}

#[derive(Debug, Subcommand)]
enum KeyboardCommand {
    Press {
        #[command(flatten)]
        mutation: MutationOptions,
        /// Stable named key, for example `enter` or `control_left`.
        #[arg(long)]
        key: String,
        #[arg(long, default_value_t = 0)]
        hold_ms: u16,
    },
    /// Submit typed `keyboard_key_down` Command JSON.
    Down(MutationDocument),
    /// Submit typed `keyboard_key_up` Command JSON.
    Up(MutationDocument),
    /// Submit typed `keyboard_chord` Command JSON.
    Chord(MutationDocument),
    /// Submit typed `text_insert` Command JSON.
    Text(MutationDocument),
    /// Submit typed `input_reset` Command JSON.
    Reset(MutationDocument),
}

#[derive(Debug, Args)]
struct MutationOptions {
    /// Existing caller-owned lease capability.
    #[arg(long, conflicts_with = "with_lease")]
    lease_id: Option<String>,
    /// Explicitly acquire a short lease, wait for the command, then release it.
    #[arg(long, conflicts_with = "lease_id")]
    with_lease: bool,
    /// Caller-owned deduplication ID for safe recovery/retry.
    #[arg(long)]
    command_id: Option<String>,
}

#[derive(Debug, Subcommand)]
enum WindowsCommand {
    List(InputDocument),
    #[command(name = "find", visible_alias = "query")]
    Query(InputDocument),
    Show(InputDocument),
    Activate(MutationDocument),
    Close(MutationDocument),
    Move(MutationDocument),
    Resize(MutationDocument),
    State(MutationDocument),
    Capture(InputDocument),
    Wait(InputDocument),
}

#[derive(Debug, Subcommand)]
enum ElementsCommand {
    Query(InputDocument),
    Show(InputDocument),
    Invoke(MutationDocument),
    Click(MutationDocument),
    Focus(MutationDocument),
    Text(MutationDocument),
    Value(MutationDocument),
    Wait(InputDocument),
}

#[derive(Debug, Subcommand)]
enum ClipboardCommand {
    #[command(name = "get", visible_alias = "read")]
    Read(InputDocument),
    Set(MutationDocument),
    Clear(MutationDocument),
    Paste(MutationDocument),
}

#[derive(Debug, Subcommand)]
enum AppCommand {
    /// Reserved command name; v1 returns an explicit unsupported-capability error.
    List,
    Launch {
        #[command(flatten)]
        mutation: MutationOptions,
        #[arg(long)]
        profile: String,
        #[arg(long = "arg")]
        arguments: Vec<String>,
    },
    Status {
        #[command(flatten)]
        process: InputDocument,
        #[command(flatten)]
        mutation: MutationOptions,
    },
    Terminate {
        #[command(flatten)]
        process: InputDocument,
        #[command(flatten)]
        mutation: MutationOptions,
        #[arg(long)]
        grace_ms: Option<u32>,
    },
    /// Reserved command name; v1 returns an explicit unsupported-capability error.
    Logs,
}

#[derive(Debug, Args)]
struct InputDocument {
    /// Typed request/Command JSON file, or `-` for bounded stdin.
    #[arg(long, value_name = "PATH")]
    input: PathBuf,
}

#[derive(Debug, Args)]
struct MutationDocument {
    #[command(flatten)]
    input: InputDocument,
    #[command(flatten)]
    mutation: MutationOptions,
}

#[derive(Debug, Args)]
struct ScreenshotArgs {
    #[command(flatten)]
    request: InputDocument,
    /// New file to receive the verified screenshot bytes.
    #[arg(long)]
    output: PathBuf,
}

#[derive(Debug, Args)]
struct EventsArgs {
    /// Exact topic filter; repeat for multiple topics. Omit for all authorized topics.
    #[arg(long = "topic")]
    topics: Vec<String>,
    /// Last globally processed sequence; replay starts strictly after it.
    #[arg(long)]
    since_sequence: Option<u64>,
    /// Exit successfully after this many observations.
    #[arg(long)]
    count: Option<u64>,
    /// Exit successfully after this overall local duration.
    #[arg(long)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Subcommand)]
enum EventsCommand {
    Watch(EventsArgs),
}

#[derive(Debug, Subcommand)]
enum ViewerCommand {
    #[command(name = "url", alias = "ticket")]
    Url {
        #[arg(long)]
        origin: String,
    },
}

#[derive(Debug, Subcommand)]
enum ArtifactCommand {
    /// Upload one bounded clipboard-input object.
    Upload {
        /// Bounded regular file containing raw bytes.
        #[arg(long)]
        input: PathBuf,
        /// Valid HTTP media type for the immutable object.
        #[arg(long)]
        content_type: String,
    },
    /// Download and verify one artifact reference into a new file.
    Download {
        #[command(flatten)]
        artifact: InputDocument,
        #[arg(long)]
        output: PathBuf,
    },
    /// Delete one exact artifact reference.
    Delete {
        #[command(flatten)]
        artifact: InputDocument,
    },
}

#[derive(Debug)]
enum CliError {
    Sdk(SdkError),
    Input(String),
    Io(io::Error),
    DoctorFailed,
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sdk(error) => error.fmt(formatter),
            Self::Input(error) => formatter.write_str(error),
            Self::Io(error) => write!(formatter, "local I/O failed ({:?})", error.kind()),
            Self::DoctorFailed => formatter.write_str("one or more doctor checks failed"),
        }
    }
}

impl From<SdkError> for CliError {
    fn from(value: SdkError) -> Self {
        Self::Sdk(value)
    }
}

impl From<io::Error> for CliError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("xenoteerctl: {error}");
            ExitCode::from(exit_code(&error))
        }
    }
}

async fn run(cli: Cli) -> Result<(), CliError> {
    let output_mode = if cli.json {
        OutputMode::Json
    } else {
        cli.output
    };
    let token = load_token(&cli)?;
    let client = XenoteerClient::connect(&cli.base_url, token).await?;

    match cli.command {
        TopLevel::Status => output(output_mode, client.status())?,
        TopLevel::Capabilities => output(output_mode, &client.status().capabilities)?,
        TopLevel::Doctor(arguments) => doctor(output_mode, &client, arguments).await?,
        command => {
            let desktop = client.desktop()?;
            match command {
                TopLevel::Lease { command } => match command {
                    LeaseCommand::State => output(output_mode, &desktop.control_state().await?)?,
                    LeaseCommand::Acquire { ttl_ms } => {
                        let lease = desktop.acquire_control(ttl_ms).await?;
                        output(
                            output_mode,
                            &serde_json::json!({
                                "lease_id": lease.id(),
                                "expires_at": lease.expires_at(),
                                "release_required": true
                            }),
                        )?
                    }
                    LeaseCommand::Renew { lease_id, ttl_ms } => output(
                        output_mode,
                        &desktop
                            .renew_control(parse_wire(&lease_id)?, ttl_ms)
                            .await?,
                    )?,
                    LeaseCommand::Release { lease_id } => output(
                        output_mode,
                        &desktop.release_control(parse_wire(&lease_id)?).await?,
                    )?,
                },
                TopLevel::Command { command } => match command {
                    CommandCommand::Submit { input, mutation } => {
                        let command: Command = read_json(&input.input, cli.token_stdin)?;
                        let handle = submit_mutation(&desktop, mutation, command).await?;
                        output(output_mode, handle.latest())?
                    }
                    CommandCommand::Get(identity) => {
                        let handle = desktop.command(parse_wire(&identity.command_id)?).await?;
                        output(output_mode, handle.latest())?
                    }
                    CommandCommand::Wait {
                        identity,
                        timeout_ms,
                    } => {
                        let mut handle = desktop.command(parse_wire(&identity.command_id)?).await?;
                        handle
                            .wait_terminal(Duration::from_millis(timeout_ms))
                            .await?;
                        output(output_mode, handle.latest())?
                    }
                    CommandCommand::Cancel(identity) => {
                        let mut handle = desktop.command(parse_wire(&identity.command_id)?).await?;
                        handle.cancel().await?;
                        output(output_mode, handle.latest())?
                    }
                },
                TopLevel::Mouse { command } => {
                    let (lease_id, command) = match command {
                        MouseCommand::Move {
                            mutation,
                            x,
                            y,
                            duration_ms,
                        } => (
                            mutation,
                            Command::PointerMove(xenoteer_sdk::PointerMoveCommand {
                                target: Point::new(x, y),
                                duration_ms: Some(duration_ms),
                                curve: xenoteer_sdk::PointerCurve::Smooth,
                            }),
                        ),
                        MouseCommand::Click {
                            mutation,
                            x,
                            y,
                            button,
                            count,
                            duration_ms,
                        } => (
                            mutation,
                            Command::PointerClick(xenoteer_sdk::PointerClickCommand {
                                target: PointerClickTarget::Root {
                                    point: Point::new(x, y),
                                },
                                button: button.into(),
                                count,
                                duration_ms: Some(duration_ms),
                                curve: xenoteer_sdk::PointerCurve::Smooth,
                                pre_click_dwell_ms: 0,
                                press_duration_ms: 0,
                                inter_click_interval_ms: 100,
                            }),
                        ),
                        MouseCommand::Drag(document) => {
                            read_command_document(document, cli.token_stdin, |command| {
                                matches!(command, Command::PointerDrag(_))
                            })?
                        }
                        MouseCommand::Scroll(document) => {
                            read_command_document(document, cli.token_stdin, |command| {
                                matches!(command, Command::PointerScroll(_))
                            })?
                        }
                        MouseCommand::Position => {
                            return Err(unsupported_v1("pointer-position observation"));
                        }
                        MouseCommand::Reset(document) => {
                            read_command_document(document, cli.token_stdin, |command| {
                                matches!(command, Command::InputReset(_))
                            })?
                        }
                    };
                    let handle = submit_mutation(&desktop, lease_id, command).await?;
                    output(output_mode, handle.latest())?
                }
                TopLevel::Keyboard { command } => match command {
                    KeyboardCommand::Press {
                        mutation,
                        key,
                        hold_ms,
                    } => {
                        let named: KeyboardNamedKey = parse_enum(&key)?;
                        let command = Command::KeyboardPress(xenoteer_sdk::KeyboardPressCommand {
                            key: KeyboardKeyIdentifier::Named { name: named },
                            hold_ms,
                        });
                        let handle = submit_mutation(&desktop, mutation, command).await?;
                        output(output_mode, handle.latest())?
                    }
                    KeyboardCommand::Down(document) => {
                        run_command_document(
                            &desktop,
                            document,
                            cli.token_stdin,
                            |command| matches!(command, Command::KeyboardKeyDown(_)),
                            output_mode,
                        )
                        .await?
                    }
                    KeyboardCommand::Up(document) => {
                        run_command_document(
                            &desktop,
                            document,
                            cli.token_stdin,
                            |command| matches!(command, Command::KeyboardKeyUp(_)),
                            output_mode,
                        )
                        .await?
                    }
                    KeyboardCommand::Chord(document) => {
                        run_command_document(
                            &desktop,
                            document,
                            cli.token_stdin,
                            |command| matches!(command, Command::KeyboardChord(_)),
                            output_mode,
                        )
                        .await?
                    }
                    KeyboardCommand::Text(document) => {
                        run_command_document(
                            &desktop,
                            document,
                            cli.token_stdin,
                            |command| matches!(command, Command::TextInsert(_)),
                            output_mode,
                        )
                        .await?
                    }
                    KeyboardCommand::Reset(document) => {
                        run_command_document(
                            &desktop,
                            document,
                            cli.token_stdin,
                            |command| matches!(command, Command::InputReset(_)),
                            output_mode,
                        )
                        .await?
                    }
                },
                TopLevel::Windows { command } => match command {
                    WindowsCommand::List(input) => {
                        let request: WindowListRequest = read_json(&input.input, cli.token_stdin)?;
                        if request.desktop_id != desktop.id()
                            || request.desktop_generation != desktop.generation()
                        {
                            return Err(CliError::Input(
                                "window request belongs to another desktop generation".to_owned(),
                            ));
                        }
                        output(
                            output_mode,
                            &desktop
                                .windows()
                                .list(request.limit, request.order, request.cursor.as_ref())
                                .await?,
                        )?
                    }
                    WindowsCommand::Query(input) => {
                        let request: WindowQueryRequest = read_json(&input.input, cli.token_stdin)?;
                        output(output_mode, &desktop.windows().query(&request).await?)?
                    }
                    WindowsCommand::Show(input) => {
                        let request: WindowSnapshotRequest =
                            read_json(&input.input, cli.token_stdin)?;
                        if request.desktop_id != desktop.id()
                            || request.desktop_generation != desktop.generation()
                        {
                            return Err(CliError::Input(
                                "window request belongs to another desktop generation".to_owned(),
                            ));
                        }
                        let WindowSnapshotTarget::Token { token } = request.target else {
                            return Err(CliError::Input(
                                "window show requires a server-issued token target".to_owned(),
                            ));
                        };
                        output(output_mode, &desktop.windows().snapshot(&token).await?)?
                    }
                    WindowsCommand::Activate(document) => {
                        run_command_document(
                            &desktop,
                            document,
                            cli.token_stdin,
                            |command| matches!(command, Command::WindowActivate(_)),
                            output_mode,
                        )
                        .await?
                    }
                    WindowsCommand::Close(document) => {
                        run_command_document(
                            &desktop,
                            document,
                            cli.token_stdin,
                            |command| matches!(command, Command::WindowClose(_)),
                            output_mode,
                        )
                        .await?
                    }
                    WindowsCommand::Move(document) | WindowsCommand::Resize(document) => {
                        run_command_document(
                            &desktop,
                            document,
                            cli.token_stdin,
                            |command| matches!(command, Command::WindowMoveResize(_)),
                            output_mode,
                        )
                        .await?
                    }
                    WindowsCommand::State(document) => {
                        run_command_document(
                            &desktop,
                            document,
                            cli.token_stdin,
                            |command| {
                                matches!(
                                    command,
                                    Command::WindowSetState(_)
                                        | Command::WindowMinimize(_)
                                        | Command::WindowMoveToWorkspace(_)
                                        | Command::WindowStack(_)
                                )
                            },
                            output_mode,
                        )
                        .await?
                    }
                    WindowsCommand::Capture(input) => {
                        let request: ScreenshotRequest = read_json(&input.input, cli.token_stdin)?;
                        output(output_mode, &desktop.capture().screenshot(&request).await?)?
                    }
                    WindowsCommand::Wait(input) => {
                        let request: WindowWaitRequest = read_json(&input.input, cli.token_stdin)?;
                        output(output_mode, &desktop.windows().wait(&request).await?)?
                    }
                },
                TopLevel::Elements { command } => match command {
                    ElementsCommand::Query(input) => {
                        let request: ElementQueryRequest =
                            read_json(&input.input, cli.token_stdin)?;
                        output(output_mode, &desktop.accessibility().query(&request).await?)?
                    }
                    ElementsCommand::Show(input) => {
                        let request: ElementSnapshotRequest =
                            read_json(&input.input, cli.token_stdin)?;
                        output(
                            output_mode,
                            &desktop.accessibility().snapshot(&request).await?,
                        )?
                    }
                    ElementsCommand::Invoke(document) => {
                        run_command_document(
                            &desktop,
                            document,
                            cli.token_stdin,
                            |command| matches!(command, Command::ElementInvoke(_)),
                            output_mode,
                        )
                        .await?
                    }
                    ElementsCommand::Click(document) => {
                        run_command_document(
                            &desktop,
                            document,
                            cli.token_stdin,
                            |command| matches!(command, Command::ElementPhysicalClick(_)),
                            output_mode,
                        )
                        .await?
                    }
                    ElementsCommand::Focus(document) => {
                        run_command_document(
                            &desktop,
                            document,
                            cli.token_stdin,
                            |command| matches!(command, Command::ElementFocus(_)),
                            output_mode,
                        )
                        .await?
                    }
                    ElementsCommand::Text(document) => {
                        run_command_document(
                            &desktop,
                            document,
                            cli.token_stdin,
                            |command| {
                                matches!(
                                    command,
                                    Command::ElementSetText(_) | Command::ElementInsertText(_)
                                )
                            },
                            output_mode,
                        )
                        .await?
                    }
                    ElementsCommand::Value(document) => {
                        run_command_document(
                            &desktop,
                            document,
                            cli.token_stdin,
                            |command| {
                                matches!(
                                    command,
                                    Command::ElementSetValue(_) | Command::ElementSelection(_)
                                )
                            },
                            output_mode,
                        )
                        .await?
                    }
                    ElementsCommand::Wait(input) => {
                        let request: ElementWaitRequest = read_json(&input.input, cli.token_stdin)?;
                        output(output_mode, &desktop.accessibility().wait(&request).await?)?
                    }
                },
                TopLevel::Clipboard { command } => match command {
                    ClipboardCommand::Read(input) => {
                        let request: ClipboardReadRequest =
                            read_json(&input.input, cli.token_stdin)?;
                        output(output_mode, &desktop.clipboard().read(&request).await?)?
                    }
                    ClipboardCommand::Set(document) => {
                        run_command_document(
                            &desktop,
                            document,
                            cli.token_stdin,
                            |command| matches!(command, Command::SelectionSet(_)),
                            output_mode,
                        )
                        .await?
                    }
                    ClipboardCommand::Clear(document) => {
                        run_command_document(
                            &desktop,
                            document,
                            cli.token_stdin,
                            |command| matches!(command, Command::SelectionClear(_)),
                            output_mode,
                        )
                        .await?
                    }
                    ClipboardCommand::Paste(document) => {
                        run_command_document(
                            &desktop,
                            document,
                            cli.token_stdin,
                            |command| matches!(command, Command::TextInsert(_)),
                            output_mode,
                        )
                        .await?
                    }
                },
                TopLevel::App { command } => match command {
                    AppCommand::List => {
                        return Err(unsupported_v1("registered-application listing"));
                    }
                    AppCommand::Logs => {
                        return Err(unsupported_v1("managed-process log streaming"));
                    }
                    AppCommand::Launch {
                        mutation,
                        profile,
                        arguments,
                    } => {
                        let profile = ApplicationId::new(profile).map_err(|_| {
                            CliError::Input("invalid application profile".to_owned())
                        })?;
                        let arguments = arguments
                            .into_iter()
                            .map(ApplicationArgument::new)
                            .collect::<Result<Vec<_>, _>>()
                            .map_err(|_| {
                                CliError::Input("invalid application argument".to_owned())
                            })?;
                        let handle = submit_mutation(
                            &desktop,
                            mutation,
                            Command::ApplicationLaunch(xenoteer_sdk::ApplicationLaunchCommand {
                                application: profile,
                                arguments,
                            }),
                        )
                        .await?;
                        output(output_mode, handle.latest())?
                    }
                    AppCommand::Status { process, mutation } => {
                        let process: ProcessRef = read_json(&process.input, cli.token_stdin)?;
                        let handle = submit_mutation(
                            &desktop,
                            mutation,
                            Command::ProcessStatus(xenoteer_sdk::ProcessStatusCommand { process }),
                        )
                        .await?;
                        output(output_mode, handle.latest())?
                    }
                    AppCommand::Terminate {
                        process,
                        mutation,
                        grace_ms,
                    } => {
                        let process: ProcessRef = read_json(&process.input, cli.token_stdin)?;
                        let handle = submit_mutation(
                            &desktop,
                            mutation,
                            Command::ProcessTerminate(xenoteer_sdk::ProcessTerminateCommand {
                                process,
                                grace_ms,
                            }),
                        )
                        .await?;
                        output(output_mode, handle.latest())?
                    }
                },
                TopLevel::Screenshot(arguments) => {
                    let request: ScreenshotRequest =
                        read_json(&arguments.request.input, cli.token_stdin)?;
                    let result = desktop.capture().screenshot(&request).await?;
                    let ScreenshotDelivery::Artifact { artifact } = &result.delivery else {
                        return Err(CliError::Input(
                            "server selected inline screenshot delivery without a binary body"
                                .to_owned(),
                        ));
                    };
                    if arguments.output == Path::new("-") {
                        let mut stdout = tokio::io::stdout();
                        desktop
                            .artifacts()
                            .download_to(artifact, &mut stdout)
                            .await?;
                        stdout.flush().await?;
                    } else {
                        download_to_new_file(&desktop, artifact, &arguments.output).await?;
                        output(output_mode, &result)?;
                    }
                }
                TopLevel::Events {
                    command: EventsCommand::Watch(arguments),
                } => {
                    stream_events(&desktop, arguments).await?;
                }
                TopLevel::Artifact { command } => match command {
                    ArtifactCommand::Upload {
                        input,
                        content_type,
                    } => {
                        let content_type =
                            ArtifactContentType::new(content_type).map_err(|_| {
                                CliError::Input("invalid artifact content type".to_owned())
                            })?;
                        let (body, content_length) =
                            open_regular_file(&input, xenoteer_sdk::MAX_CLIPBOARD_ARTIFACT_BYTES)?;
                        let artifact = desktop
                            .artifacts()
                            .upload_clipboard_input_stream(content_type, content_length, body)
                            .await?;
                        output(output_mode, &artifact)?
                    }
                    ArtifactCommand::Download {
                        artifact,
                        output: path,
                    } => {
                        let artifact: ArtifactRef = read_json(&artifact.input, cli.token_stdin)?;
                        download_to_new_file(&desktop, &artifact, &path).await?;
                        output(
                            output_mode,
                            &serde_json::json!({
                                "artifact_id": artifact.artifact_id,
                                "content_length": artifact.content_length.to_string(),
                                "output": path
                            }),
                        )?
                    }
                    ArtifactCommand::Delete { artifact } => {
                        let artifact: ArtifactRef = read_json(&artifact.input, cli.token_stdin)?;
                        desktop.artifacts().delete(&artifact).await?;
                        output(
                            output_mode,
                            &serde_json::json!({"deleted": true, "artifact_id": artifact.artifact_id}),
                        )?
                    }
                },
                TopLevel::ViewerTicket {
                    command: ViewerCommand::Url { origin },
                } => {
                    let request = ViewerTicketRequest {
                        desktop_id: desktop.id(),
                        desktop_generation: desktop.generation(),
                        mode: ViewerMode::ViewOnly,
                    };
                    output(
                        output_mode,
                        &desktop.viewer().ticket(&origin, &request).await?,
                    )?
                }
                TopLevel::Status | TopLevel::Capabilities | TopLevel::Doctor(_) => {
                    return Err(CliError::Input("invalid command dispatch".to_owned()));
                }
            };
        }
    };
    client.close().await;
    Ok(())
}

async fn submit_mutation(
    desktop: &Desktop,
    options: MutationOptions,
    command: Command,
) -> Result<xenoteer_sdk::CommandHandle, CliError> {
    if command.requires_control_lease() && options.lease_id.is_none() && !options.with_lease {
        return Err(CliError::Input(
            "this command requires --lease-id or explicit --with-lease".to_owned(),
        ));
    }
    let command_id = options
        .command_id
        .as_deref()
        .map(parse_wire)
        .transpose()?
        .unwrap_or_else(CommandId::new);
    // This identity is visible before lease acquisition or command network I/O.
    eprintln!("xenoteerctl: command_id={command_id}");
    if options.with_lease {
        let mut lease = desktop.acquire_control(Some(60_000)).await?;
        // The ephemeral capability is observable before any command effect and
        // before the mandatory release attempt, just like the command ID.
        eprintln!("xenoteerctl: lease_id={}", lease.id());
        let command_result = async {
            let mut handle = lease.submit_with(command_id, None, command).await?;
            handle.wait_terminal(Duration::from_secs(60)).await?;
            Ok::<_, SdkError>(handle)
        }
        .await;
        // Scoped acquisition always attempts explicit release, including after
        // ambiguous submission or local wait failure. A release failure wins
        // because the caller must know the lease capability may still be live.
        lease.release().await?;
        return Ok(command_result?);
    }
    let lease_id = options.lease_id.as_deref().map(parse_wire).transpose()?;
    Ok(desktop
        .submit_with(command_id, lease_id, None, command)
        .await?)
}

fn unsupported_v1(capability: &str) -> CliError {
    CliError::Input(format!(
        "{capability} is not supported by the negotiated v1 protocol"
    ))
}

fn read_command_document(
    document: MutationDocument,
    token_stdin: bool,
    expected: impl FnOnce(&Command) -> bool,
) -> Result<(MutationOptions, Command), CliError> {
    let command: Command = read_json(&document.input.input, token_stdin)?;
    if !expected(&command) {
        return Err(CliError::Input(
            "typed command JSON does not match the selected subcommand".to_owned(),
        ));
    }
    Ok((document.mutation, command))
}

async fn run_command_document(
    desktop: &Desktop,
    document: MutationDocument,
    token_stdin: bool,
    expected: impl FnOnce(&Command) -> bool,
    output_mode: OutputMode,
) -> Result<(), CliError> {
    let (options, command) = read_command_document(document, token_stdin, expected)?;
    let handle = submit_mutation(desktop, options, command).await?;
    output(output_mode, handle.latest())
}

async fn stream_events(desktop: &Desktop, arguments: EventsArgs) -> Result<(), CliError> {
    if arguments.count == Some(0) || arguments.count.is_some_and(|count| count > 1_000_000) {
        return Err(CliError::Input(
            "event count must be between 1 and 1000000".to_owned(),
        ));
    }
    let timeout = match arguments.timeout_ms {
        Some(0) => {
            return Err(CliError::Input(
                "event timeout must be greater than zero".to_owned(),
            ));
        }
        Some(milliseconds) if milliseconds > 86_400_000 => {
            return Err(CliError::Input(
                "event timeout must not exceed 24 hours".to_owned(),
            ));
        }
        Some(milliseconds) => Some(Duration::from_millis(milliseconds)),
        None => None,
    };
    let topics = arguments
        .topics
        .into_iter()
        .map(EventTopic::new)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| CliError::Input("invalid event topic".to_owned()))?;
    let mut events = desktop.events(topics, arguments.since_sequence).await?;
    let deadline = timeout.map(|timeout| tokio::time::Instant::now() + timeout);
    let mut observed = 0_u64;
    loop {
        let next = if let Some(deadline) = deadline {
            match tokio::time::timeout_at(deadline, events.next()).await {
                Ok(next) => next,
                Err(_) => return Ok(()),
            }
        } else {
            events.next().await
        };
        let item = next.ok_or(SdkError::Transport)?;
        let mut terminal_error = None;
        let value = match item {
            EventStreamItem::Event(event) => {
                serde_json::json!({"kind": "event", "event": event})
            }
            EventStreamItem::ResyncRequired {
                reason,
                dropped_through,
                latest_sequence,
            } => {
                terminal_error = Some(CliError::Input(
                    "event continuity was lost; refresh snapshots before subscribing again"
                        .to_owned(),
                ));
                serde_json::json!({
                    "kind": "resync_required",
                    "reason": reason,
                    "dropped_through": dropped_through.map(|value| value.to_string()),
                    "latest_sequence": latest_sequence.map(|value| value.to_string())
                })
            }
            EventStreamItem::UnknownMessage { message_type, raw } => serde_json::json!({
                "kind": "unknown_message",
                "message_type": message_type,
                "raw": raw
            }),
            EventStreamItem::MalformedKnownMessage { message_type } => serde_json::json!({
                "kind": "malformed_known_message",
                "message_type": message_type
            }),
            EventStreamItem::ServerError {
                request_id,
                code,
                detail,
            } => {
                terminal_error = Some(CliError::Sdk(SdkError::EventRejected {
                    code,
                    detail: detail.clone(),
                }));
                serde_json::json!({
                    "kind": "server_error",
                    "request_id": request_id,
                    "code": code,
                    "detail": detail
                })
            }
            EventStreamItem::Closed { reason } => {
                terminal_error = Some(CliError::Input(format!("event stream closed: {reason:?}")));
                serde_json::json!({"kind": "closed", "reason": format!("{reason:?}")})
            }
            _ => serde_json::json!({"kind": "unknown_sdk_item"}),
        };
        let line = serde_json::to_string(&value)
            .map_err(|_| CliError::Input("event serialization failed".to_owned()))?;
        {
            let mut stdout = io::stdout().lock();
            writeln!(stdout, "{line}")?;
            stdout.flush()?;
        }
        if let Some(error) = terminal_error {
            return Err(error);
        }
        observed = observed.saturating_add(1);
        if arguments.count.is_some_and(|count| observed >= count) {
            return Ok(());
        }
    }
}

async fn download_to_new_file(
    desktop: &Desktop,
    artifact: &ArtifactRef,
    path: &Path,
) -> Result<(), CliError> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    let mut file = tokio::fs::File::from_std(file);
    let result = desktop.artifacts().download_to(artifact, &mut file).await;
    if let Err(error) = result {
        drop(file);
        let _cleanup = tokio::fs::remove_file(path).await;
        return Err(error.into());
    }
    file.shutdown().await?;
    file.sync_all().await?;
    Ok(())
}

async fn doctor(
    mode: OutputMode,
    client: &XenoteerClient,
    arguments: DoctorArgs,
) -> Result<(), CliError> {
    let status = client.status();
    let mut ready = matches!(
        status.desktop.state,
        xenoteer_sdk::DesktopState::Ready | xenoteer_sdk::DesktopState::Degraded
    );
    let unavailable = status
        .capabilities
        .capabilities()
        .iter()
        .filter(|capability| {
            !matches!(
                capability.status(),
                xenoteer_sdk::CapabilityStatus::Available
            )
        })
        .map(|capability| capability.id().as_str())
        .collect::<Vec<_>>();
    let mut performed = vec![
        "authentication",
        "status_shape",
        "protocol_overlap",
        "desktop_generation",
        "capability_report",
    ];
    if arguments.viewer {
        performed.push("viewer_ticket_probe");
    }
    if arguments.browser {
        performed.push("browser_registered_capability");
    }
    let mut viewer_ticket_issued = false;
    if arguments.viewer {
        let desktop = client.desktop()?;
        let request = ViewerTicketRequest {
            desktop_id: desktop.id(),
            desktop_generation: desktop.generation(),
            mode: ViewerMode::ViewOnly,
        };
        desktop
            .viewer()
            .ticket(&arguments.viewer_origin, &request)
            .await?;
        viewer_ticket_issued = true;
    }
    let browser_registered_available =
        status.capabilities.capabilities().iter().any(|capability| {
            capability.id().as_str() == "application.browser.registered"
                && matches!(
                    capability.status(),
                    xenoteer_sdk::CapabilityStatus::Available
                )
        });
    if arguments.browser {
        ready &= browser_registered_available;
    }
    let mut input_command_id = None;
    if arguments.input {
        let desktop = client.desktop()?;
        let lease_id = arguments
            .lease_id
            .as_deref()
            .ok_or_else(|| CliError::Input("--input requires --lease-id".to_owned()))
            .and_then(parse_wire::<ControlLeaseId>)?;
        let command_id = CommandId::new();
        eprintln!("xenoteerctl: command_id={command_id}");
        let mut handle = desktop
            .submit_with(
                command_id,
                Some(lease_id),
                None,
                Command::InputReset(xenoteer_sdk::InputResetCommand {}),
            )
            .await?;
        handle.wait_terminal(Duration::from_secs(10)).await?;
        ready &= handle.latest().lifecycle().is_terminal();
        input_command_id = Some(command_id);
        performed.push("owned_input_reset");
    }
    output(
        mode,
        &serde_json::json!({
            "ok": ready,
            "protocol": client.negotiated_protocol(),
            "desktop_state": status.desktop.state,
            "desktop_generation": status.desktop.generation,
            "non_available_capabilities": unavailable,
            "checks": performed,
            "requested": {
                "viewer": arguments.viewer,
                "browser": arguments.browser,
                "input": arguments.input
            },
            "viewer_ticket_issued": viewer_ticket_issued,
            "browser_registered_available": browser_registered_available,
            "input_command_id": input_command_id
        }),
    )?;
    if !ready {
        return Err(CliError::DoctorFailed);
    }
    Ok(())
}

fn output(mode: OutputMode, value: &impl Serialize) -> Result<(), CliError> {
    let result = match mode {
        OutputMode::Human => serde_json::to_string_pretty(value),
        OutputMode::Json => serde_json::to_string(value),
    }
    .map_err(|_| CliError::Input("response serialization failed".to_owned()))?;
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{result}")?;
    stdout.flush()?;
    Ok(())
}

fn load_token(cli: &Cli) -> Result<Vec<u8>, CliError> {
    let mut token = if let Some(path) = &cli.token_file {
        read_private_token_file(path)?
    } else if cli.token_stdin {
        read_bounded(io::stdin().lock(), 1024)?
    } else {
        env::var_os(&cli.token_env)
            .ok_or_else(|| {
                CliError::Input(format!(
                    "token environment variable {} is unset",
                    cli.token_env
                ))
            })?
            .as_encoded_bytes()
            .to_vec()
    };
    if token.ends_with(b"\r\n") {
        token.truncate(token.len() - 2);
    } else if token.ends_with(b"\n") {
        token.pop();
    }
    Ok(token)
}

fn read_private_token_file(path: &Path) -> Result<Vec<u8>, CliError> {
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?
    };
    #[cfg(not(unix))]
    let file = fs::OpenOptions::new().read(true).open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(CliError::Input(
            "token file must be a regular non-symlink file".to_owned(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(CliError::Input(
                "token file permissions must not grant group/other access".to_owned(),
            ));
        }
        if metadata.uid() != nix::unistd::geteuid().as_raw() {
            return Err(CliError::Input(
                "token file must be owned by the effective user".to_owned(),
            ));
        }
    }
    read_bounded(file, 1024)
}

fn read_json<T: DeserializeOwned>(path: &Path, token_stdin: bool) -> Result<T, CliError> {
    let bytes = if path == Path::new("-") {
        if token_stdin {
            return Err(CliError::Input(
                "stdin cannot carry both the token and a JSON document".to_owned(),
            ));
        }
        read_bounded(io::stdin().lock(), MAX_INPUT_BYTES)?
    } else {
        let metadata = fs::metadata(path)?;
        if !metadata.is_file() || metadata.len() > MAX_INPUT_BYTES {
            return Err(CliError::Input(
                "input must be a bounded regular file".to_owned(),
            ));
        }
        read_bounded(fs::File::open(path)?, MAX_INPUT_BYTES)?
    };
    serde_json::from_slice(&bytes)
        .map_err(|_| CliError::Input("input is not valid typed JSON".to_owned()))
}

fn read_bounded(mut reader: impl Read, maximum: u64) -> Result<Vec<u8>, CliError> {
    let mut bytes = Vec::new();
    reader.by_ref().take(maximum + 1).read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > maximum {
        return Err(CliError::Input("input exceeds its byte limit".to_owned()));
    }
    Ok(bytes)
}

fn open_regular_file(path: &Path, maximum: u64) -> Result<(tokio::fs::File, u64), CliError> {
    let file = fs::File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > maximum {
        return Err(CliError::Input(
            "artifact input must be a non-empty bounded regular file".to_owned(),
        ));
    }
    let length = metadata.len();
    Ok((tokio::fs::File::from_std(file), length))
}

fn parse_wire<T: DeserializeOwned>(value: &str) -> Result<T, CliError> {
    serde_json::from_value(serde_json::Value::String(value.to_owned()))
        .map_err(|_| CliError::Input("identifier is not valid".to_owned()))
}

fn parse_enum<T: DeserializeOwned>(value: &str) -> Result<T, CliError> {
    serde_json::from_value(serde_json::Value::String(value.to_owned()))
        .map_err(|_| CliError::Input("enum value is not valid".to_owned()))
}

fn exit_code(error: &CliError) -> u8 {
    match error {
        CliError::Input(_) | CliError::Io(_) => 2,
        CliError::DoctorFailed => 7,
        CliError::Sdk(SdkError::Problem(problem)) => match problem.code() {
            ErrorCode::AuthenticationRequired => 3,
            ErrorCode::PermissionDenied => 4,
            ErrorCode::NotFound | ErrorCode::StaleReference => 5,
            ErrorCode::LeaseConflict
            | ErrorCode::CommandIdConflict
            | ErrorCode::AmbiguousTarget => 6,
            ErrorCode::RequestOutcomeUnknown => 8,
            _ => 7,
        },
        CliError::Sdk(_) => 7,
    }
}

#[cfg(test)]
mod tests {
    use serde::ser::{Error as _, Serializer};

    use super::*;

    struct SerializationFailure;

    impl Serialize for SerializationFailure {
        fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            Err(S::Error::custom("deliberate serialization failure"))
        }
    }

    #[test]
    fn output_serialization_failure_is_an_error() {
        assert!(matches!(
            output(OutputMode::Json, &SerializationFailure),
            Err(CliError::Input(message)) if message == "response serialization failed"
        ));
    }
}
