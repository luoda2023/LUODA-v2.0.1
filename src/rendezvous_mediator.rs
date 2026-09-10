use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, RwLock,
    },
    time::{Duration, Instant},
};

use uuid::Uuid;

use hbb_common::{
    allow_err,
    anyhow::{self, bail},
    config::{
        self, is_loopback_or_test_server, keys::*, option2bool, use_ws, Config, CONNECT_TIMEOUT,
        DEFAULT_DIRECT_PORT, REG_INTERVAL, RENDEZVOUS_PORT,
    },
    futures::future::join_all,
    log,
    protobuf::Message as _,
    rendezvous_proto::*,
    sleep,
    socket_client::{self, connect_tcp, is_ipv4, new_direct_udp_for, new_udp_for},
    tokio::{self, select, sync::Mutex, time::interval},
    udp::FramedSocket,
    AddrMangle, IntoTargetAddr, ResultType, Stream, TargetAddr,
};

use crate::{
    check_port,
    server::{check_zombie, new as new_server, ServerPtr},
};

type Message = RendezvousMessage;

lazy_static::lazy_static! {
    static ref SOLVING_PK_MISMATCH: Mutex<String> = Default::default();
    static ref LAST_MSG: Mutex<(SocketAddr, Instant)> = Mutex::new((SocketAddr::new([0; 4].into(), 0), Instant::now()));
    static ref LAST_RELAY_MSG: Mutex<(SocketAddr, Instant)> = Mutex::new((SocketAddr::new([0; 4].into(), 0), Instant::now()));
}
static SHOULD_EXIT: AtomicBool = AtomicBool::new(false);
static MANUAL_RESTARTED: AtomicBool = AtomicBool::new(false);
static SENT_REGISTER_PK: AtomicBool = AtomicBool::new(false);
const DIRECT_ACCESS_STATUS_OPTION: &str = "direct-access-status";

fn set_direct_access_status(status: &str) {
    if Config::get_option(DIRECT_ACCESS_STATUS_OPTION) != status {
        Config::set_option(DIRECT_ACCESS_STATUS_OPTION.to_owned(), status.to_owned());
        if status != "listening" {
            Config::set_option("direct-access-public-port".to_owned(), String::new());
            Config::set_option("upnp-status".to_owned(), status.to_owned());
        }
        crate::runtime_logger::info("DIRECT_SERVER", &format!("status={status}"));
    }
}

#[derive(Clone)]
pub struct RendezvousMediator {
    addr: TargetAddr<'static>,
    host: String,
    host_prefix: String,
    keep_alive: i32,
}

impl RendezvousMediator {
    pub fn restart() {
        SHOULD_EXIT.store(true, Ordering::SeqCst);
        MANUAL_RESTARTED.store(true, Ordering::SeqCst);
        log::info!("server restart");
    }

    pub async fn start_all() {
        crate::test_nat_type();
        if config::is_outgoing_only() {
            loop {
                if SHOULD_EXIT.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                sleep(1.).await;
            }
        }
        crate::hbbs_http::sync::start();
        #[cfg(target_os = "windows")]
        // Auto-update disabled for LUODA custom build
        if false && crate::platform::is_installed() && crate::is_server() {
            crate::updater::start_auto_update();
        }
        check_zombie();
        let server = new_server();
        if config::option2bool("stop-service", &Config::get_option("stop-service")) {
            crate::test_rendezvous_server();
        }
        let server_cloned = server.clone();
        tokio::spawn(async move {
            direct_server(server_cloned).await;
        });
        #[cfg(target_os = "android")]
        let start_lan_listening = true;
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        // LUODA: 便携 / 免安装运行时同样必须启动 LAN 发现监听。
        //
        // 上游用 `is_installed()`（注册表 InstallLocation 指向的 exe 是否存在）
        // 作闸门，结果是「绿色版 / 免安装运行」永远不绑 21119。可用 `netstat`
        // 直接验证：进程只有 21118 / 50116，没有 21119。
        //
        // 后果是**同网段设备发来的 ping 无人应答**，对方拿不到本机局域网地址，
        // 只能回落信令链路；而手机端经 WebSocket 注册后端口被写死为 0，TCP
        // 打洞在协议层就不可用。于是「手机连 PC」的三条直连路径
        // （显式 IP / LAN 缓存 / 连接前补发现）全部落空，必然失败 ——
        // 这是华为 / OPPO 连不上 PC 的根因。
        //
        // LAN 发现是纯功能能力，与安装形态无关。多实例争抢同一端口由
        // `start_listening` 内的绑定重试 + `allow_err!` 兜底，不会崩。
        let start_lan_listening = true;
        if start_lan_listening {
            std::thread::spawn(move || {
                allow_err!(super::lan::start_listening());
            });
        }
        // It is ok to run xdesktop manager when the headless function is not allowed.
        #[cfg(target_os = "linux")]
        if crate::is_server() {
            crate::platform::linux_desktop_manager::start_xdesktop();
        }
        scrap::codec::test_av1();
        loop {
            let timeout = Arc::new(RwLock::new(CONNECT_TIMEOUT));
            let conn_start_time = Instant::now();
            *SOLVING_PK_MISMATCH.lock().await = "".to_owned();
            if !config::option2bool("stop-service", &Config::get_option("stop-service"))
                && !crate::platform::installing_service()
            {
                let mut futs = Vec::new();
                let servers = Config::get_rendezvous_servers();
                SHOULD_EXIT.store(false, Ordering::SeqCst);
                MANUAL_RESTARTED.store(false, Ordering::SeqCst);
                for host in servers.clone() {
                    let server = server.clone();
                    let timeout = timeout.clone();
                    futs.push(tokio::spawn(async move {
                        if let Err(err) = Self::start(server, host).await {
                            let err = format!("rendezvous mediator error: {err}");
                            // When user reboot, there might be below error, waiting too long
                            // (CONNECT_TIMEOUT 18s) will make user think there is bug
                            if err.contains("10054") || err.contains("11001") {
                                // No such host is known. (os error 11001)
                                // An existing connection was forcibly closed by the remote host. (os error 10054): also happens for UDP
                                *timeout.write().unwrap() = 3000;
                            }
                            log::error!("{err}");
                        }
                        // SHOULD_EXIT here is to ensure once one exits, the others also exit.
                        SHOULD_EXIT.store(true, Ordering::SeqCst);
                    }));
                }
                join_all(futs).await;
            } else {
                server.write().unwrap().close_connections();
            }
            Config::reset_online();
            let timeout = *timeout.read().unwrap();
            if !MANUAL_RESTARTED.load(Ordering::SeqCst) {
                let elapsed = conn_start_time.elapsed().as_millis() as u64;
                if elapsed < timeout {
                    sleep(((timeout - elapsed) / 1000) as _).await;
                }
            } else {
                // https://github.com/luoda/luoda/issues/12233
                sleep(0.033).await;
            }
        }
    }

    fn get_host_prefix(host: &str) -> String {
        host.split(".")
            .next()
            .map(|x| {
                if x.parse::<i32>().is_ok() {
                    host.to_owned()
                } else {
                    x.to_owned()
                }
            })
            .unwrap_or(host.to_owned())
    }

    pub async fn start_udp(server: ServerPtr, host: String) -> ResultType<()> {
        let host = check_port(&host, RENDEZVOUS_PORT);
        log::info!("start udp: {host}");
        let (mut socket, mut addr) = new_udp_for(&host, CONNECT_TIMEOUT).await?;
        let mut rz = Self {
            addr: addr.clone(),
            host: host.clone(),
            host_prefix: Self::get_host_prefix(&host),
            keep_alive: crate::DEFAULT_KEEP_ALIVE,
        };

        let mut timer = crate::luoda_interval(interval(crate::TIMER_OUT));
        const MIN_REG_TIMEOUT: i64 = 3_000;
        const MAX_REG_TIMEOUT: i64 = 30_000;
        let mut reg_timeout = MIN_REG_TIMEOUT;
        const MAX_FAILS1: i64 = 2;
        const MAX_FAILS2: i64 = 4;
        const DNS_INTERVAL: i64 = 60_000;
        // During recovery (no successful RegisterPeerResponse yet, or we just came
        // back from a failure streak) re-send registration faster than the default
        // 15s REG_INTERVAL. This shortens the window in which the peer is absent
        // from hbbs's online table and a caller gets "ID does not exist".
        const RECOVERY_REG_INTERVAL: i64 = 5_000;
        let mut fails = 0;
        let mut last_register_resp: Option<Instant> = None;
        let mut last_register_sent: Option<Instant> = None;
        let mut last_dns_check = Instant::now();
        let mut old_latency = 0;
        let mut ema_latency = 0;
        loop {
            let mut update_latency = || {
                last_register_resp = Some(Instant::now());
                fails = 0;
                reg_timeout = MIN_REG_TIMEOUT;
                let mut latency = last_register_sent
                    .map(|x| x.elapsed().as_micros() as i64)
                    .unwrap_or(0);
                last_register_sent = None;
                if latency < 0 || latency > 1_000_000 {
                    return;
                }
                if ema_latency == 0 {
                    ema_latency = latency;
                } else {
                    ema_latency = latency / 30 + (ema_latency * 29 / 30);
                    latency = ema_latency;
                }
                let mut n = latency / 5;
                if n < 3000 {
                    n = 3000;
                }
                if (latency - old_latency).abs() > n || old_latency <= 0 {
                    Config::update_latency(&host, latency);
                    log::debug!("Latency of {}: {}ms", host, latency as f64 / 1000.);
                    old_latency = latency;
                }
            };
            select! {
                n = socket.next() => {
                    match n {
                        Some(Ok((bytes, _))) => {
                            if let Ok(msg) = Message::parse_from_bytes(&bytes) {
                                rz.handle_resp(msg.union, Sink::Framed(&mut socket, &addr), &server, &mut update_latency).await?;
                            } else {
                                log::debug!("Non-protobuf message bytes received: {:?}", bytes);
                            }
                        },
                        Some(Err(e)) => bail!("Failed to receive next: {}", e),  // maybe socks5 tcp disconnected
                        None => {
                            bail!("Socket receive none. Maybe socks5 server is down.");
                        },
                    }
                },
                _ = timer.tick() => {
                    if SHOULD_EXIT.load(Ordering::SeqCst) {
                        break;
                    }
                    let now = Some(Instant::now());
                    // Use a shorter re-registration interval while the peer is not
                    // yet confirmed online on hbbs (no successful response so far)
                    // or right after failures. Once healthy, fall back to REG_INTERVAL.
                    let cur_interval = if fails > 0 || last_register_resp.is_none() {
                        RECOVERY_REG_INTERVAL
                    } else {
                        REG_INTERVAL
                    };
                    let expired = last_register_resp.map(|x| x.elapsed().as_millis() as i64 >= cur_interval).unwrap_or(true);
                    let timeout = last_register_sent.map(|x| x.elapsed().as_millis() as i64 >= reg_timeout).unwrap_or(false);
                    // temporarily disable exponential backoff for android before we add wakeup trigger to force connect in android
                    #[cfg(not(any(target_os = "android", target_os = "ios")))]
                    if crate::using_public_server() { // only turn on this for public server, may help DDNS self-hosting user.
                        if timeout && reg_timeout < MAX_REG_TIMEOUT {
                            reg_timeout += MIN_REG_TIMEOUT;
                        }
                    }
                    if timeout || (last_register_sent.is_none() && expired) {
                        if timeout {
                            fails += 1;
                            if crate::rendezvous_transport::should_fallback_to_tcp(fails) {
                                Config::update_latency(&host, -1);
                                bail!(
                                    "UDP rendezvous registration timed out after {} attempts",
                                    fails
                                );
                            }
                            if fails >= MAX_FAILS2 {
                                Config::update_latency(&host, -1);
                                old_latency = 0;
                                if last_dns_check.elapsed().as_millis() as i64 > DNS_INTERVAL {
                                    // in some case of network reconnect (dial IP network),
                                    // old UDP socket not work any more after network recover
                                    if let Some((s, new_addr)) = socket_client::rebind_udp_for(&rz.host).await? {
                                        socket = s;
                                        rz.addr = new_addr.clone();
                                        addr = new_addr;
                                    }
                                    last_dns_check = Instant::now();
                                }
                            } else if fails >= MAX_FAILS1 {
                                Config::update_latency(&host, 0);
                                old_latency = 0;
                            }
                        }
                        rz.register_peer(Sink::Framed(&mut socket, &addr)).await?;
                        last_register_sent = now;
                    }
                }
            }
        }
        Ok(())
    }

    #[inline]
    async fn handle_resp(
        &mut self,
        msg: Option<rendezvous_message::Union>,
        sink: Sink<'_>,
        server: &ServerPtr,
        update_latency: &mut impl FnMut(),
    ) -> ResultType<()> {
        match msg {
            Some(rendezvous_message::Union::RegisterPeerResponse(rpr)) => {
                update_latency();
                if rpr.request_pk {
                    log::info!("request_pk received from {}", self.host);
                    self.register_pk(sink).await?;
                } else {
                    crate::runtime_logger::info(
                        "NETWORK",
                        &format!(
                            "rendezvous peer registration acknowledged; server={}",
                            self.host
                        ),
                    );
                }
            }
            Some(rendezvous_message::Union::RegisterPkResponse(rpr)) => {
                update_latency();
                match rpr.result.enum_value() {
                    Ok(register_pk_response::Result::OK) => {
                        Config::set_key_confirmed(true);
                        Config::set_host_key_confirmed(&self.host_prefix, true);
                        *SOLVING_PK_MISMATCH.lock().await = "".to_owned();
                        // RegisterPk confirms identity but does not keep the peer
                        // online. Do not wait for the next 15-second refresh.
                        self.register_peer(sink).await?;
                        crate::runtime_logger::info(
                            "NETWORK",
                            &format!(
                                "identity confirmed; peer registration sent; server={}",
                                self.host
                            ),
                        );
                    }
                    Ok(register_pk_response::Result::UUID_MISMATCH) => {
                        self.handle_uuid_mismatch(sink).await?;
                    }
                    _ => {
                        log::error!("unknown RegisterPkResponse");
                    }
                }
                if rpr.keep_alive > 0 {
                    self.keep_alive = rpr.keep_alive * 1000;
                    log::info!("keep_alive: {}ms", self.keep_alive);
                }
            }
            Some(rendezvous_message::Union::PunchHole(ph)) => {
                let rz = self.clone();
                let server = server.clone();
                tokio::spawn(async move {
                    allow_err!(rz.handle_punch_hole(ph, server).await);
                });
            }
            Some(rendezvous_message::Union::RequestRelay(rr)) => {
                let rz = self.clone();
                let server = server.clone();
                tokio::spawn(async move {
                    allow_err!(rz.handle_request_relay(rr, server).await);
                });
            }
            Some(rendezvous_message::Union::FetchLocalAddr(fla)) => {
                let rz = self.clone();
                let server = server.clone();
                tokio::spawn(async move {
                    allow_err!(rz.handle_intranet(fla, server).await);
                });
            }
            Some(rendezvous_message::Union::ConfigureUpdate(cu)) => {
                let v0 = Config::get_rendezvous_servers();
                Config::set_option(
                    "rendezvous-servers".to_owned(),
                    cu.rendezvous_servers.join(","),
                );
                Config::set_serial(cu.serial);
                if v0 != Config::get_rendezvous_servers() {
                    Self::restart();
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub async fn start_tcp(server: ServerPtr, host: String) -> ResultType<()> {
        let host = check_port(&host, RENDEZVOUS_PORT);
        log::info!("start tcp: {}", hbb_common::websocket::check_ws(&host));
        let mut conn = match connect_tcp(host.clone(), CONNECT_TIMEOUT).await {
            Ok(c) => c,
            Err(err) => {
                log::warn!("connect_tcp failed for {host} ({err}), falling back to raw TCP");
                crate::runtime_logger::warn(
                    "NETWORK",
                    &format!("TCP rendezvous connect failed; server={host}; error={err}; fallback=raw-TCP"),
                );
                hbb_common::socket_client::connect_tcp_local(host.clone(), None, CONNECT_TIMEOUT)
                    .await?
            }
        };
        let key = crate::get_key(true).await;
        crate::secure_tcp(&mut conn, &key).await?;
        let mut rz = Self {
            addr: conn.local_addr().into_target_addr()?,
            host: host.clone(),
            host_prefix: Self::get_host_prefix(&host),
            keep_alive: crate::DEFAULT_KEEP_ALIVE,
        };
        let mut timer = crate::luoda_interval(interval(crate::TIMER_OUT));
        let mut last_recv_msg = Instant::now();
        Config::set_host_key_confirmed(&rz.host_prefix, false);
        let mut last_register_sent: Option<Instant> = None;
        loop {
            let mut update_latency = || {
                let latency = last_register_sent
                    .map(|x| x.elapsed().as_micros() as i64)
                    .unwrap_or(0);
                Config::update_latency(&host, latency);
                log::debug!("Latency of {}: {}ms", host, latency as f64 / 1000.);
            };
            select! {
                res = conn.next() => {
                    last_recv_msg = Instant::now();
                    let bytes = res.ok_or_else(|| anyhow::anyhow!("Rendezvous connection is reset by the peer"))??;
                    if bytes.is_empty() {
                        // After fixing frequent register_pk, for websocket, nginx need to set proxy_read_timeout to more than 60 seconds, eg: 120s
                        // https://serverfault.com/questions/1060525/why-is-my-websocket-connection-gets-closed-in-60-seconds
                        conn.send_bytes(bytes::Bytes::new()).await?;
                        continue; // heartbeat
                    }
                    let msg = Message::parse_from_bytes(&bytes)?;
                    rz.handle_resp(msg.union, Sink::Stream(&mut conn), &server, &mut update_latency).await?
                }
                _ = timer.tick() => {
                    if SHOULD_EXIT.load(Ordering::SeqCst) {
                        break;
                    }
                    // https://www.emqx.com/en/blog/mqtt-keep-alive
                    if last_recv_msg.elapsed().as_millis() as u64 > rz.keep_alive as u64 * 3 / 2 {
                        bail!("Rendezvous connection is timeout");
                    }
                    if last_register_sent
                        .map(|x| x.elapsed().as_millis() as i64)
                        .unwrap_or(REG_INTERVAL)
                        >= REG_INTERVAL
                    {
                        // register_peer() sends RegisterPk while the key is not
                        // confirmed yet, then RegisterPeer afterwards, so the
                        // peer stays in the server's punch table. Sending only
                        // RegisterPk here left mobile (WebSocket/TCP) peers
                        // unregistered: punches could not be routed to them.
                        rz.register_peer(Sink::Stream(&mut conn)).await?;
                        last_register_sent = Some(Instant::now());
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn start(server: ServerPtr, host: String) -> ResultType<()> {
        log::info!("start rendezvous mediator of {}", host);
        // Windows Server detection is no longer needed here - hbbs TCP does not
        // support RegisterPeer, so all platforms (including Windows Server and
        // regular Windows VPS) must use UDP for rendezvous registration.
        //If the investment agent type is http or https, then tcp forwarding is enabled.
        if crate::rendezvous_transport::should_start_with_tcp(
            cfg!(debug_assertions) && option_env!("TEST_TCP").is_some(),
            Config::is_proxy(),
            use_ws(),
            crate::is_udp_disabled(),
            false,
        ) {
            match Self::start_tcp(server.clone(), host.clone()).await {
                Ok(()) => Ok(()),
                Err(err) if !SHOULD_EXIT.load(Ordering::SeqCst) => {
                    log::warn!("TCP rendezvous failed for {host}, falling back to UDP: {err}");
                    crate::runtime_logger::warn(
                        "NETWORK",
                        &format!("TCP rendezvous failed; server={host}; fallback=UDP; error={err}"),
                    );
                    Self::start_udp(server, host).await
                }
                Err(err) => Err(err),
            }
        } else {
            match Self::start_udp(server.clone(), host.clone()).await {
                Ok(()) => Ok(()),
                Err(err) if !SHOULD_EXIT.load(Ordering::SeqCst) => {
                    log::warn!("UDP rendezvous failed for {host}, falling back to TCP: {err}");
                    crate::runtime_logger::warn(
                        "NETWORK",
                        &format!("UDP rendezvous failed; server={host}; fallback=TCP; error={err}"),
                    );
                    Self::start_tcp(server, host).await
                }
                Err(err) => Err(err),
            }
        }
    }

    async fn handle_request_relay(&self, rr: RequestRelay, server: ServerPtr) -> ResultType<()> {
        let addr = AddrMangle::decode(&rr.socket_addr);
        let last = *LAST_RELAY_MSG.lock().await;
        *LAST_RELAY_MSG.lock().await = (addr, Instant::now());
        // skip duplicate relay request messages
        if last.0 == addr && last.1.elapsed().as_millis() < 100 {
            return Ok(());
        }

        self.create_relay(
            rr.socket_addr.into(),
            rr.relay_server,
            rr.uuid,
            server,
            rr.secure,
            false,
            Default::default(),
            rr.control_permissions.clone().into_option(),
        )
        .await
    }

    async fn create_relay(
        &self,
        socket_addr: Vec<u8>,
        relay_server: String,
        uuid: String,
        server: ServerPtr,
        secure: bool,
        initiate: bool,
        socket_addr_v6: bytes::Bytes,
        control_permissions: Option<ControlPermissions>,
    ) -> ResultType<()> {
        let peer_addr = AddrMangle::decode(&socket_addr);
        log::info!(
            "create_relay requested from {:?}, relay_server: {}, uuid: {}, secure: {}",
            peer_addr,
            relay_server,
            uuid,
            secure,
        );

        let mut socket = match connect_tcp(&*self.host, CONNECT_TIMEOUT).await {
            Ok(socket) => socket,
            Err(err) => {
                log::warn!(
                    "WebSocket rendezvous relay failed for {}, falling back to raw TCP: {}",
                    self.host,
                    err
                );
                socket_client::connect_tcp_local(&*self.host, None, CONNECT_TIMEOUT).await?
            }
        };

        let mut msg_out = Message::new();
        let mut rr = RelayResponse {
            socket_addr: socket_addr.into(),
            version: crate::VERSION.to_owned(),
            socket_addr_v6,
            ..Default::default()
        };
        if initiate {
            rr.uuid = uuid.clone();
            rr.relay_server = relay_server.clone();
            rr.set_id(Config::get_id());
        }
        msg_out.set_relay_response(rr);
        socket.send(&msg_out).await?;
        crate::create_relay_connection(
            server,
            relay_server,
            uuid,
            peer_addr,
            secure,
            is_ipv4(&self.addr),
            control_permissions,
        )
        .await;
        Ok(())
    }

    async fn handle_intranet(&self, fla: FetchLocalAddr, server: ServerPtr) -> ResultType<()> {
        let addr = AddrMangle::decode(&fla.socket_addr);
        let last = *LAST_MSG.lock().await;
        *LAST_MSG.lock().await = (addr, Instant::now());
        // skip duplicate punch hole messages
        if last.0 == addr && last.1.elapsed().as_millis() < 100 {
            return Ok(());
        }
        let peer_addr_v6 = hbb_common::AddrMangle::decode(&fla.socket_addr_v6);
        let relay_server = self.get_relay_server(fla.relay_server.clone());
        let relay = Config::is_proxy();
        let mut socket_addr_v6 = Default::default();
        if peer_addr_v6.port() > 0 && !relay {
            socket_addr_v6 = start_ipv6(
                peer_addr_v6,
                addr,
                server.clone(),
                fla.control_permissions.clone().into_option(),
            )
            .await;
        }
        if is_ipv4(&self.addr) && !relay {
            // When the peer reports a private (LAN) IPv4 address, attempt a
            // direct intranet connection even if local TCP listening is disabled.
            // `disable-tcp-listen` only turns off the inbound listener on this
            // side; it should not prevent us from dialing out to a peer we can
            // already see on the same LAN. This makes "connect by ID inside one
            // LAN" actually use the fast LAN path instead of falling back to relay.
            let peer_is_lan_ipv4 = match addr {
                std::net::SocketAddr::V4(v4) => v4.ip().is_private(),
                _ => false,
            };
            let allow_intranet = peer_is_lan_ipv4 || !config::is_disable_tcp_listen();
            if allow_intranet {
                if let Err(err) = self
                    .handle_intranet_(
                        fla.clone(),
                        server.clone(),
                        relay_server.clone(),
                        socket_addr_v6.clone(),
                    )
                    .await
                {
                    log::debug!("Failed to handle intranet: {:?}, will try relay", err);
                } else {
                    return Ok(());
                }
            }
        }
        let uuid = Uuid::new_v4().to_string();
        self.create_relay(
            fla.socket_addr.into(),
            relay_server,
            uuid,
            server,
            true,
            true,
            socket_addr_v6,
            fla.control_permissions.into_option(),
        )
        .await
    }

    async fn handle_intranet_(
        &self,
        fla: FetchLocalAddr,
        server: ServerPtr,
        relay_server: String,
        socket_addr_v6: bytes::Bytes,
    ) -> ResultType<()> {
        let peer_addr = AddrMangle::decode(&fla.socket_addr);
        log::debug!("Handle intranet from {:?}", peer_addr);
        let mut socket = connect_tcp(&*self.host, CONNECT_TIMEOUT).await?;
        let local_addr = socket.local_addr();
        // we saw invalid local_addr while using proxy, local_addr.ip() == "::1"
        let local_addr: SocketAddr =
            format!("{}:{}", local_addr.ip(), local_addr.port()).parse()?;
        let mut msg_out = Message::new();
        msg_out.set_local_addr(LocalAddr {
            id: Config::get_id(),
            socket_addr: AddrMangle::encode(peer_addr).into(),
            local_addr: AddrMangle::encode(local_addr).into(),
            relay_server,
            version: crate::VERSION.to_owned(),
            socket_addr_v6,
            ..Default::default()
        });
        let bytes = msg_out.write_to_bytes()?;
        socket.send_raw(bytes).await?;
        crate::accept_connection(
            server.clone(),
            socket,
            peer_addr,
            true,
            fla.control_permissions.into_option(),
        )
        .await;
        Ok(())
    }

    async fn handle_punch_hole(&self, ph: PunchHole, server: ServerPtr) -> ResultType<()> {
        let mut peer_addr = AddrMangle::decode(&ph.socket_addr);
        let last = *LAST_MSG.lock().await;
        *LAST_MSG.lock().await = (peer_addr, Instant::now());
        // skip duplicate punch hole messages
        if last.0 == peer_addr && last.1.elapsed().as_millis() < 100 {
            return Ok(());
        }
        let peer_addr_v6 = hbb_common::AddrMangle::decode(&ph.socket_addr_v6);
        let relay = Config::is_proxy() || ph.force_relay;
        let mut socket_addr_v6 = Default::default();
        let control_permissions = ph.control_permissions.into_option();
        if peer_addr_v6.port() > 0 && !relay {
            socket_addr_v6 = start_ipv6(
                peer_addr_v6,
                peer_addr,
                server.clone(),
                control_permissions.clone(),
            )
            .await;
        }
        let relay_server = self.get_relay_server(ph.relay_server);
        // If the caller's reported address is a private (LAN) IPv4, never force a
        // relay – we can reach it directly on the same LAN.  `disable-tcp-listen`
        // only turns off our own inbound listener; it must not prevent us from
        // dialing out to a peer on the same LAN.  Without this override, an ID
        // connection between two machines on one LAN silently fell through to
        // the relay when either side had TCP listening off and the UDP
        // hole-punch port was unavailable.
        let peer_is_lan_ipv4 = match peer_addr {
            std::net::SocketAddr::V4(v4) => v4.ip().is_private(),
            _ => false,
        };
        // for ensure, websocket go relay directly
        if (ph.nat_type.enum_value() == Ok(NatType::SYMMETRIC)
            || Config::get_nat_type() == NatType::SYMMETRIC as i32
            || relay
            || (config::is_disable_tcp_listen() && ph.udp_port <= 0))
            && !peer_is_lan_ipv4
        {
            let uuid = Uuid::new_v4().to_string();
            return self
                .create_relay(
                    ph.socket_addr.into(),
                    relay_server,
                    uuid,
                    server,
                    true,
                    true,
                    socket_addr_v6.clone(),
                    control_permissions,
                )
                .await;
        }
        use hbb_common::protobuf::Enum;
        let nat_type = NatType::from_i32(Config::get_nat_type()).unwrap_or(NatType::UNKNOWN_NAT);
        // Report our direct_server port so the caller can try a direct TCP
        // connection to it.  Essential for VPS with public IPs (public↔public
        // direct connect) where UDP punch hole may fail and TCP
        // simultaneous-open is unreliable: the caller can connect straight to
        // our listening direct-server port instead of falling back to relay.
        let direct_port = if Config::get_option(DIRECT_ACCESS_STATUS_OPTION) == "listening" {
            get_direct_port()
        } else {
            0
        };
        let msg_punch = PunchHoleSent {
            socket_addr: ph.socket_addr,
            id: Config::get_id(),
            relay_server,
            nat_type: nat_type.into(),
            version: crate::VERSION.to_owned(),
            socket_addr_v6,
            upnp_port: direct_port,
            ..Default::default()
        };
        if ph.udp_port > 0 {
            peer_addr.set_port(ph.udp_port as u16);
            self.punch_udp_hole(peer_addr, server, msg_punch, control_permissions)
                .await?;
            return Ok(());
        }
        log::debug!("Punch tcp hole to {:?}", peer_addr);
        let mut socket = {
            let socket = connect_tcp(&*self.host, CONNECT_TIMEOUT).await?;
            let local_addr = socket.local_addr();
            // key important here for punch hole to tell my gateway incoming peer is safe.
            // it can not be async here, because local_addr can not be reused, we must close the connection before use it again.
            allow_err!(socket_client::connect_tcp_local(peer_addr, Some(local_addr), 30).await);
            socket
        };
        let mut msg_out = Message::new();
        msg_out.set_punch_hole_sent(msg_punch);
        let bytes = msg_out.write_to_bytes()?;
        socket.send_raw(bytes).await?;
        crate::accept_connection(server.clone(), socket, peer_addr, true, control_permissions)
            .await;
        Ok(())
    }

    async fn punch_udp_hole(
        &self,
        peer_addr: SocketAddr,
        server: ServerPtr,
        msg_punch: PunchHoleSent,
        control_permissions: Option<ControlPermissions>,
    ) -> ResultType<()> {
        let mut msg_out = Message::new();
        msg_out.set_punch_hole_sent(msg_punch);
        let (socket, addr) = new_direct_udp_for(&self.host).await?;
        let data = msg_out.write_to_bytes()?;
        socket.send_to(&data, addr).await?;
        let socket_cloned = socket.clone();
        tokio::spawn(async move {
            for _ in 0..2 {
                let tm = (hbb_common::time_based_rand() % 20 + 10) as f32 / 1000.;
                hbb_common::sleep(tm).await;
                socket.send_to(&data, addr).await.ok();
            }
        });
        udp_nat_listen(
            socket_cloned.clone(),
            peer_addr,
            peer_addr,
            server,
            control_permissions,
        )
        .await?;
        Ok(())
    }

    async fn register_pk(&mut self, socket: Sink<'_>) -> ResultType<()> {
        let mut msg_out = Message::new();
        let pk = Config::get_key_pair().1;
        let uuid = hbb_common::get_uuid();
        let id = Config::get_id();
        msg_out.set_register_pk(RegisterPk {
            id,
            uuid: uuid.into(),
            pk: pk.into(),
            no_register_device: Config::no_register_device(),
            ..Default::default()
        });
        socket.send(&msg_out).await?;
        SENT_REGISTER_PK.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn handle_uuid_mismatch(&mut self, socket: Sink<'_>) -> ResultType<()> {
        {
            let mut solving = SOLVING_PK_MISMATCH.lock().await;
            if solving.is_empty() || *solving == self.host {
                log::info!("UUID_MISMATCH received from {}", self.host);
                Config::set_key_confirmed(false);
                Config::update_id();
                *solving = self.host.clone();
            } else {
                return Ok(());
            }
        }
        self.register_pk(socket).await
    }

    async fn register_peer(&mut self, socket: Sink<'_>) -> ResultType<()> {
        let solving = SOLVING_PK_MISMATCH.lock().await;
        if !(solving.is_empty() || *solving == self.host) {
            return Ok(());
        }
        drop(solving);
        let key_confirmed = Config::get_key_confirmed();
        let host_key_confirmed = Config::get_host_key_confirmed(&self.host_prefix);
        if !key_confirmed || !host_key_confirmed {
            // Only the RegisterPk handshake is sent here - the ID is NOT yet
            // registered as an online peer on hbbs. This is the most common reason
            // for "ID does not exist" on the caller side: the controlled peer keeps
            // sending handshakes but never reaches the RegisterPeer step (e.g. UDP
            // packet loss, key mismatch, or the hbbs response never arrives).
            log::info!(
                "register_pk of {} due to key not confirmed (key_confirmed={}, host_key_confirmed={})",
                self.host_prefix, key_confirmed, host_key_confirmed
            );
            crate::runtime_logger::warn(
                "NETWORK",
                &format!(
                    "id not registered yet; sending RegisterPk handshake only; server={}; key_confirmed={}; host_key_confirmed={}",
                    self.host, key_confirmed, host_key_confirmed
                ),
            );
            return self.register_pk(socket).await;
        }
        let id = Config::get_id();
        log::trace!(
            "Register my id {:?} to rendezvous server {:?}",
            id,
            self.addr,
        );
        let mut msg_out = Message::new();
        let serial = Config::get_serial();
        msg_out.set_register_peer(RegisterPeer {
            id,
            serial,
            ..Default::default()
        });
        socket.send(&msg_out).await?;
        Ok(())
    }

    /// TCP-specific registration: sends RegisterPeer directly without the
    /// RegisterPk handshake.  The hbbs TCP listener returns NOT_SUPPORT for
    /// RegisterPk, so we bypass the key-confirmation check entirely.
    async fn register_peer_tcp(&mut self, socket: Sink<'_>) -> ResultType<()> {
        let id = Config::get_id();
        log::info!(
            "Register (TCP) my id {:?} to rendezvous server {:?}",
            id,
            self.addr,
        );
        let mut msg_out = Message::new();
        let serial = Config::get_serial();
        msg_out.set_register_peer(RegisterPeer {
            id,
            serial,
            ..Default::default()
        });
        socket.send(&msg_out).await?;
        Ok(())
    }

    fn get_relay_server(&self, provided_by_rendezvous_server: String) -> String {
        let mut relay_server = Config::get_option("relay-server");
        if is_loopback_or_test_server(&relay_server) {
            log::warn!(
                "Ignoring invalid configured relay server '{}' and using rendezvous fallback",
                relay_server
            );
            relay_server.clear();
        }
        if relay_server.is_empty() {
            relay_server = provided_by_rendezvous_server;
        }
        if relay_server.is_empty() {
            relay_server = crate::increase_port(&self.host, 1);
        }
        relay_server
    }
}

static DIRECT_PORT: std::sync::OnceLock<std::sync::Mutex<i32>> = std::sync::OnceLock::new();

fn get_direct_port() -> i32 {
    let mtx = DIRECT_PORT.get_or_init(|| {
        // Keep the documented port stable so entering a bare IP uses the
        // same port. A small deterministic fallback range handles conflicts.
        std::sync::Mutex::new(DEFAULT_DIRECT_PORT)
    });
    *mtx.lock().unwrap()
}

/// Mark the current port as failed (e.g. port already in use),
/// incrementing through the deterministic bare-IP fallback range.
fn invalidate_direct_port() {
    if let Some(mtx) = DIRECT_PORT.get() {
        let mut port = mtx.lock().unwrap();
        let failed_port = *port;
        if *port < DEFAULT_DIRECT_PORT + crate::direct_access::DIRECT_PORT_RANGE as i32 {
            *port += 1;
        } else {
            *port = DEFAULT_DIRECT_PORT;
        }
        log::info!("Direct port {} was unavailable, trying {}", failed_port, *port);
    }
}

/// Reset the direct port back to DEFAULT_DIRECT_PORT (21118).
/// Called when the listener exits (e.g. service stopped) so the next
/// restart will try the canonical port first instead of being stuck
/// on a fallback port forever.
fn reset_direct_port() {
    if let Some(mtx) = DIRECT_PORT.get() {
        let mut port = mtx.lock().unwrap();
        if *port != DEFAULT_DIRECT_PORT {
            log::info!("Resetting direct port from {} back to {}", *port, DEFAULT_DIRECT_PORT);
            *port = DEFAULT_DIRECT_PORT;
        }
    }
}

pub fn ensure_direct_port() -> i32 {
    get_direct_port()
}

async fn direct_server(server: ServerPtr) {
    let mut listener = None;
    let mut port = 0;
    set_direct_access_status("starting");
    loop {
        let disabled = !option2bool(
            OPTION_DIRECT_SERVER,
            &Config::get_option(OPTION_DIRECT_SERVER),
        ) || option2bool("stop-service", &Config::get_option("stop-service"));
        if disabled && listener.is_none() {
            set_direct_access_status("stopped");
        }
        if !disabled && listener.is_none() {
            port = get_direct_port();
            match hbb_common::tcp::listen_any(port as _).await {
                Ok(l) => {
                    listener = Some(l);
                    set_direct_access_status("listening");
                    log::info!(
                        "Direct server listening on: {:?}",
                        listener.as_ref().map(|l| l.local_addr())
                    );
                    // Sync the actual port to UI option so the IP:port display stays correct
                    // even when the default port was unavailable and we fell back.
                    Config::set_option("direct-access-port".to_owned(), port.to_string());
                    #[cfg(not(any(target_os = "android", target_os = "ios")))]
                    {
                        let upnp_port = port as u16;
                        Config::set_option("upnp-status".to_owned(), "pending".to_owned());
                        Config::set_option("upnp-error".to_owned(), "".to_owned());
                        Config::set_option("direct-access-public-port".to_owned(), "".to_owned());
std::thread::spawn(move || {
 let mut mapped_port = None;
 let mut last_error = String::new();
 for attempt in 1..=3 {
 // Early-exit if the direct listener changed ports or stopped,
 // so we don't waste time on stale UPnP mappings.
 if Config::get_option("direct-access-port") != upnp_port.to_string()
 || Config::get_option(DIRECT_ACCESS_STATUS_OPTION) != "listening"
 {
 return;
 }
 match crate::upnp::add_port_mapping(upnp_port) {
 Ok(port) => {
 mapped_port = Some(port);
 break;
 }
 Err(error) => last_error = error,
 }
 if attempt < 3 {
 std::thread::sleep(Duration::from_secs(2 * attempt));
 }
 }

                            // Ignore a late result if the direct listener changed ports.
                            if Config::get_option("direct-access-port") != upnp_port.to_string()
                                || Config::get_option(DIRECT_ACCESS_STATUS_OPTION) != "listening"
                            {
                                return;
                            }
                            if let Some(external_port) = mapped_port {
                                Config::set_option(
                                    "direct-access-public-port".to_owned(),
                                    external_port.to_string(),
                                );
                                Config::set_option("upnp-error".to_owned(), "".to_owned());
                                Config::set_option("upnp-status".to_owned(), "ok".to_owned());
                            } else {
                                Config::set_option("upnp-error".to_owned(), last_error);
                                Config::set_option("upnp-status".to_owned(), "fail".to_owned());
                            }
                        });
                    }
                    #[cfg(any(target_os = "android", target_os = "ios"))]
                    {
                        Config::set_option("upnp-status".to_owned(), "unsupported".to_owned());
                    }
                }
                Err(err) => {
                    set_direct_access_status("error");
                    log::error!(
                        "Failed to start direct server on port: {}, error: {}",
                        port,
                        err
                    );
                    invalidate_direct_port();
                    sleep(1.).await;
                }
            }
        }
        if let Some(l) = listener.as_mut() {
            if disabled || port != get_direct_port() {
                log::info!("Exit direct access listen");
                listener = None;
                set_direct_access_status("stopped");
                // 重置端口到 21118，下次启动可重新使用默认端口
                reset_direct_port();
                continue;
            }
            if let Ok(Ok((tcp_stream, addr))) = hbb_common::timeout(1000, l.accept()).await {
                tcp_stream.set_nodelay(true).ok();
                log::info!("direct access from {}", addr);
                let local_addr = tcp_stream
                    .local_addr()
                    .unwrap_or(Config::get_any_listen_addr(true));
                let server = server.clone();
                tokio::spawn(async move {
                    allow_err!(
                        crate::server::create_tcp_connection(
                            server,
                            hbb_common::Stream::from(tcp_stream, local_addr),
                            addr,
                            false,
                            None,
                        )
                        .await
                    );
                });
            } else {
                sleep(0.1).await;
            }
        } else {
            sleep(1.).await;
        }
    }
}

enum Sink<'a> {
    Framed(&'a mut FramedSocket, &'a TargetAddr<'a>),
    Stream(&'a mut Stream),
}

impl Sink<'_> {
    async fn send(self, msg: &Message) -> ResultType<()> {
        match self {
            Sink::Framed(socket, addr) => socket.send(msg, addr.to_owned()).await,
            Sink::Stream(stream) => stream.send(msg).await,
        }
    }
}

async fn start_ipv6(
    peer_addr_v6: SocketAddr,
    peer_addr_v4: SocketAddr,
    server: ServerPtr,
    control_permissions: Option<ControlPermissions>,
) -> bytes::Bytes {
    crate::test_ipv6().await;
    if let Some((socket, local_addr_v6)) = crate::get_ipv6_socket().await {
        let server = server.clone();
        tokio::spawn(async move {
            allow_err!(
                udp_nat_listen(
                    socket.clone(),
                    peer_addr_v6,
                    peer_addr_v4,
                    server,
                    control_permissions
                )
                .await
            );
        });
        return local_addr_v6;
    }
    Default::default()
}

async fn udp_nat_listen(
    socket: Arc<tokio::net::UdpSocket>,
    peer_addr: SocketAddr,
    peer_addr_v4: SocketAddr,
    server: ServerPtr,
    control_permissions: Option<ControlPermissions>,
) -> ResultType<()> {
    let tm = Instant::now();
    let socket_cloned = socket.clone();
    let func = async {
        socket.connect(peer_addr).await?;
        let res = crate::punch_udp(socket.clone(), true).await?;
        let stream = crate::kcp_stream::KcpStream::accept(
            socket,
            Duration::from_millis(CONNECT_TIMEOUT as _),
            res,
        )
        .await?;
        crate::server::create_tcp_connection(
            server,
            stream.1,
            peer_addr_v4,
            true,
            control_permissions,
        )
        .await?;
        Ok(())
    };
    func.await.map_err(|e: anyhow::Error| {
        anyhow::anyhow!(
            "Stop listening on {:?} for remote {peer_addr} with KCP, {:?} elapsed: {e}",
            socket_cloned.local_addr(),
            tm.elapsed()
        )
    })?;
    Ok(())
}

// When config is not yet synced from root, register_pk may have already been sent with a new generated pk.
// After config sync completes, the pk may change. This struct detects pk changes and triggers
// a re-registration by setting key_confirmed to false.
// NOTE:
// This only corrects PK registration for the current ID. If root uses a non-default mac-generated ID,
// this does not resolve the multi-ID issue by itself.
pub struct CheckIfResendPk {
    pk: Option<Vec<u8>>,
}
impl CheckIfResendPk {
    pub fn new() -> Self {
        Self {
            pk: Config::get_cached_pk(),
        }
    }
}
impl Drop for CheckIfResendPk {
    fn drop(&mut self) {
        if SENT_REGISTER_PK.load(Ordering::SeqCst) && Config::get_cached_pk() != self.pk {
            Config::set_key_confirmed(false);
            log::info!("Set key_confirmed to false due to pk changed, will resend register_pk");
        }
    }
}
