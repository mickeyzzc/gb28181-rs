//! Library-hygiene guards: regression tests that scan the crate's own
//! source so the classes of problems fixed in v0.6.0 cannot silently
//! reappear.
//!
//! Each guard strips everything from the first `#[cfg(test)]` onward (the
//! crate keeps its tests in-file), then asserts over the production code.

use std::fs;
use std::path::PathBuf;

/// Production (non-test) portion of every file under src/.
fn production_sources() -> Vec<(String, String)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut out = Vec::new();
    let mut stack = vec![dir];
    while let Some(d) = stack.pop() {
        let entries = fs::read_dir(&d).expect("read src dir");
        for entry in entries {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let content = fs::read_to_string(&path).expect("read source");
            let prod = match content.find("#[cfg(test)]") {
                Some(idx) => content[..idx].to_string(),
                None => content,
            };
            out.push((
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("?")
                    .to_string(),
                prod,
            ));
        }
    }
    out
}

/// No direct stdout/stderr printing in library code — everything goes
/// through the `log` facade so hosts can capture or silence it.
#[test]
fn no_direct_print_macros_in_library_code() {
    for (name, src) in production_sources() {
        assert!(
            !src.contains("println!(") && !src.contains("eprintln!("),
            "{name} uses println!/eprintln! — use the log facade instead"
        );
    }
}

/// No product branding baked into library code: the identity strings this
/// crate puts on the wire (User-Agent, catalog Manufacturer/Model/Name)
/// must come from configuration with neutral defaults.
#[test]
fn no_product_branding_in_library_code() {
    const BANNED: &[&str] = &[
        "mibee-rec/",   // the old hardcoded SIP User-Agent
        "\"MiBee\"",    // the old hardcoded catalog manufacturer
        "MiBee Camera", // the old hardcoded catalog/device name
        "\"OV5647\"",   // the old hardcoded catalog model
    ];
    for (name, src) in production_sources() {
        for banned in BANNED {
            assert!(
                !src.contains(banned),
                "{name} contains hardcoded branding {banned:?} — make it configurable with a neutral default"
            );
        }
    }
}

/// No panics or bare unwraps reachable by consumers in library code.
/// (`expect` on infallible-only cases is still avoided; lock poisoning is
/// recovered explicitly.)
#[test]
fn no_panics_or_bare_unwraps_in_library_code() {
    for (name, src) in production_sources() {
        assert!(
            !src.contains("panic!("),
            "{name} contains panic! — return an error instead"
        );
        assert!(
            !src.contains(".unwrap()"),
            "{name} contains .unwrap() — handle the error case explicitly"
        );
    }
}

/// No lab/example IP addresses baked into library code — endpoints come
/// from configuration. Documented exception: `src/config.rs` keeps the
/// spec-example serde defaults (kept for config-file backward compatibility;
/// `warn_on_example_defaults` flags them at startup).
#[test]
fn no_private_lab_ips_in_library_code() {
    for (name, src) in production_sources() {
        if name == "config.rs" {
            continue; // deliberate, documented serde example defaults
        }
        assert!(
            !src.contains("192.168."),
            "{name} contains a 192.168.x address — take it from configuration"
        );
        assert!(
            !src.contains("10.0."),
            "{name} contains a 10.0.x address — take it from configuration"
        );
    }
}

/// No hardcoded SIP port 5060 as a Via/Contact literal in library code:
/// the advertised port must be the configured local port. (5060 remains
/// the *default* config value, which is different from being hardcoded
/// into message builders.)
#[test]
fn sip_builders_do_not_hardcode_port_5060() {
    let sip = production_sources()
        .into_iter()
        .find(|(name, _)| name == "sip.rs")
        .expect("src/sip.rs present");
    assert!(
        !sip.1.contains("{}:5060") && !sip.1.contains(", 5060,"),
        "sip.rs hardcodes port 5060 in a message builder"
    );
}
