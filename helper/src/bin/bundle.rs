//! `bundle --target-dir DIR --out DIR --version X.Y.Z --license FILE`: lays out
//! `bateri-browser.app` from the binaries cargo built in `--target-dir` (the main app, the
//! Chromium framework and the five helper apps), makes it a hidden app, and adds the licences.
//! Signing is the Makefile's: it has to go inside out and needs the identity.

use anyhow::{Context, Result, bail};
use cef::build_util::mac::{BundleInfo, bundle};
use std::path::PathBuf;

const NAME: &str = "bateri-browser";
const HELPER: &str = "bateri-browser-helper";

fn flag(name: &str) -> Result<String> {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == name {
            return args.next().with_context(|| format!("{name} needs a value"));
        }
    }
    bail!("{name} is required")
}

fn main() -> Result<()> {
    let target = PathBuf::from(flag("--target-dir")?);
    let out = PathBuf::from(flag("--out")?);
    let version = semver::Version::parse(&flag("--version")?)?;
    let license = PathBuf::from(flag("--license")?);
    let app_path = out.join(format!("{NAME}.app"));
    if app_path.exists() {
        std::fs::remove_dir_all(&app_path)?;
    }
    std::fs::create_dir_all(&out)?;
    let info = BundleInfo::new(
        NAME,
        "dev.bateri.browser",
        "bateri Browser",
        "English",
        version,
    );
    let app = bundle(&out, &target, NAME, HELPER, None, info)?;

    // Hidden: no Dock icon, not in the app switcher, not among the programs. The helper apps
    // inside already are; the template leaves the main app out.
    let plist_path = app.join("Contents/Info.plist");
    let mut plist = plist::Value::from_file(&plist_path)?
        .into_dictionary()
        .context("Info.plist is not a dictionary")?;
    plist.insert("LSUIElement".into(), plist::Value::String("1".into()));
    plist.insert(
        "NSHighResolutionCapable".into(),
        plist::Value::Boolean(true),
    );
    plist::Value::Dictionary(plist).to_file_xml(&plist_path)?;

    // The licences ride along: ours, the engine's own (its BSD licence is the header every
    // source file of the engine carries; the distribution has no separate file), and the
    // third-party notices Chromium generates.
    let resources = app.join("Contents/Resources");
    std::fs::copy(&license, resources.join("LICENSE"))?;
    let cef_dir = cef_dll_sys::get_cef_dir().context("CEF distribution not found")?;
    let credits = cef_dir.join("CREDITS.html");
    std::fs::copy(&credits, resources.join("CREDITS.html"))
        .with_context(|| format!("{}", credits.display()))?;
    let header = std::fs::read_to_string(cef_dir.join("include/cef_base.h"))?;
    let notice: Vec<&str> = header
        .lines()
        .take_while(|l| l.starts_with("//"))
        .map(|l| {
            l.trim_start_matches("//")
                .strip_prefix(' ')
                .unwrap_or(l.trim_start_matches("//"))
        })
        .collect();
    if notice.len() < 10 {
        bail!("the engine's licence header was not where it was expected");
    }
    std::fs::write(
        resources.join("LICENSE-CEF.txt"),
        format!("Chromium Embedded Framework\n\n{}\n", notice.join("\n")),
    )?;
    println!("{}", app.display());
    Ok(())
}
