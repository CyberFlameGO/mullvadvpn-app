mod driver;
mod path_monitor;
mod volume_monitor;
mod windows;

use crate::{
    tunnel::TunnelMetadata,
    tunnel_state_machine::TunnelCommand,
    windows::{get_best_route, AddressFamily},
    winnet::{
        self, get_best_default_route, interface_luid_to_ip, WinNetAddrFamily, WinNetCallbackHandle,
    },
};
use futures::channel::{mpsc, oneshot};
use std::{
    collections::HashSet,
    ffi::{OsStr, OsString},
    fmt::Write,
    io, mem,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddrV4},
    os::windows::io::{AsRawHandle, RawHandle},
    ptr,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc as sync_mpsc, Arc, Mutex, Weak,
    },
    time::Duration,
};
use talpid_types::{tunnel::ErrorStateCause, ErrorExt};
use widestring::WideCStr;
use winapi::{
    shared::{
        ifdef::NET_LUID,
        minwindef::{FALSE, TRUE},
        netioapi::MIB_IPFORWARD_ROW2,
    },
    um::{
        handleapi::CloseHandle,
        ioapiset::GetOverlappedResult,
        minwinbase::OVERLAPPED,
        synchapi::{CreateEventW, SetEvent, WaitForMultipleObjects, WaitForSingleObject},
        winbase::{INFINITE, WAIT_ABANDONED_0, WAIT_OBJECT_0},
    },
};

const DRIVER_EVENT_BUFFER_SIZE: usize = 2048;
const RESERVED_IP_V4: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 123);

const PUBLIC_INTERNET_ADDRESS_V4: Ipv4Addr = Ipv4Addr::new(193, 138, 218, 78);

/// Errors that may occur in [`SplitTunnel`].
#[derive(err_derive::Error, Debug)]
#[error(no_from)]
pub enum Error {
    /// Failed to initialize the driver
    #[error(display = "Failed to initialize driver")]
    InitializationError(#[error(source)] driver::DeviceHandleError),

    /// Failed to set paths to excluded applications
    #[error(display = "Failed to set list of excluded applications")]
    SetConfiguration(#[error(source)] io::Error),

    /// Failed to register interface IP addresses
    #[error(display = "Failed to register IP addresses for exclusions")]
    RegisterIps(#[error(source)] io::Error),

    /// Failed to clear interface IP addresses
    #[error(display = "Failed to clear registered IP addresses")]
    ClearIps(#[error(source)] io::Error),

    /// Failed to set up the driver event loop
    #[error(display = "Failed to set up the driver event loop")]
    EventThreadError(#[error(source)] io::Error),

    /// Failed to obtain default route
    #[error(display = "Failed to obtain the default route")]
    ObtainDefaultRoute(#[error(source)] winnet::Error),

    /// Failed to obtain an IP address given a network interface LUID
    #[error(display = "Failed to obtain IP address for interface LUID")]
    LuidToIp(#[error(source)] winnet::Error),

    /// Failed to set up callback for monitoring default route changes
    #[error(display = "Failed to register default route change callback")]
    RegisterRouteChangeCallback,

    /// Unexpected IP parsing error
    #[error(display = "Failed to parse IP address")]
    IpParseError,

    /// The request handling thread is stuck
    #[error(display = "The ST request thread is stuck")]
    RequestThreadStuck,

    /// The request handling thread is down
    #[error(display = "The ST request thread is down")]
    RequestThreadDown,

    /// Failed to start the NTFS reparse point monitor
    #[error(display = "Failed to start path monitor")]
    StartPathMonitor(#[error(source)] io::Error),

    /// A previous path update has not yet completed
    #[error(display = "A previous update is not yet complete")]
    AlreadySettingPaths,
}

/// Manages applications whose traffic to exclude from the tunnel.
pub struct SplitTunnel {
    runtime: tokio::runtime::Handle,
    request_tx: RequestTx,
    event_thread: Option<std::thread::JoinHandle<()>>,
    quit_event: Arc<QuitEvent>,
    _route_change_callback: Option<WinNetCallbackHandle>,
    daemon_tx: Weak<mpsc::UnboundedSender<TunnelCommand>>,
    async_path_update_in_progress: Arc<AtomicBool>,
}

struct QuitEvent(RawHandle);

unsafe impl Send for QuitEvent {}
unsafe impl Sync for QuitEvent {}

impl QuitEvent {
    fn new() -> Self {
        Self(unsafe { CreateEventW(ptr::null_mut(), TRUE, FALSE, ptr::null()) })
    }

    fn set_event(&self) -> io::Result<()> {
        if unsafe { SetEvent(self.0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for QuitEvent {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

enum Request {
    SetPaths(Vec<OsString>),
    RegisterIps(InterfaceAddresses),
}
type RequestResponseTx = sync_mpsc::Sender<Result<(), Error>>;
type RequestTx = sync_mpsc::Sender<(Request, RequestResponseTx)>;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default, PartialEq, Clone)]
struct InterfaceAddresses {
    tunnel_ipv4: Option<Ipv4Addr>,
    tunnel_ipv6: Option<Ipv6Addr>,
    internet_ipv4: Option<Ipv4Addr>,
    internet_ipv6: Option<Ipv6Addr>,
}

struct EventThreadContext {
    handle: Arc<driver::DeviceHandle>,
    event_overlapped: OVERLAPPED,
    quit_event: Arc<QuitEvent>,
}
unsafe impl Send for EventThreadContext {}

impl SplitTunnel {
    /// Initialize the driver.
    pub fn new(
        runtime: tokio::runtime::Handle,
        daemon_tx: Weak<mpsc::UnboundedSender<TunnelCommand>>,
        volume_update_rx: mpsc::UnboundedReceiver<()>,
    ) -> Result<Self, Error> {
        let (request_tx, handle) = Self::spawn_request_thread(volume_update_rx)?;

        let mut event_overlapped: OVERLAPPED = unsafe { mem::zeroed() };
        event_overlapped.hEvent =
            unsafe { CreateEventW(ptr::null_mut(), TRUE, FALSE, ptr::null()) };
        if event_overlapped.hEvent == ptr::null_mut() {
            return Err(Error::EventThreadError(io::Error::last_os_error()));
        }

        let quit_event = Arc::new(QuitEvent::new());

        let event_context = EventThreadContext {
            handle: handle.clone(),
            event_overlapped,
            quit_event: quit_event.clone(),
        };

        let event_thread = std::thread::spawn(move || {
            use driver::{EventBody, EventId};

            // Take ownership of the entire struct (Rust 2021 edition change)
            let _ = &event_context;

            let mut data_buffer = Vec::with_capacity(DRIVER_EVENT_BUFFER_SIZE);
            let mut returned_bytes = 0u32;

            let event_objects = [
                event_context.event_overlapped.hEvent,
                event_context.quit_event.0,
            ];

            loop {
                if unsafe { WaitForSingleObject(event_context.quit_event.0, 0) == WAIT_OBJECT_0 } {
                    // Quit event was signaled
                    break;
                }

                if let Err(error) = unsafe {
                    driver::device_io_control_buffer_async(
                        event_context.handle.as_raw_handle(),
                        driver::DriverIoctlCode::DequeEvent as u32,
                        Some(&mut data_buffer),
                        None,
                        &event_context.event_overlapped,
                    )
                } {
                    log::error!(
                        "{}",
                        error.display_chain_with_msg("device_io_control failed")
                    );
                    continue;
                }

                let result = unsafe {
                    WaitForMultipleObjects(
                        event_objects.len() as u32,
                        &event_objects[0],
                        FALSE,
                        INFINITE,
                    )
                };

                let signaled_index = if result >= WAIT_OBJECT_0
                    && result < WAIT_OBJECT_0 + event_objects.len() as u32
                {
                    result - WAIT_OBJECT_0
                } else if result >= WAIT_ABANDONED_0
                    && result < WAIT_ABANDONED_0 + event_objects.len() as u32
                {
                    result - WAIT_ABANDONED_0
                } else {
                    let error = io::Error::last_os_error();
                    log::error!(
                        "{}",
                        error.display_chain_with_msg("WaitForMultipleObjects failed")
                    );

                    continue;
                };

                if event_context.quit_event.0 == event_objects[signaled_index as usize] {
                    // Quit event was signaled
                    break;
                }

                let result = unsafe {
                    GetOverlappedResult(
                        event_context.handle.as_raw_handle(),
                        &event_context.event_overlapped as *const _ as *mut _,
                        &mut returned_bytes,
                        TRUE,
                    )
                };

                if result == 0 {
                    let error = io::Error::last_os_error();
                    log::error!(
                        "{}",
                        error.display_chain_with_msg("GetOverlappedResult failed")
                    );

                    continue;
                }

                unsafe { data_buffer.set_len(returned_bytes as usize) };

                let event = driver::parse_event_buffer(&data_buffer);

                let (event_id, event_body) = match event {
                    Some((event_id, event_body)) => (event_id, event_body),
                    None => continue,
                };

                let event_str = match &event_id {
                    EventId::StartSplittingProcess | EventId::ErrorStartSplittingProcess => {
                        "Start splitting process"
                    }
                    EventId::StopSplittingProcess | EventId::ErrorStopSplittingProcess => {
                        "Stop splitting process"
                    }
                    EventId::ErrorMessage => "ErrorMessage",
                    _ => "Unknown event ID",
                };

                match event_body {
                    EventBody::SplittingEvent {
                        process_id,
                        reason,
                        image,
                    } => {
                        log::trace!(
                            "{}:\n\tpid: {}\n\treason: {:?}\n\timage: {:?}",
                            event_str,
                            process_id,
                            reason,
                            image,
                        );
                    }
                    EventBody::SplittingError { process_id, image } => {
                        log::error!(
                            "FAILED: {}:\n\tpid: {}\n\timage: {:?}",
                            event_str,
                            process_id,
                            image,
                        );
                    }
                    EventBody::ErrorMessage { status, message } => {
                        log::error!("NTSTATUS {:#x}: {}", status, message.to_string_lossy())
                    }
                }
            }

            log::debug!("Stopping split tunnel event thread");

            unsafe { CloseHandle(event_context.event_overlapped.hEvent) };
        });

        Ok(SplitTunnel {
            runtime,
            request_tx,
            event_thread: Some(event_thread),
            quit_event,
            _route_change_callback: None,
            daemon_tx,
            async_path_update_in_progress: Arc::new(AtomicBool::new(false)),
        })
    }

    fn spawn_request_thread(
        volume_update_rx: mpsc::UnboundedReceiver<()>,
    ) -> Result<(RequestTx, Arc<driver::DeviceHandle>), Error> {
        let (tx, rx): (RequestTx, _) = sync_mpsc::channel();
        let (init_tx, init_rx) = sync_mpsc::channel();

        let monitored_paths = Arc::new(Mutex::new(vec![]));
        let monitored_paths_copy = monitored_paths.clone();

        let (monitor_tx, monitor_rx) = sync_mpsc::channel();

        let path_monitor = path_monitor::PathMonitor::spawn(monitor_tx.clone())
            .map_err(Error::StartPathMonitor)?;
        let volume_monitor = volume_monitor::VolumeMonitor::spawn(
            path_monitor.clone(),
            monitor_tx,
            monitored_paths.clone(),
            volume_update_rx,
        );

        std::thread::spawn(move || {
            let result = driver::DeviceHandle::new()
                .map(Arc::new)
                .map_err(Error::InitializationError);
            let handle = match result {
                Ok(handle) => {
                    let _ = init_tx.send(Ok(handle.clone()));
                    handle
                }
                Err(error) => {
                    let _ = init_tx.send(Err(error));
                    return;
                }
            };

            let mut previous_addresses = InterfaceAddresses::default();

            while let Ok((request, response_tx)) = rx.recv() {
                let response = match request {
                    Request::SetPaths(paths) => {
                        let mut monitored_paths_guard = monitored_paths.lock().unwrap();

                        let result = if paths.len() > 0 {
                            handle.set_config(&paths).map_err(Error::SetConfiguration)
                        } else {
                            handle.clear_config().map_err(Error::SetConfiguration)
                        };

                        if result.is_ok() {
                            if let Err(error) = path_monitor.set_paths(&paths) {
                                log::error!(
                                    "{}",
                                    error.display_chain_with_msg("Failed to update path monitor")
                                );
                                monitored_paths_guard.clear();
                            } else {
                                *monitored_paths_guard = paths.to_vec();
                            }
                        }

                        result
                    }
                    Request::RegisterIps(mut ips) => {
                        if ips.internet_ipv4.is_none() && ips.internet_ipv6.is_none() {
                            ips.tunnel_ipv4 = None;
                            ips.tunnel_ipv6 = None;
                        }
                        if previous_addresses == ips {
                            Ok(())
                        } else {
                            if previous_addresses.internet_ipv4 != ips.internet_ipv4
                                && ips.internet_ipv4.is_some()
                            {
                                print_routes_info();
                            }

                            let result = handle
                                .register_ips(
                                    ips.tunnel_ipv4,
                                    ips.tunnel_ipv6,
                                    ips.internet_ipv4,
                                    ips.internet_ipv6,
                                )
                                .map_err(Error::RegisterIps);
                            if result.is_ok() {
                                previous_addresses = ips;
                            }
                            result
                        }
                    }
                };
                if response_tx.send(response).is_err() {
                    log::error!("A response could not be sent for a completed request");
                }
            }

            drop(volume_monitor);
            if let Err(error) = path_monitor.shutdown() {
                log::error!(
                    "{}",
                    error.display_chain_with_msg("Failed to shut down path monitor")
                );
            }
        });

        let handle = init_rx
            .recv_timeout(REQUEST_TIMEOUT)
            .map_err(|_| Error::RequestThreadStuck)??;

        let handle_copy = handle.clone();

        std::thread::spawn(move || {
            while let Ok(()) = monitor_rx.recv() {
                let paths = monitored_paths_copy.lock().unwrap();
                let result = if paths.len() > 0 {
                    log::debug!("Re-resolving excluded paths");
                    handle_copy.set_config(&*paths)
                } else {
                    continue;
                };
                if let Err(error) = result {
                    log::error!(
                        "{}",
                        error.display_chain_with_msg("Failed to update excluded paths")
                    );
                }
            }
        });

        Ok((tx, handle))
    }

    fn send_request(&self, request: Request) -> Result<(), Error> {
        Self::send_request_inner(&self.request_tx, request)
    }

    fn send_request_inner(request_tx: &RequestTx, request: Request) -> Result<(), Error> {
        let (response_tx, response_rx) = sync_mpsc::channel();

        request_tx
            .send((request, response_tx))
            .map_err(|_| Error::RequestThreadDown)?;

        response_rx
            .recv_timeout(REQUEST_TIMEOUT)
            .map_err(|_| Error::RequestThreadStuck)?
    }

    /// Set a list of applications to exclude from the tunnel.
    pub fn set_paths_sync<T: AsRef<OsStr>>(&self, paths: &[T]) -> Result<(), Error> {
        self.send_request(Request::SetPaths(
            paths
                .iter()
                .map(|path| path.as_ref().to_os_string())
                .collect(),
        ))
    }

    /// Set a list of applications to exclude from the tunnel.
    pub fn set_paths<T: AsRef<OsStr>>(
        &self,
        paths: &[T],
        result_tx: oneshot::Sender<Result<(), Error>>,
    ) {
        let busy = self
            .async_path_update_in_progress
            .swap(true, Ordering::SeqCst);
        if busy {
            let _ = result_tx.send(Err(Error::AlreadySettingPaths));
            return;
        }
        let (response_tx, response_rx) = sync_mpsc::channel();
        let request = Request::SetPaths(
            paths
                .iter()
                .map(|path| path.as_ref().to_os_string())
                .collect(),
        );
        let request_tx = self.request_tx.clone();

        let wait_task = move || {
            request_tx
                .send((request, response_tx))
                .map_err(|_| Error::RequestThreadDown)?;
            response_rx.recv().map_err(|_| Error::RequestThreadDown)?
        };
        let in_progress = self.async_path_update_in_progress.clone();
        self.runtime.spawn_blocking(move || {
            let _ = result_tx.send(wait_task());
            in_progress.store(false, Ordering::SeqCst);
        });
    }

    /// Instructs the driver to redirect traffic from sockets bound to 0.0.0.0, ::, or the
    /// tunnel addresses (if any) to the default route.
    pub fn set_tunnel_addresses(&mut self, metadata: Option<&TunnelMetadata>) -> Result<(), Error> {
        let mut tunnel_ipv4 = None;
        let mut tunnel_ipv6 = None;

        if let Some(metadata) = metadata {
            for ip in &metadata.ips {
                match ip {
                    IpAddr::V4(address) => tunnel_ipv4 = Some(address.clone()),
                    IpAddr::V6(address) => tunnel_ipv6 = Some(address.clone()),
                }
            }
        }

        let tunnel_ipv4 = Some(tunnel_ipv4.unwrap_or(RESERVED_IP_V4));
        let context_mutex = Arc::new(Mutex::new(
            SplitTunnelDefaultRouteChangeHandlerContext::new(
                self.request_tx.clone(),
                self.daemon_tx.clone(),
                tunnel_ipv4,
                tunnel_ipv6,
            ),
        ));

        self._route_change_callback = None;
        let mut context = context_mutex.lock().unwrap();
        let callback = winnet::add_default_route_change_callback(
            Some(split_tunnel_default_route_change_handler),
            context_mutex.clone(),
        )
        .map(Some)
        .map_err(|_| Error::RegisterRouteChangeCallback)?;

        context.initialize_internet_addresses()?;
        context.register_ips()?;
        self._route_change_callback = callback;

        Ok(())
    }

    /// Instructs the driver to stop redirecting tunnel traffic and INADDR_ANY.
    pub fn clear_tunnel_addresses(&mut self) -> Result<(), Error> {
        self._route_change_callback = None;
        self.send_request(Request::RegisterIps(InterfaceAddresses::default()))
    }
}

impl Drop for SplitTunnel {
    fn drop(&mut self) {
        if let Some(_event_thread) = self.event_thread.take() {
            if let Err(error) = self.quit_event.set_event() {
                log::error!(
                    "{}",
                    error.display_chain_with_msg("Failed to close ST event thread")
                );
            }
            // Not joining `event_thread`: It may be unresponsive.
        }

        let paths: [&OsStr; 0] = [];
        if let Err(error) = self.set_paths_sync(&paths) {
            log::error!("{}", error.display_chain());
        }
    }
}

struct SplitTunnelDefaultRouteChangeHandlerContext {
    request_tx: RequestTx,
    pub daemon_tx: Weak<mpsc::UnboundedSender<TunnelCommand>>,
    pub addresses: InterfaceAddresses,
}

impl SplitTunnelDefaultRouteChangeHandlerContext {
    pub fn new(
        request_tx: RequestTx,
        daemon_tx: Weak<mpsc::UnboundedSender<TunnelCommand>>,
        tunnel_ipv4: Option<Ipv4Addr>,
        tunnel_ipv6: Option<Ipv6Addr>,
    ) -> Self {
        SplitTunnelDefaultRouteChangeHandlerContext {
            request_tx,
            daemon_tx,
            addresses: InterfaceAddresses {
                tunnel_ipv4,
                tunnel_ipv6,
                internet_ipv4: None,
                internet_ipv6: None,
            },
        }
    }

    pub fn register_ips(&self) -> Result<(), Error> {
        SplitTunnel::send_request_inner(
            &self.request_tx,
            Request::RegisterIps(self.addresses.clone()),
        )
    }

    pub fn initialize_internet_addresses(&mut self) -> Result<(), Error> {
        // Identify IP address that gives us Internet access
        let internet_ipv4 = get_best_default_route(WinNetAddrFamily::IPV4)
            .map_err(Error::ObtainDefaultRoute)?
            .map(|route| {
                log::debug!("Preferred interface: {}", route.interface_luid);
                interface_luid_to_ip(WinNetAddrFamily::IPV4, route.interface_luid).map(|ip| {
                    ip.or_else(|| {
                        log::warn!("No IPv4 address was found for the default route interface");
                        None
                    })
                })
            })
            .transpose()
            .map_err(Error::LuidToIp)?
            .flatten();
        let internet_ipv6 = get_best_default_route(WinNetAddrFamily::IPV6)
            .map_err(Error::ObtainDefaultRoute)?
            .map(|route| {
                interface_luid_to_ip(WinNetAddrFamily::IPV6, route.interface_luid).map(|ip| {
                    ip.or_else(|| {
                        log::warn!("No IPv6 address was found for the default route interface");
                        None
                    })
                })
            })
            .transpose()
            .map_err(Error::LuidToIp)?
            .flatten();

        self.addresses.internet_ipv4 = internet_ipv4
            .map(|addr| Ipv4Addr::try_from(addr).map_err(|_| Error::IpParseError))
            .transpose()?;
        self.addresses.internet_ipv6 = internet_ipv6
            .map(|addr| Ipv6Addr::try_from(addr).map_err(|_| Error::IpParseError))
            .transpose()?;
        Ok(())
    }
}

unsafe extern "system" fn split_tunnel_default_route_change_handler(
    event_type: winnet::WinNetDefaultRouteChangeEventType,
    address_family: WinNetAddrFamily,
    default_route: winnet::WinNetDefaultRoute,
    ctx: *mut libc::c_void,
) {
    use winnet::WinNetDefaultRouteChangeEventType::*;

    // Update the "internet interface" IP when best default route changes
    let ctx_mutex = &mut *(ctx as *mut Arc<Mutex<SplitTunnelDefaultRouteChangeHandlerContext>>);
    let mut ctx = ctx_mutex.lock().expect("ST route handler mutex poisoned");

    let daemon_tx = ctx.daemon_tx.upgrade();
    let maybe_send = move |content| {
        if let Some(tx) = daemon_tx {
            let _ = tx.unbounded_send(content);
        }
    };

    let result = match event_type {
        DefaultRouteChanged | DefaultRouteUpdatedDetails => {
            if address_family == WinNetAddrFamily::IPV4 {
                log::debug!("Preferred interface: {}", default_route.interface_luid);
            }

            match interface_luid_to_ip(address_family, default_route.interface_luid) {
                Ok(Some(ip)) => match IpAddr::from(ip) {
                    IpAddr::V4(addr) => ctx.addresses.internet_ipv4 = Some(addr),
                    IpAddr::V6(addr) => ctx.addresses.internet_ipv6 = Some(addr),
                },
                Ok(None) => {
                    log::warn!("Failed to obtain default route interface address");
                    match address_family {
                        WinNetAddrFamily::IPV4 => {
                            ctx.addresses.internet_ipv4 = None;
                        }
                        WinNetAddrFamily::IPV6 => {
                            ctx.addresses.internet_ipv6 = None;
                        }
                    }
                }
                Err(error) => {
                    log::error!(
                        "{}",
                        error.display_chain_with_msg(
                            "Failed to obtain default route interface address"
                        )
                    );
                    maybe_send(TunnelCommand::Block(ErrorStateCause::SplitTunnelError));
                    return;
                }
            };

            ctx.register_ips()
        }
        // no default route
        DefaultRouteRemoved => {
            if address_family == WinNetAddrFamily::IPV4 {
                log::debug!("Preferred interface: 0");
            }

            match address_family {
                WinNetAddrFamily::IPV4 => {
                    ctx.addresses.internet_ipv4 = None;
                }
                WinNetAddrFamily::IPV6 => {
                    ctx.addresses.internet_ipv6 = None;
                }
            }
            ctx.register_ips()
        }
    };

    if let Err(error) = result {
        log::error!(
            "{}",
            error.display_chain_with_msg("Failed to register new addresses in split tunnel driver")
        );
        maybe_send(TunnelCommand::Block(ErrorStateCause::SplitTunnelError));
    }
}

fn print_routes_info() {
    use crate::windows::get_ip_forward_table;

    let mut buf = String::new();
    let mut adapters = HashSet::new();

    let rows = match get_ip_forward_table(Some(crate::windows::AddressFamily::Ipv4)) {
        Ok(rows) => rows,
        Err(error) => {
            log::warn!("{}", error.display_chain());
            return;
        }
    };

    for row in &rows {
        adapters.insert(row.InterfaceLuid.Value);
        if let Err(_error) = print_route_info(&mut buf, row) {
            log::warn!("Failed to dump route info for {}", row.InterfaceLuid.Value);
        }
    }

    for iface in adapters {
        let luid = NET_LUID { Value: iface };
        if let Err(_error) = print_interface_info(&mut buf, luid) {
            log::warn!("Failed to dump interface info for {}", iface);
        }
    }

    log::debug!("Beginning logging of route details");
    log::debug!("{}", buf);
    log::debug!("Finished logging of route details");
}

fn print_route_info<W: Write>(mut buf: W, row: &MIB_IPFORWARD_ROW2) -> std::fmt::Result {
    use crate::windows::try_socketaddr_from_inet_sockaddr;

    writeln!(buf, "Logging route details")?;
    writeln!(buf, "  LUID: {}", row.InterfaceLuid.Value)?;

    if let Ok(addr) = try_socketaddr_from_inet_sockaddr(row.DestinationPrefix.Prefix) {
        writeln!(
            buf,
            "  Destination prefix: {}/{}",
            addr.ip(),
            row.DestinationPrefix.PrefixLength
        )?;
    }

    writeln!(buf, "  IfType: {}", row.InterfaceLuid.IfType())?;
    writeln!(
        buf,
        "  NextHop: {:?}",
        try_socketaddr_from_inet_sockaddr(row.NextHop).ok()
    )?;
    writeln!(buf, "  ValidLifetime: {}", row.ValidLifetime)?;
    writeln!(buf, "  PreferredLifetime: {}", row.PreferredLifetime)?;
    writeln!(buf, "  Metric: {}", row.Metric)?;
    writeln!(buf, "  Protocol: {}", row.Protocol)?;
    writeln!(buf, "  Loopback: {}", row.Loopback)?;
    writeln!(buf, "  Immortal: {}", row.Immortal)?;
    writeln!(buf, "  Age: {}", row.Age)?;
    writeln!(buf, "  Origin: {}", row.Origin)?;

    Ok(())
}

fn print_interface_info<W: Write>(mut buf: W, luid: NET_LUID) -> std::fmt::Result {
    use crate::windows::{get_if_entry, get_ip_interface_entry};

    writeln!(buf, "Logging interface details")?;
    writeln!(buf, "  LUID: {}", luid.Value)?;

    if let Ok(entry) = get_ip_interface_entry(AddressFamily::Ipv4, &luid) {
        writeln!(buf, "  Ip.Metric: {}", entry.Metric)?;
        writeln!(buf, "  Ip.Connected: {}", entry.Connected)?;
    }

    if let Ok(row) = get_if_entry(luid) {
        if let Ok(alias) = WideCStr::from_slice_truncate(&row.Alias) {
            writeln!(buf, "  Iface.Alias: {}", alias.to_string_lossy())?;
        }
        if let Ok(desc) = WideCStr::from_slice_truncate(&row.Description) {
            writeln!(buf, "  Iface.Description: {}", desc.to_string_lossy())?;
        }
        writeln!(buf, "  Iface.MediaType: {}", row.MediaType)?;
        writeln!(buf, "  Iface.TunnelType: {}", row.TunnelType)?;
        writeln!(
            buf,
            "  Iface.PhysicalMediumType: {}",
            row.PhysicalMediumType
        )?;

        writeln!(
            buf,
            "  Iface.InterfaceAndOperStatusFlags: {:b}",
            row.InterfaceAndOperStatusFlags.bitfield
        )?;

        writeln!(buf, "  Iface.AdminStatus: {}", row.AdminStatus)?;
        writeln!(buf, "  Iface.OperStatus: {}", row.OperStatus)?;
        writeln!(buf, "  Iface.MediaConnectState: {}", row.MediaConnectState)?;
    }

    let luid_ip = interface_luid_to_ip(WinNetAddrFamily::IPV4, luid.Value)
        .ok()
        .flatten()
        .map(IpAddr::from);
    writeln!(buf, "  InterfaceLuidToIp: {:?}", luid_ip)?;

    let best_ip = get_best_route(
        Some(luid),
        SocketAddrV4::new(PUBLIC_INTERNET_ADDRESS_V4, 80).into(),
    )
    .ok()
    .map(|(_, ip)| ip);
    writeln!(buf, "  GetBestRoute: {:?}", best_ip)?;

    if best_ip != luid_ip {
        writeln!(buf, "  BestIp != LuidIp")?;
    }

    Ok(())
}
