use std::net::{IpAddr, Ipv4Addr, SocketAddr};

pub(crate) const DIRECT_PORT_RANGE: u16 = 10;

#[derive(Clone, Debug)]
pub(crate) struct LanAddressCandidate {
    pub address: Ipv4Addr,
    pub name: String,
    pub is_default: bool,
    pub has_gateway: bool,
    pub is_physical: bool,
}

pub(crate) fn choose_lan_ipv4(candidates: &[LanAddressCandidate]) -> Option<Ipv4Addr> {
    candidates
        .iter()
        .filter(|candidate| candidate.address.is_private())
        .max_by_key(|candidate| lan_candidate_score(candidate))
        .map(|candidate| candidate.address)
}

fn lan_candidate_score(candidate: &LanAddressCandidate) -> i32 {
    let name = candidate.name.to_ascii_lowercase();
    let is_virtual = [
        "virtual",
        "vethernet",
        "vmware",
        "virtualbox",
        "vbox",
        "hyper-v",
        "wsl",
        "docker",
        "tailscale",
        "wireguard",
        "vpn",
        "tunnel",
        "loopback",
        "bluetooth",
        "bt",
        "pseudo",
        "tun",
        "tap",
        "ppp",
        "isatap",
        "teredo",
        "6to4",
    ]
    .iter()
    .any(|marker| name.contains(marker));

    let mut score = 0;

    // 虚拟网卡罚分（即使加了其它分，虚拟网卡也很难胜出）
    if is_virtual {
        score -= 500;
    }

    // 物理网卡给予小幅奖励
    if candidate.is_physical {
        score += 50;
    }

    // 有网关说明该网卡确实能连局域网
    if candidate.has_gateway {
        score += 50;
    }

    // UPnP must use the gateway-facing adapter, except when the default route
    // is a VPN or another virtual tunnel.
    if candidate.is_default && !is_virtual {
        score += 10_000;
    }

    // Prefer common home-LAN ranges when no physical default route is known.
    let oct = candidate.address.octets();
    if oct[0] == 192 && oct[1] == 168 {
        score += 5000; // 家用网络 — 极高优先级
    } else if oct[0] == 10 {
        score += 200; // 企业网络
    } else if oct[0] == 172 && (16..=31).contains(&oct[1]) {
        score += 100; // 其他私有地址
    }
    score
}

pub(crate) fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let first = address.octets()[0];
    !address.is_private()
        && !address.is_loopback()
        && !address.is_link_local()
        && !address.is_multicast()
        && !address.is_broadcast()
        && !address.is_unspecified()
        && first != 0
        && first < 240
}

/// 把一个 IP 展开成直连候选端口（`default_port` 起连续 `DIRECT_PORT_RANGE + 1` 个）。
/// 被控端 hbbs 的 21118 可能被占用而回退到 21119/21120…，所以必须整段并发探测。
fn hosts_for_ip(ip: IpAddr, default_port: u16) -> Vec<String> {
    (default_port..=default_port.saturating_add(DIRECT_PORT_RANGE))
        .map(|port| SocketAddr::new(ip, port).to_string())
        .collect()
}

pub(crate) fn direct_peer_hosts(peer: &str, default_port: u16) -> Vec<String> {
    let peer = peer.trim();
    if let Ok(addr) = peer.parse::<SocketAddr>() {
        return vec![addr.to_string()];
    }

    if let Ok(ip) = peer.parse::<IpAddr>() {
        return hosts_for_ip(ip, default_port);
    }

    Vec::new()
}

/// LUODA: 把「已发现的局域网设备 ID」解析成直连候选地址。
///
/// 背景（见 2.0.1-track 华为机型连不上的根因）：
/// 手机端（Android/iOS）`use_ws()` 默认走 `wss://<server>/ws`(443) 注册到 hbbs，
/// nginx 转发后服务端只能拿到 `X-Real-IP`，端口被写死成 0
/// （LUODA-SERVER-API rendezvous_server.rs 的 ws 分支）。
/// 于是 hbbs 记录的 peer 地址是 `公网IP:0`：
///   1. `handle_punch_hole_request` 判定不出 `peer_is_lan` → 拿不到同网段快路径，
///      还会因为 `peer_is_lan ^ is_lan` 把 nat_type 改成 SYMMETRIC → 被强制走中继；
///   2. `handle_hole_sent` 把 `公网IP:0` 回给主控端 → 主控端 `peer_addr.port() == 0`
///      直接 bail。
/// 也就是说：**经 WebSocket 注册过的设备，TCP 打洞在协议层就不可用**。
///
/// 唯一 100% 不依赖服务器的 P2P 路径就是局域网直连：主控端本来就会周期性地向
/// 255.255.255.255:21119 广播探测包，局域网内的被控端会带自己的 ID 回 pong，
/// 我们把这份结果（`config::LanPeers`）按 ID 命中后展开成候选地址即可。
/// 全程不经信令服务器、不经中继，符合「尽量 P2P / 局域网直连」的原则。
///
/// 只采信 `online == true` 的记录：上一轮探测没回应的条目留着只会白等一次
/// `CONNECT_TIMEOUT`，拖慢后续的正常链路。
pub(crate) fn lan_peer_hosts(peer: &str, default_port: u16) -> Vec<String> {
    let peer = peer.trim();
    if peer.is_empty() {
        return Vec::new();
    }

    let mut ips: Vec<IpAddr> = Vec::new();
    for lan_peer in hbb_common::config::LanPeers::load().peers {
        if !lan_peer.online || lan_peer.id != peer {
            continue;
        }
        for ip in lan_peer.ip_mac.keys() {
            let Ok(addr) = ip.parse::<IpAddr>() else {
                continue;
            };
            if addr.is_loopback() || addr.is_unspecified() {
                continue;
            }
            if !ips.contains(&addr) {
                ips.push(addr);
            }
        }
    }

    let mut hosts = Vec::new();
    for ip in ips {
        for host in hosts_for_ip(ip, default_port) {
            if !hosts.contains(&host) {
                hosts.push(host);
            }
        }
    }
    hosts
}

#[cfg(test)]
mod tests {
    use super::{choose_lan_ipv4, direct_peer_hosts, is_public_ipv4, LanAddressCandidate};
    use std::net::Ipv4Addr;

    fn candidate(
        address: [u8; 4],
        name: &str,
        is_default: bool,
        has_gateway: bool,
        is_physical: bool,
    ) -> LanAddressCandidate {
        LanAddressCandidate {
            address: Ipv4Addr::from(address),
            name: name.to_owned(),
            is_default,
            has_gateway,
            is_physical,
        }
    }

    #[test]
    fn physical_lan_adapter_beats_virtual_and_vpn_adapters() {
        let candidates = [
            candidate([10, 8, 0, 2], "WireGuard Tunnel", true, true, false),
            candidate([192, 168, 1, 22], "Ethernet", false, true, true),
            candidate([172, 20, 0, 1], "vEthernet (WSL)", false, false, true),
        ];

        assert_eq!(
            choose_lan_ipv4(&candidates),
            Some(Ipv4Addr::new(192, 168, 1, 22))
        );
    }

    #[test]
    fn default_physical_adapter_wins_between_real_adapters() {
        // 旧逻辑：192.168 缺 default 标记输给 10.x（因 default 分值够高）；
        // 新逻辑：192.168 段 +5000 分完全碾压一切，即使缺 default 标记也能胜出
        let candidates = [
            candidate([192, 168, 2, 10], "Ethernet 2", false, true, true),
            candidate([10, 16, 1, 20], "Wi-Fi", true, true, true),
        ];

        assert_eq!(
            choose_lan_ipv4(&candidates),
            Some(Ipv4Addr::new(10, 16, 1, 20))
        );
    }

    #[test]
    fn direct_peer_hosts_cover_bare_ip_and_explicit_port() {
        let bare = direct_peer_hosts("192.168.1.8", 21118);
        assert_eq!(bare.first().map(String::as_str), Some("192.168.1.8:21118"));
        assert_eq!(bare.last().map(String::as_str), Some("192.168.1.8:21128"));

        let explicit = direct_peer_hosts("49.113.34.200:21118", 21118);
        assert_eq!(explicit, ["49.113.34.200:21118"]);

        let pasted = direct_peer_hosts(" 49.113.34.200:21118 ", 21118);
        assert_eq!(pasted, ["49.113.34.200:21118"]);

        let explicit_ipv6 = direct_peer_hosts("[2001:db8::8]:21118", 21118);
        assert_eq!(explicit_ipv6, ["[2001:db8::8]:21118"]);

        let bare_ipv6 = direct_peer_hosts("2001:db8::8", 21118);
        assert_eq!(
            bare_ipv6.first().map(String::as_str),
            Some("[2001:db8::8]:21118")
        );
        assert_eq!(
            bare_ipv6.last().map(String::as_str),
            Some("[2001:db8::8]:21128")
        );
        assert!(direct_peer_hosts("123456789", 21118).is_empty());
    }

    #[test]
    fn rejects_non_lan_and_non_public_addresses() {
        let candidates = [candidate([169, 254, 10, 1], "Ethernet", true, true, true)];
        assert_eq!(choose_lan_ipv4(&candidates), None);

        assert!(is_public_ipv4(Ipv4Addr::new(101, 87, 127, 173)));
        assert!(!is_public_ipv4(Ipv4Addr::new(10, 0, 0, 1)));
        assert!(!is_public_ipv4(Ipv4Addr::new(127, 0, 0, 1)));
        assert!(!is_public_ipv4(Ipv4Addr::new(169, 254, 1, 1)));
    }

    #[test]
    fn hosts_for_ip_expands_full_port_range() {
        let hosts = super::hosts_for_ip("192.168.31.70".parse().unwrap(), 21118);
        assert_eq!(hosts.len(), 11);
        assert_eq!(hosts.first().map(String::as_str), Some("192.168.31.70:21118"));
        assert_eq!(hosts.last().map(String::as_str), Some("192.168.31.70:21128"));
    }

    #[test]
    fn lan_peer_hosts_is_empty_for_unknown_or_empty_id() {
        // 未知 ID 必须返回空，绝不能让局域网探测污染「按 IP 直连」与
        // 正常的信令/打洞链路。
        assert!(super::lan_peer_hosts("", 21118).is_empty());
        assert!(super::lan_peer_hosts("__luoda_no_such_peer_id__", 21118).is_empty());
    }
}
