use std::{net::IpAddr, path::Path};

use cata::{output::Format, Command, Container};
use clap::Parser;
use eyre::Result;
use kube::{api::Api, runtime::events::Reporter};
use pkcs8::{Document, PrivateKeyInfo};
use russh::{server::Config, MethodSet};
use russh_keys::key::KeyPair;
use ssh_key::PrivateKey;
use warp::Filter;

use crate::{
    health,
    openid::{self, Fetch},
    resources,
    ssh::{self, ControllerBuilder},
};

static CLIENT_ID: &str = "P3g7SKU42Wi4Z86FnNDqfiRtQRYgWsqx";
static OID_CONFIG_URL: &str = "https://kuberift.us.auth0.com/.well-known/openid-configuration";

static CONTROLLER_NAME: &str = "ssh.kuberift.com";

#[derive(Parser, Container)]
pub struct Serve {
    #[clap(from_global)]
    output: Format,

    // TODO(thomas): fetch these from the CRD
    #[clap(long, default_value = "1hr")]
    inactivity_timeout: humantime::Duration,
    /// Client ID for the OpenID provider that will be used.
    #[clap(long, default_value = CLIENT_ID, env = "KUBERIFT_CLIENT_ID")]
    client_id: String,
    /// URL to the OpenID configuration. This is how the server knows what
    /// endpoints to use and how to validate tokens.
    #[clap(long, default_value = OID_CONFIG_URL, env = "KUBERIFT_OID_CONFIG_URL")]
    openid_configuration: String,
    /// Claim of the `id_token` to use as the user's ID.
    #[clap(long, default_value = "email")]
    claim: String,

    #[clap(long, default_value = "127.0.0.1")]
    address: String,

    #[clap(long, default_value = "2222")]
    ssh_port: u16,
    #[clap(long, default_value = "8080")]
    health_port: u16,

    /// Path to a private Key to use. Must be in PEM format, but can either be
    /// openssl or openssh. A key is generated by default if unset. Just like
    /// any other SSH server, it is important to maintain the key between
    /// invocations so that your users have the same host key.
    #[clap(long, value_parser = load_key, default_value = "")]
    key: KeyPair,

    #[clap(long)]
    no_create: bool,
}

impl Serve {
    async fn serve_http(&self) -> Result<()> {
        let metrics = warp::path("metrics").and_then(health::metrics);

        warp::serve(metrics)
            .run((self.address.parse::<IpAddr>()?, self.health_port))
            .await;

        Ok(())
    }

    async fn serve_ssh(&self) -> Result<()> {
        let cfg = kube::Config::infer().await?;

        let reporter = Reporter {
            controller: CONTROLLER_NAME.into(),
            instance: Some(hostname::get()?.to_string_lossy().into()),
        };

        let ctrl = ControllerBuilder::default()
            .config(cfg)
            .reporter(Some(reporter.clone()))
            .build()?;

        if !self.no_create {
            resources::create(&Api::all(ctrl.client()?), true).await?;
        }

        let server_cfg = Config {
            inactivity_timeout: Some(self.inactivity_timeout.into()),
            methods: MethodSet::PUBLICKEY | MethodSet::KEYBOARD_INTERACTIVE,
            // TODO(thomas): how important is this? It has a negative impact on
            // UX because public key will be first, causing users to wait for
            // the first time. Maybe there's something to do with submethods?
            auth_rejection_time: std::time::Duration::from_secs(0),
            auth_rejection_time_initial: Some(std::time::Duration::from_secs(0)),
            keys: vec![self.key.clone()],
            ..Default::default()
        };

        let cfg = openid::Config::fetch(&self.openid_configuration).await?;
        let jwks = cfg.jwks().await?;

        ssh::UIServer::new(
            ctrl,
            openid::ProviderBuilder::default()
                .claim(self.claim.clone())
                .client_id(self.client_id.clone())
                .config(cfg)
                .jwks(jwks)
                .build()?,
        )
        .run(server_cfg, (self.address.clone(), self.ssh_port))
        .await
    }
}

#[async_trait::async_trait]
impl Command for Serve {
    #[allow(clippy::blocks_in_conditions)]
    #[tracing::instrument(err, skip(self), fields(activity = "serve"))]
    async fn run(&self) -> Result<()> {
        tokio::select! {
            result = self.serve_http() => result,
            result = self.serve_ssh() => result,
        }
    }
}

fn load_key(val: &str) -> Result<KeyPair> {
    if val.is_empty() {
        return Ok(KeyPair::generate_ed25519().expect("key was generated"));
    }

    let pth = Path::new(val);

    if !pth.exists() {
        return Err(eyre::eyre!("Key file does not exist: {}", val));
    }

    if let Ok(key) = PrivateKey::read_openssh_file(pth) {
        return Ok(KeyPair::try_from(&key)?);
    }

    Ok(KeyPair::try_from(PrivateKeyInfo::try_from(
        Document::read_pem_file(pth)?.1.as_bytes(),
    )?)?)
}
