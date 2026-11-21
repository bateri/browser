//! The helper's command line: the launch contract `browser_ipc::Launch::args` writes.
//!
//! Chromium's own command-line handling is off (`command_line_args_disabled`), so a switch given
//! to the helper does nothing to the engine: `--no-sandbox` is ignored here like any unknown
//! argument. The five values are all required; without a proxy the engine would reach the
//! network directly, which the helper never does.

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub key: String,
    pub socket: PathBuf,
    pub mach: String,
    /// Where profiles live (canonical, and a directory only this user can enter).
    pub root: PathBuf,
    /// The port of bateri's proxy on 127.0.0.1.
    pub proxy_port: u16,
    pub mock_keychain: bool,
}

/// What a missing or wrong argument looks like to the person who started the helper by hand.
#[derive(Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// No `--key`: nobody started this on bateri's behalf.
    NoKey,
    Missing(&'static str),
    Bad(&'static str, String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::NoKey => write!(f, "no key, nothing to do"),
            ConfigError::Missing(name) => write!(f, "--{name} is required"),
            ConfigError::Bad(name, why) => write!(f, "--{name}: {why}"),
        }
    }
}

/// `http://127.0.0.1:PORT` and nothing else.
pub fn parse_proxy(value: &str) -> Result<u16, String> {
    let port = value
        .strip_prefix("http://127.0.0.1:")
        .ok_or("must be http://127.0.0.1:PORT")?;
    match port.parse::<u16>() {
        Ok(p) if p != 0 && p.to_string() == port => Ok(p),
        _ => Err("must be http://127.0.0.1:PORT".into()),
    }
}

pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Config, ConfigError> {
    let (mut key, mut socket, mut mach, mut root, mut proxy) = (None, None, None, None, None);
    let mut mock_keychain = false;
    for arg in args {
        let Some(flag) = arg.strip_prefix("--") else {
            continue;
        };
        let (name, value) = match flag.split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (flag, None),
        };
        match (name, value) {
            ("key", v) => key = v,
            ("socket", v) => socket = v,
            ("mach", v) => mach = v,
            ("root", v) => root = v,
            ("proxy", v) => proxy = v,
            ("mock-keychain", None) => mock_keychain = true,
            _ => {}
        }
    }
    let key = key.filter(|k| !k.is_empty()).ok_or(ConfigError::NoKey)?;
    let socket = socket.ok_or(ConfigError::Missing("socket"))?;
    let mach = mach.ok_or(ConfigError::Missing("mach"))?;
    let root = root.ok_or(ConfigError::Missing("root"))?;
    let proxy = proxy.ok_or(ConfigError::Missing("proxy"))?;
    let proxy_port = parse_proxy(&proxy).map_err(|e| ConfigError::Bad("proxy", e))?;
    Ok(Config {
        key,
        socket: socket.into(),
        mach,
        root: root.into(),
        proxy_port,
        mock_keychain,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    const FULL: [&str; 5] = [
        "--key=k",
        "--socket=/tmp/s",
        "--mach=m",
        "--root=/tmp/r",
        "--proxy=http://127.0.0.1:9000",
    ];

    #[test]
    fn all_five_values_make_a_config() {
        let config = parse(args(&FULL)).unwrap();
        assert_eq!(config.proxy_port, 9000);
        assert!(!config.mock_keychain);
        let mut more = args(&FULL);
        more.push("--mock-keychain".into());
        assert!(parse(more).unwrap().mock_keychain);
    }

    #[test]
    fn no_key_means_nothing_to_do() {
        assert_eq!(parse(args(&FULL[1..])).unwrap_err(), ConfigError::NoKey);
        assert_eq!(parse(args(&[])).unwrap_err(), ConfigError::NoKey);
        let mut empty = args(&FULL[1..]);
        empty.push("--key=".into());
        assert_eq!(parse(empty).unwrap_err(), ConfigError::NoKey);
    }

    #[test]
    fn every_other_value_is_required() {
        for (i, name) in [(1, "socket"), (2, "mach"), (3, "root"), (4, "proxy")] {
            let mut list = args(&FULL);
            list.remove(i);
            assert_eq!(parse(list).unwrap_err(), ConfigError::Missing(name));
        }
    }

    #[test]
    fn the_proxy_is_bateris_loopback_one() {
        for bad in [
            "http://127.0.0.1",
            "http://127.0.0.1:0",
            "http://127.0.0.1:99999",
            "http://127.0.0.1:080",
            "http://localhost:9000",
            "http://10.0.0.1:9000",
            "https://127.0.0.1:9000",
            "socks5://127.0.0.1:9000",
            "127.0.0.1:9000",
            "",
        ] {
            assert!(parse_proxy(bad).is_err(), "{bad}");
        }
        assert_eq!(parse_proxy("http://127.0.0.1:65535"), Ok(65535));
    }

    #[test]
    fn engine_switches_are_not_the_helpers() {
        let mut list = args(&FULL);
        list.push("--no-sandbox".into());
        list.push("--proxy-server=http://evil:1".into());
        assert!(parse(list).is_ok());
    }
}
