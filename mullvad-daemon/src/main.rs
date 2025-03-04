#![deny(rust_2018_idioms)]

use mullvad_daemon::{
    logging,
    management_interface::{ManagementInterfaceEventBroadcaster, ManagementInterfaceServer},
    rpc_uniqueness_check,
    runtime::new_runtime_builder,
    version, Daemon, DaemonCommandChannel, DaemonCommandSender,
};
use std::{path::PathBuf, thread, time::Duration};
use talpid_types::ErrorExt;

mod cli;
mod exception_logging;
mod shutdown;
#[cfg(windows)]
mod system_service;

const DAEMON_LOG_FILENAME: &str = "daemon.log";

fn main() {
    let config = cli::get_config();
    let log_dir = init_logging(config).unwrap_or_else(|error| {
        eprintln!("{}", error);
        std::process::exit(1)
    });

    log::trace!("Using configuration: {:?}", config);

    let runtime = new_runtime_builder().build().unwrap_or_else(|error| {
        eprintln!("{}", error.display_chain());
        std::process::exit(1);
    });

    let exit_code = match runtime.block_on(run_platform(config, log_dir)) {
        Ok(_) => 0,
        Err(error) => {
            log::error!("{}", error);
            1
        }
    };
    log::debug!("Process exiting with code {}", exit_code);
    std::process::exit(exit_code);
}

fn init_logging(config: &cli::Config) -> Result<Option<PathBuf>, String> {
    let log_dir = get_log_dir(config)?;
    let log_file = log_dir.as_ref().map(|dir| dir.join(DAEMON_LOG_FILENAME));

    logging::init_logger(
        config.log_level,
        log_file.as_ref(),
        config.log_stdout_timestamps,
    )
    .map_err(|e| e.display_chain_with_msg("Unable to initialize logger"))?;
    log_panics::init();
    exception_logging::enable();
    version::log_version();
    if let Some(ref log_dir) = log_dir {
        log::info!("Logging to {}", log_dir.display());
    }
    Ok(log_dir)
}

fn get_log_dir(config: &cli::Config) -> Result<Option<PathBuf>, String> {
    if config.log_to_file {
        Ok(Some(mullvad_paths::log_dir().map_err(|e| {
            e.display_chain_with_msg("Unable to get log directory")
        })?))
    } else {
        Ok(None)
    }
}

#[cfg(windows)]
async fn run_platform(config: &cli::Config, log_dir: Option<PathBuf>) -> Result<(), String> {
    if config.run_as_service {
        system_service::run()
    } else if config.restart_service {
        let restart_result = system_service::restart_service().map_err(|e| e.display_chain());
        if restart_result.is_ok() {
            log::info!("Restarted the service.");
        }
        restart_result
    } else {
        if config.register_service {
            let install_result = system_service::install_service().map_err(|e| e.display_chain());
            if install_result.is_ok() {
                println!("Installed the service.");
            }
            install_result
        } else {
            run_standalone(log_dir).await
        }
    }
}

#[cfg(not(windows))]
async fn run_platform(_config: &cli::Config, log_dir: Option<PathBuf>) -> Result<(), String> {
    run_standalone(log_dir).await
}

async fn run_standalone(log_dir: Option<PathBuf>) -> Result<(), String> {
    if rpc_uniqueness_check::is_another_instance_running().await {
        return Err("Another instance of the daemon is already running".to_owned());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if let Err(err) = tokio::fs::remove_file(mullvad_paths::get_rpc_socket_path()).await {
        if err.kind() != std::io::ErrorKind::NotFound {
            log::error!("Failed to remove old RPC socket: {}", err);
        }
    }

    if !running_as_admin() {
        log::warn!("Running daemon as a non-administrator user, clients might refuse to connect");
    }

    let daemon = create_daemon(log_dir).await?;

    let shutdown_handle = daemon.shutdown_handle();
    shutdown::set_shutdown_signal_handler(move || shutdown_handle.shutdown())
        .map_err(|e| e.display_chain())?;

    daemon.run().await.map_err(|e| e.display_chain())?;

    log::info!("Mullvad daemon is quitting");
    thread::sleep(Duration::from_millis(500));
    Ok(())
}

async fn create_daemon(
    log_dir: Option<PathBuf>,
) -> Result<Daemon<ManagementInterfaceEventBroadcaster>, String> {
    let resource_dir = mullvad_paths::get_resource_dir();
    let settings_dir = mullvad_paths::settings_dir()
        .map_err(|e| e.display_chain_with_msg("Unable to get settings dir"))?;
    let cache_dir = mullvad_paths::cache_dir()
        .map_err(|e| e.display_chain_with_msg("Unable to get cache dir"))?;

    let command_channel = DaemonCommandChannel::new();
    let event_listener = spawn_management_interface(command_channel.sender()).await?;

    Daemon::start(
        log_dir,
        resource_dir,
        settings_dir,
        cache_dir,
        event_listener,
        command_channel,
    )
    .await
    .map_err(|e| e.display_chain_with_msg("Unable to initialize daemon"))
}

async fn spawn_management_interface(
    command_sender: DaemonCommandSender,
) -> Result<ManagementInterfaceEventBroadcaster, String> {
    let (socket_path, event_broadcaster) = ManagementInterfaceServer::start(command_sender)
        .await
        .map_err(|error| {
        error.display_chain_with_msg("Unable to start management interface server")
    })?;

    log::info!("Management interface listening on {}", socket_path);

    Ok(event_broadcaster)
}

#[cfg(unix)]
fn running_as_admin() -> bool {
    let uid = unsafe { libc::getuid() };
    uid == 0
}

#[cfg(windows)]
fn running_as_admin() -> bool {
    // TODO: Check if user is administrator correctly on Windows.
    true
}
