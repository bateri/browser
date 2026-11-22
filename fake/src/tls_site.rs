//! A self-signed https and wss server a test owns: what a developer's own server is to the
//! browser. The certificate is made with the system's `openssl`, the server is a small Python
//! script (both are on a Mac that builds the helper), and the stub proxy carries a `CONNECT` to
//! it (`StubProxy::tunnel`).

use crate::scratch;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

const SCRIPT: &str = include_str!("tls_site.py");

pub struct TlsSite {
    pub port: u16,
    /// Where every path the server was asked for is written, one per line.
    log: PathBuf,
    child: Child,
}

impl TlsSite {
    /// A server whose certificate, signed by itself, names `names` (comma separated DNS names).
    pub fn start(names: &str) -> Self {
        let dir = scratch("tls");
        let (cert, key, log) = (
            dir.join("cert.pem"),
            dir.join("key.pem"),
            dir.join("log.txt"),
        );
        let sans: Vec<String> = names.split(',').map(|n| format!("DNS:{n}")).collect();
        let config = dir.join("openssl.cnf");
        std::fs::write(
            &config,
            format!(
                "[req]\ndistinguished_name=dn\nx509_extensions=ext\nprompt=no\n[dn]\nCN={}\n[ext]\nsubjectAltName={}\n",
                names.split(',').next().unwrap(),
                sans.join(",")
            ),
        )
        .expect("write the certificate request");
        let made = Command::new("openssl")
            .args([
                "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "30", "-config",
            ])
            .arg(&config)
            .arg("-keyout")
            .arg(&key)
            .arg("-out")
            .arg(&cert)
            .stderr(Stdio::null())
            .status()
            .expect("run openssl");
        assert!(made.success(), "openssl could not make a certificate");
        let script = dir.join("tls_site.py");
        std::fs::write(&script, SCRIPT).expect("write the server");
        std::fs::write(&log, "").expect("write the log");
        let mut child = Command::new("python3")
            .arg("-I")
            .arg(&script)
            .arg(&cert)
            .arg(&key)
            .arg(&log)
            .stdout(Stdio::piped())
            .spawn()
            .expect("start the TLS server");
        let mut line = String::new();
        let mut out = std::io::BufReader::new(child.stdout.take().unwrap());
        std::io::BufRead::read_line(&mut out, &mut line).expect("the server's port");
        let port = line.trim().parse().expect("the server's port is a number");
        Self { port, log, child }
    }

    /// The paths the server has been asked for so far.
    pub fn requests(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

impl Drop for TlsSite {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
