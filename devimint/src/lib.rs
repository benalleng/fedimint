use std::collections::HashMap;
use std::env;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bitcoincore_rpc::RpcApi;
use federation::{run_dkg, Federation};
use fedimint_client::module::gen::{ClientModuleGenRegistry, DynClientModuleGen};
use fedimint_client_legacy::modules::mint::MintClientGen;
use fedimint_client_legacy::{module_decode_stubs, UserClient, UserClientConfig};
use fedimint_core::config::load_from_file;
use fedimint_core::db::Database;
use fedimint_ln_client::LightningClientGen;
use fedimint_logging::LOG_DEVIMINT;
use fedimint_wallet_client::WalletClientGen;
use tokio::fs;
use tracing::info;

pub mod util;
pub mod vars;
use util::*;
use vars::utf8;

mod external;
pub use external::{
    external_daemons, open_channel, Bitcoind, Electrs, Esplora, ExternalDaemons, LightningNode,
    Lightningd, Lnd,
};

pub mod federation;

pub struct DevFed {
    pub bitcoind: Bitcoind,
    pub cln: Lightningd,
    pub lnd: Lnd,
    pub fed: Federation,
    pub gw_cln: Gatewayd,
    pub gw_lnd: Gatewayd,
    pub electrs: Electrs,
    pub esplora: Esplora,
    pub faucet: Faucet,
}

#[derive(Clone)]
pub struct Gatewayd {
    _process: ProcessHandle,
    ln: Option<LightningNode>,
}

impl Gatewayd {
    pub async fn new(process_mgr: &ProcessManager, ln: LightningNode) -> Result<Self> {
        let ln_name = ln.name();
        let test_dir = &process_mgr.globals.FM_TEST_DIR;
        let gateway_env: HashMap<String, String> = match ln {
            LightningNode::Cln(_) => HashMap::from_iter([
                (
                    "FM_GATEWAY_DATA_DIR".to_owned(),
                    format!("{}/gw-cln", utf8(test_dir)),
                ),
                (
                    "FM_GATEWAY_LISTEN_ADDR".to_owned(),
                    "127.0.0.1:8175".to_owned(),
                ),
                (
                    "FM_GATEWAY_API_ADDR".to_owned(),
                    "http://127.0.0.1:8175".to_owned(),
                ),
            ]),
            LightningNode::Lnd(_) => HashMap::from_iter([
                (
                    "FM_GATEWAY_DATA_DIR".to_owned(),
                    format!("{}/gw-lnd", utf8(test_dir)),
                ),
                (
                    "FM_GATEWAY_LISTEN_ADDR".to_owned(),
                    "127.0.0.1:28175".to_owned(),
                ),
                (
                    "FM_GATEWAY_API_ADDR".to_owned(),
                    "http://127.0.0.1:28175".to_owned(),
                ),
            ]),
        };
        let process = process_mgr
            .spawn_daemon(
                &format!("gatewayd-{ln_name}"),
                cmd!("gatewayd", ln_name).envs(gateway_env),
            )
            .await?;

        Ok(Self {
            ln: Some(ln),
            _process: process,
        })
    }

    pub fn lightning_name(&self) -> String {
        if let Some(ln) = &self.ln {
            return ln.name().to_string();
        }

        "None".to_string()
    }

    pub fn set_lightning_node(&mut self, ln_node: LightningNode) {
        self.ln = Some(ln_node);
    }

    pub async fn stop_lightning_node(&mut self) -> Result<()> {
        match self.ln.take() {
            Some(LightningNode::Lnd(lnd)) => lnd.kill().await,
            Some(LightningNode::Cln(cln)) => cln.kill().await,
            None => Err(anyhow::anyhow!(
                "Cannot stop an already stopped Lightning Node"
            )),
        }
    }

    pub async fn cmd(&self) -> Command {
        match &self.ln {
            Some(LightningNode::Cln(_)) => {
                cmd!("gateway-cli", "--rpcpassword=theresnosecondbest")
            }
            Some(LightningNode::Lnd(_)) => {
                cmd!(
                    "gateway-cli",
                    "--rpcpassword=theresnosecondbest",
                    "-a",
                    "http://127.0.0.1:28175"
                )
            }
            None => {
                panic!("Cannot execute command when gateway is disconnected from Lightning Node");
            }
        }
    }

    pub async fn connect_fed(&self, fed: &Federation) -> Result<()> {
        let connect_str = poll_value("connect info", || async {
            match cmd!(fed, "connect-info").out_json().await {
                Ok(info) => Ok(Some(
                    info["connect_info"]
                        .as_str()
                        .context("connect_info must be string")?
                        .to_owned(),
                )),
                Err(_) => Ok(None),
            }
        })
        .await?;
        poll("gateway connect-fed", || async {
            Ok(cmd!(self, "connect-fed", connect_str.clone())
                .run()
                .await
                .is_ok())
        })
        .await?;
        Ok(())
    }
}

#[derive(Clone)]
pub struct Faucet {
    _process: ProcessHandle,
}

impl Faucet {
    pub async fn new(process_mgr: &ProcessManager) -> Result<Self> {
        let connect_string =
            fs::read_to_string(process_mgr.globals.FM_DATA_DIR.join("client-connect")).await?;

        Ok(Self {
            _process: process_mgr
                .spawn_daemon(
                    "faucet",
                    cmd!("faucet", "--connect-string={connect_string}"),
                )
                .await?,
        })
    }
}

pub async fn dev_fed(process_mgr: &ProcessManager) -> Result<DevFed> {
    let start_time = fedimint_core::time::now();
    let bitcoind = Bitcoind::new(process_mgr).await?;
    let ((cln, lnd, gw_cln, gw_lnd), electrs, esplora, (fed, faucet)) = tokio::try_join!(
        async {
            let (cln, lnd) = tokio::try_join!(
                Lightningd::new(process_mgr, bitcoind.clone()),
                Lnd::new(process_mgr, bitcoind.clone())
            )?;
            info!(LOG_DEVIMINT, "lightning started");
            let (gw_cln, gw_lnd, _) = tokio::try_join!(
                Gatewayd::new(process_mgr, LightningNode::Cln(cln.clone())),
                Gatewayd::new(process_mgr, LightningNode::Lnd(lnd.clone())),
                open_channel(&bitcoind, &cln, &lnd),
            )?;
            info!(LOG_DEVIMINT, "gateways started");
            Ok((cln, lnd, gw_cln, gw_lnd))
        },
        Electrs::new(process_mgr, bitcoind.clone()),
        Esplora::new(process_mgr, bitcoind.clone()),
        async {
            let fed_size = process_mgr.globals.FM_FED_SIZE;
            run_dkg(process_mgr, fed_size).await?;
            info!(LOG_DEVIMINT, "dkg done");
            tokio::try_join!(
                Federation::new(process_mgr, bitcoind.clone(), 0..fed_size),
                Faucet::new(process_mgr)
            )
        },
    )?;
    info!(LOG_DEVIMINT, "federation and gateways started");
    tokio::try_join!(gw_cln.connect_fed(&fed), gw_lnd.connect_fed(&fed))?;
    fed.await_gateways_registered().await?;
    info!(LOG_DEVIMINT, "gateways registered");
    fed.use_gateway(&gw_cln).await?;
    info!(
        LOG_DEVIMINT,
        "starting dev federation took {:?}",
        start_time.elapsed()?
    );
    Ok(DevFed {
        bitcoind,
        cln,
        lnd,
        faucet,
        fed,
        gw_cln,
        gw_lnd,
        electrs,
        esplora,
    })
}
