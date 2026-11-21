//! `notice --version V --chromium C --arch A --name FILE --size N --sha256 HEX [--minimum V]`:
//! prints the release notice (`Manifest`) as JSON, for the release recipe.

use browser_ipc::manifest::{Asset, FORMAT, Manifest};
use browser_ipc::message::PROTOCOL;

fn flag(name: &str) -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == name {
            return args.next();
        }
    }
    None
}

fn need(name: &str) -> String {
    flag(name).unwrap_or_else(|| {
        eprintln!("notice: {name} is required");
        std::process::exit(2)
    })
}

fn main() {
    let version = need("--version");
    let notice = Manifest {
        format: FORMAT,
        minimum: flag("--minimum").unwrap_or_else(|| version.clone()),
        version,
        chromium: need("--chromium"),
        protocol: PROTOCOL,
        assets: vec![Asset {
            arch: need("--arch"),
            name: need("--name"),
            size: need("--size").parse().unwrap_or_else(|_| {
                eprintln!("notice: --size is not a number");
                std::process::exit(2)
            }),
            sha256: need("--sha256"),
        }],
    };
    let text = serde_json::to_string_pretty(&notice).expect("serialize");
    if let Err(e) = Manifest::parse(text.as_bytes()) {
        eprintln!("notice: {e}");
        std::process::exit(2);
    }
    println!("{text}");
}
