use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use mimikyu_core::driver::EngineKind;

pub fn parse_engine(s: &str) -> Result<EngineKind, String> {
    s.parse::<EngineKind>().map_err(|e| e.to_string())
}

#[derive(Debug, Parser)]
#[command(
    name = "mimikyu",
    version,
    about = "Fast browser automation, session/profile isolation, and environment consistency control plane for AI agents"
)]
pub struct Cli {
    /// Emit machine-readable JSON envelope: {"ok": true, "data": ...} or {"ok": false, "error": "..."}
    #[arg(long, global = true)]
    pub json: bool,

    /// Target session identifier
    #[arg(long, global = true, default_value = "default", visible_alias = "context")]
    pub session: String,

    /// Browser engine (chromium | firefox)
    #[arg(long, global = true, value_parser = parse_engine)]
    pub engine: Option<EngineKind>,

    /// State directory for persistent profiles and daemon sockets
    #[arg(long, global = true, allow_hyphen_values = true)]
    pub state_dir: Option<PathBuf>,

    /// Execute directly in-process without starting or connecting to the background daemon
    #[arg(long, global = true)]
    pub no_daemon: bool,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Discover local Chromium & Firefox binaries and check host readiness
    Doctor,

    /// List, locate, or install Mimikyu-managed Chromium & Firefox browser runtimes
    Browser(BrowserArgs),

    /// Launch and manage hardened privacy browser vaults
    Vault(VaultArgs),

    /// Manage or run the warm background Unix-socket daemon
    Daemon(DaemonArgs),

    /// Create, list, close, export, or import browser sessions
    Session(SessionArgs),

    /// Close an active browser session (alias for `session close`)
    Close {
        /// Optional session identifier (defaults to global --session)
        #[arg(value_name = "SESSION_ID")]
        session_id: Option<String>,
    },

    /// Manage persistent browser profiles
    Profile(ProfileArgs),

    /// Manage active browser contexts (alias for session listing/closing)
    Context(ContextArgs),

    /// Navigate the active session to a URL
    Open {
        /// Target URL (http://, https://, data:, about:)
        url: String,
    },

    /// Navigate backward in session history
    Back,

    /// Navigate forward in session history
    Forward,

    /// Reload the current page
    Reload,

    /// Capture a compact @eN semantic accessibility tree snapshot
    Snapshot(SnapshotArgs),

    /// Click an element by @eN ref or CSS selector
    Click {
        /// Target @eN ref or CSS selector
        target: String,
        /// Use kinematic humanize pointer motion
        #[arg(long)]
        humanize: bool,
    },

    /// Clear and fill an input element by @eN ref or CSS selector
    Fill {
        /// Target @eN ref or CSS selector
        target: String,
        /// Value to fill
        #[arg(allow_hyphen_values = true)]
        value: String,
        /// Use kinematic humanize typing
        #[arg(long)]
        humanize: bool,
    },

    /// Type text sequentially into an input element by @eN ref or CSS selector
    Type {
        /// Target @eN ref or CSS selector
        target: String,
        /// Text to type
        #[arg(allow_hyphen_values = true)]
        text: String,
        /// Use kinematic humanize typing cadence
        #[arg(long)]
        humanize: bool,
    },

    /// Select an option in a <select> element by value or label
    Select {
        /// Target @eN ref or CSS selector
        target: String,
        /// Option value or label
        #[arg(allow_hyphen_values = true)]
        value: String,
    },

    /// Upload a local file into a file input element
    Upload {
        /// Target @eN ref or CSS selector
        target: String,
        /// Local file path
        #[arg(allow_hyphen_values = true)]
        path: PathBuf,
    },

    /// Scroll the viewport by pixel deltas
    Scroll {
        /// Horizontal scroll delta in pixels
        #[arg(allow_hyphen_values = true)]
        dx: f64,
        /// Vertical scroll delta in pixels
        #[arg(allow_hyphen_values = true)]
        dy: f64,
        /// Use kinematic humanize wheel motion
        #[arg(long)]
        humanize: bool,
    },

    /// Press a keyboard key or key combination
    Press {
        /// Key name (e.g. Enter, Tab, Escape)
        key: String,
    },

    /// Wait for page idle, text, URL substring, or selector state
    Wait(WaitArgs),

    /// Execute an ordered array of browser actions in a single call
    Batch(BatchArgs),

    /// Extract structured content from the current page
    Extract {
        /// Extraction mode (markdown | text | links | tables)
        mode: String,
    },

    /// Evaluate JavaScript in isolated world or opt-in main world
    Eval {
        /// JavaScript expression or body to evaluate
        #[arg(allow_hyphen_values = true)]
        js: String,
        /// Evaluate in the page main world instead of isolated world
        #[arg(long)]
        main_world: bool,
    },

    /// Capture a PNG screenshot of the viewport, full page, or target element
    Screenshot {
        /// Output PNG file path
        out: PathBuf,
        /// Optional @eN ref or CSS selector to clip
        #[arg(long)]
        target: Option<String>,
        /// Capture full scrollable page
        #[arg(long)]
        full_page: bool,
    },

    /// Export the current page as a PDF document
    Pdf {
        /// Output PDF file path
        out: PathBuf,
    },

    /// Start or stop headless screencast frame capture for a session
    Record(RecordArgs),

    /// Inspect console or network telemetry logs for a session
    Logs {
        /// Log stream kind (console | network)
        kind: String,
    },

    /// Run the stdio JSON-RPC 2.0 MCP server
    Mcp,

    /// Run the reproducible local speed and agentic benchmark suite
    Bench(BenchArgs),
}

#[derive(Debug, Args)]
pub struct DaemonArgs {
    #[command(subcommand)]
    pub action: Option<DaemonSubcommand>,

    /// Run the daemon in the foreground instead of spawning in the background
    #[arg(long)]
    pub foreground: bool,

    /// Idle shutdown timeout in seconds (0 disables idle shutdown)
    #[arg(long, value_name = "SEC")]
    pub idle_timeout: Option<u64>,
}

#[derive(Debug, Subcommand)]
pub enum DaemonSubcommand {
    /// Query whether the daemon is running for --state-dir
    Status,
    /// Gracefully stop the running daemon and clean up socket/PID files
    Stop,
}

#[derive(Debug, Args)]
pub struct SessionArgs {
    #[command(subcommand)]
    pub action: SessionSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum SessionSubcommand {
    /// Create a new browser session
    New(SessionNewArgs),
    /// List active browser sessions
    List,
    /// Close an active browser session
    Close {
        /// Optional session identifier (defaults to global --session)
        session_id: Option<String>,
    },
    /// Export session cookies and storage state to a JSON file
    Export {
        /// Destination JSON file path
        file: PathBuf,
    },
    /// Import session cookies and storage state from a JSON file
    Import {
        /// Source JSON file path
        file: PathBuf,
    },
    /// Pause automated input on a session for human interaction
    Pause {
        /// Reason shown to operators and automated callers while paused
        #[arg(long)]
        reason: Option<String>,
        /// Optional session identifier (defaults to global --session)
        #[arg(value_name = "SESSION_ID")]
        session_id: Option<String>,
    },
    /// Resume automated input on a paused session
    Resume {
        /// Optional session identifier (defaults to global --session)
        #[arg(value_name = "SESSION_ID")]
        session_id: Option<String>,
    },
    /// Hand session control to a human operator (pauses automated input)
    Takeover {
        /// Actionable reason describing what the human operator should complete
        #[arg(long)]
        reason: Option<String>,
        /// Optional session identifier (defaults to global --session)
        #[arg(value_name = "SESSION_ID")]
        session_id: Option<String>,
    },
}

#[derive(Debug, Args)]
pub struct SessionNewArgs {
    /// Named profile or directory path for persistent storage
    #[arg(long)]
    pub profile: Option<String>,

    /// Use an ephemeral temporary profile directory
    #[arg(long)]
    pub ephemeral: bool,

    /// Use a lightweight virtual browser context inside a shared pool
    #[arg(long, visible_alias = "virtual-ctx")]
    pub r#virtual: bool,

    /// Environment profile preset (win11 | macos | linux)
    #[arg(long)]
    pub preset: Option<String>,

    /// BCP 47 locale identifier (e.g. en-US, de-DE)
    #[arg(long)]
    pub locale: Option<String>,

    /// IANA timezone identifier (e.g. America/Chicago)
    #[arg(long)]
    pub timezone: Option<String>,

    /// HTTP or SOCKS5 proxy URL
    #[arg(long)]
    pub proxy: Option<String>,

    /// IP address for local GeoIP region alignment
    #[arg(long)]
    pub geoip: Option<String>,

    /// Viewport dimensions in WxH format (e.g. 1280x720)
    #[arg(long)]
    pub viewport: Option<String>,

    /// Block image requests
    #[arg(long)]
    pub block_images: bool,

    /// Block audio and video media requests
    #[arg(long)]
    pub block_media: bool,

    /// Block web font requests
    #[arg(long)]
    pub block_fonts: bool,

    /// Block curated tracker, analytics, and ad-beacon domains
    #[arg(long)]
    pub block_trackers: bool,

    /// Enable hardened privacy mode (tracker blocking, WebRTC IP leak protection, human-safe window flags)
    #[arg(long)]
    pub privacy_mode: bool,

    /// Accelerate CSS animations and transitions
    #[arg(long)]
    pub instant_animations: bool,

    /// Enable kinematic humanize pointer and typing motion by default
    #[arg(long)]
    pub humanize: bool,

    /// Run with a visible browser window instead of headless mode
    #[arg(long)]
    pub headed: bool,
}

#[derive(Debug, Args)]
pub struct ProfileArgs {
    #[command(subcommand)]
    pub action: ProfileSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum ProfileSubcommand {
    /// List named persistent profiles under --state-dir
    List,
    /// Delete a named persistent profile
    Delete {
        /// Named profile identifier
        name: String,
    },
    /// Export storage state from a named persistent profile
    Export {
        /// Named profile identifier
        name: String,
        /// Destination JSON file path
        file: PathBuf,
    },
    /// Import storage state into a named persistent profile
    Import {
        /// Named profile identifier
        name: String,
        /// Source JSON file path
        file: PathBuf,
    },
}

#[derive(Debug, Args)]
pub struct ContextArgs {
    #[command(subcommand)]
    pub action: ContextSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum ContextSubcommand {
    /// List active browser contexts/sessions
    List,
    /// Close an active browser context/session
    Close {
        /// Optional context/session identifier (defaults to global --session)
        session_id: Option<String>,
    },
}

#[derive(Debug, Args)]
pub struct SnapshotArgs {
    /// Include structural diff from the previous snapshot
    #[arg(long)]
    pub diff: bool,

    /// Maximum accessibility tree traversal depth
    #[arg(long, allow_hyphen_values = true)]
    pub max_depth: Option<i64>,

    /// Keep only interactive elements and their ancestors
    #[arg(long)]
    pub interactive_only: bool,

    /// Resolve bounding boxes for interactive refs
    #[arg(long)]
    pub include_bounds: bool,
}

#[derive(Debug, Args)]
pub struct WaitArgs {
    /// Wait for page lifecycle, network quiet, and layout stability
    #[arg(long)]
    pub idle: bool,

    /// Wait until page text contains this substring
    #[arg(long, allow_hyphen_values = true)]
    pub text: Option<String>,

    /// Wait until current URL contains this substring
    #[arg(long, allow_hyphen_values = true)]
    pub url: Option<String>,

    /// Wait for CSS selector state
    #[arg(long, allow_hyphen_values = true)]
    pub selector: Option<String>,

    /// Desired selector state (attached | visible | hidden | detached)
    #[arg(long, default_value = "visible")]
    pub state: String,

    /// Maximum wait duration in milliseconds
    #[arg(long)]
    pub timeout_ms: Option<u64>,

    /// Polling interval in milliseconds
    #[arg(long)]
    pub poll_interval_ms: Option<u64>,

    /// Network quiet window in milliseconds for --idle
    #[arg(long)]
    pub network_quiet_ms: Option<u64>,

    /// Optional @eN or CSS target for layout stability during --idle
    #[arg(long)]
    pub stable_target: Option<String>,
}

#[derive(Debug, Args)]
pub struct BatchArgs {
    /// Path to a JSON file containing batch actions
    #[arg(long)]
    pub file: Option<PathBuf>,

    /// Inline JSON string containing batch actions
    #[arg(long)]
    pub ops: Option<String>,
}

#[derive(Debug, Args)]
pub struct BenchArgs {
    /// Number of measured benchmark iterations
    #[arg(long, default_value_t = 3, allow_hyphen_values = true)]
    pub iterations: i64,

    /// Number of warmup iterations before measurement
    #[arg(long, default_value_t = 1, allow_hyphen_values = true)]
    pub warmup: i64,

    /// Benchmark suite selection (all | speed | agentic)
    #[arg(long, default_value = "all")]
    pub suite: String,
}

#[derive(Debug, Args)]
pub struct BrowserArgs {
    #[command(subcommand)]
    pub action: BrowserSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum BrowserSubcommand {
    /// List all discovered local and Mimikyu-managed browser binaries
    List,
    /// Print the resolved binary path for an engine (chromium | firefox)
    Path {
        /// Browser engine (chromium | firefox)
        #[arg(value_parser = parse_engine)]
        engine: EngineKind,
        /// Require a binary capable of headed GUI windows
        #[arg(long)]
        headed: bool,
    },
    /// Download and verify an official managed browser runtime into Mimikyu's local cache
    Install {
        /// Browser engine (chromium | firefox)
        #[arg(value_parser = parse_engine)]
        engine: EngineKind,
        /// Resolve target URLs and install path without downloading or modifying disk
        #[arg(long)]
        dry_run: bool,
        /// Pin a specific official release version
        #[arg(long)]
        version: Option<String>,
        /// Verify archive against an explicit 64-char hex SHA-256 digest
        #[arg(long)]
        sha256: Option<String>,
    },
}

#[derive(Debug, Args)]
pub struct VaultArgs {
    #[command(subcommand)]
    pub action: VaultSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum VaultSubcommand {
    /// Launch a headed privacy-hardened browser session backed by a persistent locked profile
    Launch(VaultLaunchArgs),
    /// List named persistent privacy vault profiles under --state-dir
    List,
}

#[derive(Debug, Args)]
pub struct VaultLaunchArgs {
    /// Persistent vault profile name
    pub name: String,

    /// Optional initial URL to open after launching the vault
    #[arg(long)]
    pub url: Option<String>,

    /// HTTP or SOCKS5 proxy URL (authenticated proxies automatically use local ProxyRelay)
    #[arg(long)]
    pub proxy: Option<String>,

    /// Environment profile preset (win11 | macos | linux)
    #[arg(long)]
    pub preset: Option<String>,

    /// BCP 47 locale identifier (e.g. en-US, de-DE)
    #[arg(long)]
    pub locale: Option<String>,

    /// IANA timezone identifier (e.g. America/Chicago)
    #[arg(long)]
    pub timezone: Option<String>,

    /// IP address for local GeoIP region alignment
    #[arg(long)]
    pub geoip: Option<String>,

    /// Viewport dimensions in WxH format (e.g. 1280x800)
    #[arg(long)]
    pub viewport: Option<String>,

    /// Enable built-in tracker and telemetry domain blocking (always enabled in privacy vault mode)
    #[arg(long)]
    pub block_trackers: bool,

    /// Run headless instead of opening a visible headed browser window
    #[arg(long)]
    pub headless: bool,
}

#[derive(Debug, Args)]
pub struct RecordArgs {
    #[command(subcommand)]
    pub action: RecordSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum RecordSubcommand {
    /// Start capturing screencast frames into an output directory
    Start {
        /// Directory where captured PNG/JPEG frames are written
        #[arg(long)]
        out_dir: PathBuf,
        /// Frame image format (png | jpeg)
        #[arg(long, default_value = "png")]
        format: String,
        /// JPEG compression quality (1..=100)
        #[arg(long)]
        quality: Option<u8>,
        /// Maximum captured frame width in pixels
        #[arg(long)]
        max_width: Option<u32>,
        /// Maximum captured frame height in pixels
        #[arg(long)]
        max_height: Option<u32>,
        /// Capture every Nth compositor frame
        #[arg(long)]
        every_nth_frame: Option<u32>,
    },
    /// Stop an active screencast recording and report captured frame count
    Stop,
}
