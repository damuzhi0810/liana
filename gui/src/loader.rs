use std::convert::From;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use iced::{
    widget::{Column, Container, ProgressBar, Row},
    Element,
};
use iced::{Alignment, Command, Length, Subscription};
use tracing::{debug, info};

use liana::{
    config::{Config, ConfigError},
    miniscript::bitcoin,
    signer::HotSigner,
    StartupError,
};

use crate::{
    app::{
        cache::Cache,
        config::Config as GUIConfig,
        settings::{self, Settings},
        wallet::Wallet,
    },
    daemon::{client, embedded::EmbeddedDaemon, model::*, Daemon, DaemonError},
    signer::Signer,
    ui::{
        component::{button, notification, text::*},
        icon,
        util::Collection,
    },
};

type Lianad = client::Lianad<client::jsonrpc::JsonRPCClient>;

pub struct Loader {
    pub datadir_path: PathBuf,
    pub network: bitcoin::Network,
    pub gui_config: GUIConfig,
    pub daemon_started: bool,

    daemon_config: Config,
    step: Step,
}

pub enum Step {
    Connecting,
    StartingDaemon,
    Syncing {
        daemon: Arc<dyn Daemon + Sync + Send>,
        progress: f64,
    },
    Error(Box<Error>),
}

#[allow(clippy::type_complexity)]
#[derive(Debug)]
pub enum Message {
    View(ViewMessage),
    Syncing(Result<GetInfoResult, DaemonError>),
    Synced(Result<(Arc<Wallet>, Cache, Arc<dyn Daemon + Sync + Send>), Error>),
    Started(Result<Arc<dyn Daemon + Sync + Send>, Error>),
    Loaded(Result<Arc<dyn Daemon + Sync + Send>, Error>),
    Failure(DaemonError),
}

impl Loader {
    pub fn new(
        datadir_path: PathBuf,
        gui_config: GUIConfig,
        daemon_config: Config,
    ) -> (Self, Command<Message>) {
        let path = socket_path(&datadir_path, daemon_config.bitcoin_config.network);
        let network = daemon_config.bitcoin_config.network;
        (
            Loader {
                network,
                datadir_path,
                daemon_config: daemon_config.clone(),
                gui_config,
                step: Step::Connecting,
                daemon_started: false,
            },
            Command::perform(connect(path, daemon_config), Message::Loaded),
        )
    }

    fn on_load(&mut self, res: Result<Arc<dyn Daemon + Sync + Send>, Error>) -> Command<Message> {
        match res {
            Ok(daemon) => {
                self.step = Step::Syncing {
                    daemon: daemon.clone(),
                    progress: 0.0,
                };
                return Command::perform(sync(daemon, false), Message::Syncing);
            }
            Err(e) => match e {
                Error::Config(_) => {
                    self.step = Step::Error(Box::new(e));
                }
                Error::Daemon(DaemonError::ClientNotSupported)
                | Error::Daemon(DaemonError::Transport(Some(ErrorKind::ConnectionRefused), _))
                | Error::Daemon(DaemonError::Transport(Some(ErrorKind::NotFound), _)) => {
                    self.step = Step::StartingDaemon;
                    self.daemon_started = true;
                    return Command::perform(
                        start_daemon(self.gui_config.daemon_config_path.clone()),
                        Message::Started,
                    );
                }
                _ => {
                    self.step = Step::Error(Box::new(e));
                }
            },
        }
        Command::none()
    }

    fn on_start(&mut self, res: Result<Arc<dyn Daemon + Sync + Send>, Error>) -> Command<Message> {
        match res {
            Ok(daemon) => {
                self.step = Step::Syncing {
                    daemon: daemon.clone(),
                    progress: 0.0,
                };
                Command::perform(sync(daemon, false), Message::Syncing)
            }
            Err(e) => {
                self.step = Step::Error(Box::new(e));
                Command::none()
            }
        }
    }

    fn on_sync(&mut self, res: Result<GetInfoResult, DaemonError>) -> Command<Message> {
        match &mut self.step {
            Step::Syncing {
                daemon, progress, ..
            } => {
                match res {
                    Ok(info) => {
                        if (info.sync - 1.0_f64).abs() < f64::EPSILON {
                            return Command::perform(
                                load_application(
                                    daemon.clone(),
                                    info,
                                    self.gui_config.clone(),
                                    self.datadir_path.clone(),
                                    self.network,
                                ),
                                Message::Synced,
                            );
                        } else {
                            *progress = info.sync
                        }
                    }
                    Err(e) => {
                        self.step = Step::Error(Box::new(e.into()));
                        return Command::none();
                    }
                };
                Command::perform(sync(daemon.clone(), true), Message::Syncing)
            }
            _ => Command::none(),
        }
    }

    pub fn stop(&mut self) {
        info!("Close requested");
        if let Step::Syncing { daemon, .. } = &mut self.step {
            if !daemon.is_external() {
                info!("Stopping internal daemon...");
                if let Some(d) = Arc::get_mut(daemon) {
                    d.stop().expect("Daemon is internal");
                    info!("Internal daemon stopped");
                } else {
                }
            }
        }
    }

    pub fn update(&mut self, message: Message) -> Command<Message> {
        match message {
            Message::View(ViewMessage::Retry) => {
                let (loader, cmd) = Self::new(
                    self.datadir_path.clone(),
                    self.gui_config.clone(),
                    self.daemon_config.clone(),
                );
                *self = loader;
                cmd
            }
            Message::Started(res) => self.on_start(res),
            Message::Loaded(res) => self.on_load(res),
            Message::Syncing(res) => self.on_sync(res),
            Message::Synced(Err(e)) => {
                self.step = Step::Error(Box::new(e));
                Command::none()
            }
            Message::Failure(_) => {
                self.daemon_started = false;
                Command::none()
            }
            _ => Command::none(),
        }
    }

    pub fn subscription(&self) -> Subscription<Message> {
        Subscription::none()
    }

    pub fn view(&self) -> Element<Message> {
        view(&self.step).map(Message::View)
    }
}

pub async fn load_application(
    daemon: Arc<dyn Daemon + Sync + Send>,
    info: GetInfoResult,
    gui_config: GUIConfig,
    datadir_path: PathBuf,
    network: bitcoin::Network,
) -> Result<(Arc<Wallet>, Cache, Arc<dyn Daemon + Sync + Send>), Error> {
    let coins = daemon.list_coins().map(|res| res.coins)?;
    let spend_txs = daemon.list_spend_transactions()?;
    let cache = Cache {
        network: info.network,
        blockheight: info.block_height,
        coins,
        spend_txs,
        ..Default::default()
    };
    let settings_path = settings_path(&datadir_path, network);
    let gui_config_hws = gui_config
        .hardware_wallets
        .as_ref()
        .cloned()
        .unwrap_or_default();

    let mut wallet = match Settings::from_file(&settings_path) {
        Ok(settings) => {
            if let Some(wallet_setting) = settings.wallets.first() {
                Wallet::new(wallet_setting.name.clone(), info.descriptors.main)
                    .with_hardware_wallets(wallet_setting.hardware_wallets.clone())
                    .with_key_aliases(wallet_setting.keys_aliases())
            } else {
                Wallet::legacy(info.descriptors.main).with_hardware_wallets(gui_config_hws)
            }
        }
        Err(settings::SettingsError::NotFound) => {
            Wallet::legacy(info.descriptors.main).with_hardware_wallets(gui_config_hws)
        }
        Err(e) => return Err(e.into()),
    };

    let hot_signers = match HotSigner::from_datadir(&datadir_path, network) {
        Ok(signers) => signers,
        Err(e) => match e {
            liana::signer::SignerError::MnemonicStorage(e) => {
                if e.kind() == std::io::ErrorKind::NotFound {
                    Vec::new()
                } else {
                    return Err(Error::HotSigner(e.to_string()));
                }
            }
            _ => return Err(Error::HotSigner(e.to_string())),
        },
    };

    let curve = bitcoin::secp256k1::Secp256k1::signing_only();
    let keys = wallet.descriptor_keys();
    if let Some(hot_signer) = hot_signers
        .into_iter()
        .find(|s| keys.contains(&s.fingerprint(&curve)))
    {
        wallet = wallet.with_signer(Signer::new(hot_signer));
    }

    Ok((Arc::new(wallet), cache, daemon))
}

#[derive(Clone, Debug)]
pub enum ViewMessage {
    Retry,
    SwitchNetwork,
}

pub fn view(step: &Step) -> Element<ViewMessage> {
    match &step {
        Step::StartingDaemon => cover(
            None,
            Column::new()
                .width(Length::Fill)
                .push(ProgressBar::new(0.0..=1.0, 0.0).width(Length::Fill))
                .push(text("Starting daemon...")),
        ),
        Step::Connecting => cover(
            None,
            Column::new()
                .width(Length::Fill)
                .push(ProgressBar::new(0.0..=1.0, 0.0).width(Length::Fill))
                .push(text("Connecting to daemon...")),
        ),
        Step::Syncing { progress, .. } => cover(
            None,
            Column::new()
                .width(Length::Fill)
                .push(ProgressBar::new(0.0..=1.0, *progress as f32).width(Length::Fill))
                .push(text("Syncing the wallet with the blockchain...")),
        ),
        Step::Error(error) => cover(
            Some(("Error while starting the internal daemon", error)),
            Column::new()
                .spacing(20)
                .width(Length::Fill)
                .align_items(Alignment::Center)
                .push(icon::plug_icon().size(100).width(Length::Units(300)))
                .push(
                    if matches!(
                        error.as_ref(),
                        Error::Daemon(DaemonError::Start(StartupError::Bitcoind(_)))
                    ) {
                        text("Liana failed to start, please check if bitcoind is running")
                    } else {
                        text("Liana failed to start")
                    },
                )
                .push(
                    Row::new()
                        .spacing(10)
                        .push(
                            button::border(None, "Use another Bitcoin network")
                                .on_press(ViewMessage::SwitchNetwork),
                        )
                        .push(
                            button::primary(None, "Retry")
                                .width(Length::Units(200))
                                .on_press(ViewMessage::Retry),
                        ),
                ),
        ),
    }
}

pub fn cover<'a, T: 'a + Clone, C: Into<Element<'a, T>>>(
    warn: Option<(&'static str, &Error)>,
    content: C,
) -> Element<'a, T> {
    Column::new()
        .push_maybe(warn.map(|w| notification::warning(w.0.to_string(), w.1.to_string())))
        .push(
            Container::new(content)
                .width(iced::Length::Fill)
                .height(iced::Length::Fill)
                .center_x()
                .center_y()
                .padding(50),
        )
        .into()
}

async fn connect(
    socket_path: PathBuf,
    _config: Config,
) -> Result<Arc<dyn Daemon + Sync + Send>, Error> {
    let client = client::jsonrpc::JsonRPCClient::new(socket_path);
    let daemon = Lianad::new(client);

    debug!("Searching for external daemon");
    daemon.get_info()?;
    info!("Connected to external daemon");

    Ok(Arc::new(daemon))
}

// Daemon can start only if a config path is given.
pub async fn start_daemon(config_path: PathBuf) -> Result<Arc<dyn Daemon + Sync + Send>, Error> {
    debug!("starting liana daemon");

    let config = Config::from_file(Some(config_path)).map_err(Error::Config)?;

    let mut daemon = EmbeddedDaemon::new(config);
    daemon.start()?;

    Ok(Arc::new(daemon))
}

async fn sync(
    daemon: Arc<dyn Daemon + Sync + Send>,
    sleep: bool,
) -> Result<GetInfoResult, DaemonError> {
    if sleep {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    daemon.get_info()
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Error {
    Settings(settings::SettingsError),
    Config(ConfigError),
    Daemon(DaemonError),
    HotSigner(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::Settings(e) => write!(f, "Settings error: {}", e),
            Self::Config(e) => write!(f, "Config error: {}", e),
            Self::Daemon(e) => write!(f, "Liana daemon error: {}", e),
            Self::HotSigner(e) => write!(f, "Failed to load hot signer: {}", e),
        }
    }
}

impl From<settings::SettingsError> for Error {
    fn from(error: settings::SettingsError) -> Self {
        Error::Settings(error)
    }
}

impl From<ConfigError> for Error {
    fn from(error: ConfigError) -> Self {
        Error::Config(error)
    }
}

impl From<DaemonError> for Error {
    fn from(error: DaemonError) -> Self {
        Error::Daemon(error)
    }
}

/// default lianad socket path is .liana/bitcoin/lianad_rpc
fn socket_path(datadir: &Path, network: bitcoin::Network) -> PathBuf {
    let mut path = datadir.to_path_buf();
    path.push(network.to_string());
    path.push("lianad_rpc");
    path
}

/// default liana settings path is .liana/bitcoin/settings.json
fn settings_path(datadir: &Path, network: bitcoin::Network) -> PathBuf {
    let mut path = datadir.to_path_buf();
    path.push(network.to_string());
    path.push(settings::DEFAULT_FILE_NAME);
    path
}
