use super::windows::{
    get_device_path, get_process_creation_time, get_process_device_path, open_process,
    ProcessAccess, ProcessSnapshot,
};
use crate::windows::as_uninit_byte_slice;
use bitflags::bitflags;
use memoffset::offset_of;
use std::{
    cell::RefCell,
    collections::HashMap,
    ffi::{OsStr, OsString},
    fs::{self, OpenOptions},
    io,
    mem::{self, size_of, MaybeUninit},
    net::{Ipv4Addr, Ipv6Addr},
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        fs::OpenOptionsExt,
        io::{AsRawHandle, RawHandle},
    },
    ptr,
    time::Duration,
};
use talpid_types::ErrorExt;
use winapi::{
    shared::{
        in6addr::IN6_ADDR,
        inaddr::IN_ADDR,
        minwindef::{FALSE, TRUE},
        ntdef::NTSTATUS,
        winerror::{
            ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_INVALID_PARAMETER, ERROR_IO_PENDING,
        },
    },
    um::{
        handleapi::CloseHandle,
        ioapiset::{DeviceIoControl, GetOverlappedResult},
        minwinbase::OVERLAPPED,
        synchapi::{CreateEventW, WaitForSingleObject},
        tlhelp32::TH32CS_SNAPPROCESS,
        winbase::{FILE_FLAG_OVERLAPPED, INFINITE, WAIT_ABANDONED, WAIT_FAILED, WAIT_OBJECT_0},
        winioctl::{FILE_ANY_ACCESS, METHOD_BUFFERED, METHOD_NEITHER},
    },
};

const DRIVER_SYMBOLIC_NAME: &str = "\\\\.\\MULLVADSPLITTUNNEL";
const ST_DEVICE_TYPE: u32 = 0x8000;

const fn ctl_code(device_type: u32, function: u32, method: u32, access: u32) -> u32 {
    device_type << 16 | access << 14 | function << 2 | method
}

#[repr(u32)]
#[allow(dead_code)]
pub enum DriverIoctlCode {
    Initialize = ctl_code(ST_DEVICE_TYPE, 1, METHOD_NEITHER, FILE_ANY_ACCESS),
    DequeEvent = ctl_code(ST_DEVICE_TYPE, 2, METHOD_BUFFERED, FILE_ANY_ACCESS),
    RegisterProcesses = ctl_code(ST_DEVICE_TYPE, 3, METHOD_BUFFERED, FILE_ANY_ACCESS),
    RegisterIpAddresses = ctl_code(ST_DEVICE_TYPE, 4, METHOD_BUFFERED, FILE_ANY_ACCESS),
    GetIpAddresses = ctl_code(ST_DEVICE_TYPE, 5, METHOD_BUFFERED, FILE_ANY_ACCESS),
    SetConfiguration = ctl_code(ST_DEVICE_TYPE, 6, METHOD_BUFFERED, FILE_ANY_ACCESS),
    GetConfiguration = ctl_code(ST_DEVICE_TYPE, 7, METHOD_BUFFERED, FILE_ANY_ACCESS),
    ClearConfiguration = ctl_code(ST_DEVICE_TYPE, 8, METHOD_NEITHER, FILE_ANY_ACCESS),
    GetState = ctl_code(ST_DEVICE_TYPE, 9, METHOD_BUFFERED, FILE_ANY_ACCESS),
    QueryProcess = ctl_code(ST_DEVICE_TYPE, 10, METHOD_BUFFERED, FILE_ANY_ACCESS),
}

#[derive(Debug, PartialEq)]
#[repr(u32)]
#[allow(dead_code)]
pub enum DriverState {
    // Default state after being loaded.
    None = 0,
    // DriverEntry has completed successfully.
    // Basically only driver and device objects are created at this point.
    Started = 1,
    // All subsystems are initialized.
    Initialized = 2,
    // User mode has registered all processes in the system.
    Ready = 3,
    // IP addresses are registered.
    // A valid configuration is registered.
    Engaged = 4,
    // Driver is unloading.
    Terminating = 5,
}

#[derive(err_derive::Error, Debug)]
#[error(display = "Unknown driver state: {}", _0)]
pub struct UnknownDriverState(u64);

impl TryFrom<u64> for DriverState {
    type Error = UnknownDriverState;

    fn try_from(state: u64) -> Result<Self, Self::Error> {
        use DriverState::*;

        match state {
            e if e == None as u64 => Ok(None),
            e if e == Started as u64 => Ok(Started),
            e if e == Initialized as u64 => Ok(Initialized),
            e if e == Ready as u64 => Ok(Ready),
            e if e == Engaged as u64 => Ok(Engaged),
            e if e == Terminating as u64 => Ok(Terminating),
            other => Err(UnknownDriverState(other)),
        }
    }
}

#[repr(u32)]
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub enum EventId {
    StartSplittingProcess = 0,
    StopSplittingProcess,

    // ErrorFlag = 0x80000000,
    ErrorStartSplittingProcess = 0x80000001,
    ErrorStopSplittingProcess,

    ErrorMessage,
}

#[derive(err_derive::Error, Debug)]
#[error(display = "Unknown event id: {}", _0)]
pub struct UnknownEventId(u32);

impl TryFrom<u32> for EventId {
    type Error = UnknownEventId;

    fn try_from(event: u32) -> Result<Self, Self::Error> {
        use EventId::*;

        match event {
            e if e == StartSplittingProcess as u32 => Ok(StartSplittingProcess),
            e if e == StopSplittingProcess as u32 => Ok(StopSplittingProcess),
            e if e == ErrorStartSplittingProcess as u32 => Ok(ErrorStartSplittingProcess),
            e if e == ErrorStopSplittingProcess as u32 => Ok(ErrorStopSplittingProcess),
            e if e == ErrorMessage as u32 => Ok(ErrorMessage),
            other => Err(UnknownEventId(other)),
        }
    }
}

pub enum EventBody {
    SplittingEvent {
        process_id: usize,
        reason: SplittingChangeReason,
        image: OsString,
    },
    SplittingError {
        process_id: usize,
        image: OsString,
    },
    ErrorMessage {
        status: NTSTATUS,
        message: OsString,
    },
}

bitflags! {
    pub struct SplittingChangeReason: u32 {
        const BY_INHERITANCE = 1;
        const BY_CONFIG = 2;
        const PROCESS_ARRIVING = 4;
        const PROCESS_DEPARTING = 8;
    }
}

pub struct DeviceHandle {
    handle: fs::File,
}

unsafe impl Sync for DeviceHandle {}
unsafe impl Send for DeviceHandle {}

#[derive(err_derive::Error, Debug)]
#[error(no_from)]
pub enum DeviceHandleError {
    /// Failed to connect because there's no such device
    #[error(display = "Failed to connect to driver, no such device. \
            The driver is probably not loaded")]
    ConnectionFailed,

    /// Failed to connect because the connection was denied
    #[error(display = "Failed to connect to driver, connection denied. \
            The exclusive connection is probably hogged")]
    ConnectionDenied,

    /// Failed to connect to driver
    #[error(display = "Failed to connect to driver")]
    ConnectionError(#[error(source)] io::Error),

    /// Failed to inquire about driver state
    #[error(display = "Failed to inquire about driver state")]
    GetStateError(#[error(source)] io::Error),

    /// Failed to initialize driver
    #[error(display = "Failed to initialize driver")]
    InitializationError(#[error(source)] io::Error),

    /// Failed to register process tree with driver
    #[error(display = "Failed to register process tree with driver")]
    RegisterProcessesError(#[error(source)] io::Error),

    /// Failed to clear configuration in driver
    #[error(display = "Failed to clear configuration in driver")]
    ClearConfigError(#[error(source)] io::Error),
}

impl DeviceHandle {
    pub fn new() -> Result<Self, DeviceHandleError> {
        // Connect to the driver
        log::trace!("Connecting to the driver");
        let handle = OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .custom_flags(FILE_FLAG_OVERLAPPED)
            .attributes(0)
            .open(DRIVER_SYMBOLIC_NAME)
            .map_err(|e| match e.raw_os_error().map(|raw| raw as u32) {
                Some(ERROR_FILE_NOT_FOUND) => DeviceHandleError::ConnectionFailed,
                Some(ERROR_ACCESS_DENIED) => DeviceHandleError::ConnectionDenied,
                _ => DeviceHandleError::ConnectionError(e),
            })?;

        let device = Self { handle };

        // Initialize the driver
        let state = device
            .get_driver_state()
            .map_err(DeviceHandleError::GetStateError)?;
        if state == DriverState::Started {
            log::trace!("Initializing driver");
            device
                .initialize()
                .map_err(DeviceHandleError::InitializationError)?;
        }

        // Initialize process tree
        let state = device
            .get_driver_state()
            .map_err(DeviceHandleError::GetStateError)?;
        if state == DriverState::Initialized {
            log::trace!("Registering processes");
            device
                .register_processes()
                .map_err(DeviceHandleError::RegisterProcessesError)?;
        }

        log::trace!("Clearing any existing exclusion config");
        device
            .clear_config()
            .map_err(DeviceHandleError::ClearConfigError)?;

        Ok(device)
    }

    fn initialize(&self) -> io::Result<()> {
        device_io_control(
            self.handle.as_raw_handle(),
            DriverIoctlCode::Initialize as u32,
            None,
            0,
            None,
        )?;
        Ok(())
    }

    fn register_processes(&self) -> io::Result<()> {
        let process_tree_buffer = serialize_process_tree(build_process_tree()?)?;
        device_io_control(
            self.handle.as_raw_handle(),
            DriverIoctlCode::RegisterProcesses as u32,
            Some(&process_tree_buffer),
            0,
            None,
        )?;
        Ok(())
    }

    pub fn register_ips(
        &self,
        tunnel_ipv4: Option<Ipv4Addr>,
        tunnel_ipv6: Option<Ipv6Addr>,
        internet_ipv4: Option<Ipv4Addr>,
        internet_ipv6: Option<Ipv6Addr>,
    ) -> io::Result<()> {
        log::debug!("Register IPs: tunnel IPv4: {:?}, tunnel IPv6 {:?}, internet IPv4: {:?}, internet IPv6: {:?}", tunnel_ipv4, tunnel_ipv6, internet_ipv4, internet_ipv6);
        let mut addresses: SplitTunnelAddresses = unsafe { mem::zeroed() };

        unsafe {
            if let Some(tunnel_ipv4) = tunnel_ipv4 {
                let tunnel_ipv4 = tunnel_ipv4.octets();
                ptr::copy_nonoverlapping(
                    &tunnel_ipv4[0] as *const u8,
                    &mut addresses.tunnel_ipv4 as *mut _ as *mut u8,
                    tunnel_ipv4.len(),
                );
            }

            if let Some(tunnel_ipv6) = tunnel_ipv6 {
                let tunnel_ipv6 = tunnel_ipv6.octets();
                ptr::copy_nonoverlapping(
                    &tunnel_ipv6[0] as *const u8,
                    &mut addresses.tunnel_ipv6 as *mut _ as *mut u8,
                    tunnel_ipv6.len(),
                );
            }

            if let Some(internet_ipv4) = internet_ipv4 {
                let internet_ipv4 = internet_ipv4.octets();
                ptr::copy_nonoverlapping(
                    &internet_ipv4[0] as *const u8,
                    &mut addresses.internet_ipv4 as *mut _ as *mut u8,
                    internet_ipv4.len(),
                );
            }

            if let Some(internet_ipv6) = internet_ipv6 {
                let internet_ipv6 = internet_ipv6.octets();
                ptr::copy_nonoverlapping(
                    &internet_ipv6[0] as *const u8,
                    &mut addresses.internet_ipv6 as *mut _ as *mut u8,
                    internet_ipv6.len(),
                );
            }
        }

        let buffer = as_uninit_byte_slice(&addresses);

        device_io_control(
            self.handle.as_raw_handle(),
            DriverIoctlCode::RegisterIpAddresses as u32,
            Some(buffer),
            0,
            None,
        )?;

        Ok(())
    }

    pub fn get_driver_state(&self) -> io::Result<DriverState> {
        let buffer = device_io_control(
            self.handle.as_raw_handle(),
            DriverIoctlCode::GetState as u32,
            None,
            size_of::<u64>() as u32,
            None,
        )?
        .unwrap();

        let raw_state: u64 = unsafe { deserialize_buffer(&buffer[0..size_of::<u64>()]) };

        Ok(DriverState::try_from(raw_state)
            .map_err(|error| io::Error::new(io::ErrorKind::Other, error))?)
    }

    pub fn set_config<T: AsRef<OsStr>>(&self, apps: &[T]) -> io::Result<()> {
        let mut device_paths = Vec::with_capacity(apps.len());
        for app in apps.as_ref() {
            match get_device_path(app.as_ref()) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    log::debug!(
                        "{}\nPath: {}",
                        error.display_chain_with_msg("Ignoring path on unmounted volume"),
                        app.as_ref().to_string_lossy()
                    );
                }
                Err(error) => return Err(error),
                Ok(path) => device_paths.push(path),
            }
        }

        if device_paths.is_empty() {
            return self.clear_config();
        }

        log::debug!("Excluded device paths:");
        for path in &device_paths {
            log::debug!("    {:?}", path);
        }

        let config = make_process_config(&device_paths);

        device_io_control(
            self.handle.as_raw_handle(),
            DriverIoctlCode::SetConfiguration as u32,
            Some(&config),
            0,
            None,
        )?;

        Ok(())
    }

    pub fn clear_config(&self) -> io::Result<()> {
        device_io_control(
            self.handle.as_raw_handle(),
            DriverIoctlCode::ClearConfiguration as u32,
            None,
            0,
            None,
        )?;

        Ok(())
    }
}

impl AsRawHandle for DeviceHandle {
    fn as_raw_handle(&self) -> RawHandle {
        self.handle.as_raw_handle()
    }
}

#[derive(Clone, Copy)]
#[repr(C)]
struct SplitTunnelAddresses {
    tunnel_ipv4: IN_ADDR,
    internet_ipv4: IN_ADDR,
    tunnel_ipv6: IN6_ADDR,
    internet_ipv6: IN6_ADDR,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ConfigurationHeader {
    // Number of entries immediately following the header.
    num_entries: usize,
    // Total byte length: header + entries + string buffer.
    total_length: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ConfigurationEntry {
    // Offset into buffer region that follows all entries.
    // The image name uses the physical path.
    name_offset: usize,
    // Byte length for non-null terminated wide char string.
    name_length: u16,
}

/// Create a buffer containing a `ConfigurationHeader` and number of `ConfigurationEntry`s,
/// followed by the same number of paths to those entries.
fn make_process_config<T: AsRef<OsStr>>(apps: &[T]) -> Vec<MaybeUninit<u8>> {
    let apps: Vec<Vec<u16>> = apps
        .iter()
        .map(|app| app.as_ref().encode_wide().collect())
        .collect();

    let total_string_size: usize = apps.iter().map(|app| size_of::<u16>() * app.len()).sum();

    let total_buffer_size = size_of::<ConfigurationHeader>()
        + size_of::<ConfigurationEntry>() * apps.len()
        + total_string_size;

    let mut buffer = Vec::<MaybeUninit<u8>>::new();
    buffer.resize(total_buffer_size, MaybeUninit::new(0));

    let (header, tail) = buffer.split_at_mut(size_of::<ConfigurationHeader>());

    // Serialize configuration header
    let header_struct = ConfigurationHeader {
        num_entries: apps.len(),
        total_length: total_buffer_size,
    };
    header.copy_from_slice(as_uninit_byte_slice(&header_struct));

    // Serialize configuration entries and strings
    let (entries, string_data) = tail.split_at_mut(apps.len() * size_of::<ConfigurationEntry>());
    let mut string_offset = 0;

    for (i, app) in apps.iter().enumerate() {
        write_string_to_buffer(string_data, string_offset, &app);

        let app_bytelen = size_of::<u16>() * app.len();
        let entry = ConfigurationEntry {
            name_offset: string_offset,
            name_length: app_bytelen as u16,
        };
        let entry_offset = size_of::<ConfigurationEntry>() * i;

        entries[entry_offset..entry_offset + size_of::<ConfigurationEntry>()]
            .copy_from_slice(as_uninit_byte_slice(&entry));

        string_offset += app_bytelen;
    }

    buffer
}

#[derive(Debug)]
struct ProcessInfo {
    pid: u32,
    parent_pid: u32,
    creation_time: u64,
    device_path: Vec<u16>,
}

/// List process identifiers, their parents, and their device paths.
fn build_process_tree() -> io::Result<Vec<ProcessInfo>> {
    let mut process_info = HashMap::new();

    let snap = ProcessSnapshot::new(TH32CS_SNAPPROCESS, 0)?;
    for entry in snap.entries() {
        let entry = entry?;

        let process = match open_process(ProcessAccess::QueryLimitedInformation, false, entry.pid) {
            Ok(handle) => Ok(handle),
            Err(error) => {
                // Skip process objects that cannot be opened
                match error.kind() {
                    // System process
                    io::ErrorKind::PermissionDenied => continue,
                    // System idle or csrss process
                    io::ErrorKind::InvalidInput => continue,
                    io::ErrorKind::Other => {
                        // Old rust lib maps INVALID_PARAMETER to "Other"
                        if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
                            continue;
                        }
                        Err(error)
                    }
                    _ => Err(error),
                }
            }
        }?;

        // TODO: Skip objects whose paths or timestamps cannot be obtained?

        process_info.insert(
            entry.pid,
            RefCell::new(ProcessInfo {
                pid: entry.pid,
                parent_pid: entry.parent_pid,
                creation_time: get_process_creation_time(process.get_raw()).unwrap_or(0),
                device_path: get_process_device_path(process.get_raw())
                    .unwrap_or(OsString::from(""))
                    .encode_wide()
                    .collect(),
            }),
        );
    }

    // Handle pid recycling
    // If the "parent" is younger than the process itself, it is not our parent.
    for info in process_info.values() {
        let mut info = info.borrow_mut();
        let parent_pid = info.parent_pid;
        if parent_pid == 0 {
            continue;
        }
        if let Some(parent_info) = process_info.get(&parent_pid) {
            if parent_info.borrow_mut().creation_time > info.creation_time {
                info.parent_pid = 0;
            }
        }
    }

    Ok(process_info
        .into_iter()
        .map(|(_, info)| info.into_inner())
        .collect())
}

#[derive(Clone, Copy)]
#[repr(C)]
struct ProcessRegistryHeader {
    // Number of entries immediately following the header.
    num_entries: usize,
    // Total byte length: header + entries + string buffer.
    total_length: usize,
}

#[derive(Clone, Copy)]
#[repr(C)]
struct ProcessRegistryEntry {
    pid: RawHandle,
    parent_pid: RawHandle,
    // Image name offset (following the last entry).
    image_name_offset: usize,
    // Image name length.
    image_name_size: u16,
}

fn serialize_process_tree(processes: Vec<ProcessInfo>) -> Result<Vec<MaybeUninit<u8>>, io::Error> {
    // Construct a buffer:
    //  ProcessRegistryHeader
    //  ProcessRegistryEntry..
    //  Image names..

    let total_string_size: usize = processes
        .iter()
        .map(|info| size_of::<u16>() * info.device_path.len())
        .sum();
    let total_buffer_size = size_of::<ProcessRegistryHeader>()
        + size_of::<ProcessRegistryEntry>() * processes.len()
        + total_string_size;

    let mut buffer = Vec::new();
    buffer.resize(total_buffer_size, MaybeUninit::new(0u8));

    let (header, tail) = buffer.split_at_mut(size_of::<ProcessRegistryHeader>());
    let header_struct = ProcessRegistryHeader {
        num_entries: processes.len(),
        total_length: total_buffer_size,
    };
    header.copy_from_slice(as_uninit_byte_slice(&header_struct));

    let (entries, string_data) =
        tail.split_at_mut(size_of::<ProcessRegistryEntry>() * processes.len());

    let mut string_offset = 0;

    for (i, entry) in processes.into_iter().enumerate() {
        let mut out_entry = ProcessRegistryEntry {
            pid: entry.pid as usize as RawHandle,
            parent_pid: entry.parent_pid as usize as RawHandle,
            image_name_size: 0,
            image_name_offset: 0,
        };

        if !entry.device_path.is_empty() {
            write_string_to_buffer(string_data, string_offset, &entry.device_path);

            out_entry.image_name_size = (entry.device_path.len() * size_of::<u16>()) as u16;
            out_entry.image_name_offset = string_offset;

            string_offset += size_of::<u16>() * entry.device_path.len();
        }

        let entry_offset = size_of::<ProcessRegistryEntry>() * i;
        entries[entry_offset..entry_offset + size_of::<ProcessRegistryEntry>()]
            .copy_from_slice(as_uninit_byte_slice(&out_entry));
    }

    Ok(buffer)
}

#[repr(C)]
struct EventHeader {
    event_id: EventId,
    event_size: usize,
    event_data: [u8; 0],
}

#[repr(C)]
struct SplittingEventHeader {
    process_id: usize,
    reason: u32,
    image_name_length: u16,
    image_name_data: [u16; 0],
}

#[repr(C)]
struct SplittingErrorEventHeader {
    process_id: usize,
    image_name_length: u16,
    image_name_data: [u16; 0],
}

#[repr(C)]
struct ErrorMessageEventHeader {
    status: NTSTATUS,
    error_message_length: u16,
    error_message_data: [u16; 0],
}

/// Parses an event returned by the ST driver.
///
/// # Panics
///
/// This may panic if `buffer` contains invalid data.
pub fn parse_event_buffer(buffer: &[u8]) -> Option<(EventId, EventBody)> {
    // SAFETY: This panics if `buffer` is too small.
    let raw_event_id: u32 = unsafe { deserialize_buffer(&buffer[0..mem::size_of::<u32>()]) };
    let _event_id = EventId::try_from(raw_event_id)
        .map_err(|error| {
            log::error!(
                "{}",
                error.display_chain_with_msg("Failed to parse ST event buffer")
            );
            error
        })
        .ok()?;

    // SAFETY: The event id is known to be valid.
    let event_header: EventHeader =
        unsafe { deserialize_buffer(&buffer[0..offset_of!(EventHeader, event_data)]) };

    let (_, buffer) = buffer.split_at(offset_of!(EventHeader, event_data));

    match event_header.event_id {
        EventId::StartSplittingProcess | EventId::StopSplittingProcess => {
            // SAFETY: This will panic if the buffer is too small to contain the message.
            let event: SplittingEventHeader = unsafe {
                deserialize_buffer(&buffer[0..offset_of!(SplittingEventHeader, image_name_data)])
            };
            let string_byte_offset = offset_of!(SplittingEventHeader, image_name_data);
            let image = buffer_to_osstring(
                &buffer
                    [string_byte_offset..(string_byte_offset + event.image_name_length as usize)],
            );

            Some((
                event_header.event_id,
                EventBody::SplittingEvent {
                    process_id: event.process_id,
                    reason: SplittingChangeReason::from_bits(event.reason).unwrap_or_else(|| {
                        log::error!("Dropping unknown bits from splitting change reason. Original reason: {:b}", event.reason);
                        SplittingChangeReason::from_bits_truncate(event.reason)
                    }),
                    image,
                },
            ))
        }
        EventId::ErrorStartSplittingProcess | EventId::ErrorStopSplittingProcess => {
            // SAFETY: This will panic if the buffer is too small to contain the message.
            let event: SplittingErrorEventHeader = unsafe {
                deserialize_buffer(
                    &buffer[0..offset_of!(SplittingErrorEventHeader, image_name_data)],
                )
            };
            let string_byte_offset = offset_of!(SplittingErrorEventHeader, image_name_data);
            let image = buffer_to_osstring(
                &buffer
                    [string_byte_offset..(string_byte_offset + event.image_name_length as usize)],
            );

            Some((
                event_header.event_id,
                EventBody::SplittingError {
                    process_id: event.process_id,
                    image,
                },
            ))
        }
        EventId::ErrorMessage => {
            // SAFETY: This will panic if the buffer is too small to contain the message.
            let event: ErrorMessageEventHeader = unsafe {
                deserialize_buffer(
                    &buffer[0..offset_of!(ErrorMessageEventHeader, error_message_data)],
                )
            };
            let string_byte_offset = offset_of!(ErrorMessageEventHeader, error_message_data);
            let message = buffer_to_osstring(
                &buffer[string_byte_offset
                    ..(string_byte_offset + event.error_message_length as usize)],
            );

            Some((
                event_header.event_id,
                EventBody::ErrorMessage {
                    status: event.status,
                    message,
                },
            ))
        }
    }
}

/// Send an IOCTL code to the given device handle.
/// `input` specifies an optional buffer to send.
/// Upon success, a buffer of size `output_size` is returned, or None if `output_size` is 0.
pub fn device_io_control(
    device: RawHandle,
    ioctl_code: u32,
    input: Option<&[MaybeUninit<u8>]>,
    output_size: u32,
    timeout: Option<Duration>,
) -> Result<Option<Vec<u8>>, io::Error> {
    struct HandleOwner {
        handle: RawHandle,
    }
    impl Drop for HandleOwner {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.handle) };
        }
    }

    let mut overlapped: OVERLAPPED = unsafe { mem::zeroed() };
    overlapped.hEvent = unsafe { CreateEventW(ptr::null_mut(), TRUE, FALSE, ptr::null()) };

    if overlapped.hEvent == ptr::null_mut() {
        return Err(io::Error::last_os_error());
    }

    let _handle_owner = HandleOwner {
        handle: overlapped.hEvent,
    };

    let mut out_buffer = if output_size > 0 {
        Some(Vec::with_capacity(output_size as usize))
    } else {
        None
    };

    device_io_control_buffer(
        device,
        ioctl_code,
        input,
        out_buffer.as_mut(),
        &overlapped,
        timeout,
    )
    .map(|()| out_buffer)
}

/// Send an IOCTL code to the given device handle.
/// `input` specifies an optional buffer to send.
/// Upon success, `output` buffer will contain at most `output.capacity()` bytes of data.
pub fn device_io_control_buffer(
    device: RawHandle,
    ioctl_code: u32,
    input: Option<&[MaybeUninit<u8>]>,
    mut output: Option<&mut Vec<u8>>,
    overlapped: &OVERLAPPED,
    timeout: Option<Duration>,
) -> Result<(), io::Error> {
    let input_ptr = match input {
        Some(input) => input as *const _ as *mut _,
        None => ptr::null_mut(),
    };
    let input_len = input.map(|input| input.len()).unwrap_or(0);

    let out_ptr = match output {
        Some(ref mut output) => output.as_mut_ptr() as *mut _,
        None => ptr::null_mut(),
    };
    let output_size = if let Some(ref output) = output {
        output.capacity()
    } else {
        0
    };

    let event = overlapped.hEvent;

    let mut returned_bytes = 0u32;
    let overlapped = overlapped as *const _ as *mut _;

    let result = unsafe {
        DeviceIoControl(
            device as *mut _,
            ioctl_code,
            input_ptr,
            input_len as u32,
            out_ptr,
            output_size as u32,
            &mut returned_bytes,
            overlapped,
        )
    };

    if result != 0 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "Expected pending operation",
        ));
    }

    let last_error = io::Error::last_os_error();
    if last_error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
        return Err(last_error);
    }

    let timeout = timeout
        .map(|timeout| timeout.as_millis() as u32)
        .unwrap_or(INFINITE);
    let result = unsafe { WaitForSingleObject(event, timeout) };
    match result {
        WAIT_FAILED => return Err(io::Error::last_os_error()),
        WAIT_ABANDONED => return Err(io::Error::new(io::ErrorKind::Other, "abandoned mutex")),
        WAIT_OBJECT_0 => (),
        error => return Err(io::Error::from_raw_os_error(error as i32)),
    }

    let result =
        unsafe { GetOverlappedResult(device as *mut _, overlapped, &mut returned_bytes, FALSE) };

    if result == 0 {
        return Err(io::Error::last_os_error());
    }

    if let Some(ref mut output) = output {
        unsafe { output.set_len(returned_bytes as usize) };
    }

    Ok(())
}

/// Send an IOCTL code to the given device handle.
/// `input` specifies an optional buffer to send.
/// The result must be obtained using `GetOverlappedResult[Ex]`.
pub unsafe fn device_io_control_buffer_async(
    device: RawHandle,
    ioctl_code: u32,
    mut output: Option<&mut Vec<u8>>,
    input: Option<&[u8]>,
    overlapped: &OVERLAPPED,
) -> Result<(), io::Error> {
    let input_ptr = match input {
        Some(input) => input as *const _ as *mut _,
        None => ptr::null_mut(),
    };
    let input_len = input.map(|input| input.len()).unwrap_or(0);

    let out_ptr = match output {
        Some(ref mut output) => output.as_mut_ptr() as *mut _,
        None => ptr::null_mut(),
    };
    let output_size = if let Some(ref output) = output {
        output.capacity()
    } else {
        0
    };

    let overlapped = overlapped as *const _ as *mut _;

    let result = DeviceIoControl(
        device as *mut _,
        ioctl_code,
        input_ptr,
        input_len as u32,
        out_ptr,
        output_size as u32,
        ptr::null_mut(),
        overlapped,
    );

    if result != 0 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "Expected pending operation",
        ));
    }

    let last_error = io::Error::last_os_error();
    if last_error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
        return Err(last_error);
    }

    Ok(())
}

/// Reads the value from `buffer`, zeroing any remaining bytes.
///
/// # Safety
///
/// The caller must ensure that `T` is initialized by the byte buffer.
///
/// # Panics
///
/// This panics if `buffer` is larger than `T`.
unsafe fn deserialize_buffer<T>(buffer: &[u8]) -> T {
    assert!(buffer.len() <= mem::size_of::<T>());

    let mut instance = MaybeUninit::zeroed();
    ptr::copy_nonoverlapping(
        buffer.as_ptr(),
        instance.as_mut_ptr() as *mut u8,
        buffer.len(),
    );
    instance.assume_init()
}

fn buffer_to_osstring(buffer: &[u8]) -> OsString {
    let mut out_buf = Vec::new();
    out_buf.resize((buffer.len() + 1) / mem::size_of::<u16>(), 0u16);

    // SAFETY: `out_buf` contains enough bytes to store all of `buffer`.
    unsafe {
        ptr::copy_nonoverlapping(
            buffer as *const _ as *const u16,
            out_buf.as_mut_ptr(),
            out_buf.len(),
        )
    };
    OsStringExt::from_wide(&out_buf)
}

/// Inserts a string into `buffer` at a given `byte_offset`.
///
/// # Panics
///
/// This panics if either `byte_offset` or `byte_offset + 2 * string.len() - 1` is
/// an out of bounds index for `buffer`.
fn write_string_to_buffer(buffer: &mut [MaybeUninit<u8>], byte_offset: usize, string: &[u16]) {
    for (i, byte) in string
        .iter()
        .flat_map(|word| word.to_ne_bytes().into_iter())
        .enumerate()
    {
        buffer[byte_offset + i] = MaybeUninit::new(byte);
    }
}
