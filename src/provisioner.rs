use cloudevents::{event::AttributeValue, Data, Event};
use drogue_client::{
    core::v1::{ConditionStatus, Conditions},
    dialect,
    meta::v1::CommonMetadataMut,
    registry::v1::Device,
    Section, Translator,
};
use std::collections::HashSet;
use futures::stream::StreamExt;
use paho_mqtt as mqtt;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::{join, time::Duration};

pub type DrogueClient = drogue_client::registry::v1::Client;

pub struct Operator {
    client: mqtt::AsyncClient,
    group_id: Option<String>,
    application: String,
    gateways: Mutex<Vec<String>>,
    registry: DrogueClient,
    interval: Duration,
}

impl Operator {
    pub fn new(
        client: mqtt::AsyncClient,
        group_id: Option<String>,
        application: String,
        registry: DrogueClient,
        interval: Duration,
    ) -> Self {
        Self {
            client,
            group_id,
            application,
            registry,
            interval,
            gateways: Mutex::new(Vec::new()),
        }
    }

    pub async fn publish_gateways(&self, command: BtMeshCommand) {
        if let Ok(command) = serde_json::to_vec(&command) {
            let gws = self.gateways.lock().await;
            for gw in gws.iter() {
                let topic = format!("command/{}/{}/btmesh", self.application, gw);
                log::info!("Sending command to gateway {}", gw);
                let message = mqtt::Message::new(topic, &command[..], 1);
                if let Err(e) = self.client.publish(message).await {
                    log::warn!("Error publishing command back to device: {:?}", e);
                }
            }
        }
    }

    pub async fn provision_devices(&self, mut devices: Vec<Device>) {
        for device in devices.iter_mut() {
            if let Some(Ok(spec)) = device.section::<BtMeshSpec>() {
                let status: BtMeshStatus =
                    if let Some(Ok(status)) = device.section::<BtMeshStatus>() {
                        status
                    } else {
                        BtMeshStatus {
                            address: None,
                            conditions: Default::default(),
                        }
                    };

                let uuid = spec.device.to_ascii_lowercase();
                let mut updated = false;
                updated |= Self::ensure_alias(device, &uuid);

                if device.metadata.deletion_timestamp.is_none() {
                    log::info!("Device {} is active", device.metadata.name);
                    updated |= device.metadata.ensure_finalizer("btmesh-operator");
                    self.update_device(device, status.clone(), updated).await;

                    // Send provisioning command for this device
                    if status.address.is_none() {
                        log::info!(
                            "Sending provisioning command to device {}",
                            device.metadata.name
                        );

                        self.publish_gateways(BtMeshCommand {
                            command: BtMeshOperation::Provision {
                                device: uuid.clone(),
                            },
                        })
                        .await;
                    }
                } else {
                    self.update_device(device, status.clone(), updated).await;
                    log::info!(
                        "Device {} is being deleted, sending reset command",
                        device.metadata.name
                    );
                    log::debug!("Device state: {:?}", device);
                    if let Some(address) = &status.address {
                        let command = BtMeshCommand {
                            command: BtMeshOperation::Reset {
                                address: *address,
                                device: device.metadata.name.clone(),
                            },
                        };
                        self.publish_gateways(command).await;
                    }
                }
            }
        }
    }

    pub async fn update_device(&self, device: &mut Device, status: BtMeshStatus, update: bool) {
        let updated = if let Some(Ok(s)) = device.section::<BtMeshStatus>() {
            status != s || update
        } else {
            update
        };

        log::info!("Updating device: {}", updated);
        if updated {
            if let Ok(_) = device.set_section::<BtMeshStatus>(status) {
                log::debug!("Updating device state: {:?}", device);
                match self.registry.update_device(&device).await {
                    Ok(_) => log::debug!("Device {} status updated", device.metadata.name),
                    Err(e) => {
                        log::warn!(
                            "Device {} status update error: {:?}",
                            device.metadata.name,
                            e
                        );
                    }
                }
            }
        }
    }

    pub async fn reconcile_devices(&self) {
        log::info!("Reconciling devices with interval {:?}", self.interval);

        loop {
            let devices = self
                .registry
                .list_devices(&self.application, None)
                .await
                .unwrap_or(None)
                .unwrap_or(Vec::new());
            let mut gateways = Vec::new();
            for device in devices.iter() {
                if let Some("gateway") = device.metadata.labels.get("role").map(|s| s.as_str()) {
                    gateways.push(device.metadata.name.clone());
                }
            }
            let mut gws = self.gateways.lock().await;
            log::info!("Loaded gateways: {:?}", gateways);
            *gws = gateways;
            drop(gws);
            self.provision_devices(devices).await;
            tokio::time::sleep(self.interval).await;
        }
    }

    pub async fn run(&mut self) -> Result<(), anyhow::Error> {
        if let Some(group_id) = &self.group_id {
            self.client.subscribe(
                format!("$shared/{}/app/{}", &group_id, &self.application),
                1,
            );
        } else {
            self.client
                .subscribe(format!("app/{}", &self.application), 1);
        }

        let stream = self.client.get_stream(100);
        join!(self.reconcile_devices(), self.process_events(stream));
        Ok(())
    }

    async fn lookup_device(&self, device: &str) -> Option<Device> {
        self.registry
            .list_devices(&self.application, None)
            .await
            .unwrap_or(None)
            .unwrap_or(Vec::new())
            .iter()
            .find(|d| {
                let aliases: HashSet<String> = d
                    .spec
                    .get("alias")
                    .map(|s| {
                        if let Some(v) = s.as_array() {
                            v.iter()
                                .map(|e| e.as_str().map(|s| s.to_string()))
                                .flatten()
                                .collect()
                        } else {
                            HashSet::new()
                        }
                    })
                    .unwrap_or(HashSet::new());
                d.metadata.name == device || aliases.contains(device)
            }).map(|d| d.clone())
    }

    fn ensure_alias(device: &mut Device, alias: &str) -> bool {
        let mut aliases: Vec<String> = device
            .spec
            .get("alias")
            .map(|s| {
                if let Some(v) = s.as_array() {
                    v.iter()
                        .map(|e| e.as_str().map(|s| s.to_string()))
                        .flatten()
                        .collect()
                } else {
                    Vec::new()
                }
            })
            .unwrap_or(Vec::new());

        let mut ret = false;
        let alias = alias.to_string();
        if !aliases.contains(&alias) {
            aliases.push(alias);
            ret = true;
        }

        device
            .spec
            .insert("alias".to_string(), serde_json::json!(aliases));
        ret
    }

    pub async fn process_events(
        &self,
        mut stream: paho_mqtt::AsyncReceiver<Option<mqtt::Message>>,
    ) {
        log::info!("Processing events events");
        loop {
            if let Some(m) = stream.next().await {
                if let Some(m) = m {
                    match serde_json::from_slice::<Event>(m.payload()) {
                        Ok(e) => {
                            let mut subject = String::new();
                            for a in e.iter() {
                                log::trace!("Attribute {:?}", a);
                                if a.0 == "subject" {
                                    if let AttributeValue::String(s) = a.1 {
                                        subject = s.to_string();
                                    }
                                }
                            }

                            if subject == "devices" {
                                log::debug!("Got event on devices channel: {:?}", e);
                                let devices = self
                                    .registry
                                    .list_devices(&self.application, None)
                                    .await
                                    .unwrap_or(None)
                                    .unwrap_or(Vec::new());

                                self.provision_devices(devices).await;
                            } else if subject == "btmesh" {
                                log::debug!("Got event on btmesh channel: {:?}", e);

                                let event: Option<BtMeshEvent> = match e.data() {
                                    Some(Data::Json(v)) => serde_json::from_value(v.clone())
                                        .map(|e| Some(e))
                                        .unwrap_or(None),
                                    _ => None,
                                };

                                if let Some(event) = event {
                                    // Reset events are not sent on behalf of devices
                                    let device = match &event.status {
                                        BtMeshDeviceState::Reset { device, error: _ } => {
                                            device.clone()
                                        }
                                        BtMeshDeviceState::Provisioned { device, address: _ } => {
                                            device.clone()
                                        }
                                        BtMeshDeviceState::Provisioning { device, error: _ } => {
                                            device.clone()
                                        }
                                    };

                                    log::debug!("Lookup device {}", device);
                                    let device = self.lookup_device(&device).await;

                                    if let Some(mut device) = device {
                                        let mut updated = false;
                                        if device.metadata.deletion_timestamp.is_none() {
                                            updated |=
                                                device.metadata.ensure_finalizer("btmesh-operator");
                                        }
                                        let mut status: BtMeshStatus = if let Some(Ok(status)) =
                                            device.section::<BtMeshStatus>()
                                        {
                                            status
                                        } else {
                                            BtMeshStatus {
                                                address: None,
                                                conditions: Default::default(),
                                            }
                                        };
                                        log::debug!("Found device! Original status: {:?}", status);

                                        match &event.status {
                                            BtMeshDeviceState::Reset { device: _, error } => {
                                                if let Some(error) = error {
                                                    let mut condition = ConditionStatus::default();
                                                    condition.status = Some(true);
                                                    condition.reason =
                                                        Some("Error resetting device".to_string());
                                                    condition.message = Some(error.clone());
                                                    status
                                                        .conditions
                                                        .update("Provisioned", condition);
                                                    status.conditions.update("Provisioning", false);
                                                    updated = true;
                                                } else {
                                                    status.conditions.update("Provisioned", false);
                                                    status.conditions.update("Provisioning", false);
                                                    device
                                                        .metadata
                                                        .remove_finalizer("btmesh-operator");
                                                    updated = true;
                                                }
                                            }
                                            // If we're provisioned, update the status and insert alias in spec if its not already there
                                            BtMeshDeviceState::Provisioned {
                                                device: _,
                                                address,
                                            } => {
                                                status.conditions.update("Provisioned", true);
                                                status.conditions.update("Provisioning", false);
                                                status.address = Some(*address);
                                                let a = address.to_be_bytes();
                                                let alias = format!("{:02x}{:02x}", a[0], a[1]);
                                                Self::ensure_alias(&mut device, &alias);
                                                updated = true;
                                            }
                                            BtMeshDeviceState::Provisioning {
                                                device: _,
                                                error,
                                            } => {
                                                // If we're provisioned, we cant move back to being provisioning!
                                                if status.address.is_none() {
                                                    status.conditions.update("Provisioning", true);
                                                    let mut condition = ConditionStatus::default();
                                                    if let Some(error) = error {
                                                        condition.status = Some(false);
                                                        condition.reason = Some(
                                                            "Error provisioning device".to_string(),
                                                        );
                                                        condition.message = Some(error.clone());
                                                    }
                                                    status
                                                        .conditions
                                                        .update("Provisioned", condition);
                                                }
                                            }
                                        }
                                        log::debug!("Going to update device. Device: {:?}. Status: {:?} Updated {:?}", device, status, updated);
                                        self.update_device(&mut device, status, updated).await;
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            log::warn!("Error parsing event: {:?}", e);
                            break;
                        }
                    }
                }
            }
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct BtMeshEvent {
    pub status: BtMeshDeviceState,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct BtMeshCommand {
    pub command: BtMeshOperation,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum BtMeshOperation {
    #[serde(rename = "provision")]
    Provision { device: String },
    #[serde(rename = "reset")]
    Reset { device: String, address: u16 },
}

#[derive(Serialize, Deserialize, Debug, Eq, PartialEq, Clone)]
pub enum BtMeshDeviceState {
    #[serde(rename = "provisioning")]
    Provisioning {
        device: String,
        error: Option<String>,
    },

    #[serde(rename = "provisioned")]
    Provisioned { device: String, address: u16 },

    #[serde(rename = "reset")]
    Reset {
        device: String,
        error: Option<String>,
    },
}

dialect!(BtMeshSpec [Section::Spec => "btmesh"]);

#[derive(Serialize, Deserialize, Debug)]
pub struct BtMeshSpec {
    pub device: String,
}

dialect!(BtMeshStatus [Section::Status => "btmesh"]);

#[derive(Serialize, Deserialize, Debug, Clone, Eq, PartialEq)]
pub struct BtMeshStatus {
    pub conditions: Conditions,
    pub address: Option<u16>,
}
