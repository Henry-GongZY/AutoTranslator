//! CLI entry point. Spawned and supervised by the native client.

use anyhow::{bail, Result};
use clap::Parser;
use tracing_subscriber::EnvFilter;

use translator_core::ipc::transport::{bind, Endpoint};
use translator_core::ipc::{serve, ServeOptions};

#[derive(Debug, Parser)]
#[command(
    name = "translator-core",
    version,
    about = "Realtime system-audio subtitle core (phase 1)"
)]
struct Args {
    /// Windows named pipe to listen on, e.g. `\\.\pipe\translator-core-v1`
    #[arg(long)]
    pipe: Option<String>,

    /// Unix domain socket path to listen on
    #[arg(long)]
    socket: Option<String>,

    /// tracing filter, e.g. `info`, `debug`, `translator_core=debug`
    #[arg(long, default_value = "info")]
    log_level: String,

    /// Keep serving after the controlling client disconnects (debugging aid)
    #[arg(long)]
    keep_alive: bool,

    /// Sentence for the mock recogniser; repeatable. Defaults to a built-in set.
    #[arg(long = "mock-sentence")]
    mock_sentence: Vec<String>,
}

fn main() -> Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&args.log_level)),
        )
        .with_writer(std::io::stderr)
        .init();

    let endpoint = match (args.pipe.as_deref(), args.socket.as_deref()) {
        (Some(pipe), _) => Endpoint::Pipe(pipe.to_string()),
        (None, Some(path)) => Endpoint::UnixSocket(path.to_string()),
        (None, None) => Endpoint::default_for_platform(),
    };

    #[cfg(windows)]
    if matches!(endpoint, Endpoint::UnixSocket(_)) {
        bail!("--socket is not supported on Windows; use --pipe instead");
    }
    #[cfg(unix)]
    if matches!(endpoint, Endpoint::Pipe(_)) {
        bail!("--pipe is not supported on this platform; use --socket instead");
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    // `bind` must run inside the runtime: registering a named pipe needs the
    // tokio IO driver to be alive.
    runtime.block_on(async move {
        let listener = bind(&endpoint)?;
        let options = ServeOptions {
            exit_on_disconnect: !args.keep_alive,
            mock_sentences: args.mock_sentence,
        };

        tracing::info!(
            "translator-core {} listening on {}",
            translator_core::VERSION,
            endpoint.describe()
        );

        tokio::select! {
            result = serve(listener, options) => result?,
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("received ctrl-c, shutting down");
            }
        }
        Ok::<(), anyhow::Error>(())
    })
}
