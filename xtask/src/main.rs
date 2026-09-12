//! Release plumbing.
//!
//! Two documents describe a release, and both are generated here rather than
//! in YAML, for the same reason the bundle is a script and not a sequence of
//! workflow steps: a release has to be reproducible by hand when CI is not
//! the thing that is broken.
//!
//!   xtask manifest --version 0.2.0 --base-url URL \
//!       --cli aarch64-apple-darwin=path/to/archive.tar.gz [--cli …] \
//!       [--app path/to/PruneJuice-0.2.0.zip] \
//!       [--notes-url URL] --out update-manifest.json
//!
//!   xtask appcast --version 0.2.0 --zip PATH --url URL \
//!       --signature ED_SIG [--notes-url URL] [--previous appcast.xml] \
//!       --out appcast.xml
//!
//! The manifest is what the CLI reads; the appcast is what Sparkle reads. They
//! describe independently versioned products. Each app archive contains its
//! pinned helper; a CLI release does not change an installed app.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};

/// Must match `prune_juice_core::update::SCHEMA`. There is a test.
const SCHEMA: u32 = 1;

/// Sparkle's `sparkle:minimumSystemVersion`, matching `LSMinimumSystemVersion`
/// in the bundle. Sparkle refuses to offer an update a machine cannot run,
/// which is the whole reason to state it.
const MINIMUM_MACOS: &str = "14.0";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("manifest") => manifest(&args[1..]),
        Some("appcast") => appcast(&args[1..]),
        Some(other) => bail!("unknown command `{other}`; expected `manifest` or `appcast`"),
        None => {
            eprintln!("{}", include_str!("usage.txt"));
            Ok(())
        }
    }
}

/// `--flag value` pairs, plus repeated flags collected in order.
fn parse(args: &[String]) -> Result<BTreeMap<String, Vec<String>>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let Some(flag) = a.strip_prefix("--") else {
            bail!("unexpected argument `{a}`");
        };
        let value = it
            .next()
            .ok_or_else(|| anyhow!("--{flag} needs a value"))?
            .clone();
        out.entry(flag.to_string()).or_default().push(value);
    }
    Ok(out)
}

fn one<'a>(args: &'a BTreeMap<String, Vec<String>>, flag: &str) -> Result<&'a str> {
    args.get(flag)
        .and_then(|v| v.first())
        .map(String::as_str)
        .ok_or_else(|| anyhow!("--{flag} is required"))
}

fn sha256_hex(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut h = Sha256::new();
    h.update(&bytes);
    Ok(h.finalize().iter().fold(String::new(), |mut s, b| {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
        s
    }))
}

// --- the CLI's manifest ---------------------------------------------------

fn manifest(args: &[String]) -> Result<()> {
    let args = parse(args)?;
    let version = one(&args, "version")?.trim_start_matches('v').to_string();
    let base = one(&args, "base-url")?.trim_end_matches('/').to_string();
    let out = PathBuf::from(one(&args, "out")?);

    let mut artifacts = Vec::new();
    for pair in args.get("cli").into_iter().flatten() {
        let (target, path) = pair
            .split_once('=')
            .ok_or_else(|| anyhow!("--cli takes <target-triple>=<path>, got `{pair}`"))?;
        artifacts.push(entry("cli", target, Path::new(path), &base)?);
    }
    if artifacts.is_empty() {
        // A manifest with no CLI build would tell every copy that an update
        // exists and then have nothing to give it.
        bail!("--cli is required at least once: a release with no CLI artifact is not a release");
    }
    for path in args.get("app").into_iter().flatten() {
        // Listed so the manifest is a complete description of the release.
        // The CLI never installs it — Sparkle owns the app.
        artifacts.push(entry(
            "app",
            "universal-apple-darwin",
            Path::new(path),
            &base,
        )?);
    }

    let mut doc = serde_json::Map::new();
    doc.insert("schema".into(), SCHEMA.into());
    doc.insert("version".into(), version.into());
    doc.insert("published".into(), rfc3339(now())?.into());
    if let Some(url) = args.get("notes-url").and_then(|v| v.first()) {
        doc.insert("notes_url".into(), url.clone().into());
    }
    doc.insert("artifacts".into(), serde_json::Value::Array(artifacts));

    let text = serde_json::to_string_pretty(&serde_json::Value::Object(doc))? + "\n";
    std::fs::write(&out, &text).with_context(|| format!("writing {}", out.display()))?;
    eprintln!("wrote {} ({} bytes)", out.display(), text.len());
    Ok(())
}

fn entry(kind: &str, target: &str, path: &Path, base: &str) -> Result<serde_json::Value> {
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("{} has no file name", path.display()))?
        .to_string_lossy()
        .to_string();
    let bytes = std::fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .len();
    if bytes == 0 {
        bail!("{} is empty", path.display());
    }
    Ok(serde_json::json!({
        "kind": kind,
        "target": target,
        "url": format!("{base}/{name}"),
        "sha256": sha256_hex(path)?,
        "bytes": bytes,
    }))
}

// --- Sparkle's appcast ----------------------------------------------------

fn validate_signature(signature: &str) -> Result<()> {
    let body = signature.strip_suffix("==").unwrap_or("");
    if body.len() != 86
        || !body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
        || !matches!(body.as_bytes().last(), Some(b'A' | b'Q' | b'g' | b'w'))
    {
        bail!("--signature must be a base64-encoded 64-byte Ed25519 signature");
    }
    Ok(())
}

fn appcast(args: &[String]) -> Result<()> {
    let args = parse(args)?;
    let version = one(&args, "version")?.trim_start_matches('v').to_string();
    let zip = PathBuf::from(one(&args, "zip")?);
    let url = one(&args, "url")?.to_string();
    let signature = one(&args, "signature")?.to_string();
    validate_signature(&signature)?;
    let out = PathBuf::from(one(&args, "out")?);
    let notes = args.get("notes-url").and_then(|v| v.first()).cloned();

    let length = std::fs::metadata(&zip)
        .with_context(|| format!("stat {}", zip.display()))?
        .len();

    let mut item = String::new();
    item.push_str("    <item>\n");
    item.push_str(&format!("      <title>{version}</title>\n"));
    item.push_str(&format!("      <pubDate>{}</pubDate>\n", rfc822(now())?));
    item.push_str(&format!(
        "      <sparkle:version>{version}</sparkle:version>\n"
    ));
    item.push_str(&format!(
        "      <sparkle:shortVersionString>{version}</sparkle:shortVersionString>\n"
    ));
    item.push_str(&format!(
        "      <sparkle:minimumSystemVersion>{MINIMUM_MACOS}</sparkle:minimumSystemVersion>\n"
    ));
    if let Some(notes) = &notes {
        item.push_str(&format!(
            "      <sparkle:releaseNotesLink>{}</sparkle:releaseNotesLink>\n",
            escape(notes)
        ));
    }
    item.push_str(&format!(
        "      <enclosure url=\"{}\" length=\"{length}\" \
         type=\"application/octet-stream\" sparkle:edSignature=\"{}\"/>\n",
        escape(&url),
        escape(&signature)
    ));
    item.push_str("    </item>\n");

    // Carry the previous releases forward so the feed keeps a version history
    // rather than shrinking to one entry every time. An entry for the version
    // being released is dropped first: re-releasing a tag must replace its
    // item, not sit beside it.
    let mut items = vec![item];
    if let Some(previous) = args.get("previous").and_then(|v| v.first()) {
        match std::fs::read_to_string(previous) {
            Ok(text) => items.extend(
                existing_items(&text)
                    .into_iter()
                    .filter(|i| !mentions_version(i, &version)),
            ),
            Err(e) => eprintln!("no previous appcast at {previous} ({e}); starting a new feed"),
        }
    }

    let mut doc = String::from("<?xml version=\"1.0\" standalone=\"yes\"?>\n");
    doc.push_str(
        "<rss xmlns:sparkle=\"http://www.andymatuschak.org/xml-namespaces/sparkle\" \
         version=\"2.0\">\n",
    );
    doc.push_str("  <channel>\n");
    doc.push_str("    <title>Prune Juice</title>\n");
    for i in &items {
        doc.push_str(i);
    }
    doc.push_str("  </channel>\n</rss>\n");

    std::fs::write(&out, &doc).with_context(|| format!("writing {}", out.display()))?;
    eprintln!("wrote {} ({} items)", out.display(), items.len());
    Ok(())
}

/// Pull `<item>…</item>` blocks out of an existing feed.
///
/// String scanning rather than an XML parser, deliberately: the input is a
/// document this same code wrote, the alternative is a dependency, and a
/// malformed previous feed degrades to "start a new one" instead of failing a
/// release.
fn existing_items(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<item>") {
        let Some(end) = rest[start..].find("</item>") else {
            break;
        };
        let end = start + end + "</item>".len();
        out.push(format!("    {}\n", rest[start..end].trim()));
        rest = &rest[end..];
    }
    out
}

fn mentions_version(item: &str, version: &str) -> bool {
    item.contains(&format!("<sparkle:version>{version}</sparkle:version>"))
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// --- dates ----------------------------------------------------------------
//
// Two formats, no dependency. `chrono` for two timestamps in a build tool
// would be the most-compiled crate in the workspace for the least reason.

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Days since the epoch to (year, month, day). Howard Hinnant's `civil_from_days`.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn parts(unix: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (unix / 86_400) as i64;
    let secs = unix % 86_400;
    let (y, m, d) = civil(days);
    (
        y,
        m,
        d,
        (secs / 3600) as u32,
        (secs % 3600 / 60) as u32,
        (secs % 60) as u32,
    )
}

fn rfc3339(unix: u64) -> Result<String> {
    let (y, m, d, hh, mm, ss) = parts(unix);
    Ok(format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z"))
}

/// RFC 822, which is what an RSS `pubDate` is.
fn rfc822(unix: u64) -> Result<String> {
    const DAY: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MON: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (y, m, d, hh, mm, ss) = parts(unix);
    // 1970-01-01 was a Thursday, which is why the table starts there.
    let dow = DAY[((unix / 86_400) % 7) as usize];
    let mon = MON[(m - 1) as usize];
    Ok(format!(
        "{dow}, {d:02} {mon} {y:04} {hh:02}:{mm:02}:{ss:02} +0000"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manifest_schema_matches_what_the_cli_understands() {
        // These are two crates that must agree. If one moves, this fails.
        assert_eq!(SCHEMA, prune_juice_schema());
    }

    /// Read the constant out of the core crate's source rather than depending
    /// on it: xtask must stay buildable on its own.
    fn prune_juice_schema() -> u32 {
        let src = include_str!("../../crates/prune-juice-core/src/update/mod.rs");
        let line = src
            .lines()
            .find(|l| l.contains("pub const SCHEMA: u32"))
            .expect("SCHEMA is declared in the update module");
        line.rsplit('=')
            .next()
            .unwrap()
            .trim()
            .trim_end_matches(';')
            .parse()
            .expect("SCHEMA is a number")
    }

    #[test]
    fn dates_are_formatted_in_both_shapes_the_documents_need() {
        // 2026-09-10T00:00:00Z, a Thursday.
        let t = 1_788_998_400;
        assert_eq!(rfc3339(t).unwrap(), "2026-09-10T00:00:00Z");
        assert_eq!(rfc822(t).unwrap(), "Thu, 10 Sep 2026 00:00:00 +0000");
        assert_eq!(rfc3339(0).unwrap(), "1970-01-01T00:00:00Z");
        assert_eq!(rfc822(0).unwrap(), "Thu, 01 Jan 1970 00:00:00 +0000");
    }

    #[test]
    fn previous_items_are_carried_forward_and_the_reissued_one_is_replaced() {
        let old = "<rss><channel>\
            <item><sparkle:version>0.1.0</sparkle:version></item>\
            <item><sparkle:version>0.2.0</sparkle:version></item>\
            </channel></rss>";
        let items = existing_items(old);
        assert_eq!(items.len(), 2);
        // Releasing 0.2.0 again replaces its entry rather than duplicating it.
        let kept: Vec<_> = items
            .iter()
            .filter(|i| !mentions_version(i, "0.2.0"))
            .collect();
        assert_eq!(kept.len(), 1);
        assert!(kept[0].contains("0.1.0"));
    }

    #[test]
    fn a_malformed_previous_feed_yields_no_items_rather_than_panicking() {
        assert!(existing_items("<rss><channel><item>unterminated").is_empty());
        assert!(existing_items("").is_empty());
    }

    #[test]
    fn urls_and_signatures_are_escaped_into_the_xml() {
        assert_eq!(escape("a&b<c>\"d\""), "a&amp;b&lt;c&gt;&quot;d&quot;");
    }

    #[test]
    fn appcasts_refuse_empty_and_malformed_signatures() {
        assert!(validate_signature("").is_err());
        assert!(validate_signature("not a signature").is_err());
        assert!(validate_signature(&"A".repeat(88)).is_err());
        assert!(validate_signature(&format!("{}==", "A".repeat(86))).is_ok());
        assert!(validate_signature(&format!("{}B==", "A".repeat(85))).is_err());
    }

    #[test]
    fn repeated_flags_collect_and_missing_values_are_refused() {
        let args = parse(&[
            "--cli".into(),
            "a=1".into(),
            "--cli".into(),
            "b=2".into(),
            "--out".into(),
            "x".into(),
        ])
        .unwrap();
        assert_eq!(args["cli"], vec!["a=1", "b=2"]);
        assert_eq!(one(&args, "out").unwrap(), "x");
        assert!(one(&args, "version").is_err());
        assert!(parse(&["--cli".into()]).is_err());
    }
}
