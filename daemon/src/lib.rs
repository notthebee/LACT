pub mod config;
pub mod daemon_connection;
pub mod gpu_controller;
pub mod hw_mon;

use config::{Config, GpuConfig};
use gpu_controller::PowerProfile;
use pciid_parser::PciDatabase;
use rand::prelude::*;
use serde::{Deserialize, Serialize};
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::Command;
use std::{
    collections::{BTreeMap, HashMap},
    fs,
};
use std::{
    io::{Read, Write},
    path::PathBuf,
};

use crate::gpu_controller::GpuController;

pub const SOCK_PATH: &str = "/tmp/amdgpu-configurator.sock";

pub struct Daemon {
    gpu_controllers: HashMap<u32, GpuController>,
    listener: UnixListener,
    config: Config,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Action {
    CheckAlive,
    GetConfig,
    SetConfig(Config),
    GetGpus,
    GetInfo(u32),
    GetStats(u32),
    StartFanControl(u32),
    StopFanControl(u32),
    GetFanControl(u32),
    SetFanCurve(u32, BTreeMap<i64, f64>),
    SetPowerCap(u32, i64),
    GetPowerCap(u32),
    SetPowerProfile(u32, PowerProfile),
    // SetGPUPowerState(u32, u32, i64, Option<i64>),
    SetGPUMaxPowerState(u32, i64, Option<i64>),
    SetVRAMMaxClock(u32, i64),
    CommitGPUPowerStates(u32),
    ResetGPUPowerStates(u32),
    Shutdown,
}

impl Daemon {
    pub fn new(unprivileged: bool) -> Daemon {
        if fs::metadata(SOCK_PATH).is_ok() {
            fs::remove_file(SOCK_PATH).expect("Failed to take control over socket");
        }

        let listener = UnixListener::bind(SOCK_PATH).unwrap();

        Command::new("chmod")
            .arg("664")
            .arg(SOCK_PATH)
            .output()
            .expect("Failed to chmod");

        Command::new("chown")
            .arg("nobody:wheel")
            .arg(SOCK_PATH)
            .output()
            .expect("Failed to chown");

        Command::new("chown")
            .arg("nobody:wheel")
            .arg(SOCK_PATH)
            .output()
            .expect("Failed to chown");

        let config_path = PathBuf::from("/etc/lact.json");
        let mut config = if unprivileged {
            Config::new(&config_path)
        } else {
            match Config::read_from_file(&config_path) {
                Ok(c) => {
                    log::info!("Loaded config from {}", c.config_path.to_string_lossy());
                    c
                }
                Err(_) => {
                    log::info!("Config not found, creating");
                    let c = Config::new(&config_path);
                    //c.save().unwrap();
                    c
                }
            }
        };

        log::info!("Using config {:?}", config);

        let gpu_controllers = Self::load_gpu_controllers(&mut config);

        if !unprivileged {
            config.save().unwrap();
        }

        Daemon {
            listener,
            gpu_controllers,
            config,
        }
    }

    fn load_gpu_controllers(config: &mut Config) -> HashMap<u32, GpuController> {
        let pci_db = match config.allow_online_update {
            Some(true) => match Self::get_pci_db_online() {
                Ok(db) => Some(db),
                Err(e) => {
                    log::info!("Error updating PCI db: {:?}", e);
                    None
                }
            },
            Some(false) | None => None,
        };

        let mut gpu_controllers: HashMap<u32, GpuController> = HashMap::new();

        'entries: for entry in
            fs::read_dir("/sys/class/drm").expect("Could not open /sys/class/drm")
        {
            let entry = entry.unwrap();
            if entry.file_name().len() == 5 {
                if entry.file_name().to_str().unwrap().split_at(4).0 == "card" {
                    log::info!("Initializing {:?}", entry.path());

                    let mut controller =
                        GpuController::new(entry.path().join("device"), GpuConfig::new(), &pci_db);

                    let current_identifier = controller.get_identifier();

                    log::info!(
                        "Searching the config for GPU with identifier {:?}",
                        current_identifier
                    );

                    log::info!("{}", &config.gpu_configs.len());
                    for (id, (gpu_identifier, gpu_config)) in &config.gpu_configs {
                        log::info!("Comparing with {:?}", gpu_identifier);
                        if current_identifier == *gpu_identifier {
                            controller.load_config(&gpu_config);
                            gpu_controllers.insert(id.clone(), controller);
                            log::info!("already known");
                            continue 'entries;
                        }

                        /*if gpu_info.pci_slot == gpu_identifier.pci_id
                            && gpu_info.vendor_data.card_model == gpu_identifier.card_model
                            && gpu_info.vendor_data.gpu_model == gpu_identifier.gpu_model
                        {
                            controller.load_config(&gpu_config);
                            gpu_controllers.insert(id.clone(), controller);
                            log::info!("already known");
                            continue 'entries;
                        }*/
                    }

                    log::info!("initializing for the first time");

                    let id: u32 = random();

                    config
                        .gpu_configs
                        .insert(id, (controller.get_identifier(), controller.get_config()));
                    gpu_controllers.insert(id, controller);
                }
            }
        }

        gpu_controllers
    }

    fn get_pci_db_online() -> Result<PciDatabase, reqwest::Error> {
        let vendors = reqwest::blocking::get("https://pci.endpoint.ml/devices.json")?.json()?;
        Ok(PciDatabase { vendors })
    }

    pub fn listen(mut self) {
        let listener = self.listener.try_clone().expect("couldn't try_clone");
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    //let mut controller = self.gpu_controller.clone();
                    //thread::spawn(move || Daemon::handle_connection(&mut controller, stream));
                    //Daemon::handle_connection(&mut self.gpu_controllers, stream);
                    Daemon::handle_connection(&mut self, stream);
                }
                Err(err) => {
                    log::error!("Error: {}", err);
                    break;
                }
            }
        }
    }

    fn handle_connection(&mut self, mut stream: UnixStream) {
        log::trace!("Reading buffer");
        let mut buffer = Vec::<u8>::new();
        stream.read_to_end(&mut buffer).unwrap();
        //log::trace!("finished reading, buffer size {}", buffer.len());
        log::trace!("Attempting to deserialize {:?}", &buffer);
        //log::trace!("{:?}", action);

        match bincode::deserialize::<Action>(&buffer) {
            Ok(action) => {
                log::trace!("Executing action {:?}", action);
                let response: Result<DaemonResponse, DaemonError> = match action {
                    Action::CheckAlive => Ok(DaemonResponse::OK),
                    Action::GetGpus => {
                        let mut gpus: HashMap<u32, Option<String>> = HashMap::new();
                        for (id, controller) in &self.gpu_controllers {
                            gpus.insert(*id, controller.get_info().vendor_data.gpu_model.clone());
                        }
                        Ok(DaemonResponse::Gpus(gpus))
                    }
                    Action::GetStats(i) => match self.gpu_controllers.get(&i) {
                        Some(controller) => match controller.get_stats() {
                            Ok(stats) => Ok(DaemonResponse::GpuStats(stats)),
                            Err(_) => Err(DaemonError::HWMonError),
                        },
                        None => Err(DaemonError::InvalidID),
                    },
                    Action::GetInfo(i) => match self.gpu_controllers.get(&i) {
                        Some(controller) => {
                            Ok(DaemonResponse::GpuInfo(controller.get_info().clone()))
                        }
                        None => Err(DaemonError::InvalidID),
                    },
                    Action::StartFanControl(i) => match self.gpu_controllers.get_mut(&i) {
                        Some(controller) => match controller.start_fan_control() {
                            Ok(_) => {
                                self.config.gpu_configs.insert(
                                    i,
                                    (controller.get_identifier(), controller.get_config()),
                                );
                                self.config.save().unwrap();
                                Ok(DaemonResponse::OK)
                            }
                            Err(_) => Err(DaemonError::HWMonError),
                        },
                        None => Err(DaemonError::InvalidID),
                    },
                    Action::StopFanControl(i) => match self.gpu_controllers.get_mut(&i) {
                        Some(controller) => match controller.stop_fan_control() {
                            Ok(_) => {
                                self.config.gpu_configs.insert(
                                    i,
                                    (controller.get_identifier(), controller.get_config()),
                                );
                                self.config.save().unwrap();
                                Ok(DaemonResponse::OK)
                            }
                            Err(_) => Err(DaemonError::HWMonError),
                        },
                        None => Err(DaemonError::InvalidID),
                    },
                    Action::GetFanControl(i) => match self.gpu_controllers.get(&i) {
                        Some(controller) => match controller.get_fan_control() {
                            Ok(info) => Ok(DaemonResponse::FanControlInfo(info)),
                            Err(_) => Err(DaemonError::HWMonError),
                        },
                        None => Err(DaemonError::InvalidID),
                    },
                    Action::SetFanCurve(i, curve) => match self.gpu_controllers.get_mut(&i) {
                        Some(controller) => match controller.set_fan_curve(curve) {
                            Ok(_) => {
                                self.config.gpu_configs.insert(
                                    i,
                                    (controller.get_identifier(), controller.get_config()),
                                );
                                self.config.save().unwrap();
                                Ok(DaemonResponse::OK)
                            }
                            Err(_) => Err(DaemonError::HWMonError),
                        },
                        None => Err(DaemonError::InvalidID),
                    },
                    Action::SetPowerCap(i, cap) => match self.gpu_controllers.get_mut(&i) {
                        Some(controller) => match controller.set_power_cap(cap) {
                            Ok(_) => {
                                self.config.gpu_configs.insert(
                                    i,
                                    (controller.get_identifier(), controller.get_config()),
                                );
                                self.config.save().unwrap();
                                Ok(DaemonResponse::OK)
                            }
                            Err(_) => Err(DaemonError::HWMonError),
                        },
                        None => Err(DaemonError::InvalidID),
                    },
                    Action::GetPowerCap(i) => match self.gpu_controllers.get(&i) {
                        Some(controller) => match controller.get_power_cap() {
                            Ok(cap) => Ok(DaemonResponse::PowerCap(cap)),
                            Err(_) => Err(DaemonError::HWMonError),
                        },
                        None => Err(DaemonError::InvalidID),
                    },
                    Action::SetPowerProfile(i, profile) => match self.gpu_controllers.get_mut(&i) {
                        Some(controller) => match controller.set_power_profile(profile) {
                            Ok(_) => {
                                self.config.gpu_configs.insert(
                                    i,
                                    (controller.get_identifier(), controller.get_config()),
                                );
                                self.config.save().unwrap();
                                Ok(DaemonResponse::OK)
                            }
                            Err(_) => Err(DaemonError::ControllerError),
                        },
                        None => Err(DaemonError::InvalidID),
                    },
                    /*Action::SetGPUPowerState(i, num, clockspeed, voltage) => {
                        match self.gpu_controllers.get_mut(&i) {
                            Some(controller) => {
                                match controller.set_gpu_power_state(num, clockspeed, voltage) {
                                    Ok(_) => {
                                        self.config.gpu_configs.insert(
                                            i,
                                            (controller.get_identifier(), controller.get_config()),
                                        );
                                        self.config.save().unwrap();
                                        Ok(DaemonResponse::OK)
                                    }
                                    Err(_) => Err(DaemonError::ControllerError),
                                }
                            }
                            None => Err(DaemonError::InvalidID),
                        }
                    }*/
                    Action::SetGPUMaxPowerState(i, clockspeed, voltage) => {
                        match self.gpu_controllers.get_mut(&i) {
                            Some(controller) => {
                                match controller.set_gpu_max_power_state(clockspeed, voltage) {
                                    Ok(()) => {
                                        self.config.gpu_configs.insert(
                                            i,
                                            (controller.get_identifier(), controller.get_config()),
                                        );
                                        self.config.save().unwrap();
                                        Ok(DaemonResponse::OK)
                                    }
                                    Err(_) => Err(DaemonError::ControllerError),
                                }
                            }
                            None => Err(DaemonError::InvalidID),
                        }
                    }
                    Action::SetVRAMMaxClock(i, clockspeed) => {
                        match self.gpu_controllers.get_mut(&i) {
                            Some(controller) => {
                                match controller.set_vram_max_clockspeed(clockspeed) {
                                    Ok(()) => {
                                        self.config.gpu_configs.insert(
                                            i,
                                            (controller.get_identifier(), controller.get_config()),
                                        );
                                        self.config.save().unwrap();
                                        Ok(DaemonResponse::OK)
                                    }
                                    Err(_) => Err(DaemonError::ControllerError),
                                }
                            }
                            None => Err(DaemonError::InvalidID),
                        }
                    }
                    Action::CommitGPUPowerStates(i) => match self.gpu_controllers.get_mut(&i) {
                        Some(controller) => match controller.commit_gpu_power_states() {
                            Ok(_) => {
                                self.config.gpu_configs.insert(
                                    i,
                                    (controller.get_identifier(), controller.get_config()),
                                );
                                self.config.save().unwrap();
                                Ok(DaemonResponse::OK)
                            }
                            Err(_) => Err(DaemonError::ControllerError),
                        },
                        None => Err(DaemonError::InvalidID),
                    },
                    Action::ResetGPUPowerStates(i) => match self.gpu_controllers.get_mut(&i) {
                        Some(controller) => match controller.reset_gpu_power_states() {
                            Ok(_) => {
                                self.config.gpu_configs.insert(
                                    i,
                                    (controller.get_identifier(), controller.get_config()),
                                );
                                self.config.save().unwrap();
                                Ok(DaemonResponse::OK)
                            }
                            Err(_) => Err(DaemonError::ControllerError),
                        },
                        None => Err(DaemonError::InvalidID),
                    },
                    Action::Shutdown => {
                        for (id, controller) in &mut self.gpu_controllers {
                            #[allow(unused_must_use)]
                            {
                                controller.reset_gpu_power_states();
                                controller.commit_gpu_power_states();
                                controller.set_power_profile(PowerProfile::Auto);

                                if self
                                    .config
                                    .gpu_configs
                                    .get(id)
                                    .unwrap()
                                    .1
                                    .fan_control_enabled
                                {
                                    controller.stop_fan_control();
                                }
                            }
                            fs::remove_file(SOCK_PATH).expect("Failed to remove socket");
                        }
                        std::process::exit(0);
                    }
                    Action::SetConfig(config) => {
                        self.config = config;
                        self.gpu_controllers.clear();
                        self.gpu_controllers = Self::load_gpu_controllers(&mut self.config);
                        self.config.save().expect("Failed to save config");
                        Ok(DaemonResponse::OK)
                    }
                    Action::GetConfig => Ok(DaemonResponse::Config(self.config.clone())),
                };

                log::trace!("Responding");
                stream
                    .write_all(&bincode::serialize(&response).unwrap())
                    .expect("Failed writing response");
                //stream
                //    .shutdown(std::net::Shutdown::Write)
                //    .expect("Could not shut down");
                log::trace!("Finished responding");
            }
            Err(_) => {
                println!("Failed deserializing action");
            }
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub enum DaemonResponse {
    OK,
    GpuInfo(gpu_controller::GpuInfo),
    GpuStats(gpu_controller::GpuStats),
    Gpus(HashMap<u32, Option<String>>),
    PowerCap((i64, i64)),
    FanControlInfo(gpu_controller::FanControlInfo),
    Config(Config),
}

#[derive(Serialize, Deserialize, Debug)]
pub enum DaemonError {
    ConnectionFailed,
    InvalidID,
    HWMonError,
    ControllerError,
}
