//! What the helper puts on the engine's command line and in every request context's preferences,
//! as plain data: nothing leaves the process except through bateri's proxy.
//!
//! A switch the engine does not know is ignored without a word, which is how three of the first
//! ones (`dns-prefetch-disable`, `no-proxy-server-autodetect-fallback`,
//! `force-webrtc-ip-handling-policy`) were never in force. So the names live here, apart from the
//! code that applies them, and the end-to-end suite looks each one up in the engine's binary
//! (`fake/tests/e2e.rs` pulls this file in with `#[path]`).

/// The proxy: every request, loopback included (a page on localhost is bateri's to see too).
pub const PROXY_SERVER: &str = "proxy-server";

/// Only for tests and development: the keychain is not the user's.
pub const MOCK_KEYCHAIN: &str = "use-mock-keychain";

/// Switches that take no value.
pub const PLAIN: &[&str] = &[
    // Without this the engine hands a proxy's 407 to Chrome's own login prompt, which an
    // off-screen browser does not have, and `auth_credentials` is never called.
    "disable-chrome-login-prompt",
    "noerrdialogs",
    // No background services, no QUIC (UDP), nothing that phones home.
    "disable-background-networking",
    "disable-component-update",
    "disable-quic",
    "disable-sync",
    "disable-default-apps",
    "no-pings",
    "disable-domain-reliability",
];

/// Features turned off (`disable-features`). The macOS notification bridge starts a helper of its
/// own that registers the app with the system's notification center, which makes the system ask
/// the person whether "bateri Browser" may send notifications; the helper never shows any (a
/// page's request is refused), so it must not appear there at all.
pub const DISABLED_FEATURES: &[&str] = &[
    "OptimizationHints",
    "Translate",
    "MediaRouter",
    "AutofillServerCommunication",
    "SafeBrowsing",
    "NativeNotifications",
];

/// The host-resolver rule: no name is resolved by this process, only the proxy's own address (an
/// IP literal, which needs no lookup) is let through. The page's host names go to the proxy as
/// names and are resolved where the proxy runs.
pub const HOST_RESOLVER_RULES: &str = "MAP * ~NOTFOUND , EXCLUDE 127.0.0.1";

/// Switches with a value, besides `proxy-server` (its port is the launch's) and
/// `disable-features` (`DISABLED_FEATURES`).
pub const VALUED: &[(&str, &str)] = &[
    // `<-loopback>`: `localhost` goes through the proxy too.
    ("proxy-bypass-list", "<-loopback>"),
    // WebRTC's UDP does not go through an HTTP proxy: with this policy it does not go at all.
    ("webrtc-ip-handling-policy", WEBRTC_POLICY),
    ("host-resolver-rules", HOST_RESOLVER_RULES),
];

pub const WEBRTC_POLICY: &str = "disable_non_proxied_udp";

/// What a preference is set to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pref {
    Str(&'static str),
    Int(i32),
}

/// `net.network_prediction_options`: Chromium's `kDisabled`. It is the switch DNS prefetching and
/// preconnecting answer to (`dns-prefetch-disable` is no switch of this engine).
pub const NETWORK_PREDICTION_DISABLED: i32 = 2;

/// Preferences every request context is held to. The switch above already decides the first one;
/// the helper reads each back and does not open a browser in a context where it is not in force.
pub const PREFS: &[(&str, Pref)] = &[
    ("webrtc.ip_handling_policy", Pref::Str(WEBRTC_POLICY)),
    (
        "net.network_prediction_options",
        Pref::Int(NETWORK_PREDICTION_DISABLED),
    ),
];

/// Every switch name used, for a check against the engine's binary (the tests').
#[cfg_attr(not(test), allow(dead_code))]
pub fn names() -> Vec<&'static str> {
    let mut all = vec![PROXY_SERVER, MOCK_KEYCHAIN, "disable-features"];
    all.extend(PLAIN);
    all.extend(VALUED.iter().map(|(name, _)| *name));
    all
}

/// The `disable-features` value.
pub fn disabled_features() -> String {
    DISABLED_FEATURES.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_known_dead_switches_are_gone() {
        for dead in [
            "dns-prefetch-disable",
            "no-proxy-server-autodetect-fallback",
            "force-webrtc-ip-handling-policy",
        ] {
            assert!(!names().contains(&dead), "{dead}");
        }
    }

    #[test]
    fn no_name_is_given_twice() {
        let mut all = names();
        all.sort_unstable();
        let before = all.len();
        all.dedup();
        assert_eq!(before, all.len());
    }

    #[test]
    fn the_resolver_rule_keeps_the_proxys_own_address_and_nothing_else() {
        assert!(HOST_RESOLVER_RULES.starts_with("MAP * ~NOTFOUND"));
        assert!(HOST_RESOLVER_RULES.ends_with("EXCLUDE 127.0.0.1"));
    }

    #[test]
    fn webrtc_is_held_to_proxied_udp_in_the_switch_and_in_the_preference() {
        assert!(VALUED.contains(&("webrtc-ip-handling-policy", "disable_non_proxied_udp")));
        assert!(PREFS.contains(&(
            "webrtc.ip_handling_policy",
            Pref::Str("disable_non_proxied_udp")
        )));
    }
}
