pub(crate) const UDP_REGISTRATION_FAILURE_LIMIT: i64 = 4;

pub(crate) fn should_start_with_tcp(
    test_tcp: bool,
    proxy: bool,
    websocket: bool,
    udp_disabled: bool,
    _windows_server: bool,
) -> bool {
    // NOTE: Do NOT force TCP for Windows Server.  The hbbs TCP listener does
    // not handle RegisterPeer – peer registration only works over UDP.
    // Forcing TCP on VPS causes "Connecting to LUODA network" forever.
    //
    // NOTE: `websocket` (= use_ws()) 也不再强制先走 TCP/WSS 注册。
    //
    // 历史上移动端因为运营商常封 21116-21119 而被改成"优先 WSS(443) 注册"，
    // 但 hbbs 的 handle_punch_hole_request 里同内网判定是
    //     let same_intranet = !ws && (peer_is_lan && is_lan || a.ip() == b.ip());
    // 且经 nginx 转发后服务端只能拿到 X-Real-IP、对端端口被写死 0，于是：
    //   * `!ws` 不成立  → 永远命不中 FetchLocalAddr（交换本地地址直连）快路径；
    //   * 对端地址变成 公网IP:0 → 主控端 `peer_addr.port() == 0` 直接 bail；
    //   * `peer_is_lan ^ is_lan` 还会把 nat_type 改成 SYMMETRIC → 被主动推去中继。
    // 结论：只要设备是"经 WebSocket 注册"的，TCP 打洞在协议层就不可用，
    // 跨网络时只能吃中继。
    //
    // 因此改为：移动端同样优先用原生 UDP 注册，让服务端看到真实源地址+端口，
    // 打洞与非对称 NAT 的判定才有意义。UDP 连续失败
    // UDP_REGISTRATION_FAILURE_LIMIT 次后（Android/iOS 不做指数退避，约 12s）
    // `RendezvousMediator::start` 会自动降级回 TCP/WSS，行为不退化。
    //
    // `websocket` 仍保留在签名里，方便日志/后续按平台细化策略，但不再参与判定。
    let _ = websocket;
    test_tcp || proxy || udp_disabled
}

pub(crate) fn should_fallback_to_tcp(consecutive_failures: i64) -> bool {
    consecutive_failures >= UDP_REGISTRATION_FAILURE_LIMIT
}

#[cfg(test)]
mod tests {
    use super::{should_fallback_to_tcp, should_start_with_tcp};

    #[test]
    fn windows_server_uses_udp_like_other_platforms() {
        // hbbs TCP does not support RegisterPeer, so Windows Server must use UDP
        assert!(!should_start_with_tcp(false, false, false, false, true));
        assert!(!should_start_with_tcp(false, false, false, false, false));
    }

    #[test]
    fn udp_is_default_unless_tcp_is_requested() {
        assert!(!should_start_with_tcp(false, false, false, false, false));
        assert!(should_start_with_tcp(false, true, false, false, false));
        assert!(should_start_with_tcp(false, false, false, true, false));
    }

    #[test]
    fn websocket_does_not_force_tcp_registration() {
        // use_ws() 为 true（移动端默认）时**不再**先走 TCP/WSS。
        // 见 should_start_with_tcp 的说明：只有原生 UDP 注册才能让 hbbs
        // 拿到真实端口并通过 `!ws` 的同内网快路径，打洞才可用。
        assert!(!should_start_with_tcp(false, false, true, false, false));
        // 但 proxy 仍然强制 TCP（代理环境下 UDP 不可用）。
        assert!(should_start_with_tcp(false, true, true, false, false));
        // udp_disabled 同理。
        assert!(should_start_with_tcp(false, false, true, true, false));
    }

    #[test]
    fn repeated_udp_registration_timeouts_trigger_tcp_fallback() {
        // The threshold is UDP_REGISTRATION_FAILURE_LIMIT (4).  Below it we keep
        // retrying on UDP (which is the only transport that can register the
        // peer ID on hbbs); at/above it we fall back to TCP.
        assert!(!should_fallback_to_tcp(1));
        assert!(!should_fallback_to_tcp(2));
        assert!(!should_fallback_to_tcp(3));
        assert!(should_fallback_to_tcp(4));
        assert!(should_fallback_to_tcp(5));
    }
}
