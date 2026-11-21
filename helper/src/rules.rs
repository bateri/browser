//! The helper's own rules, as plain functions: what may be loaded, what a profile folder may be
//! called, how a zoom ratio maps to Chromium's zoom level. No engine in here, so they are tested
//! without one.

/// The longest address the helper takes.
pub const MAX_ADDRESS: usize = 8192;

/// Why an address was not taken.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The scheme is not `http`, `https`, `about:blank` or `data:`.
    Scheme,
    /// Too long, or it has whitespace or something that is not printable ASCII.
    Address,
}

impl Refusal {
    pub fn reason(&self) -> &'static str {
        match self {
            Refusal::Scheme => "scheme",
            Refusal::Address => "address",
        }
    }
}

/// Whether a navigation requested through `Command::Navigate` is taken.
pub fn check_address(url: &str) -> Result<(), Refusal> {
    if url.len() > MAX_ADDRESS || !url.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        return Err(Refusal::Address);
    }
    if scheme_allowed(url, true) {
        Ok(())
    } else {
        Err(Refusal::Scheme)
    }
}

/// Whether the engine may navigate a frame to `url` (redirects and links included).
///
/// The set is `http`, `https`, `about:blank` and `data:`. A frame inside a page may also be
/// `about:srcdoc` or a `blob:` the page made itself: those carry the page's own origin and no
/// address of anyone else's, and an ordinary page uses them.
pub fn scheme_allowed(url: &str, main_frame: bool) -> bool {
    let lower = url.trim_start().to_ascii_lowercase();
    if lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("data:")
        || lower == "about:blank"
        || lower.starts_with("about:blank#")
    {
        return true;
    }
    !main_frame && (lower == "about:srcdoc" || lower.starts_with("blob:"))
}

/// A profile folder name: one path component, nothing that climbs out or hides.
pub fn profile_dir_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Zoom ratios the engine is asked for are held to this range.
/// A menu label without its mnemonic marker: the engine's labels carry the Windows convention
/// (`&Back`, `Save &as...`, a literal ampersand written `&&`), which a Mac menu does not show.
pub fn menu_label(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut chars = label.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '&' {
            if chars.peek() == Some(&'&') {
                out.push('&');
                chars.next();
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub const ZOOM_MIN: f64 = 0.25;
pub const ZOOM_MAX: f64 = 5.0;

/// Chromium's zoom level steps by powers of 1.2.
pub fn ratio_to_level(ratio: f64) -> f64 {
    ratio.clamp(ZOOM_MIN, ZOOM_MAX).ln() / 1.2f64.ln()
}

pub fn level_to_ratio(level: f64) -> f64 {
    1.2f64.powf(level)
}

/// A view's size in points and its scale, held to what a surface can be.
pub fn clamp_view(width: u32, height: u32, scale: f32) -> (i32, i32, f32) {
    let scale = if scale.is_finite() {
        scale.clamp(1.0, 4.0)
    } else {
        1.0
    };
    (
        width.clamp(1, 8192) as i32,
        height.clamp(1, 8192) as i32,
        scale,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_labels_lose_their_mnemonics() {
        assert_eq!(menu_label("&Back"), "Back");
        assert_eq!(menu_label("Save &as…"), "Save as…");
        assert_eq!(menu_label("Tom && Jerry"), "Tom & Jerry");
        assert_eq!(menu_label("Print…"), "Print…");
        assert_eq!(menu_label(""), "");
    }

    #[test]
    fn the_scheme_set_is_closed() {
        for ok in [
            "http://localhost:3000/",
            "HTTPS://example.com",
            "about:blank",
            "data:text/html,hi",
        ] {
            assert_eq!(check_address(ok), Ok(()), "{ok}");
        }
        for bad in [
            "file:///etc/hosts",
            "chrome://settings",
            "devtools://devtools/bundled/inspector.html",
            "view-source:http://a.test/",
            "javascript:alert(1)",
            "about:srcdoc",
            "blob:http://a.test/1",
            "ftp://a.test/",
            "//a.test/",
            "localhost:3000",
        ] {
            assert_eq!(check_address(bad), Err(Refusal::Scheme), "{bad}");
        }
    }

    #[test]
    fn an_address_is_printable_ascii_without_blanks() {
        assert_eq!(check_address("http://a.test/a b"), Err(Refusal::Address));
        assert_eq!(check_address("http://a.test/\n"), Err(Refusal::Address));
        assert_eq!(check_address("http://ü.test/"), Err(Refusal::Address));
        let long = format!("http://a.test/{}", "a".repeat(MAX_ADDRESS));
        assert_eq!(check_address(&long), Err(Refusal::Address));
    }

    #[test]
    fn frames_inside_a_page_may_be_srcdoc_and_blob() {
        assert!(scheme_allowed("about:srcdoc", false));
        assert!(scheme_allowed("blob:http://a.test/x", false));
        assert!(!scheme_allowed("about:srcdoc", true));
        assert!(!scheme_allowed("file:///", false));
    }

    #[test]
    fn profile_folders_stay_inside_the_root() {
        for ok in ["a", "work-1", "x_y.z"] {
            assert!(profile_dir_ok(ok), "{ok}");
        }
        for bad in ["", ".", "..", ".hidden", "a/b", "../x", "a b", "ü", "a\0b"] {
            assert!(!profile_dir_ok(bad), "{bad:?}");
        }
        assert!(!profile_dir_ok(&"a".repeat(65)));
    }

    #[test]
    fn zoom_levels_round_trip_and_clamp() {
        assert_eq!(ratio_to_level(1.0), 0.0);
        let r = level_to_ratio(ratio_to_level(2.0));
        assert!((r - 2.0).abs() < 1e-9);
        assert_eq!(ratio_to_level(100.0), ratio_to_level(ZOOM_MAX));
        assert_eq!(ratio_to_level(0.0), ratio_to_level(ZOOM_MIN));
    }

    #[test]
    fn a_view_is_held_to_what_a_surface_can_be() {
        assert_eq!(clamp_view(0, 99999, f32::NAN), (1, 8192, 1.0));
        assert_eq!(clamp_view(800, 600, 2.0), (800, 600, 2.0));
    }
}
