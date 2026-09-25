use std::io::{Read, Write};
use std::path::Path;
use std::sync::OnceLock;

use crate::format::Invalid;

/// Whether fetching `sources` would touch the network - which is what needs
/// the user's consent. A `file://` source does not.
pub fn needs_network(sources: &[String]) -> bool {
    sources.iter().any(|s| s.starts_with("https://"))
        && !sources
            .iter()
            .any(|s| s.starts_with("file://") && local(s).is_some_and(|p| p.exists()))
}

/// Fetch from the first source that works into `dest` (created, must not
/// exist), reading at most `limit` bytes when a limit is known.
pub fn fetch(sources: &[String], limit: Option<u64>, dest: &Path) -> Result<u64, Invalid> {
    if sources.is_empty() {
        return Err(Invalid::new("the spec names no source to fetch from"));
    }
    let mut failures = Vec::new();
    for source in sources {
        let _ = std::fs::remove_file(dest);
        match one(source, limit, dest) {
            Ok(size) => return Ok(size),
            Err(e) => failures.push(format!("{source}: {e}")),
        }
    }
    let _ = std::fs::remove_file(dest);
    Err(Invalid::new(failures.join("; ")))
}

fn one(source: &str, limit: Option<u64>, dest: &Path) -> Result<u64, String> {
    let reader: Box<dyn Read> = if source.starts_with("file://") {
        let path = local(source).ok_or("not a local path")?;
        Box::new(std::fs::File::open(path).map_err(|e| e.to_string())?)
    } else if source.starts_with("https://") {
        let response = agent()
            .get(source)
            // The artifact is already compressed, and a server that
            // compressed it again would have the client undo that and
            // hash the wrong bytes.
            .header("accept-encoding", "identity")
            .call()
            .map_err(|e| e.to_string())?;
        Box::new(response.into_body().into_reader())
    } else {
        return Err("only https:// and file:// sources are fetched".into());
    };
    let mut out = std::fs::File::create_new(dest).map_err(|e| e.to_string())?;
    // One byte past the limit, so that "exactly the limit" and "more than it"
    // can be told apart.
    let cap = limit.map_or(u64::MAX, |l| l.saturating_add(1));
    let written = std::io::copy(&mut reader.take(cap), &mut out).map_err(|e| e.to_string())?;
    out.flush().map_err(|e| e.to_string())?;
    if let Some(limit) = limit
        && written > limit
    {
        return Err(format!("larger than the {limit} bytes the spec gives"));
    }
    Ok(written)
}

/// `file:///a/b` as `/a/b`.
fn local(source: &str) -> Option<&Path> {
    source
        .strip_prefix("file://")
        .filter(|path| path.starts_with('/'))
        .map(Path::new)
}

/// One agent for the process: it holds the connection pool.
fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .user_agent(concat!("rosetta3/", env!("CARGO_PKG_VERSION")))
            // A redirect to plain HTTP would put a MITM in front of a file
            // whose hash is checked anyway - but there is no reason to allow
            // it, and a refusal is easier to diagnose than a hash mismatch.
            .https_only(true)
            .timeout_connect(Some(std::time::Duration::from_secs(30)))
            .timeout_global(Some(std::time::Duration::from_secs(600)))
            .build()
            .into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify::tests::scratch;

    #[test]
    fn file_sources_are_read_and_bounded() {
        let at = scratch("fetch");
        let source = at.join("source");
        std::fs::write(&source, b"0123456789").unwrap();
        let url = format!("file://{}", source.display());
        let dest = at.join("dest");
        assert_eq!(
            fetch(std::slice::from_ref(&url), Some(10), &dest).unwrap(),
            10
        );
        assert_eq!(std::fs::read(&dest).unwrap(), b"0123456789");
        assert!(fetch(std::slice::from_ref(&url), Some(9), &dest).is_err());
        assert!(!dest.exists(), "a refused fetch leaves nothing behind");
        // The first working source wins.
        let missing = format!("file://{}", at.join("nope").display());
        assert_eq!(fetch(&[missing, url.clone()], None, &dest).unwrap(), 10);
        assert!(!needs_network(&[url]));
        let _ = std::fs::remove_dir_all(&at);
    }
}
