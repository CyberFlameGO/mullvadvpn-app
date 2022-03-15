use super::{Error, Result};
use crate::{device::DeviceService, DaemonEventSender, InternalDaemonEvent};
use mullvad_types::{
    account::AccountToken, device::DeviceData, relay_constraints::Constraint,
    settings::SettingsVersion, wireguard::WireguardData,
};
use talpid_core::mpsc::Sender;
use talpid_types::ErrorExt;

// ======================================================
// Section for vendoring types and values that
// this settings version depend on. See `mod.rs`.

/// Representation of a transport protocol, either UDP or TCP.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportProtocol {
    /// Represents the UDP transport protocol.
    Udp,
    /// Represents the TCP transport protocol.
    Tcp,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct TransportPort {
    pub protocol: TransportProtocol,
    pub port: Constraint<u16>,
}

/// Contains obfuscation settings
#[derive(Debug, Default, Clone, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ObfuscationSettings {
    pub selected_obfuscation: SelectedObfuscation,
    pub udp2tcp: Udp2TcpObfuscationSettings,
}

#[derive(Debug, Default, Clone, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct Udp2TcpObfuscationSettings {
    pub port: Constraint<u16>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectedObfuscation {
    Auto,
    Off,
    Udp2Tcp,
}

impl Default for SelectedObfuscation {
    fn default() -> Self {
        SelectedObfuscation::Auto
    }
}

// ======================================================

/// This is an open ended migration. There is no v6 yet!
/// The migrations performed by this function are still backwards compatible.
/// The JSON coming out of this migration can be read by any v5 compatible daemon.
///
/// When further migrations are needed, add them here and if they are not backwards
/// compatible then create v6 and "close" this migration for further modification.
///
/// # Changes to the format
///
/// The ability to disable WireGuard multihop while preserving the entry location was added.
/// So a new field, `use_multihop` is introduced. We want this to default to `true` iff:
///  * `use_mulithop` was not present in the settings
///  * A multihop entry location had been previously specified.
///
/// It is also no longer valid to have `entry_location` set to null. So remove the field if it
/// is null in order to make it default back to the default location.
///
/// This also removes the account token and WireGuard key from the settings, looks up the
/// corresponding device, and eventually stores them in `device.json` instead. This is done by
/// sending the `DeviceMigrationEvent` event to the daemon. Because this is fallible, it can
/// result in the account token and private key being lost. This should not be not critical since
/// the account token is also stored in the account history.
pub(crate) async fn migrate(
    settings: &mut serde_json::Value,
    rest_handle: mullvad_rpc::rest::MullvadRestHandle,
    daemon_tx: DaemonEventSender,
) -> Result<()> {
    let migration_data = migrate_inner(settings).await?;

    if let Some(migration_data) = migration_data {
        let api_handle = rest_handle.availability.clone();
        let service = DeviceService::new(rest_handle, api_handle);
        match (migration_data.token, migration_data.wg_data) {
            (token, Some(wg_data)) => {
                log::info!("Creating a new device cache from previous settings");
                tokio::spawn(cache_from_wireguard_key(daemon_tx, service, token, wg_data));
            }
            (token, None) => {
                log::info!("Generating a new device for the account");
                tokio::spawn(cache_from_account(daemon_tx, service, token));
            }
        }
    }

    Ok(())
}

struct MigrationData {
    token: AccountToken,
    wg_data: Option<WireguardData>,
}

fn get_wireguard_constraints(settings: &mut serde_json::Value) -> Option<&mut serde_json::Value> {
    if let Some(relay_settings) = settings.get_mut("relay_settings") {
        if let Some(normal) = relay_settings.get_mut("normal") {
            return normal.get_mut("wireguard_constraints");
        }
    }
    None
}

async fn migrate_inner(settings: &mut serde_json::Value) -> Result<Option<MigrationData>> {
    if !version_matches(settings) {
        return Ok(None);
    }
    if let Some(wireguard_constraints) = get_wireguard_constraints(settings) {
        if let Some(location) = wireguard_constraints.get("entry_location") {
            if wireguard_constraints.get("use_multihop").is_none() {
                if location.is_null() {
                    // "Null" is no longer valid. It is not an option.
                    wireguard_constraints
                        .as_object_mut()
                        .ok_or(Error::NoMatchingVersion)?
                        .remove("entry_location");
                } else {
                    wireguard_constraints["use_multihop"] = serde_json::json!(true);
                }
            }
        }
        //
        // The field `pub port: Constraint<TransportPort>` is now `pub port: Constraint<u16>`.
        // Data is migrated as follows:
        // If the existing field has `protocol == Tcp` configured, then we need to create a corresponding
        // setting to enable the Udp2Tcp obfuscator. In this case, the port constraint is moved as well.
        // Otherwise the existing port constraint is moved into the new field.
        //
        if let Some(port) = wireguard_constraints.get("port") {
            let port_constraint: Constraint<TransportPort> =
                serde_json::from_value(port.clone()).map_err(Error::ParseError)?;
            if let Some(transport_port) = port_constraint.option() {
                let (port, obfuscation_settings) =
                    match (transport_port.protocol, transport_port.port) {
                        (TransportProtocol::Udp, port) => (serde_json::json!(port), None),
                        (TransportProtocol::Tcp, port) => (
                            serde_json::json!(Constraint::<u16>::Any),
                            Some(serde_json::json!(create_migrated_obfuscation_settings(
                                port
                            ))),
                        ),
                    };
                wireguard_constraints["port"] = port;
                if let Some(obfuscation_settings) = obfuscation_settings {
                    settings["obfuscation_settings"] = obfuscation_settings;
                }
            }
        }
    }

    let migration_data =
        if let Some(token) = settings.get("account_token").filter(|t| !t.is_null()) {
            let token: AccountToken =
                serde_json::from_value(token.clone()).map_err(Error::ParseError)?;
            let mig_data = if let Some(wg_data) = settings.get("wireguard").filter(|wg| !wg.is_null()) {
                let wg_data: WireguardData =
                    serde_json::from_value(wg_data.clone()).map_err(Error::ParseError)?;
                Some(MigrationData {
                    token,
                    wg_data: Some(wg_data),
                })
            } else {
                Some(MigrationData {
                    token,
                    wg_data: None,
                })
            };

            let settings_map = settings.as_object_mut().ok_or(Error::NoMatchingVersion)?;
            settings_map.remove("account_token");
            settings_map.remove("wireguard");

            mig_data
        }
        else
        {
            None
        };

    settings["settings_version"] = serde_json::json!(SettingsVersion::V6);

    Ok(migration_data)
}

//
// Create an ObfuscationSettings struct that replaces the `protocol == TCP` setting
// that was previously used on the wireguard constraints.
// If a port is specified, this is the remote port to be used for Udp2Tcp.
//
fn create_migrated_obfuscation_settings(port: Constraint<u16>) -> ObfuscationSettings {
    ObfuscationSettings {
        selected_obfuscation: SelectedObfuscation::Udp2Tcp,
        udp2tcp: Udp2TcpObfuscationSettings { port },
    }
}

fn version_matches(settings: &mut serde_json::Value) -> bool {
    settings
        .get("settings_version")
        .map(|version| version == SettingsVersion::V5 as u64)
        .unwrap_or(false)
}

async fn cache_from_wireguard_key(
    daemon_tx: DaemonEventSender,
    service: DeviceService,
    token: AccountToken,
    wg_data: WireguardData,
) {
    let devices = match service.list_devices_with_backoff(token.clone()).await {
        Ok(devices) => devices,
        Err(error) => {
            log::error!(
                "{}",
                error.display_chain_with_msg("Failed to enumerate devices for account")
            );
            return;
        }
    };

    for device in devices.into_iter() {
        if device.pubkey == wg_data.private_key.public_key() {
            let _ = daemon_tx.send(InternalDaemonEvent::DeviceMigrationEvent(DeviceData {
                token,
                device,
                wg_data,
            }));
            return;
        }
    }
    log::info!("The existing WireGuard key is not valid; generating a new device");
    cache_from_account(daemon_tx, service, token).await;
}

async fn cache_from_account(
    daemon_tx: DaemonEventSender,
    service: DeviceService,
    token: AccountToken,
) {
    match service.generate_for_account_with_backoff(token).await {
        Ok(device_data) => {
            let _ = daemon_tx.send(InternalDaemonEvent::DeviceMigrationEvent(device_data));
        }
        Err(error) => log::error!(
            "{}",
            error.display_chain_with_msg("Failed to generate new device for account")
        ),
    }
}

#[cfg(test)]
mod test {
    use super::{migrate_inner, version_matches};
    use serde_json;

    pub const V5_SETTINGS: &str = r#"
{
  "account_token": "1234",
  "relay_settings": {
    "normal": {
      "location": {
        "only": {
          "country": "se"
        }
      },
      "tunnel_protocol": "any",
      "wireguard_constraints": {
        "port": {
            "only": {
              "protocol": "tcp",
              "port": "any"
            }
        },
        "ip_version": "any",
        "entry_location": "any"
      },
      "openvpn_constraints": {
        "port": {
          "only": {
            "protocol": "udp",
            "port": {
              "only": 1195
            }
          }
        }
      }
    }
  },
  "bridge_settings": {
    "normal": {
      "location": "any"
    }
  },
  "bridge_state": "auto",
  "allow_lan": true,
  "block_when_disconnected": false,
  "auto_connect": false,
  "tunnel_options": {
    "openvpn": {
      "mssfix": null
    },
    "wireguard": {
      "mtu": null,
      "rotation_interval": {
          "secs": 86400,
          "nanos": 0
      }
    },
    "generic": {
      "enable_ipv6": false
    },
    "dns_options": {
      "state": "default",
      "default_options": {
        "block_ads": false,
        "block_trackers": false
      },
      "custom_options": {
        "addresses": [
          "1.1.1.1",
          "1.2.3.4"
        ]
      }
    }
  },
  "settings_version": 5
}
"#;

    pub const V6_SETTINGS: &str = r#"
{
  "relay_settings": {
    "normal": {
      "location": {
        "only": {
          "country": "se"
        }
      },
      "tunnel_protocol": "any",
      "wireguard_constraints": {
        "port": "any",
        "ip_version": "any",
        "use_multihop": true,
        "entry_location": "any"
      },
      "openvpn_constraints": {
        "port": {
          "only": {
            "protocol": "udp",
            "port": {
              "only": 1195
            }
          }
        }
      }
    }
  },
  "bridge_settings": {
    "normal": {
      "location": "any"
    }
  },
  "obfuscation_settings": {
    "selected_obfuscation": "udp2_tcp",
    "udp2tcp": {
      "port": "any"
    }
  },
  "bridge_state": "auto",
  "allow_lan": true,
  "block_when_disconnected": false,
  "auto_connect": false,
  "tunnel_options": {
    "openvpn": {
      "mssfix": null
    },
    "wireguard": {
      "mtu": null,
      "rotation_interval": {
          "secs": 86400,
          "nanos": 0
      }
    },
    "generic": {
      "enable_ipv6": false
    },
    "dns_options": {
      "state": "default",
      "default_options": {
        "block_ads": false,
        "block_trackers": false
      },
      "custom_options": {
        "addresses": [
          "1.1.1.1",
          "1.2.3.4"
        ]
      }
    }
  },
  "settings_version": 6
}
"#;

    #[tokio::test]
    async fn test_v5_to_v6_migration() {
        let mut old_settings = serde_json::from_str(V5_SETTINGS).unwrap();

        assert!(version_matches(&mut old_settings));
        migrate_inner(&mut old_settings).await.unwrap();
        let new_settings: serde_json::Value = serde_json::from_str(V6_SETTINGS).unwrap();

        assert_eq!(&old_settings, &new_settings);
    }
}
