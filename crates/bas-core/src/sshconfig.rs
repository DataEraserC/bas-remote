//! SSH config text manipulation.
//!
//! Pure string work: the protocol layer has no filesystem, so a host reads the
//! file, hands the text in, and writes back what comes out. That keeps the one
//! risky part — deciding whether a file may be written at all — in the host,
//! where the paths live, and keeps this crate testable without touching disk.
//!
//! Entries are delimited so only this plugin's own blocks are ever touched:
//!
//! ```text
//! # >>> dsh-bas-remote <section>
//! Host <section>
//!   HostName 127.0.0.1
//!   Port <port>
//!   User <user>
//!   IdentityFile <key>
//!   NoHostAuthenticationForLocalhost yes
//! # <<< dsh-bas-remote <section>
//! ```

/// Marker that opens a managed block.
#[must_use]
pub fn block_start(section: &str) -> String {
    format!("# >>> dsh-bas-remote {section}")
}

/// Marker that closes a managed block.
#[must_use]
pub fn block_end(section: &str) -> String {
    format!("# <<< dsh-bas-remote {section}")
}

/// Render the `Host` block for one dev space.
///
/// `markers` wraps it in this crate's delimiters (what gets written into a
/// file); without them it is the text a user pastes into their own config.
#[must_use]
pub fn block(
    section: &str,
    identity_file: &str,
    port: u16,
    user: &str,
    host_name: &str,
    markers: bool,
) -> String {
    let mut lines = Vec::new();
    if markers {
        lines.push(block_start(section));
    }
    lines.push(format!("Host {section}"));
    lines.push(format!("  HostName {host_name}"));
    lines.push(format!("  Port {port}"));
    lines.push(format!("  User {user}"));
    lines.push(format!("  IdentityFile {identity_file}"));
    lines.push("  NoHostAuthenticationForLocalhost yes".to_string());
    if markers {
        lines.push(block_end(section));
    }
    lines.join("\n")
}

/// Remove the managed block of `section`, returning the new text.
#[must_use]
pub fn strip_block(text: &str, section: &str) -> (String, bool) {
    let start = block_start(section);
    let end = block_end(section);
    let mut kept: Vec<&str> = Vec::new();
    let mut skipping = false;
    let mut removed = false;
    for line in text.split('\n') {
        if !skipping && line.trim() == start {
            skipping = true;
            removed = true;
            continue;
        }
        if skipping {
            if line.trim() == end {
                skipping = false;
            }
            continue;
        }
        kept.push(line);
    }
    (kept.join("\n"), removed)
}

/// Collapse runs of blank lines and drop leading ones.
#[must_use]
pub fn collapse_blank_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blanks = 0usize;
    for line in text.split('\n') {
        if line.is_empty() {
            blanks += 1;
            if blanks > 1 {
                continue;
            }
        } else {
            blanks = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    let trimmed = out.trim_start_matches('\n');
    trimmed.trim_end_matches('\n').to_string()
}

/// One entry to write into a config file.
#[derive(Debug, Clone)]
pub struct Entry<'a> {
    pub section: &'a str,
    pub identity_file: &'a str,
    pub port: u16,
    pub user: &'a str,
    pub host_name: &'a str,
    /// Written only when the file is being created, so a user's own header is
    /// never duplicated on a later update.
    pub header: &'a str,
}

impl<'a> Entry<'a> {
    /// An entry with the defaults this plugin uses.
    #[must_use]
    pub fn new(section: &'a str, identity_file: &'a str, port: u16) -> Self {
        Self {
            section,
            identity_file,
            port,
            user: "user",
            host_name: "127.0.0.1",
            header: "",
        }
    }
}

/// Insert or refresh the managed block of one entry.
///
/// Returns the new text and whether it differs from the input, so a host can
/// skip the write when nothing changed.
#[must_use]
pub fn upsert_block(text: &str, existed: bool, entry: &Entry<'_>) -> (String, bool) {
    let (section, identity_file, port, user, host_name, header) = (
        entry.section,
        entry.identity_file,
        entry.port,
        entry.user,
        entry.host_name,
        entry.header,
    );
    let (stripped, _) = strip_block(text, section);
    let rendered = format!(
        "{}\n",
        block(section, identity_file, port, user, host_name, true)
    );
    let base = collapse_blank_lines(&stripped);
    let prefix = if existed || !base.is_empty() {
        ""
    } else {
        header
    };
    let separator = if base.is_empty() || base.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let next = format!("{prefix}{base}{separator}{rendered}");
    let next = next.trim_end_matches('\n').to_string();
    let current = text.trim_end_matches('\n').to_string();
    let changed = next != current;
    (next, changed)
}

/// List the `Host` aliases this crate manages in a config text.
#[must_use]
pub fn managed_sections(text: &str) -> Vec<String> {
    const PREFIX: &str = "# >>> dsh-bas-remote ";
    text.split('\n')
        .filter_map(|line| {
            let trimmed = line.trim();
            trimmed.strip_prefix(PREFIX).map(str::to_string)
        })
        .collect()
}

/// Read the `Port` recorded in the managed block of `section`.
#[must_use]
pub fn managed_port(text: &str, section: &str) -> Option<u16> {
    let start = block_start(section);
    let end = block_end(section);
    let mut inside = false;
    for line in text.split('\n') {
        let trimmed = line.trim();
        if trimmed == start {
            inside = true;
            continue;
        }
        if inside && trimmed == end {
            return None;
        }
        if inside
            && let Some(rest) = trimmed.strip_prefix("Port ")
            && let Ok(port) = rest.trim().parse::<u16>()
        {
            return Some(port);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_with_and_without_markers() {
        let plain = block("h.example.ws-1", "/k", 44000, "user", "127.0.0.1", false);
        assert!(plain.starts_with("Host h.example.ws-1\n"));
        assert!(!plain.contains("# >>>"));
        let marked = block("s", "/k", 1, "user", "127.0.0.1", true);
        assert!(marked.starts_with("# >>> dsh-bas-remote s\n"));
        assert!(marked.ends_with("# <<< dsh-bas-remote s"));
    }

    #[test]
    fn upsert_is_idempotent_and_scoped() {
        let original = "Host other\n  HostName 1.2.3.4\n";
        let (once, changed) = upsert_block(original, true, &Entry::new("s", "/k", 100));
        assert!(changed);
        assert!(once.contains("Host other"));
        let (twice, changed_again) = upsert_block(&once, true, &Entry::new("s", "/k", 100));
        assert!(!changed_again, "a second upsert must be a no-op");
        assert_eq!(twice, once);
        let (updated, changed_port) = upsert_block(&once, true, &Entry::new("s", "/k", 200));
        assert!(changed_port);
        assert!(updated.contains("Port 200"));
        assert_eq!(updated.matches("Host s").count(), 1, "no duplicate block");
    }

    #[test]
    fn strip_removes_only_the_named_block() {
        let text = format!(
            "Host a\n{}\nHost b\n",
            block("s", "/k", 1, "user", "127.0.0.1", true)
        );
        let (stripped, removed) = strip_block(&text, "s");
        assert!(removed);
        assert!(!stripped.contains("dsh-bas-remote"));
        assert!(stripped.contains("Host a"));
        assert!(stripped.contains("Host b"));
        assert!(!strip_block(&text, "nope").1);
    }

    #[test]
    fn reads_back_what_it_wrote() {
        let (text, _) = upsert_block(
            "",
            false,
            &Entry {
                header: "# header\n",
                ..Entry::new("s", "/k", 4321)
            },
        );
        assert!(text.starts_with("# header"));
        assert_eq!(managed_port(&text, "s"), Some(4321));
        assert_eq!(managed_sections(&text), vec!["s".to_string()]);
    }

    #[test]
    fn header_only_on_creation() {
        let (created, _) = upsert_block(
            "",
            false,
            &Entry {
                header: "# H\n",
                ..Entry::new("s", "/k", 1)
            },
        );
        assert!(created.starts_with("# H"));
        let (second, _) = upsert_block(
            &created,
            true,
            &Entry {
                header: "# H\n",
                ..Entry::new("s2", "/k", 2)
            },
        );
        assert_eq!(second.matches("# H").count(), 1, "header not duplicated");
    }
}
