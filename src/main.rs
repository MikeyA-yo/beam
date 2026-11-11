use std::net::Ipv4Addr;
use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use beam::{browser, config, files, net, server, upload};

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

    /// Don't open the QR code page in the browser at startup.
    #[arg(long)]
    no_open: bool,

    /// Files to offer to the phone. They are added to the shared folder:
    /// hard-linked when possible (instant, no extra space), otherwise copied.
    files: Vec<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("beam=info")),
        )
        .with_target(false)
        .without_time()
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

    let phone_urls: Vec<String> = net::lan_candidates()
        .into_iter()
        .map(|ip| phone_url(ip, port, &token))
        .collect();
    // Exchanges the token for a cookie in this PC's browser, then shows /qr.
    let qr_page = format!("http://127.0.0.1:{port}/?t={token}&next=qr");

    print_banner(
        &shared.display,
        port,
        cli.port,
        &phone_urls,
        &qr_page,
        &token,
    );

    if !cli.no_open
        && !phone_urls.is_empty()
        && let Err(e) = browser::open(&qr_page)
    {
        println!("(Could not open the browser: {e}. Open the link above instead.)");
        println!();
    }

    if !cli.files.is_empty() {
        let dir = shared.canonical.clone();
        // Copies from another drive can take a while; don't hold up the server.
        tokio::task::spawn_blocking(move || share_files(&cli.files, &dir));
    }

    let state = server::AppState::new(token, shared.canonical).with_phone_urls(phone_urls);
    server::serve(listener, state, shutdown_signal())
        .await
        .context("server stopped unexpectedly")?;

    println!("Stopped. The QR code from this run no longer works.");
    Ok(())
}

fn phone_url(ip: Ipv4Addr, port: u16, token: &str) -> String {
    format!("http://{ip}:{port}/?t={token}")
}

fn print_banner(
    dir: &std::path::Path,
    port: u16,
    requested_port: u16,
    phone_urls: &[String],
    qr_page: &str,
    token: &str,
) {
    println!();
    println!("Beam is running.");
    println!("Shared folder: {}", dir.display());
    if port != requested_port {
        println!("Port {requested_port} was busy, using {port} instead.");
    }
    println!();

    match phone_urls.split_first() {
        Some((best, others)) => {
            println!("Scan this with your phone's camera:");
            println!();
            if let Err(e) = qr2term::print_qr(best) {
                println!("(could not draw the QR code: {e})");
            }
            println!();
            println!("  {best}");
            println!();
            println!("Bigger QR code in your browser:");
            println!("  {qr_page}");
            if !others.is_empty() {
                println!();
                println!("If the phone can't open the link, try one of these instead:");
                for url in others {
                    println!("  {url}");
                }
            }
        }
        None => {
            println!("No local network address found. Is this PC connected to Wi-Fi?");
            println!(
                "On this PC only: {}",
                phone_url(Ipv4Addr::LOCALHOST, port, token)
            );
        }
    }

    println!();
    println!(
        "Phone can't connect? Allow Beam through Windows Firewall on private networks,          set the Wi-Fi profile to Private, or connect this PC to the phone's hotspot."
    );
    println!("Press Ctrl+C to stop.");
    println!();
}

fn share_files(paths: &[PathBuf], dir: &std::path::Path) {
    for path in paths {
        match files::add_to_share(path, dir) {
            Ok(files::Shared::Linked(name)) => tracing::info!("Sharing {name:?}"),
            Ok(files::Shared::Copied(name)) => {
                tracing::info!("Sharing {name:?} (copied, because it is on another drive)");
            }
            Ok(files::Shared::AlreadyThere(name)) => {
                tracing::info!("Sharing {name:?} (already in the shared folder)");
            }
            Err(e) => tracing::warn!("Not shared: {e}"),
        }
    }
}

async fn shutdown_signal() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        tracing::error!("could not listen for Ctrl+C: {e}");
        // Without a signal handler, keep serving rather than exit immediately.
        std::future::pending::<()>().await;
    }
}
