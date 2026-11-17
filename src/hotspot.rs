//! Turning this PC into a Wi-Fi hotspot, so the phone connects straight to
//! it. Through a router, every byte crosses the air twice on one shared
//! channel. Straight to the PC it crosses once, and on 5 GHz when the
//! hardware allows. That's what flash-transfer apps do too.
//!
//! This drives Windows' own Mobile Hotspot. Beam keeps the network name
//! and password the person already has, may switch the band to 5 GHz and
//! keep Windows from turning an idle hotspot off, and puts all of that
//! back when it stops. It only turns the hotspot off if it turned it on.
//!
//! Like changing the shared folder, this is for the person at the PC only.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Mutex, MutexGuard, PoisonError};

use axum::Json;
use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::server::AppState;
use crate::{qr, share};

/// Windows always gives the PC this address on its hotspot.
pub const ADDRESS: Ipv4Addr = Ipv4Addr::new(192, 168, 137, 1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Band {
    Auto,
    TwoPointFour,
    Five,
}

impl Band {
    pub fn label(self) -> &'static str {
        match self {
            Band::Auto => "a band Windows picks",
            Band::TwoPointFour => "2.4 GHz",
            Band::Five => "5 GHz",
        }
    }
}

/// The hotspot's Wi-Fi network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Network {
    pub ssid: String,
    /// Shown on the PC's own screen and put in the QR code. Never logged.
    pub password: String,
    pub band: Band,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum HotspotError {
    #[error("Beam can only make a hotspot on Windows")]
    Unsupported,
    #[error(
        "this PC isn't connected to any network. Windows only makes a hotspot while the PC is \
         on Wi-Fi or a cable, so connect it first"
    )]
    NoConnection,
    #[error("{0}")]
    NotAllowed(&'static str),
    #[error("Wi-Fi is turned off on this PC. Turn it on, then try again")]
    WifiOff,
    #[error("Windows couldn't start the hotspot ({0})")]
    Failed(String),
}

/// What Beam changed, to put back later.
#[derive(Debug, Default)]
// Only Windows makes a hotspot, so elsewhere nothing ever changes.
#[cfg_attr(not(windows), allow(dead_code))]
struct Changes {
    turned_on: bool,
    band: Option<Band>,
    idle_timeout: bool,
}

pub struct Hotspot {
    /// The link phones on the hotspot open, token included.
    beam_url: String,
    changes: Mutex<Changes>,
}

impl Hotspot {
    pub fn new(beam_url: String) -> Self {
        Self {
            beam_url,
            changes: Mutex::new(Changes::default()),
        }
    }

    pub fn beam_url(&self) -> &str {
        &self.beam_url
    }

    /// The hotspot's network if it's on. Blocking.
    pub fn status(&self) -> Result<Option<Network>, HotspotError> {
        platform::status()
    }

    /// Turns the hotspot on, or uses it as it is if it's already on.
    /// Blocking.
    pub fn start(&self) -> Result<Network, HotspotError> {
        let mut changes = self.lock();
        platform::start(&mut changes)
    }

    /// Turns the hotspot off and puts back what Beam changed. Blocking.
    pub fn stop(&self) -> Result<(), HotspotError> {
        let mut changes = self.lock();
        platform::stop(&mut changes, true)
    }

    /// When Beam exits: off again only if Beam turned it on. Blocking.
    pub fn cleanup(&self) {
        let mut changes = self.lock();
        if let Err(e) = platform::stop(&mut changes, false) {
            tracing::warn!("Could not turn the hotspot back off: {e}");
        }
    }

    pub fn turned_on_by_beam(&self) -> bool {
        self.lock().turned_on
    }

    // Each change is recorded right after it's made, so a panic can't
    // leave this half-updated.
    fn lock(&self) -> MutexGuard<'_, Changes> {
        self.changes.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The text of a QR code that phone cameras offer to join as Wi-Fi.
/// Format from the ZXing project, which every phone camera follows.
pub fn wifi_qr(ssid: &str, password: &str) -> String {
    fn escape(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            if matches!(c, '\\' | ';' | ',' | ':' | '"') {
                out.push('\\');
            }
            out.push(c);
        }
        out
    }
    format!("WIFI:T:WPA;S:{};P:{};;", escape(ssid), escape(password))
}

#[cfg(windows)]
mod platform {
    use windows::Networking::Connectivity::{
        ConnectionProfile, NetworkConnectivityLevel, NetworkInformation,
    };
    use windows::Networking::NetworkOperators::{
        NetworkOperatorTetheringAccessPointConfiguration, NetworkOperatorTetheringManager,
        TetheringCapability, TetheringOperationStatus, TetheringOperationalState,
        TetheringWiFiBand,
    };

    use super::{Band, Changes, HotspotError, Network};

    type Manager = NetworkOperatorTetheringManager;

    impl From<windows::core::Error> for HotspotError {
        fn from(e: windows::core::Error) -> Self {
            Self::Failed(e.message().trim().to_owned())
        }
    }

    fn band_of(b: TetheringWiFiBand) -> Band {
        match b {
            TetheringWiFiBand::TwoPointFourGigahertz => Band::TwoPointFour,
            TetheringWiFiBand::FiveGigahertz => Band::Five,
            _ => Band::Auto,
        }
    }

    fn wifi_band(b: Band) -> TetheringWiFiBand {
        match b {
            Band::Auto => TetheringWiFiBand::Auto,
            Band::TwoPointFour => TetheringWiFiBand::TwoPointFourGigahertz,
            Band::Five => TetheringWiFiBand::FiveGigahertz,
        }
    }

    /// The connection the hotspot shares: the internet one if there is
    /// one, otherwise any connected network (a router without internet).
    fn manager() -> Result<Manager, HotspotError> {
        let profile = NetworkInformation::GetInternetConnectionProfile()
            .ok()
            .or_else(any_connected_profile)
            .ok_or(HotspotError::NoConnection)?;
        let capability = Manager::GetTetheringCapabilityFromConnectionProfile(&profile)?;
        let refused = match capability {
            TetheringCapability::Enabled => {
                return Ok(Manager::CreateFromConnectionProfile(&profile)?);
            }
            TetheringCapability::DisabledByGroupPolicy => {
                "this PC's settings, set by its administrator, don't allow a hotspot"
            }
            TetheringCapability::DisabledByHardwareLimitation => {
                "this PC's Wi-Fi can't make a hotspot"
            }
            _ => "Windows doesn't allow a hotspot on this PC's current connection",
        };
        Err(HotspotError::NotAllowed(refused))
    }

    fn any_connected_profile() -> Option<ConnectionProfile> {
        let profiles = NetworkInformation::GetConnectionProfiles().ok()?;
        profiles.into_iter().find(|p| {
            p.GetNetworkConnectivityLevel()
                .is_ok_and(|l| l != NetworkConnectivityLevel::None)
        })
    }

    fn network_of(
        config: &NetworkOperatorTetheringAccessPointConfiguration,
    ) -> Result<Network, HotspotError> {
        Ok(Network {
            ssid: config.Ssid()?.to_string(),
            password: config.Passphrase()?.to_string(),
            band: band_of(config.Band()?),
        })
    }

    fn is_on(m: &Manager) -> Result<bool, HotspotError> {
        Ok(m.TetheringOperationalState()? == TetheringOperationalState::On)
    }

    /// Whatever stops Windows from answering (no connection, no Wi-Fi at
    /// all, as on a desktop with only a cable) also means there's no
    /// hotspot. Turning it on is where the reason gets reported.
    pub fn status() -> Result<Option<Network>, HotspotError> {
        let current = manager().and_then(|m| {
            if !is_on(&m)? {
                return Ok(None);
            }
            network_of(&m.GetCurrentAccessPointConfiguration()?).map(Some)
        });
        Ok(current.unwrap_or_else(|e| {
            tracing::debug!("hotspot status: {e}");
            None
        }))
    }

    pub fn start(changes: &mut Changes) -> Result<Network, HotspotError> {
        let m = manager()?;
        if is_on(&m)? {
            return network_of(&m.GetCurrentAccessPointConfiguration()?);
        }
        let result = turn_on(&m, changes);
        if result.is_err() {
            // Don't leave a changed band or timeout behind a failed start.
            restore(&m, changes);
        }
        result
    }

    fn turn_on(m: &Manager, changes: &mut Changes) -> Result<Network, HotspotError> {
        // 5 GHz when the hardware has it. If the PC's own Wi-Fi is busy on
        // 2.4 GHz, a single radio may still have to stay there; Windows
        // handles that.
        let config = m.GetCurrentAccessPointConfiguration()?;
        let band = band_of(config.Band()?);
        if band != Band::Five
            && config
                .IsBandSupported(TetheringWiFiBand::FiveGigahertz)
                .unwrap_or(false)
        {
            config.SetBand(TetheringWiFiBand::FiveGigahertz)?;
            m.ConfigureAccessPointAsync(&config)?.join()?;
            changes.band.get_or_insert(band);
        }

        // Windows turns a hotspot with no devices off after a few minutes,
        // possibly before the phone has joined. Not every PC lets an app
        // change that, so it's best effort.
        if Manager::IsNoConnectionsTimeoutEnabled().unwrap_or(false) {
            match Manager::DisableNoConnectionsTimeout() {
                Ok(()) => changes.idle_timeout = true,
                Err(e) => tracing::debug!("could not keep the hotspot from timing out: {e}"),
            }
        }

        let result = m.StartTetheringAsync()?.join()?;
        match result.Status()? {
            TetheringOperationStatus::Success => {}
            TetheringOperationStatus::WiFiDeviceOff => return Err(HotspotError::WifiOff),
            status => {
                let detail = result
                    .AdditionalErrorMessage()
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                tracing::debug!("hotspot start: {status:?} {detail}");
                return Err(HotspotError::Failed(if detail.is_empty() {
                    format!("{status:?}")
                } else {
                    detail
                }));
            }
        }
        changes.turned_on = true;
        network_of(&m.GetCurrentAccessPointConfiguration()?)
    }

    /// `always`: the person asked to turn it off. Otherwise (Beam exiting)
    /// only if Beam turned it on.
    pub fn stop(changes: &mut Changes, always: bool) -> Result<(), HotspotError> {
        if !always && !changes.turned_on && changes.band.is_none() && !changes.idle_timeout {
            return Ok(());
        }
        let m = manager()?;
        let mut result = Ok(());
        if (always || changes.turned_on) && is_on(&m)? {
            let r = m.StopTetheringAsync()?.join()?;
            let status = r.Status()?;
            if status != TetheringOperationStatus::Success {
                result = Err(HotspotError::Failed(format!("{status:?}")));
            }
        }
        changes.turned_on = false;
        restore(&m, changes);
        result
    }

    fn restore(m: &Manager, changes: &mut Changes) {
        if let Some(band) = changes.band.take() {
            let restored = m.GetCurrentAccessPointConfiguration().and_then(|config| {
                config.SetBand(wifi_band(band))?;
                m.ConfigureAccessPointAsync(&config)?.join()
            });
            if let Err(e) = restored {
                tracing::debug!("could not restore the hotspot band: {e}");
            }
        }
        if std::mem::take(&mut changes.idle_timeout)
            && let Err(e) = Manager::EnableNoConnectionsTimeout()
        {
            tracing::debug!("could not restore the hotspot timeout: {e}");
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use super::{Changes, HotspotError, Network};

    pub fn status() -> Result<Option<Network>, HotspotError> {
        Ok(None)
    }

    pub fn start(_: &mut Changes) -> Result<Network, HotspotError> {
        Err(HotspotError::Unsupported)
    }

    pub fn stop(_: &mut Changes, _always: bool) -> Result<(), HotspotError> {
        Ok(())
    }
}

// ---- Handlers, for the PC's page only ----

#[derive(Serialize)]
pub struct HotspotInfo {
    /// False off Windows: the page then doesn't offer it.
    pub supported: bool,
    pub on: bool,
    pub turned_on_by_beam: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkInfo>,
}

#[derive(Serialize)]
pub struct NetworkInfo {
    pub ssid: String,
    pub password: String,
    pub band: &'static str,
    /// Step 1: join the PC's Wi-Fi.
    pub wifi_qr: String,
    /// Step 2: open Beam.
    pub beam_qr: String,
    pub beam_url: String,
}

impl HotspotInfo {
    fn of(hotspot: &Hotspot, network: Option<Network>) -> Self {
        Self {
            supported: cfg!(windows),
            on: network.is_some(),
            turned_on_by_beam: hotspot.turned_on_by_beam(),
            network: network.map(|n| NetworkInfo {
                wifi_qr: qr::svg(&wifi_qr(&n.ssid, &n.password)).unwrap_or_default(),
                beam_qr: qr::svg(hotspot.beam_url()).unwrap_or_default(),
                beam_url: hotspot.beam_url().to_owned(),
                band: n.band.label(),
                ssid: n.ssid,
                password: n.password,
            }),
        }
    }
}

/// Requests that change something must be JSON, so another website can't
/// send them without the browser asking first.
#[derive(Deserialize)]
pub struct NoBody {}

fn error(status: StatusCode, message: impl ToString) -> Response {
    #[derive(Serialize)]
    struct ErrorBody {
        error: String,
    }
    let body = ErrorBody {
        error: message.to_string(),
    };
    (status, Json(body)).into_response()
}

async fn run<T: Send + 'static>(
    state: &AppState,
    job: impl FnOnce(&Hotspot) -> Result<T, HotspotError> + Send + 'static,
) -> Result<(T, Option<Network>), Box<Response>> {
    let hotspot = state.hotspot.clone();
    let done = tokio::task::spawn_blocking(move || {
        let value = job(&hotspot)?;
        let status = hotspot.status()?;
        Ok::<_, HotspotError>((value, status))
    })
    .await;
    match done {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(Box::new(error(StatusCode::CONFLICT, e))),
        Err(e) => {
            tracing::error!("hotspot task failed: {e}");
            Err(Box::new(error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error",
            )))
        }
    }
}

/// `GET /api/hotspot`
pub async fn get_handler(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    if !share::from_this_pc(&peer) {
        return error(
            StatusCode::FORBIDDEN,
            "only the PC running Beam can do that",
        );
    }
    match run(&state, |_| Ok(())).await {
        Ok(((), network)) => Json(HotspotInfo::of(&state.hotspot, network)).into_response(),
        Err(res) => *res,
    }
}

/// `POST /api/hotspot/start`
pub async fn start_handler(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(NoBody {}): Json<NoBody>,
) -> Response {
    if !share::from_this_pc(&peer) {
        return error(
            StatusCode::FORBIDDEN,
            "only the PC running Beam can do that",
        );
    }
    match run(&state, Hotspot::start).await {
        Ok((network, _)) => {
            log_on(&network);
            Json(HotspotInfo::of(&state.hotspot, Some(network))).into_response()
        }
        Err(res) => {
            tracing::info!("Hotspot not started.");
            *res
        }
    }
}

/// `POST /api/hotspot/stop`
pub async fn stop_handler(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(NoBody {}): Json<NoBody>,
) -> Response {
    if !share::from_this_pc(&peer) {
        return error(
            StatusCode::FORBIDDEN,
            "only the PC running Beam can do that",
        );
    }
    match run(&state, Hotspot::stop).await {
        Ok(((), network)) => {
            tracing::info!("Hotspot turned off.");
            Json(HotspotInfo::of(&state.hotspot, network)).into_response()
        }
        Err(res) => *res,
    }
}

/// The password stays out of the terminal log.
pub fn log_on(network: &Network) {
    tracing::info!(
        "Hotspot on: Wi-Fi network {:?} (set to {}).",
        network.ssid,
        network.band.label()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wifi_qr_escapes_what_the_format_reserves() {
        assert_eq!(
            wifi_qr("DESKTOP-269BU01 7882", "pass1234"),
            "WIFI:T:WPA;S:DESKTOP-269BU01 7882;P:pass1234;;"
        );
        assert_eq!(
            wifi_qr(r#"a;b,c:d\e"f"#, "p;w"),
            r#"WIFI:T:WPA;S:a\;b\,c\:d\\e\"f;P:p\;w;;"#
        );
        assert_eq!(wifi_qr("Café ☕", "x"), "WIFI:T:WPA;S:Café ☕;P:x;;");
    }

    #[test]
    fn errors_read_well_on_the_page() {
        assert_eq!(
            HotspotError::WifiOff.to_string(),
            "Wi-Fi is turned off on this PC. Turn it on, then try again"
        );
        assert!(
            HotspotError::NoConnection
                .to_string()
                .starts_with("this PC isn't connected")
        );
    }

    #[test]
    fn nothing_to_undo_means_cleanup_does_nothing() {
        // On every platform: Beam didn't touch the hotspot, so exiting
        // must not touch it either.
        let hotspot = Hotspot::new("http://192.168.137.1:8080/?t=x".into());
        assert!(!hotspot.turned_on_by_beam());
        hotspot.cleanup();
        assert!(!hotspot.turned_on_by_beam());
    }

    #[test]
    fn band_labels() {
        assert_eq!(Band::Five.label(), "5 GHz");
        assert_eq!(Band::TwoPointFour.label(), "2.4 GHz");
    }
}
