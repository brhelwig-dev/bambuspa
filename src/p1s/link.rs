use std::sync::{Arc, Mutex};

use tokio::sync::watch;

use super::StateCache;
use super::mqtt::Client;
use crate::activity::Log;
use crate::clock::Clock;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    pub ip: String,
    pub serial: String,
    pub access_code: String,
}

impl Config {
    pub fn complete(&self) -> bool {
        !self.ip.is_empty() && !self.serial.is_empty() && !self.access_code.is_empty()
    }
}

#[derive(Clone, Copy)]
pub struct Ports {
    pub mqtt: u16,
    pub camera: u16,
    pub ftp: u16,
}

impl Default for Ports {
    fn default() -> Self {
        Ports {
            mqtt: 8883,
            camera: 6000,
            ftp: 990,
        }
    }
}

#[derive(Clone)]
pub struct Link {
    cache: StateCache,
    log: Log,
    clock: Clock,
    ports: Ports,
    config: watch::Sender<Config>,
    client: Arc<Mutex<Option<Client>>>,
}

impl Link {
    pub fn new(cache: StateCache, log: Log, clock: Clock, ports: Ports) -> Link {
        Link {
            cache,
            log,
            clock,
            ports,
            config: watch::Sender::new(Config::default()),
            client: Arc::default(),
        }
    }

    pub fn config(&self) -> Config {
        self.config.borrow().clone()
    }

    pub fn configure(&self, conf: Config) {
        let mut client = self.client.lock().unwrap_or_else(|p| p.into_inner());
        drop(client.take());
        self.cache.reset();
        if conf.complete() {
            *client = Some(Client::start(
                &conf.ip,
                self.ports.mqtt,
                &conf.serial,
                &conf.access_code,
                self.cache.clone(),
                self.log.clone(),
                self.clock.clone(),
            ));
        }
        self.config.send_replace(conf);
    }

    pub fn stop(&self) {
        drop(self.client.lock().unwrap_or_else(|p| p.into_inner()).take());
    }

    pub fn watch_config(&self) -> watch::Receiver<Config> {
        self.config.subscribe()
    }

    pub fn mqtt_port(&self) -> u16 {
        self.ports.mqtt
    }

    pub fn camera_port(&self) -> u16 {
        self.ports.camera
    }

    pub fn ftp_port(&self) -> u16 {
        self.ports.ftp
    }

    fn send(&self, f: impl FnOnce(&Client)) {
        if let Some(client) = self
            .client
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            f(client);
        }
    }

    pub fn lower_bed(&self) {
        self.send(Client::lower_bed)
    }
    pub fn home(&self) {
        self.send(Client::home)
    }
    pub fn extrude(&self) {
        self.send(Client::extrude)
    }
    pub fn unload_filament(&self) {
        self.send(Client::unload_filament)
    }
    pub fn pause(&self) {
        self.send(Client::pause)
    }
    pub fn resume(&self) {
        self.send(Client::resume)
    }
    pub fn stop_print(&self) {
        self.send(Client::stop)
    }
    pub fn set_bed_temp(&self, t: i64) {
        self.send(|c| c.set_bed_temp(t))
    }
    pub fn set_nozzle_temp(&self, t: i64) {
        self.send(|c| c.set_nozzle_temp(t))
    }
    pub fn set_chamber_light(&self, on: bool) {
        self.send(|c| c.set_chamber_light(on))
    }
    #[allow(clippy::too_many_arguments)]
    pub fn set_ams_filament(
        &self,
        ams_id: i64,
        tray_id: i64,
        info_idx: &str,
        color: &str,
        kind: &str,
        min: i64,
        max: i64,
    ) {
        self.send(|c| c.set_ams_filament(ams_id, tray_id, info_idx, color, kind, min, max))
    }

    pub fn describe(&self) -> String {
        let conf = self.config();
        if conf.complete() {
            format!("printer {}", conf.ip)
        } else {
            "no printer configured".into()
        }
    }
}
