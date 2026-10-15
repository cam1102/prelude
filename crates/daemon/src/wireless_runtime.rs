use std::{
    collections::{HashMap, HashSet},
    fmt,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::RecvTimeoutError,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use platform_superbird::{AccessoryIdentitySource, SuperbirdIdentitySource};
use prelude_wireless::{
    access_point::{self, AccessPoint, AccessPointConfig, ChannelWidth, StationControl},
    bluez::{
        BluetoothConfig, BluetoothControl, BluetoothService, PhoneKind, RfcommConnection,
        RfcommProfile,
    },
    hfp, wpp,
};

use crate::{
    DaemonConfig, Error, Result, WirelessConfig,
    known_phones::{self, KnownPhone},
};

const SUPERVISOR_POLL: Duration = Duration::from_millis(100);
const ACCESS_POINT_CHECK_INTERVAL: Duration = Duration::from_secs(1);
const ACCESS_POINT_RETRY: Duration = Duration::from_secs(5);
const BLUETOOTH_RETRY: Duration = Duration::from_secs(5);
const RECONNECT_DELAY: Duration = Duration::from_secs(3);
const RECONNECT_INTERVAL: Duration = Duration::from_secs(20);
const FOCUS_WINDOW: Duration = Duration::from_secs(60);
const ACCESS_POINT_WAIT: Duration = Duration::from_secs(20);
const RFCOMM_WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const PROJECTION_WRITE_TIMEOUT: Duration = Duration::from_secs(4);
const PASSPHRASE_LEN: usize = 16;
const PASSPHRASE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";

#[derive(Clone, PartialEq, Eq)]
pub struct AccessPointInfo {
    pub ssid: String,
    pub passphrase: String,
    pub bssid: String,
    pub channel: u8,
    pub frequency_mhz: u32,
    pub ipv4: Ipv4Addr,
    pub ipv6_link_local: Option<Ipv6Addr>,
    pub interface: String,
    pub interface_index: u32,
    pub stations: StationControl,
}

impl fmt::Debug for AccessPointInfo {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AccessPointInfo")
            .field("ssid", &self.ssid)
            .field("passphrase", &"[redacted]")
            .field("bssid", &self.bssid)
            .field("channel", &self.channel)
            .field("ipv4", &self.ipv4)
            .field("ipv6_link_local", &self.ipv6_link_local)
            .field("interface", &self.interface)
            .finish()
    }
}

pub(crate) type SharedAccessPoint = Arc<RwLock<Option<AccessPointInfo>>>;

#[derive(Debug)]
struct PhoneSlot<T> {
    connection: Mutex<Option<T>>,
    in_use: Arc<AtomicBool>,
    ended_by_phone: Arc<AtomicBool>,
}

pub struct WirelessPhone<T> {
    id: u64,
    peer: String,
    slot: Arc<PhoneSlot<T>>,
}

pub type WirelessAndroidAutoPhone = WirelessPhone<TcpStream>;
pub type WirelessCarPlayPhone = WirelessPhone<RfcommConnection>;

impl<T> Clone for WirelessPhone<T> {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            peer: self.peer.clone(),
            slot: Arc::clone(&self.slot),
        }
    }
}

impl<T> fmt::Debug for WirelessPhone<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WirelessPhone")
            .field("id", &self.id)
            .field("peer", &self.peer)
            .field("in_use", &self.is_in_use())
            .finish()
    }
}

impl<T> PartialEq for WirelessPhone<T> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl<T> Eq for WirelessPhone<T> {}

impl<T> WirelessPhone<T> {
    pub fn peer(&self) -> &str {
        &self.peer
    }

    pub fn is_in_use(&self) -> bool {
        self.slot.in_use.load(Ordering::Acquire)
    }

    pub fn claim(&self) -> Option<(T, WirelessLease)> {
        let connection = self.slot.connection.lock().ok()?.take()?;
        self.slot.in_use.store(true, Ordering::Release);
        Some((
            connection,
            WirelessLease {
                in_use: Arc::clone(&self.slot.in_use),
                ended_by_phone: Arc::clone(&self.slot.ended_by_phone),
            },
        ))
    }

    fn is_finished(&self) -> bool {
        !self.is_in_use()
            && self
                .slot
                .connection
                .lock()
                .map_or(true, |connection| connection.is_none())
    }
}

#[derive(Debug)]
pub struct WirelessLease {
    in_use: Arc<AtomicBool>,
    ended_by_phone: Arc<AtomicBool>,
}

impl WirelessLease {
    pub fn ended_by_phone(&self) {
        self.ended_by_phone.store(true, Ordering::Release);
    }
}

impl Drop for WirelessLease {
    fn drop(&mut self) {
        self.in_use.store(false, Ordering::Release);
    }
}

pub struct WirelessPhones<T> {
    phones: Arc<Mutex<Vec<WirelessPhone<T>>>>,
    next_id: Arc<AtomicU64>,
    ended_by_phone: Arc<AtomicBool>,
}

impl<T> Clone for WirelessPhones<T> {
    fn clone(&self) -> Self {
        Self {
            phones: Arc::clone(&self.phones),
            next_id: Arc::clone(&self.next_id),
            ended_by_phone: Arc::clone(&self.ended_by_phone),
        }
    }
}

impl<T> Default for WirelessPhones<T> {
    fn default() -> Self {
        Self {
            phones: Arc::default(),
            next_id: Arc::default(),
            ended_by_phone: Arc::default(),
        }
    }
}

impl<T> fmt::Debug for WirelessPhones<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WirelessPhones")
            .field(
                "phones",
                &self.phones.lock().map(|phones| phones.len()).ok(),
            )
            .finish()
    }
}

impl<T> WirelessPhones<T> {
    pub fn snapshot(&self) -> Vec<WirelessPhone<T>> {
        let Ok(mut phones) = self.phones.lock() else {
            return Vec::new();
        };
        phones.retain(|phone| !phone.is_finished());
        phones.clone()
    }

    fn take_ended_by_phone(&self) -> bool {
        self.ended_by_phone.swap(false, Ordering::AcqRel)
    }

    fn offer(&self, connection: T, peer: String) -> bool {
        let Ok(mut phones) = self.phones.lock() else {
            return false;
        };
        phones.retain(|phone| !phone.is_finished());
        if phones.iter().any(WirelessPhone::is_in_use) {
            return false;
        }
        phones.clear();
        phones.push(WirelessPhone {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            peer,
            slot: Arc::new(PhoneSlot {
                connection: Mutex::new(Some(connection)),
                in_use: Arc::new(AtomicBool::new(false)),
                ended_by_phone: Arc::clone(&self.ended_by_phone),
            }),
        });
        true
    }
}

pub(crate) type SharedBluetoothAddress = Arc<RwLock<Option<[u8; 6]>>>;
pub(crate) type SharedBluetoothControl = Arc<RwLock<Option<BluetoothControl>>>;
type SharedPeer = Arc<Mutex<Option<String>>>;
type SharedFocus = Arc<Mutex<Option<(String, Instant)>>>;
type SharedDismissed = Arc<Mutex<HashSet<String>>>;
type SharedStation = Arc<Mutex<Option<String>>>;
type SharedDenied = Arc<Mutex<HashMap<String, String>>>;
type SharedRadio = Arc<RwLock<(u8, u16)>>;

#[derive(Debug, Clone)]
pub struct WirelessPhoneDirectory {
    state_dir: PathBuf,
    android_auto: WirelessPhones<TcpStream>,
    carplay: WirelessPhones<RfcommConnection>,
    android_auto_peer: SharedPeer,
    bluetooth_control: SharedBluetoothControl,
    focus: SharedFocus,
    dismissed: SharedDismissed,
    access_point: SharedAccessPoint,
    android_auto_station: SharedStation,
    denied: SharedDenied,
}

impl WirelessPhoneDirectory {
    pub fn phones(&self) -> Vec<KnownPhone> {
        known_phones::load(&self.state_dir)
    }

    pub fn active(&self) -> Option<String> {
        active_peer(&self.carplay, &self.android_auto, &self.android_auto_peer)
    }

    pub fn connect(&self, address: &str) -> io::Result<()> {
        let phone = known_phones::promote(&self.state_dir, address)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown phone"))?;
        set_dismissed(&self.dismissed, &phone.address, false);
        self.admit_station(&phone.address);
        if let Ok(mut focus) = self.focus.lock() {
            *focus = Some((phone.address.clone(), Instant::now() + FOCUS_WINDOW));
        }
        for queued in self.carplay.snapshot() {
            if queued.peer() != phone.address {
                drop(queued.claim());
            }
        }
        let android_auto_queued_for_target = self
            .android_auto_peer
            .lock()
            .is_ok_and(|peer| peer.as_deref() == Some(phone.address.as_str()));
        if !android_auto_queued_for_target {
            for queued in self.android_auto.snapshot() {
                drop(queued.claim());
            }
        }
        if let Some(control) = current_control(&self.bluetooth_control) {
            control.connect(&phone.address, phone.kind);
        }
        Ok(())
    }

    pub fn disconnect(&self, address: &str) {
        if let Some(control) = current_control(&self.bluetooth_control) {
            control.disconnect(address);
        }
    }

    pub fn dismiss(&self, address: &str) {
        set_dismissed(&self.dismissed, address, true);
        if let Ok(mut focus) = self.focus.lock()
            && focus.as_ref().is_some_and(|(target, _)| target == address)
        {
            *focus = None;
        }
        for queued in self.carplay.snapshot() {
            if queued.peer() == address {
                drop(queued.claim());
            }
        }
        if self
            .android_auto_peer
            .lock()
            .is_ok_and(|peer| peer.as_deref() == Some(address))
        {
            for queued in self.android_auto.snapshot() {
                drop(queued.claim());
            }
        }
        self.disconnect(address);
        self.deny_station(address);
    }

    pub fn forget(&self, address: &str) -> io::Result<()> {
        known_phones::forget(&self.state_dir, address)?;
        set_dismissed(&self.dismissed, address, false);
        self.admit_station(address);
        if let Ok(mut focus) = self.focus.lock()
            && focus.as_ref().is_some_and(|(target, _)| target == address)
        {
            *focus = None;
        }
        if let Some(control) = current_control(&self.bluetooth_control) {
            control.remove(address);
        }
        Ok(())
    }

    fn deny_station(&self, address: &str) {
        let owns_station = self
            .android_auto_peer
            .lock()
            .is_ok_and(|peer| peer.as_deref() == Some(address));
        let Some(station) = owns_station
            .then(|| self.android_auto_station.lock().ok()?.clone())
            .flatten()
        else {
            return;
        };
        if let Ok(mut denied) = self.denied.lock() {
            denied.insert(address.to_owned(), station.clone());
        }
        let Some(stations) = current_stations(&self.access_point) else {
            return;
        };
        match stations.deny(&station) {
            Ok(()) => tracing::info!(peer = address, station, "kicked the phone off Wi-Fi"),
            Err(error) => {
                tracing::warn!(peer = address, station, %error, "kicking the phone off Wi-Fi failed");
            }
        }
    }

    fn admit_station(&self, address: &str) {
        let Some(station) = self
            .denied
            .lock()
            .ok()
            .and_then(|mut denied| denied.remove(address))
        else {
            return;
        };
        if let Some(stations) = current_stations(&self.access_point)
            && let Err(error) = stations.admit(&station)
        {
            tracing::warn!(peer = address, station, %error, "readmitting the phone to Wi-Fi failed");
        }
    }
}

fn current_stations(access_point: &SharedAccessPoint) -> Option<StationControl> {
    access_point
        .read()
        .ok()?
        .as_ref()
        .map(|info| info.stations.clone())
}

fn active_peer(
    carplay: &WirelessPhones<RfcommConnection>,
    android_auto: &WirelessPhones<TcpStream>,
    android_auto_peer: &SharedPeer,
) -> Option<String> {
    if let Some(phone) = carplay
        .snapshot()
        .into_iter()
        .find(WirelessPhone::is_in_use)
    {
        return Some(phone.peer().to_owned());
    }
    if android_auto.snapshot().iter().any(WirelessPhone::is_in_use) {
        return android_auto_peer.lock().ok()?.clone();
    }
    None
}

fn current_control(control: &SharedBluetoothControl) -> Option<BluetoothControl> {
    control.read().ok().and_then(|control| control.clone())
}

fn set_dismissed(dismissed: &SharedDismissed, address: &str, value: bool) {
    let Ok(mut dismissed) = dismissed.lock() else {
        return;
    };
    if value {
        dismissed.insert(address.to_owned());
    } else {
        dismissed.remove(address);
    }
}

fn is_dismissed(dismissed: &SharedDismissed, address: &str) -> bool {
    dismissed
        .lock()
        .is_ok_and(|dismissed| dismissed.contains(address))
}

fn focus_allows(focus: &SharedFocus, peer: &str) -> bool {
    let Ok(mut focus) = focus.lock() else {
        return true;
    };
    match focus.as_ref() {
        Some((_, until)) if Instant::now() >= *until => {
            *focus = None;
            true
        }
        Some((target, _)) => target == peer,
        None => true,
    }
}

#[derive(Debug)]
pub struct WirelessService {
    android_auto: WirelessPhones<TcpStream>,
    carplay: WirelessPhones<RfcommConnection>,
    access_point: SharedAccessPoint,
    bluetooth_address: SharedBluetoothAddress,
    bluetooth_control: SharedBluetoothControl,
    directory: WirelessPhoneDirectory,
    radio: SharedRadio,
    phone_audio: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl WirelessService {
    pub fn start(config: &DaemonConfig) -> Result<Option<Self>> {
        let wireless = &config.wireless;
        if !wireless.enabled {
            return Ok(None);
        }
        let android_auto = WirelessPhones::default();
        let carplay = WirelessPhones::default();
        let access_point = SharedAccessPoint::default();
        let bluetooth_address = SharedBluetoothAddress::default();
        let bluetooth_control = SharedBluetoothControl::default();
        let radio = Arc::new(RwLock::new((wireless.channel, wireless.channel_width_mhz)));
        let phone_audio = Arc::new(AtomicBool::new(config.android_auto.disable_audio_output));
        let stop = Arc::new(AtomicBool::new(false));
        let state_dir = wireless_state_dir(&config.state_dir);
        let directory = WirelessPhoneDirectory {
            state_dir: state_dir.clone(),
            android_auto: android_auto.clone(),
            carplay: carplay.clone(),
            android_auto_peer: SharedPeer::default(),
            bluetooth_control: Arc::clone(&bluetooth_control),
            focus: SharedFocus::default(),
            dismissed: SharedDismissed::default(),
            access_point: Arc::clone(&access_point),
            android_auto_station: SharedStation::default(),
            denied: SharedDenied::default(),
        };
        let mut threads = vec![spawn_named("prelude-wifi-ap", {
            let state_dir = state_dir.clone();
            let wireless = wireless.clone();
            let projection = ProjectionListener {
                phones: android_auto.clone(),
                peer: Arc::clone(&directory.android_auto_peer),
                dismissed: Arc::clone(&directory.dismissed),
                station: Arc::clone(&directory.android_auto_station),
                denied: Arc::clone(&directory.denied),
                read_timeout: Duration::from_millis(config.android_auto.session_io_timeout_ms),
            };
            let access_point = Arc::clone(&access_point);
            let radio = Arc::clone(&radio);
            let stop = Arc::clone(&stop);
            move || {
                supervise_access_point(
                    &wireless,
                    &state_dir,
                    &projection,
                    &access_point,
                    &radio,
                    &stop,
                );
            }
        })?];
        threads.push(spawn_named("prelude-bluetooth", {
            let bluetooth = BluetoothConfig {
                adapter: wireless.bluetooth_adapter.clone(),
                alias: config.android_auto.head_unit_name.clone(),
                discoverable: wireless.discoverable,
                android_auto: wireless.android_auto,
                carplay: wireless.carplay,
            };
            let dispatch = BluetoothDispatch {
                wireless: wireless.clone(),
                access_point: Arc::clone(&access_point),
                android_auto: android_auto.clone(),
                carplay: carplay.clone(),
                bluetooth_address: Arc::clone(&bluetooth_address),
                bluetooth_control: Arc::clone(&bluetooth_control),
                android_auto_peer: Arc::clone(&directory.android_auto_peer),
                focus: Arc::clone(&directory.focus),
                dismissed: Arc::clone(&directory.dismissed),
                directory: directory.clone(),
                phone_audio: Arc::clone(&phone_audio),
                state_dir,
                stop: Arc::clone(&stop),
            };
            move || dispatch.run(&bluetooth)
        })?);
        Ok(Some(Self {
            android_auto,
            carplay,
            access_point,
            bluetooth_address,
            bluetooth_control,
            directory,
            radio,
            phone_audio,
            stop,
            threads,
        }))
    }

    pub fn set_radio(&self, channel: u8, width: ChannelWidth) {
        if let Ok(mut radio) = self.radio.write() {
            *radio = (channel, width.mhz());
        }
    }

    pub fn set_phone_audio(&self, enabled: bool) {
        self.phone_audio.store(enabled, Ordering::Relaxed);
    }

    pub fn directory(&self) -> WirelessPhoneDirectory {
        self.directory.clone()
    }

    pub fn android_auto_phones(&self) -> WirelessPhones<TcpStream> {
        self.android_auto.clone()
    }

    pub fn carplay_phones(&self) -> WirelessPhones<RfcommConnection> {
        self.carplay.clone()
    }

    pub(crate) fn shared_access_point(&self) -> SharedAccessPoint {
        Arc::clone(&self.access_point)
    }

    pub(crate) fn shared_bluetooth_address(&self) -> SharedBluetoothAddress {
        Arc::clone(&self.bluetooth_address)
    }

    pub(crate) fn shared_bluetooth_control(&self) -> SharedBluetoothControl {
        Arc::clone(&self.bluetooth_control)
    }

    pub fn access_point(&self) -> Option<AccessPointInfo> {
        self.access_point.read().ok()?.clone()
    }
}

impl Drop for WirelessService {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

fn spawn_named(name: &str, body: impl FnOnce() + Send + 'static) -> Result<JoinHandle<()>> {
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(body)
        .map_err(|error| Error::Runtime(format!("spawning {name} failed: {error}")))
}

fn sleep_unless_stopped(stop: &AtomicBool, duration: Duration) {
    let deadline = Instant::now() + duration;
    while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
        thread::sleep(SUPERVISOR_POLL);
    }
}

struct ProjectionListener {
    phones: WirelessPhones<TcpStream>,
    peer: SharedPeer,
    dismissed: SharedDismissed,
    station: SharedStation,
    denied: SharedDenied,
    read_timeout: Duration,
}

fn supervise_access_point(
    wireless: &WirelessConfig,
    state_dir: &Path,
    projection: &ProjectionListener,
    shared: &SharedAccessPoint,
    radio: &SharedRadio,
    stop: &AtomicBool,
) {
    let mut waiting_logged = false;
    while !stop.load(Ordering::Relaxed) {
        let (channel, channel_width_mhz) = current_radio(radio, wireless);
        let wireless = WirelessConfig {
            channel,
            channel_width_mhz,
            ..wireless.clone()
        };
        let interface = match &wireless.wifi_interface {
            Some(interface) => Some(interface.clone())
                .filter(|interface| Path::new("/sys/class/net").join(interface).exists()),
            None => prelude_wireless::access_point::detect_interface(),
        };
        let Some(interface) = interface else {
            if !waiting_logged {
                tracing::info!("waiting for a USB Wi-Fi adapter");
                waiting_logged = true;
            }
            sleep_unless_stopped(stop, Duration::from_secs(1));
            continue;
        };
        waiting_logged = false;
        if let Err(error) = run_access_point(
            &wireless, &interface, state_dir, projection, shared, radio, stop,
        ) {
            tracing::warn!(interface, %error, "wireless access point stopped");
        }
        if let Ok(mut shared) = shared.write() {
            *shared = None;
        }
        let (channel, channel_width_mhz) = current_radio(radio, &wireless);
        if (channel, channel_width_mhz) != (wireless.channel, wireless.channel_width_mhz) {
            tracing::info!(
                channel,
                channel_width_mhz,
                "restarting the access point with the new Wi-Fi settings"
            );
            continue;
        }
        sleep_unless_stopped(stop, ACCESS_POINT_RETRY);
    }
}

fn run_access_point(
    wireless: &WirelessConfig,
    interface: &str,
    state_dir: &Path,
    projection: &ProjectionListener,
    shared: &SharedAccessPoint,
    radio: &SharedRadio,
    stop: &AtomicBool,
) -> Result<()> {
    let config = resolve_access_point(wireless, interface, state_dir)?;
    let mut access_point = AccessPoint::start(config)
        .map_err(|error| Error::Runtime(format!("starting the access point failed: {error}")))?;
    let listener = TcpListener::bind(SocketAddrV4::new(
        wireless.access_point_address,
        wireless.android_auto_port,
    ))
    .and_then(|listener| listener.set_nonblocking(true).map(|()| listener))
    .map_err(|error| {
        Error::Runtime(format!(
            "binding the Android Auto projection listener failed: {error}"
        ))
    })?;
    publish(shared, &mut access_point);
    deny_stations(&access_point.stations(), &projection.denied);

    let mut next_check = Instant::now() + ACCESS_POINT_CHECK_INTERVAL;
    while !stop.load(Ordering::Relaxed)
        && current_radio(radio, wireless) == (wireless.channel, wireless.channel_width_mhz)
    {
        match listener.accept() {
            Ok((stream, peer)) => accept_projection(wireless, interface, projection, stream, peer),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(SUPERVISOR_POLL);
            }
            Err(error) => tracing::warn!(%error, "accepting a projection connection failed"),
        }
        if Instant::now() >= next_check {
            access_point
                .check()
                .map_err(|error| Error::Runtime(error.to_string()))?;
            if shared.read().is_ok_and(|info| {
                info.as_ref()
                    .is_some_and(|info| info.ipv6_link_local.is_none())
            }) {
                publish(shared, &mut access_point);
            }
            next_check = Instant::now() + ACCESS_POINT_CHECK_INTERVAL;
        }
    }
    Ok(())
}

fn deny_stations(stations: &StationControl, denied: &SharedDenied) {
    let denied: Vec<String> = denied
        .lock()
        .map(|denied| denied.values().cloned().collect())
        .unwrap_or_default();
    for station in denied {
        if let Err(error) = stations.deny(&station) {
            tracing::warn!(station, %error, "keeping a disconnected phone off Wi-Fi failed");
        }
    }
}

fn current_radio(radio: &SharedRadio, wireless: &WirelessConfig) -> (u8, u16) {
    radio
        .read()
        .map_or((wireless.channel, wireless.channel_width_mhz), |radio| {
            *radio
        })
}

fn publish(shared: &SharedAccessPoint, access_point: &mut AccessPoint) {
    let ipv6_link_local = access_point.refresh_link_local();
    let config = access_point.config();
    let status = access_point.status();
    let info = AccessPointInfo {
        ssid: config.ssid.clone(),
        passphrase: config.passphrase.clone(),
        bssid: status.bssid.clone(),
        channel: config.channel,
        frequency_mhz: config.frequency_mhz(),
        ipv4: status.ipv4,
        ipv6_link_local,
        interface: config.interface.clone(),
        interface_index: status.interface_index,
        stations: access_point.stations(),
    };
    if let Ok(mut shared) = shared.write() {
        *shared = Some(info);
    }
}

fn accept_projection(
    wireless: &WirelessConfig,
    interface: &str,
    projection: &ProjectionListener,
    stream: TcpStream,
    peer: SocketAddr,
) {
    let address = match peer {
        SocketAddr::V4(peer)
            if peer.ip().octets()[..3] == wireless.access_point_address.octets()[..3] =>
        {
            Some(*peer.ip())
        }
        _ => None,
    };
    let Some(address) = address else {
        tracing::warn!(%peer, "rejecting a projection connection from outside the access point");
        return;
    };
    let bootstrapped_by = projection.peer.lock().ok().and_then(|peer| peer.clone());
    if bootstrapped_by.is_some_and(|address| is_dismissed(&projection.dismissed, &address)) {
        tracing::info!(%peer, "ignoring a projection connection from a disconnected phone");
        return;
    }
    let configured = stream
        .set_nonblocking(false)
        .and_then(|()| stream.set_nodelay(true))
        .and_then(|()| stream.set_read_timeout(Some(projection.read_timeout)))
        .and_then(|()| stream.set_write_timeout(Some(PROJECTION_WRITE_TIMEOUT)));
    if let Err(error) = configured {
        tracing::warn!(%peer, %error, "configuring the projection connection failed");
        return;
    }
    if projection.phones.offer(stream, peer.to_string()) {
        let station = access_point::station_for(interface, address);
        tracing::info!(%peer, station, "wireless Android Auto phone connected");
        if let Ok(mut current) = projection.station.lock() {
            *current = station;
        }
    } else {
        tracing::info!(%peer, "ignoring a projection connection while a wireless session runs");
    }
}

fn resolve_access_point(
    wireless: &WirelessConfig,
    interface: &str,
    state_dir: &Path,
) -> Result<AccessPointConfig> {
    let ssid = match &wireless.ssid {
        Some(ssid) => ssid.clone(),
        None => default_ssid(interface)?,
    };
    let passphrase = match &wireless.passphrase {
        Some(passphrase) => passphrase.clone(),
        None => match device_serial_passphrase() {
            Some(serial) => serial,
            None => persisted_passphrase(state_dir)?,
        },
    };
    wireless.access_point(
        interface.to_owned(),
        ssid,
        passphrase,
        PathBuf::from("/run/prelude/wireless"),
    )
}

fn default_ssid(interface: &str) -> Result<String> {
    let address = fs::read_to_string(Path::new("/sys/class/net").join(interface).join("address"))
        .map_err(|error| {
        Error::Runtime(format!("reading the Wi-Fi adapter MAC failed: {error}"))
    })?;
    let suffix: String = address
        .trim()
        .split(':')
        .skip(4)
        .collect::<String>()
        .to_ascii_uppercase();
    Ok(format!("Prelude-{suffix}"))
}

pub(crate) fn wireless_state_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("wireless")
}

pub(crate) fn remember_phone(
    state_dir: &Path,
    control: &SharedBluetoothControl,
    kind: PhoneKind,
    peer: &str,
) -> io::Result<()> {
    let name = current_control(control).and_then(|control| control.name(peer));
    known_phones::remember(state_dir, kind, peer, name.as_deref())
}

fn device_serial_passphrase() -> Option<String> {
    let serial = match SuperbirdIdentitySource::new(env!("CARGO_PKG_VERSION")).load() {
        Ok(identity) => identity.serial_number,
        Err(error) => {
            tracing::warn!(%error, "the Car Thing serial is unavailable; using a generated Wi-Fi passphrase");
            return None;
        }
    };
    let passphrase = wpa_passphrase(&serial);
    if passphrase.is_none() {
        tracing::warn!(
            "the Car Thing serial is not a valid WPA2 passphrase; using a generated one"
        );
    }
    passphrase
}

fn wpa_passphrase(serial: &str) -> Option<String> {
    let passphrase: String = serial.chars().filter(char::is_ascii_alphanumeric).collect();
    (8..=63).contains(&passphrase.len()).then_some(passphrase)
}

fn persisted_passphrase(state_dir: &Path) -> Result<String> {
    let path = state_dir.join("passphrase");
    match fs::read_to_string(&path) {
        Ok(passphrase) if (8..=63).contains(&passphrase.trim().len()) => {
            return Ok(passphrase.trim().to_owned());
        }
        Ok(_) => {
            tracing::warn!(path = %path.display(), "regenerating an invalid stored passphrase")
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(Error::Runtime(format!(
                "reading {} failed: {error}",
                path.display()
            )));
        }
    }
    let passphrase = generate_passphrase()?;
    fs::create_dir_all(state_dir).map_err(|error| {
        Error::Runtime(format!("creating {} failed: {error}", state_dir.display()))
    })?;
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .map_err(|error| Error::Runtime(format!("creating {} failed: {error}", path.display())))?;
    file.write_all(passphrase.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|error| Error::Runtime(format!("writing {} failed: {error}", path.display())))?;
    Ok(passphrase)
}

fn generate_passphrase() -> Result<String> {
    let mut random = [0_u8; PASSPHRASE_LEN * 2];
    fs::File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut random))
        .map_err(|error| Error::Runtime(format!("reading /dev/urandom failed: {error}")))?;
    let limit = u8::MAX - u8::MAX % PASSPHRASE_ALPHABET.len() as u8;
    let passphrase: String = random
        .iter()
        .filter(|&&byte| byte < limit)
        .take(PASSPHRASE_LEN)
        .map(|&byte| char::from(PASSPHRASE_ALPHABET[usize::from(byte) % PASSPHRASE_ALPHABET.len()]))
        .collect();
    if passphrase.len() == PASSPHRASE_LEN {
        Ok(passphrase)
    } else {
        generate_passphrase()
    }
}

struct BluetoothDispatch {
    wireless: WirelessConfig,
    access_point: SharedAccessPoint,
    android_auto: WirelessPhones<TcpStream>,
    carplay: WirelessPhones<RfcommConnection>,
    bluetooth_address: SharedBluetoothAddress,
    bluetooth_control: SharedBluetoothControl,
    android_auto_peer: SharedPeer,
    focus: SharedFocus,
    dismissed: SharedDismissed,
    directory: WirelessPhoneDirectory,
    phone_audio: Arc<AtomicBool>,
    state_dir: PathBuf,
    stop: Arc<AtomicBool>,
}

impl BluetoothDispatch {
    fn run(&self, bluetooth: &BluetoothConfig) {
        while !self.stop.load(Ordering::Relaxed) {
            let (service, incoming) = match BluetoothService::start(bluetooth.clone()) {
                Ok(started) => started,
                Err(error) => {
                    tracing::warn!(%error, "Bluetooth is not ready; retrying");
                    sleep_unless_stopped(&self.stop, BLUETOOTH_RETRY);
                    continue;
                }
            };
            if let Ok(mut address) = self.bluetooth_address.write() {
                *address = Some(service.address());
            }
            if let Ok(mut control) = self.bluetooth_control.write() {
                *control = Some(service.control());
            }
            let mut next_page = Instant::now() + RECONNECT_DELAY;
            while !self.stop.load(Ordering::Relaxed) && service.is_running() {
                match incoming.recv_timeout(Duration::from_millis(500)) {
                    Ok(connection) => self.accept(connection),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
                if self.android_auto.take_ended_by_phone() {
                    self.dismiss_android_auto_peer();
                }
                if Instant::now() >= next_page {
                    next_page = Instant::now() + RECONNECT_INTERVAL;
                    self.page_last_phone(&service);
                }
            }
            if let Ok(mut control) = self.bluetooth_control.write() {
                *control = None;
            }
            drop(service);
        }
    }

    fn dismiss_android_auto_peer(&self) {
        let Some(peer) = self
            .android_auto_peer
            .lock()
            .ok()
            .and_then(|peer| peer.clone())
        else {
            return;
        };
        tracing::info!(
            peer,
            "the phone ended Android Auto; disconnecting it until it is chosen in settings"
        );
        self.directory.dismiss(&peer);
    }

    fn page_last_phone(&self, service: &BluetoothService) {
        if !self.carplay.snapshot().is_empty() || !self.android_auto.snapshot().is_empty() {
            return;
        }
        let Some(phone) = known_phones::load(&self.state_dir).into_iter().next() else {
            return;
        };
        if is_dismissed(&self.dismissed, &phone.address) {
            return;
        }
        let enabled = match phone.kind {
            PhoneKind::CarPlay => self.wireless.carplay,
            PhoneKind::AndroidAuto => self.wireless.android_auto,
        };
        if enabled {
            tracing::debug!(
                peer = phone.address,
                kind = phone.kind.as_str(),
                "paging the last phone"
            );
            service.control().connect(&phone.address, phone.kind);
        }
    }

    fn accept(&self, connection: RfcommConnection) {
        let peer = connection.peer.clone();
        if let Err(error) = connection
            .set_read_timeout(Some(wpp::poll_interval()))
            .and_then(|()| connection.set_write_timeout(Some(RFCOMM_WRITE_TIMEOUT)))
        {
            tracing::warn!(peer, %error, "configuring the RFCOMM socket failed");
            return;
        }
        if is_dismissed(&self.dismissed, &peer) {
            tracing::info!(
                peer,
                profile = ?connection.profile,
                "turning away a disconnected phone"
            );
            drop(connection);
            if let Some(control) = current_control(&self.bluetooth_control) {
                control.disconnect(&peer);
            }
            return;
        }
        if connection.profile != RfcommProfile::HandsFree && !focus_allows(&self.focus, &peer) {
            tracing::info!(
                peer,
                "ignoring a phone while another one was chosen in settings"
            );
            return;
        }
        if connection.profile == RfcommProfile::Iap2 {
            if self.carplay.offer(connection, peer.clone()) {
                tracing::info!(peer, "wireless CarPlay phone connected over Bluetooth");
            } else {
                tracing::info!(
                    peer,
                    "ignoring an iAP2 channel while a wireless CarPlay session runs"
                );
            }
            return;
        }
        let links = RfcommLinks {
            wireless: self.wireless.clone(),
            android_auto: self.android_auto.clone(),
            carplay: self.carplay.clone(),
            phone_audio: Arc::clone(&self.phone_audio),
            access_point: Arc::clone(&self.access_point),
            bluetooth_control: Arc::clone(&self.bluetooth_control),
            android_auto_peer: Arc::clone(&self.android_auto_peer),
            state_dir: self.state_dir.clone(),
        };
        let stop = Arc::clone(&self.stop);
        let spawned = spawn_named("prelude-rfcomm", move || {
            handle_rfcomm(connection, &links, &stop);
        });
        if let Err(error) = spawned {
            tracing::warn!(peer, %error, "dropping an RFCOMM connection");
        }
    }
}

struct RfcommLinks {
    wireless: WirelessConfig,
    android_auto: WirelessPhones<TcpStream>,
    carplay: WirelessPhones<RfcommConnection>,
    phone_audio: Arc<AtomicBool>,
    access_point: SharedAccessPoint,
    bluetooth_control: SharedBluetoothControl,
    android_auto_peer: SharedPeer,
    state_dir: PathBuf,
}

impl RfcommLinks {
    fn keeps_calls_on_phone(&self, peer: &str) -> bool {
        self.phone_audio.load(Ordering::Relaxed)
            && active_peer(&self.carplay, &self.android_auto, &self.android_auto_peer).as_deref()
                == Some(peer)
    }
}

fn handle_rfcomm(mut connection: RfcommConnection, links: &RfcommLinks, stop: &Arc<AtomicBool>) {
    let peer = connection.peer.clone();
    let profile = connection.profile;
    let result = match profile {
        RfcommProfile::AndroidAuto => bootstrap_android_auto(&mut connection, &peer, links, stop),
        RfcommProfile::HandsFree => run_hands_free(&mut connection, &peer, links, stop),
        RfcommProfile::Iap2 => Ok(()),
    };
    match result {
        Ok(()) => tracing::info!(peer, ?profile, "RFCOMM channel closed"),
        Err(error) => tracing::warn!(peer, ?profile, %error, "RFCOMM channel failed"),
    }
}

fn run_hands_free(
    connection: &mut RfcommConnection,
    peer: &str,
    links: &RfcommLinks,
    stop: &Arc<AtomicBool>,
) -> Result<()> {
    hfp::run_hands_free(
        connection,
        || stop.load(Ordering::Relaxed) || links.keeps_calls_on_phone(peer),
        |event| tracing::info!(peer, ?event, "hands-free"),
    )
    .map_err(|error| Error::Runtime(error.to_string()))?;
    if links.keeps_calls_on_phone(peer)
        && let Some(control) = current_control(&links.bluetooth_control)
    {
        control.release_hands_free(peer);
    }
    Ok(())
}

fn bootstrap_android_auto(
    connection: &mut RfcommConnection,
    peer: &str,
    links: &RfcommLinks,
    stop: &Arc<AtomicBool>,
) -> Result<()> {
    let wireless = &links.wireless;
    if !wireless.android_auto {
        return Ok(());
    }
    let info =
        wait_for_access_point(&links.access_point, stop, ACCESS_POINT_WAIT).ok_or_else(|| {
            Error::Runtime(
                "the access point is not up; is the USB Wi-Fi adapter connected?".to_owned(),
            )
        })?;
    let config = wpp::BootstrapConfig {
        endpoint: SocketAddrV4::new(info.ipv4, wireless.android_auto_port),
        access_point: wpp::AccessPointCredentials {
            ssid: info.ssid,
            passphrase: info.passphrase,
            bssid: info.bssid,
            frequency_mhz: info.frequency_mhz,
        },
    };
    tracing::info!(peer, "starting the Android Auto wireless bootstrap");
    wpp::run_bootstrap(connection, config, stop, |event| {
        tracing::info!(peer, ?event, "Android Auto wireless bootstrap");
        if event.phone_accepted() {
            if let Ok(mut joined) = links.android_auto_peer.lock() {
                *joined = Some(peer.to_owned());
            }
            if let Err(error) = remember_phone(
                &links.state_dir,
                &links.bluetooth_control,
                PhoneKind::AndroidAuto,
                peer,
            ) {
                tracing::warn!(peer, %error, "remembering the Android Auto phone failed");
            }
        }
    })
    .map_err(|error| Error::Runtime(error.to_string()))
}

pub(crate) fn wait_for_access_point(
    access_point: &SharedAccessPoint,
    stop: &AtomicBool,
    timeout: Duration,
) -> Option<AccessPointInfo> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(info) = access_point.read().ok().and_then(|info| info.clone()) {
            return Some(info);
        }
        if stop.load(Ordering::Relaxed) || Instant::now() >= deadline {
            return None;
        }
        thread::sleep(SUPERVISOR_POLL);
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;

    fn connected_pair() -> (TcpStream, SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let client = TcpStream::connect(listener.local_addr().expect("addr")).expect("connect");
        let peer = client.local_addr().expect("peer");
        let (server, _) = listener.accept().expect("accept");
        drop(client);
        (server, peer)
    }

    #[test]
    fn a_chosen_phone_holds_focus_until_it_expires() {
        let focus = SharedFocus::default();
        assert!(focus_allows(&focus, "A8:AB:B5:AB:02:ED"));

        *focus.lock().expect("focus") = Some((
            "A8:AB:B5:AB:02:ED".to_owned(),
            Instant::now() + FOCUS_WINDOW,
        ));
        assert!(focus_allows(&focus, "A8:AB:B5:AB:02:ED"));
        assert!(!focus_allows(&focus, "10:2B:41:7C:00:9E"));

        *focus.lock().expect("focus") = Some(("A8:AB:B5:AB:02:ED".to_owned(), Instant::now()));
        assert!(focus_allows(&focus, "10:2B:41:7C:00:9E"));
        assert!(focus.lock().expect("focus").is_none());
    }

    #[test]
    fn a_claimed_phone_stays_listed_until_its_lease_drops() {
        let phones = WirelessPhones::<TcpStream>::default();
        let (stream, peer) = connected_pair();
        assert!(phones.offer(stream, peer.to_string()));
        let listed = phones.snapshot();
        assert_eq!(listed.len(), 1);

        let (_stream, lease) = listed[0].claim().expect("claim");
        assert!(listed[0].claim().is_none());
        assert_eq!(phones.snapshot().len(), 1);

        let (stream, peer) = connected_pair();
        assert!(
            !phones.offer(stream, peer.to_string()),
            "a running session keeps its slot"
        );

        drop(lease);
        assert!(phones.snapshot().is_empty());
    }

    #[test]
    fn a_phone_ended_session_is_reported_once() {
        let phones = WirelessPhones::<TcpStream>::default();
        let (stream, peer) = connected_pair();
        assert!(phones.offer(stream, peer.to_string()));
        let (_stream, lease) = phones.snapshot()[0].claim().expect("claim");
        assert!(!phones.take_ended_by_phone());
        lease.ended_by_phone();
        drop(lease);
        assert!(phones.take_ended_by_phone());
        assert!(!phones.take_ended_by_phone());
    }

    #[test]
    fn an_unclaimed_connection_is_replaced_by_a_retry() {
        let phones = WirelessPhones::<TcpStream>::default();
        let (first, peer) = connected_pair();
        assert!(phones.offer(first, peer.to_string()));
        let first_id = phones.snapshot()[0].id;
        let (second, peer) = connected_pair();
        assert!(phones.offer(second, peer.to_string()));
        let listed = phones.snapshot();
        assert_eq!(listed.len(), 1);
        assert_ne!(listed[0].id, first_id);
    }

    #[test]
    fn device_serials_become_passphrases_only_when_wpa2_accepts_them() {
        assert_eq!(
            wpa_passphrase("8556-R235-0Q1A\n").as_deref(),
            Some("8556R2350Q1A")
        );
        assert_eq!(wpa_passphrase("12.34_56 78").as_deref(), Some("12345678"));
        assert_eq!(wpa_passphrase("12-34-56"), None);
        assert_eq!(wpa_passphrase(&"A".repeat(64)), None);
    }

    #[test]
    fn passphrases_are_persisted_and_well_formed() {
        let directory = tempfile::tempdir().expect("tempdir");
        let first = persisted_passphrase(directory.path()).expect("generate");
        assert_eq!(first.len(), PASSPHRASE_LEN);
        assert!(
            first
                .bytes()
                .all(|byte| PASSPHRASE_ALPHABET.contains(&byte))
        );
        assert_eq!(
            persisted_passphrase(directory.path()).expect("reload"),
            first
        );
    }
}
