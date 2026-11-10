use std::net::Ipv4Addr;
use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use beam::{config, net, server, upload};

/// Send files between this PC and your phone over the local network.
#[derive(Parser)]
#[command(name = "beam", version)]
struct Cli {
    /// Shared folder. Uploads land here and its files are offered for download.
    /// Defaults to ~/Downloads/Beam.
    #[arg(long)]
    dir: Option<PathBuf>,

    /// Port to listen on. If it is taken, the next few ports are tried.
    #[arg(long, default_value_t = 8080)]
    port: u16,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("beam=info")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();

    let dir = match cli.dir {
        Some(d) => d,
        None => config::default_dir()?,
    };
    let shared = config::prepare_dir(&dir)?;
    let removed = upload::remove_stale_parts(&shared.canonical);
    if removed > 0 {
        tracing::info!("Removed {removed} unfinished upload(s) left by an earlier run.");
    }
    let token = config::generate_token();

    let listener = server::bind(cli.port).await?;
    let port = listener
        .local_addr()
        .context("could not read the listening address")?
        .port();

    print_banner(&shared.display, port, cli.port, &token);

    let state = server::AppState::new(token, shared.canonical);
    server::serve(listener, state, shutdown_signal())
        .await
        .context("server stopped unexpectedly")?;

    println!("Stopped. The QR code from this run no longer works.");
    Ok(())
}

fn print_banner(dir: &std::path::Path, port: u16, requested_port: u16, token: &str) {
    let ips = net::lan_candidates();
    let url = |ip: Ipv4Addr| format!("http://{ip}:{port}/?t={token}");

    println!();
    println!("Beam is running.");
    println!("Shared folder: {}", dir.display());
    if port != requested_port {
        println!("Port {requested_port} was busy, using {port} instead.");
    }
    println!();

    match ips.split_first() {
        Some((&best, others)) => {
            println!("Scan this with your phone's camera:");
            println!();
            if let Err(e) = qr2term::print_qr(url(best)) {
                println!("(could not draw the QR code: {e})");
            }
            println!();
            println!("  {}", url(best));
            if !others.is_empty() {
                println!();
                println!("If that doesn't open, try one of these on the phone:");
                for &ip in others {
                    println!("  {}", url(ip));
                }
            }
        }
        None => {
            println!("No local network address found. Is this PC connected to Wi-Fi?");
            println!("On this PC only: {}", url(Ipv4Addr::LOCALHOST));
        }
    }

    println!();
    println!(
        "Phone can't connect? Allow Beam through Windows Firewall on private networks, \
         set the Wi-Fi profile to Private, or connect this PC to the phone's hotspot."
    );
    println!("Press Ctrl+C to stop.");
    println!();
}

async fn shutdown_signal() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        tracing::error!("could not listen for Ctrl+C: {e}");
        // Without a signal handler, keep serving rather than exit immediately.
        std::future::pending::<()>().await;
    }
}
