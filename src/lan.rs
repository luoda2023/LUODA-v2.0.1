#[cfg(not(target_os = "ios"))]
use hbb_common::whoami;
use hbb_common::{
    allow_err,
    anyhow::bail,
    config::Config,
    config::{self, RENDEZVOUS_PORT},
    log,
    protobuf::Message as _,
    rendezvous_proto::*,
    tokio::{
        self,
        sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
    },
    ResultType,
};

use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket},
    time::{Duration, Instant},
};

type Message = RendezvousMessage;

#[cfg(not(target_os = "ios"))]
pub(super) fn start_listening() -> ResultType<()> {
    let addr = SocketAddr::from(([0, 0, 0, 0], get_broadcast_port()));
    // 绑定失败必须重试：调用方用的是 `allow_err!`，一旦这里直接返回 Err，
    // 整个进程就永久失去局域网发现能力，且日志里只有一行 warn 很难被发现。
    // 端口被上一个进程短暂占住（Android 上服务重建、Windows 上快速重启）
    // 是最常见的原因，退避重试基本都能恢复。
    let socket = {
        let mut bound = None;
        let mut last_err = None;
        for attempt in 0..10 {
            match std::net::UdpSocket::bind(addr) {
                Ok(s) => {
                    bound = Some(s);
                    break;
                }
                Err(err) => {
                    log::warn!(
                        "lan discovery bind {addr} failed (attempt {}/10): {err}",
                        attempt + 1
                    );
                    last_err = Some(err);
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        }
        match bound {
            Some(s) => s,
            None => return Err(last_err.unwrap().into()),
        }
    };
    socket.set_read_timeout(Some(std::time::Duration::from_millis(1000)))?;
    log::info!("lan discovery listener started on {addr}");
    loop {
        let mut buf = [0; 2048];
        if let Ok((len, addr)) = socket.recv_from(&mut buf) {
            if let Ok(msg_in) = Message::parse_from_bytes(&buf[0..len]) {
                match msg_in.union {
                    Some(rendezvous_message::Union::PeerDiscovery(p)) => {
                        if p.cmd == "ping"
                            && config::option2bool(
                                "enable-lan-discovery",
                                &Config::get_option("enable-lan-discovery"),
                            )
                        {
                            let id = Config::get_id();
                            if p.id == id {
                                continue;
                            }
                            if let Some(self_addr) = get_ipaddr_by_peer(&addr) {
                                let mut msg_out = Message::new();
                                let mut hostname = crate::whoami_hostname();
                                // The default hostname is "localhost" which is a bit confusing
                                if hostname == "localhost" {
                                    hostname = "unknown".to_owned();
                                }
                                let peer = PeerDiscovery {
                                    cmd: "pong".to_owned(),
                                    mac: get_mac(&self_addr),
                                    id,
                                    hostname,
                                    username: crate::platform::get_active_username(),
                                    platform: whoami::platform().to_string(),
                                    ..Default::default()
                                };
                                msg_out.set_peer_discovery(peer);
                                socket.send_to(&msg_out.write_to_bytes()?, addr).ok();
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

/// 单轮发现的默认等待窗口。局域网 pong 一般在几十毫秒内返回，3s 足够覆盖
/// 多网卡 / 广播重传的情况。
pub(crate) const DISCOVER_WINDOW: Duration = Duration::from_secs(3);

/// 单播扫描的每网段主机数上限（/24 全扫 = 254 个）。
///
/// 比 /24 更宽的网段（/16 等）也一律按 /24 收敛：家用 / 办公环境里被控端
/// 基本都和自己落在同一个 /24，扫满 6 万多个地址既慢又会打爆内核邻居表。
const SWEEP_HOST_LIMIT: usize = 254;

/// 单播扫描的限频间隔。周期性发现每轮都扫一遍会给局域网制造**持续**流量
/// （每个网段 254 个包），所以周期性那一侧最多 45s 扫一次。
/// 连接前的补发现不受此限——那次如果扫不到，这次连接就一定失败。
const SWEEP_MIN_INTERVAL: Duration = Duration::from_secs(45);

/// `lan-sweep-subnets` 允许的额外网段数量上限。
///
/// 这是个手工配置项，正常只会填 1~2 个；设上限是为了防止把整份网段表粘进来
/// （或写错成 `0.0.0.0/0`）时，一轮发现里瞬间打出上万条 UDP。
const EXTRA_SWEEP_SUBNET_LIMIT: usize = 8;

/// 与 `direct_access::lan_candidate_score` 保持同一份名单：这些网卡是
/// VPN / 虚拟机 / 隧道，扫它们的网段既找不到被控端也白费流量。
const VIRTUAL_IFACE_MARKERS: [&str; 21] = [
    "virtual",
    "vethernet",
    "vmware",
    "virtualbox",
    "vbox",
    "hyper-v",
    "hyperv",
    "wsl",
    "docker",
    "tailscale",
    "wireguard",
    "vpn",
    "tunnel",
    "loopback",
    "bluetooth",
    "pseudo",
    "tun",
    "tap",
    "ppp",
    "isatap",
    "teredo",
];

/// 构造局域网发现的 ping 报文。
///
/// 移动端必须带上自己的 ID：手机拿不到 MAC 地址，`start_listening` 里靠
/// `p.id == self_id` 排除自己，否则会把自己"发现"一次。
fn build_ping_packet() -> ResultType<Vec<u8>> {
    let mut msg_out = Message::new();
    #[cfg(any(target_os = "android", target_os = "ios"))]
    let id = crate::ui_interface::get_id();
    // `crate::ui_interface::get_id()` will cause error:
    // `get_id()` uses async code with `current_thread`, which is not allowed in this context.
    //
    // No need to get id for desktop platforms.
    // We can use the mac address to identify the device.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    let id = "".to_owned();
    let peer = PeerDiscovery {
        cmd: "ping".to_owned(),
        id,
        ..Default::default()
    };
    msg_out.set_peer_discovery(peer);
    Ok(msg_out.write_to_bytes()?)
}

/// 读取本地配置 `lan-sweep-subnets`：除自己所在网段外，**额外**要单播扫描的网段。
///
/// 场景：两台设备挂在不同路由器后面、不共享二层广播域，但上联设备互相可达
/// （PC 在 `192.168.31.0/24`、手机在 `192.168.1.0/24`）。此时广播发现必然空手
/// 而归（广播不出路由器），而对端网段的**单播**探测仍然能到，配上对端的
/// `lan_peer_hosts` 命中就能建立局域网直连，不必退回中继。
///
/// 留空（默认）→ 不产生任何额外流量，行为与没有这个开关时完全一致。
/// 这里不做任何猜测性扫描：只有用户明确知道对端在哪个网段时才填。
fn extra_sweep_subnets() -> Vec<(Ipv4Addr, u32)> {
    parse_sweep_subnets(&config::LocalConfig::get_option("lan-sweep-subnets"))
}

/// 解析 `lan-sweep-subnets`：`地址/前缀长度` 列表，逗号 / 分号 / 空白分隔。
///
/// 容忍并跳过非法项（只 log 一行 warn），因为这一项可能被手写进配置文件，
/// 一个错字不该让整个局域网发现能力失效。
fn parse_sweep_subnets(raw: &str) -> Vec<(Ipv4Addr, u32)> {
    let mut out = Vec::new();
    for token in raw.split(|c: char| c == ',' || c == ';' || c.is_whitespace()) {
        if out.len() >= EXTRA_SWEEP_SUBNET_LIMIT {
            log::warn!(
                "lan-sweep-subnets: 超出 {EXTRA_SWEEP_SUBNET_LIMIT} 个网段上限，其余忽略"
            );
            break;
        }
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let Some((addr, prefix)) = token.split_once('/') else {
            log::warn!("lan-sweep-subnets: 忽略缺少前缀长度的项 `{token}`");
            continue;
        };
        // 前缀收到 [8, 30]：比 /8 还宽等于扫全网、比 /30 还窄只剩 1~2 个地址，
        // 都不该由这一项来表达（/31、/32 更是没有可扫的主机位）。
        let parsed = addr
            .trim()
            .parse::<Ipv4Addr>()
            .ok()
            .zip(prefix.trim().parse::<u32>().ok())
            .filter(|(_, prefix)| (8..=30).contains(prefix));
        match parsed {
            Some((addr, prefix)) => out.push((addr, prefix)),
            None => log::warn!("lan-sweep-subnets: 忽略无法解析的项 `{token}`"),
        }
    }
    out
}

/// 把 `(网段基址, 前缀长度)` 展开成该网段内待探测的主机地址列表（最多 /24 个）。
fn sweep_targets_in(network_addr: Ipv4Addr, prefix_len: u32) -> Vec<Ipv4Addr> {
    let mask = (!0u32) << (32 - prefix_len);
    let network = u32::from(network_addr) & mask;
    let broadcast = network | !mask;

    let mut targets = Vec::new();
    let mut cur = network.saturating_add(1);
    while cur < broadcast && targets.len() < SWEEP_HOST_LIMIT {
        targets.push(Ipv4Addr::from(cur));
        cur = cur.saturating_add(1);
    }
    targets
}

/// 生成本地各私有网段的「单播扫描计划」：`(本网卡地址, 该网段内待探测的主机)`。
///
/// 配对返回而不是摊平成一个地址列表：`send_to` 必须走绑定在该网卡上的
/// socket，否则多网卡机器会把 A 网段的包从 B 网卡（默认路由）发出去。
///
/// `0.0.0.0` 作为「本网卡地址」是合法取值，含义是交给路由表选出口——
/// 只用于 `extra_sweep_subnets` 里那种**已知在对端网段**的目标。
fn unicast_sweep_plan() -> Vec<(Ipv4Addr, Vec<Ipv4Addr>)> {
    // iOS 上 `default_net::get_interfaces()` 会引发 undefined symbol（见
    // `create_broadcast_sockets` 里的说明），所以整个枚举块被 cfg 掉，
    // 此时 `plan` 不需要 mut。
    #[allow(unused_mut)]
    let mut plan: Vec<(Ipv4Addr, Vec<Ipv4Addr>)> = Vec::new();
    // 已经覆盖的网段（网络号），用于给 `extra_sweep_subnets` 去重：
    // 用户把本机网段也填进去时，不该再扫一遍。
    #[allow(unused_mut)]
    let mut covered: Vec<u32> = Vec::new();

    #[cfg(not(target_os = "ios"))]
    for interface in default_net::get_interfaces() {
        let name = interface.name.to_ascii_lowercase();
        if VIRTUAL_IFACE_MARKERS.iter().any(|marker| name.contains(marker)) {
            continue;
        }
        for ipv4 in &interface.ipv4 {
            let host = ipv4.addr;
            // 只扫私有地址段：公网网段扫一遍既无意义，还可能被当成扫描行为。
            if !host.is_private() {
                continue;
            }
            // 收敛到 /24：比 /24 宽的按 /24 裁剪，比 /24 窄的按实际范围来。
            let prefix = u32::from(ipv4.prefix_len.clamp(24, 30));
            let network = u32::from(host) & ((!0u32) << (32 - prefix));
            let mut targets = sweep_targets_in(host, prefix);
            // 自己那个地址不用探。
            targets.retain(|candidate| *candidate != host);
            if !targets.is_empty() {
                covered.push(network);
                plan.push((host, targets));
            }
        }
    }

    // 额外网段：见 `extra_sweep_subnets`。绑定 `0.0.0.0` 让内核按路由表选出口。
    for (addr, prefix) in extra_sweep_subnets() {
        let prefix = prefix.clamp(8, 30);
        let network = u32::from(addr) & ((!0u32) << (32 - prefix));
        if covered.contains(&network) {
            continue;
        }
        let targets = sweep_targets_in(addr, prefix);
        if !targets.is_empty() {
            covered.push(network);
            plan.push((Ipv4Addr::UNSPECIFIED, targets));
        }
    }

    plan
}

/// 周期性发现那一侧的单播扫描限频。见 `SWEEP_MIN_INTERVAL`。
fn sweep_due() -> bool {
    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST_SWEEP_UNIX: AtomicU64 = AtomicU64::new(0);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let last = LAST_SWEEP_UNIX.load(Ordering::Relaxed);
    if now.saturating_sub(last) < SWEEP_MIN_INTERVAL.as_secs() {
        return false;
    }
    LAST_SWEEP_UNIX.store(now, Ordering::Relaxed);
    true
}

/// 一轮发现：先发广播，可选地再对每个本地私有网段发一批单播探测包，
/// 然后共用一个接收窗口收 pong。
///
/// `overall` 是硬上界：收包的阻塞 socket 跑在独立线程里，调用方不会被拖住。
async fn discover_within(overall: Duration, unicast_sweep: bool) -> ResultType<()> {
    let mut sockets = create_broadcast_sockets();
    if sockets.is_empty() {
        bail!("Found no bindable ipv4 addresses");
    }
    let out = build_ping_packet()?;
    let port = get_broadcast_port();

    let maddr = SocketAddr::from(([255, 255, 255, 255], port));
    for socket in &sockets {
        allow_err!(socket.send_to(&out, maddr));
    }

    // 单播兜底。见 `unicast_sweep_plan` 与 `client::Client::_start` 的说明：
    // 华为 / 荣耀等 ROM 会在后台把**非单播**报文直接从 Wi-Fi 驱动层丢掉，
    // 这类设备永远不回广播，但对单播 100% 响应。扫描发送是同步非阻塞的，
    // 和广播共用同一个接收窗口，所以不额外增加连接等待时间。
    let mut swept = 0usize;
    if unicast_sweep {
        for (local, targets) in unicast_sweep_plan() {
            let Ok(socket) = UdpSocket::bind(SocketAddr::from((local, 0))) else {
                log::warn!("unicast sweep: bind {local} failed, skip this subnet");
                continue;
            };
            for ip in &targets {
                if socket.send_to(&out, SocketAddr::from((*ip, port))).is_ok() {
                    swept += 1;
                }
            }
            sockets.push(socket);
        }
    }

    if swept > 0 {
        log::info!("discover ping sent (broadcast + {swept} unicast datagrams)");
    } else {
        log::info!("discover ping sent");
    }

    let rx = spawn_wait_responses(sockets, overall);
    // 只有跑满完整窗口的那一轮才把没回应的设备标记为离线。连接前补的那一轮
    // 窗口很短（1.5s），如果也去清 online 标记，会把本来在线的设备误标成离线，
    // 反而让下一次直连拿不到候选地址。
    handle_received_peers(rx, overall >= DISCOVER_WINDOW).await?;

    log::info!("discover ping done");
    Ok(())
}

/// 不依赖 `#[tokio::main]` 宏的发现实现，可以在任意 async 上下文里直接
/// `await`（例如 `client::Client::_start` 在连接前补一轮发现）。
///
/// 周期性发现：单播扫描按 `SWEEP_MIN_INTERVAL` 限频，避免持续制造扫描流量。
pub async fn discover_impl_within(overall: Duration) -> ResultType<()> {
    discover_within(overall, sweep_due()).await
}

/// 连接前补一轮发现：**广播 + 单播扫描同时发出**，共用一个接收窗口。
///
/// 与 `discover_impl_within` 分开的原因是调用方的语义不同：这里是"马上要连
/// 某个 ID，必须尽力找到它"，所以单播扫描不限频——上一条 `lan_peer_hosts`
/// 没命中时才走到这里，扫不到就真的连不上了。
///
/// 合并成一轮而不是"广播超时后再扫"是为了不让连接多等一整个窗口：
/// 「广播聋」设备（见 `unicast_sweep_plan`）在广播轮里必然是空手而归的，
/// 串行两轮就是 2 倍延迟。
pub async fn discover_lan_first_within(overall: Duration) -> ResultType<()> {
    discover_within(overall, true).await
}

pub async fn discover_impl() -> ResultType<()> {
    discover_impl_within(DISCOVER_WINDOW).await
}

#[tokio::main(flavor = "current_thread")]
pub async fn discover() -> ResultType<()> {
    discover_impl().await
}

pub fn send_wol(id: String) {
    let interfaces = default_net::get_interfaces();
    for peer in &config::LanPeers::load().peers {
        if peer.id == id {
            for (_, mac) in peer.ip_mac.iter() {
                if let Ok(mac_addr) = mac.parse() {
                    for interface in &interfaces {
                        for ipv4 in &interface.ipv4 {
                            // remove below mask check to avoid unexpected bug
                            // if (u32::from(ipv4.addr) & u32::from(ipv4.netmask)) == (u32::from(peer_ip) & u32::from(ipv4.netmask))
                            log::info!("Send wol to {mac_addr} of {}", ipv4.addr);
                            allow_err!(wol::send_wol(mac_addr, None, Some(IpAddr::V4(ipv4.addr))));
                        }
                    }
                }
            }
            break;
        }
    }
}

#[inline]
fn get_broadcast_port() -> u16 {
    (RENDEZVOUS_PORT + 3) as _
}

fn get_mac(_ip: &IpAddr) -> String {
    #[cfg(not(target_os = "ios"))]
    if let Ok(mac) = get_mac_by_ip(_ip) {
        mac.to_string()
    } else {
        "".to_owned()
    }
    #[cfg(target_os = "ios")]
    "".to_owned()
}

#[cfg(not(target_os = "ios"))]
fn get_mac_by_ip(ip: &IpAddr) -> ResultType<String> {
    for interface in default_net::get_interfaces() {
        match ip {
            IpAddr::V4(local_ipv4) => {
                if interface.ipv4.iter().any(|x| x.addr == *local_ipv4) {
                    if let Some(mac_addr) = interface.mac_addr {
                        return Ok(mac_addr.address());
                    }
                }
            }
            IpAddr::V6(local_ipv6) => {
                if interface.ipv6.iter().any(|x| x.addr == *local_ipv6) {
                    if let Some(mac_addr) = interface.mac_addr {
                        return Ok(mac_addr.address());
                    }
                }
            }
        }
    }
    bail!("No interface found for ip: {:?}", ip);
}

// Mainly from https://github.com/shellrow/default-net/blob/cf7ca24e7e6e8e566ed32346c9cfddab3f47e2d6/src/interface/shared.rs#L4
fn get_ipaddr_by_peer<A: ToSocketAddrs>(peer: A) -> Option<IpAddr> {
    let socket = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(_) => return None,
    };

    match socket.connect(peer) {
        Ok(()) => (),
        Err(_) => return None,
    };

    match socket.local_addr() {
        Ok(addr) => return Some(addr.ip()),
        Err(_) => return None,
    };
}

fn create_broadcast_sockets() -> Vec<UdpSocket> {
    let mut ipv4s = Vec::new();
    // TODO: maybe we should use a better way to get ipv4 addresses.
    // But currently, it's ok to use `[Ipv4Addr::UNSPECIFIED]` for discovery.
    // `default_net::get_interfaces()` causes undefined symbols error when `flutter build` on iOS simulator x86_64
    #[cfg(not(any(target_os = "ios")))]
    for interface in default_net::get_interfaces() {
        for ipv4 in &interface.ipv4 {
            ipv4s.push(ipv4.addr.clone());
        }
    }
    ipv4s.push(Ipv4Addr::UNSPECIFIED); // for robustness
    let mut sockets = Vec::new();
    for v4_addr in ipv4s {
        // removing v4_addr.is_private() check, https://github.com/luoda/luoda/issues/4663
        if let Ok(s) = UdpSocket::bind(SocketAddr::from((v4_addr, 0))) {
            if s.set_broadcast(true).is_ok() {
                sockets.push(s);
            }
        }
    }
    sockets
}

fn wait_response(
    socket: UdpSocket,
    timeout: Option<std::time::Duration>,
    overall: Duration,
    tx: UnboundedSender<config::DiscoveryPeer>,
) -> ResultType<()> {
    let started = Instant::now();
    let mut last_recv_time = Instant::now();

    let local_addr = socket.local_addr();
    let try_get_ip_by_peer = match local_addr.as_ref() {
        Err(..) => true,
        Ok(addr) => addr.ip().is_unspecified(),
    };
    let mut mac: Option<String> = None;

    socket.set_read_timeout(timeout)?;
    loop {
        let mut buf = [0; 2048];
        if let Ok((len, addr)) = socket.recv_from(&mut buf) {
            if let Ok(msg_in) = Message::parse_from_bytes(&buf[0..len]) {
                match msg_in.union {
                    Some(rendezvous_message::Union::PeerDiscovery(p)) => {
                        last_recv_time = Instant::now();
                        if p.cmd == "pong" {
                            let local_mac = if try_get_ip_by_peer {
                                if let Some(self_addr) = get_ipaddr_by_peer(&addr) {
                                    get_mac(&self_addr)
                                } else {
                                    "".to_owned()
                                }
                            } else {
                                match mac.as_ref() {
                                    Some(m) => m.clone(),
                                    None => {
                                        let m = if let Ok(local_addr) = local_addr {
                                            get_mac(&local_addr.ip())
                                        } else {
                                            "".to_owned()
                                        };
                                        mac = Some(m.clone());
                                        m
                                    }
                                }
                            };

                            if local_mac.is_empty() && p.mac.is_empty() || local_mac != p.mac {
                                allow_err!(tx.send(config::DiscoveryPeer {
                                    id: p.id.clone(),
                                    ip_mac: HashMap::from([
                                        (addr.ip().to_string(), p.mac.clone(),)
                                    ]),
                                    username: p.username.clone(),
                                    hostname: p.hostname.clone(),
                                    platform: p.platform.clone(),
                                    online: true,
                                }));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        if started.elapsed() >= overall || last_recv_time.elapsed().as_millis() > 3_000 {
            break;
        }
    }
    Ok(())
}

fn spawn_wait_responses(
    sockets: Vec<UdpSocket>,
    overall: Duration,
) -> UnboundedReceiver<config::DiscoveryPeer> {
    let (tx, rx) = unbounded_channel::<_>();
    for socket in sockets {
        let tx_clone = tx.clone();
        std::thread::spawn(move || {
            allow_err!(wait_response(
                socket,
                Some(std::time::Duration::from_millis(10)),
                overall,
                tx_clone
            ));
        });
    }
    rx
}

async fn handle_received_peers(
    mut rx: UnboundedReceiver<config::DiscoveryPeer>,
    mark_stale_offline: bool,
) -> ResultType<()> {
    let mut peers = config::LanPeers::load().peers;
    if mark_stale_offline {
        peers.iter_mut().for_each(|peer| {
            peer.online = false;
        });
    }

    let mut response_set = HashSet::new();
    let mut last_write_time: Option<Instant> = None;
    loop {
        tokio::select! {
            data = rx.recv() => match data {
                Some(mut peer) => {
                    let in_response_set = !response_set.insert(peer.id.clone());
                    if let Some(pos) = peers.iter().position(|x| x.is_same_peer(&peer) ) {
                        let peer1 = peers.remove(pos);
                        if in_response_set {
                            peer.ip_mac.extend(peer1.ip_mac);
                            peer.online = true;
                        }
                    }
                    peers.insert(0, peer);
                    if last_write_time.map(|t| t.elapsed().as_millis() > 300).unwrap_or(true)  {
                        config::LanPeers::store(&peers);
                        #[cfg(feature = "flutter")]
                        crate::flutter_ffi::main_load_lan_peers();
                        last_write_time = Some(Instant::now());
                    }
                }
                None => {
                    break
                }
            }
        }
    }

    config::LanPeers::store(&peers);
    #[cfg(feature = "flutter")]
    crate::flutter_ffi::main_load_lan_peers();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sweep_subnets_parses_common_forms() {
        let got = parse_sweep_subnets("192.168.1.0/24, 10.0.0.0/16;172.16.5.0/24");
        assert_eq!(
            got,
            vec![
                (Ipv4Addr::new(192, 168, 1, 0), 24),
                (Ipv4Addr::new(10, 0, 0, 0), 16),
                (Ipv4Addr::new(172, 16, 5, 0), 24),
            ]
        );
    }

    #[test]
    fn sweep_subnets_skips_garbage_without_losing_the_rest() {
        // 手写配置项，一个错字不该让整份配置作废。
        let got = parse_sweep_subnets("192.168.1.0/24, oops, 10.0.0.0, 192.168.2.0/33,10.1.0.0/24");
        assert_eq!(
            got,
            vec![
                (Ipv4Addr::new(192, 168, 1, 0), 24),
                (Ipv4Addr::new(10, 1, 0, 0), 24),
            ]
        );
    }

    #[test]
    fn sweep_subnets_empty_by_default_and_capped() {
        assert!(parse_sweep_subnets("").is_empty());
        assert!(parse_sweep_subnets("   ").is_empty());
        // 上限：多余的直接丢弃，不会把一轮发现变成扫描器。
        let many = (0..20)
            .map(|i| format!("10.{i}.0.0/24"))
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(parse_sweep_subnets(&many).len(), EXTRA_SWEEP_SUBNET_LIMIT);
    }

    #[test]
    fn sweep_targets_are_bounded_and_exclude_network_and_broadcast() {
        let targets = sweep_targets_in(Ipv4Addr::new(192, 168, 1, 99), 24);
        assert_eq!(targets.len(), 254);
        assert_eq!(targets.first().unwrap(), &Ipv4Addr::new(192, 168, 1, 1));
        assert_eq!(targets.last().unwrap(), &Ipv4Addr::new(192, 168, 1, 254));
        // 比 /24 宽也按 /24 收敛。
        assert_eq!(sweep_targets_in(Ipv4Addr::new(10, 0, 0, 7), 16).len(), 254);
        // /30 只有 2 个可用主机位。
        assert_eq!(
            sweep_targets_in(Ipv4Addr::new(10, 0, 0, 4), 30),
            vec![Ipv4Addr::new(10, 0, 0, 5), Ipv4Addr::new(10, 0, 0, 6)]
        );
    }

    /// `lan-sweep-subnets` 是显式开关：无论配置成什么，展开结果都必须有界且合法，
    /// 未配置时为空（本机/CI 都不会配置这一项）。
    #[test]
    fn extra_sweep_subnets_is_bounded() {
        let got = extra_sweep_subnets();
        assert!(got.len() <= EXTRA_SWEEP_SUBNET_LIMIT);
        assert!(got.iter().all(|(_, prefix)| (8..=30).contains(prefix)));
        assert!(parse_sweep_subnets("").is_empty());
    }
}
