use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use beam::hotspot::{self, Hotspot, Network};
use beam::share::{Added, Share};
use beam::{browser, config, net, server, upload};

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

    /// Turn this PC into a Wi-Fi hotspot and connect the phone straight to
    /// it: faster than going through a router, and no router needed.
    /// Windows only. It's turned off again when Beam stops.
    #[arg(long)]
    hotspot: bool,

    /// Files to offer to the phone, from anywhere on this PC (another
    /// drive, say). They're served from where they are, never copied.
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
    let hotspot = Arc::new(Hotspot::new(phone_url(hotspot::ADDRESS, port, &token)));

    let network = if cli.hotspot {
        start_hotspot(hotspot.clone()).await
    } else {
        None
    };

    print_banner(&Banner {
        dir: &shared.display,
        port,
        requested_port: cli.port,
        phone_urls: &phone_urls,
        qr_page: &qr_page,
        token: &token,
        hotspot: network.as_ref().map(|n| (n, hotspot.beam_url())),
    });

    if !cli.no_open
        && (!phone_urls.is_empty() || network.is_some())
        && let Err(e) = browser::open(&qr_page)
    {
        println!("(Could not open the browser: {e}. Open the link above instead.)");
        println!();
    }

    let mut state = server::AppState::new(token, shared).with_phone_urls(phone_urls);
    state.hotspot = hotspot.clone();
    // Nothing is copied, so this is quick enough to do before serving.
    share_files(&state.share, &cli.files);
    let served = server::serve(listener, state, shutdown_signal()).await;

    // Even if serving failed: don't leave the PC's hotspot on.
    let turned_off = hotspot.turned_on_by_beam();
    let _ = tokio::task::spawn_blocking(move || hotspot.cleanup()).await;
    if turned_off {
        println!("Turned this PC's hotspot back off.");
    }
    served.context("server stopped unexpectedly")?;

    println!("Stopped. The QR code from this run no longer works.");
    Ok(())
}

/// For `--hotspot`. A failure is explained and Beam carries on without it.
async fn start_hotspot(hotspot: Arc<Hotspot>) -> Option<Network> {
    println!();
    println!("Turning this PC into a Wi-Fi hotspot…");
    match tokio::task::spawn_blocking(move || hotspot.start()).await {
        Ok(Ok(network)) => {
            hotspot::log_on(&network);
            Some(network)
        }
        Ok(Err(e)) => {
            println!("Could not make a hotspot: {e}.");
            println!("Carrying on with the normal Wi-Fi connection instead.");
            None
        }
        Err(e) => {
            tracing::error!("hotspot task failed: {e}");
            None
        }
    }
}

fn phone_url(ip: Ipv4Addr, port: u16, token: &str) -> String {
    format!("http://{ip}:{port}/?t={token}")
}

struct Banner<'a> {
    dir: &'a std::path::Path,
    port: u16,
    requested_port: u16,
    phone_urls: &'a [String],
    qr_page: &'a str,
    token: &'a str,
    /// The hotspot's network and the link for phones on it, if it's on.
    hotspot: Option<(&'a Network, &'a str)>,
}

fn print_qr(data: &str) {
    if let Err(e) = qr2term::print_qr(data) {
        println!("(could not draw the QR code: {e})");
    }
}

fn print_banner(b: &Banner) {
    println!();
    println!("Beam is running.");
    println!("Shared folder: {}", b.dir.display());
    if b.port != b.requested_port {
        println!(
            "Port {} was busy, using {} instead.",
            b.requested_port, b.port
        );
    }
    println!();

    if let Some((network, url)) = b.hotspot {
        // The band asked for; Windows may not get it (see the page's note).
        println!(
            "This PC is now a Wi-Fi hotspot (set to {}).",
            network.band.label()
        );
        println!();
        println!("1. On the phone, join the Wi-Fi network {:?}", network.ssid);
        println!("   with password {:?}, or scan this:", network.password);
        println!();
        print_qr(&hotspot::wifi_qr(&network.ssid, &network.password));
        println!();
        println!("2. Then scan this to open Beam:");
        println!();
        print_qr(url);
        println!();
        println!("  {url}");
        println!();
        println!("Both codes, bigger, in your browser:");
        println!("  {}", b.qr_page);
        if !b.phone_urls.is_empty() {
            println!();
            println!("A phone on the same Wi-Fi as this PC can use these instead:");
            for url in b.phone_urls {
                println!("  {url}");
            }
        }
    } else {
        match b.phone_urls.split_first() {
            Some((best, others)) => {
                println!("Scan this with your phone's camera:");
                println!();
                print_qr(best);
                println!();
                println!("  {best}");
                println!();
                println!("Bigger QR code in your browser, where you can also share another");
                println!("folder, or connect the phone directly for faster transfers:");
                println!("  {}", b.qr_page);
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
                    phone_url(Ipv4Addr::LOCALHOST, b.port, b.token)
                );
            }
        }
    }

    println!();
    println!("Phone can't connect? Allow Beam through Windows Firewall (on public networks too");
    println!("when using this PC's hotspot), set the Wi-Fi profile to Private, or start Beam");
    println!("with --hotspot to connect the phone straight to this PC.");
    println!("Press Ctrl+C or close this window to stop.");
    println!();
}

fn share_files(share: &Share, paths: &[PathBuf]) {
    for path in paths {
        match share.add_file(path) {
            Ok(Added::Shared(extra)) => {
                tracing::info!("Sharing {:?} from {}", extra.name, extra.display.display());
            }
            Ok(Added::InFolder(name)) => {
                tracing::info!("Sharing {name:?} (already in the shared folder)");
            }
            Ok(Added::AlreadyShared(_)) => {}
            Err(e) => tracing::warn!("Not shared: {e}"),
        }
    }
}

async fn shutdown_signal() {
    tokio::select! {
        () = ctrl_c() => {}
        () = window_closed() => {}
    }
}

async fn ctrl_c() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        tracing::error!("could not listen for Ctrl+C: {e}");
        // Without a signal handler, keep serving rather than exit immediately.
        std::future::pending::<()>().await;
    }
}

/// Closing the console window is how most people stop Beam. Windows then
/// allows about 5 seconds, enough to finish up and turn the hotspot off.
#[cfg(windows)]
async fn window_closed() {
    match tokio::signal::windows::ctrl_close() {
        Ok(mut close) => {
            close.recv().await;
        }
        Err(e) => {
            tracing::debug!("could not listen for the window closing: {e}");
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(not(windows))]
async fn window_closed() {
    std::future::pending::<()>().await;
}
