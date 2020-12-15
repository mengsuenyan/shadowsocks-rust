//! Shadowsocks Manager server

use std::{collections::HashMap, io, net::SocketAddr, sync::Arc, time::Duration};

use futures::future::{self, AbortHandle};
use log::{error, info};
use shadowsocks::{
    config::{ServerConfig, ServerType},
    context::{Context, SharedContext},
    crypto::v1::CipherKind,
    dns_resolver::DnsResolver,
    manager::protocol::{
        self,
        AddRequest,
        AddResponse,
        ErrorResponse,
        ListResponse,
        ManagerRequest,
        PingResponse,
        RemoveRequest,
        RemoveResponse,
        StatRequest,
    },
    net::ConnectOpts,
    plugin::PluginConfig,
    ManagerListener,
    ServerAddr,
};
use tokio::sync::Mutex;

use crate::{
    config::{ManagerConfig, ManagerServerHost, Mode},
    local::acl::AccessControl,
    net::FlowStat,
    server::Server,
};

struct ServerInstance {
    flow_stat: Arc<FlowStat>,
    abortable: AbortHandle,
    svr_cfg: ServerConfig,
}

impl Drop for ServerInstance {
    fn drop(&mut self) {
        self.abortable.abort();
    }
}

pub struct Manager {
    context: SharedContext,
    servers: Mutex<HashMap<u16, ServerInstance>>,
    svr_cfg: ManagerConfig,
    mode: Mode,
    connect_opts: ConnectOpts,
    udp_expiry_duration: Option<Duration>,
    udp_capacity: Option<usize>,
    nodelay: bool,
    acl: Option<Arc<AccessControl>>,
}

impl Manager {
    pub fn new(svr_cfg: ManagerConfig) -> Manager {
        Manager::with_context(svr_cfg, Context::new_shared(ServerType::Server))
    }

    pub(crate) fn with_context(svr_cfg: ManagerConfig, context: SharedContext) -> Manager {
        Manager {
            context,
            servers: Mutex::new(HashMap::new()),
            svr_cfg,
            mode: Mode::TcpOnly,
            connect_opts: ConnectOpts::default(),
            udp_expiry_duration: None,
            udp_capacity: None,
            nodelay: false,
            acl: None,
        }
    }

    pub fn set_connect_opts(&mut self, opts: ConnectOpts) {
        self.connect_opts = opts;
    }

    pub fn set_udp_expiry_duration(&mut self, d: Duration) {
        self.udp_expiry_duration = Some(d);
    }

    pub fn set_udp_capacity(&mut self, c: usize) {
        self.udp_capacity = Some(c);
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    pub fn config(&self) -> &ManagerConfig {
        &self.svr_cfg
    }

    pub fn set_nodelay(&mut self, nodelay: bool) {
        self.nodelay = nodelay;
    }

    pub fn set_dns_resolver(&mut self, resolver: Arc<DnsResolver>) {
        let context = Arc::get_mut(&mut self.context).expect("cannot set DNS resolver on a shared context");
        context.set_dns_resolver(resolver)
    }

    pub fn set_acl(&mut self, acl: Arc<AccessControl>) {
        self.acl = Some(acl);
    }

    pub async fn run(self) -> io::Result<()> {
        let mut listener = ManagerListener::bind(&self.context, &self.svr_cfg.addr).await?;

        let local_addr = listener.local_addr()?;
        info!("shadowsocks manager server listening on {}", local_addr);

        loop {
            let (req, peer_addr) = match listener.recv_from().await {
                Ok(r) => r,
                Err(err) => {
                    error!("manager recv_from error: {}", err);
                    continue;
                }
            };

            match req {
                ManagerRequest::Add(ref req) => match self.handle_add(req).await {
                    Ok(rsp) => {
                        let _ = listener.send_to(&rsp, &peer_addr).await;
                    }
                    Err(err) => {
                        error!("add server_port: {} failed, error: {}", req.server_port, err);
                        let rsp = ErrorResponse(err);
                        let _ = listener.send_to(&rsp, &peer_addr).await;
                    }
                },
                ManagerRequest::Remove(ref req) => {
                    let rsp = self.handle_remove(req).await;
                    let _ = listener.send_to(&rsp, &peer_addr).await;
                }
                ManagerRequest::List(..) => {
                    let rsp = self.handle_list().await;
                    let _ = listener.send_to(&rsp, &peer_addr).await;
                }
                ManagerRequest::Ping(..) => {
                    let rsp = self.handle_ping().await;
                    let _ = listener.send_to(&rsp, &peer_addr).await;
                }
                ManagerRequest::Stat(ref stat) => self.handle_stat(stat).await,
            }
        }
    }

    pub async fn add_server(&self, svr_cfg: ServerConfig, mode: Option<Mode>) {
        let mut server = Server::with_context(self.context.clone(), svr_cfg.clone());

        server.set_connect_opts(self.connect_opts.clone());

        if let Some(d) = self.udp_expiry_duration {
            server.set_udp_expiry_duration(d);
        }

        if let Some(c) = self.udp_capacity {
            server.set_udp_capacity(c);
        }

        if self.nodelay {
            server.set_nodelay(true);
        }

        server.set_mode(mode.unwrap_or(self.mode));

        if let Some(ref acl) = self.acl {
            server.set_acl(acl.clone());
        }

        let server_port = server.config().addr().port();

        let mut servers = self.servers.lock().await;
        // Close existed server
        if let Some(v) = servers.remove(&server_port) {
            info!(
                "closed managed server listening on {}, inbound address {}",
                v.svr_cfg.addr(),
                v.svr_cfg.external_addr()
            );
        }

        let flow_stat = server.flow_stat().clone();

        let (server_fut, abortable) = future::abortable(async move { server.run().await });
        tokio::spawn(server_fut);

        servers.insert(
            server_port,
            ServerInstance {
                flow_stat,
                abortable,
                svr_cfg,
            },
        );
    }

    async fn handle_add(&self, req: &AddRequest) -> io::Result<AddResponse> {
        let addr = match self.svr_cfg.server_host {
            ManagerServerHost::Domain(ref dname) => ServerAddr::DomainName(dname.clone(), req.server_port),
            ManagerServerHost::Ip(ip) => ServerAddr::SocketAddr(SocketAddr::new(ip, req.server_port)),
        };

        let method = match req.method {
            Some(ref m) => match m.parse::<CipherKind>() {
                Ok(method) => method,
                Err(..) => {
                    error!("unrecognized method \"{}\", req: {:?}", m, req);

                    let err = format!("unrecognized method \"{}\"", m);
                    return Ok(AddResponse(err));
                }
            },
            None => self.svr_cfg.method.unwrap_or(CipherKind::CHACHA20_POLY1305),
        };

        let mut svr_cfg = ServerConfig::new(addr, req.password.clone(), method);

        if let Some(ref plugin) = req.plugin {
            let p = PluginConfig {
                plugin: plugin.clone(),
                plugin_opts: req.plugin_opts.clone(),
                plugin_args: Vec::new(),
            };
            svr_cfg.set_plugin(p);
        }

        let mode = match req.mode {
            None => None,
            Some(ref mode) => match mode.parse::<Mode>() {
                Ok(m) => Some(m),
                Err(..) => {
                    error!("unrecognized mode \"{}\", req: {:?}", mode, req);

                    let err = format!("unrecognized mode \"{}\"", mode);
                    return Ok(AddResponse(err));
                }
            },
        };

        self.add_server(svr_cfg, mode).await;

        Ok(AddResponse("ok".to_owned()))
    }

    async fn handle_remove(&self, req: &RemoveRequest) -> RemoveResponse {
        let mut servers = self.servers.lock().await;
        servers.remove(&req.server_port);
        RemoveResponse("ok".to_owned())
    }

    async fn handle_list(&self) -> ListResponse {
        let instances = self.servers.lock().await;

        let mut servers = Vec::new();

        for (_, server) in instances.iter() {
            let svr_cfg = &server.svr_cfg;

            let sc = protocol::ServerConfig {
                server_port: svr_cfg.addr().port(),
                password: svr_cfg.password().to_owned(),
                method: None,
                no_delay: None,
                plugin: None,
                plugin_opts: None,
                mode: None,
            };
            servers.push(sc);
        }

        ListResponse { servers }
    }

    async fn handle_ping(&self) -> PingResponse {
        let instances = self.servers.lock().await;

        let mut stat = HashMap::new();
        for (port, server) in instances.iter() {
            let flow_stat = &server.flow_stat;
            stat.insert(*port, flow_stat.tx() + flow_stat.rx());
        }

        PingResponse { stat }
    }

    async fn handle_stat(&self, _stat: &StatRequest) {
        // `stat` is not supported, because all servers are running in the same process of the manager
    }
}
